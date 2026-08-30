use anyhow::{anyhow, bail, Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use rusqlite::Connection;
use std::io::Write;
use std::time::Duration;

use crate::dates;
use crate::db::{self, Paper};

const BASE: &str = "https://eprint.iacr.org/oai";
const UA: &str = concat!(
    "eprint-cli/",
    env!("CARGO_PKG_VERSION"),
    " (OAI-PMH metadata harvester)"
);
/// Politeness delay between successive OAI-PMH pages.
const PAGE_DELAY: Duration = Duration::from_millis(200);

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// How far a harvest has got, for a front-end that is not the one printing the
/// progress line below.
///
/// Atomics rather than a callback because the reader is on another thread: a
/// caller with a progress bar polls these while `run` blocks somewhere inside
/// `fetch`. A callback would have to publish into exactly this kind of static
/// anyway, and would change `run`'s signature to get there. `Relaxed` is right —
/// they are a progress display, not a handshake.
pub static PROGRESS_SEEN: AtomicUsize = AtomicUsize::new(0);
/// `completeListSize` from the resumption token, or zero before the first page.
pub static PROGRESS_TOTAL: AtomicUsize = AtomicUsize::new(0);
pub static PROGRESS_RUNNING: AtomicBool = AtomicBool::new(false);

/// Ask a running harvest to stop at the next page boundary.
///
/// Needed by anything with a deadline: `fetch` retries four times and honours
/// `Retry-After`, so a server answering 503 can keep one call blocked for well
/// over a minute, and `thread::sleep` cannot be interrupted from outside.
///
/// Stopping between pages is safe, and that is not luck — each page is its own
/// transaction, and `KEY_LAST_HARVEST` is written only on a complete run. A
/// cancelled harvest therefore leaves rows behind but does not move the clock, so
/// the next run asks for the same window again. Moving the clock past data that
/// was never fetched is the one failure this file cannot recover from.
pub static CANCEL: AtomicBool = AtomicBool::new(false);

pub const KEY_LAST_HARVEST: &str = "last_harvest";
pub const KEY_LAST_ATTEMPT: &str = "last_attempt";
pub const KEY_LAST_SEEN: &str = "last_seen";
/// Start of the window that produced the last non-empty `eprint new`. The
/// archive posts in bursts, so most runs land on an empty diff; re-showing that
/// batch beats printing "nothing new" for the rest of the day.
pub const KEY_NEW_BATCH: &str = "new_batch_from";

struct Page {
    records: Vec<Record>,
    token: Option<String>,
    complete_size: Option<usize>,
    response_date: Option<String>,
}

enum Record {
    Live(Paper),
    Deleted(String),
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .user_agent(UA)
        .build()
}

fn fetch(agent: &ureq::Agent, url: &str) -> Result<String> {
    let mut last_err = None;
    for attempt in 0..4 {
        match agent.get(url).call() {
            Ok(resp) => return Ok(resp.into_string()?),
            Err(ureq::Error::Status(code, resp)) => {
                // OAI-PMH servers use 503 + Retry-After for flow control.
                let wait = resp
                    .header("Retry-After")
                    .and_then(|h| h.trim().parse::<u64>().ok())
                    .unwrap_or(2 << attempt);
                last_err = Some(anyhow!("HTTP {code} from {url}"));
                if code == 503 || code == 429 {
                    std::thread::sleep(Duration::from_secs(wait.min(30)));
                    continue;
                }
                break;
            }
            Err(e) => {
                last_err = Some(anyhow!(e));
                std::thread::sleep(Duration::from_secs(2 << attempt));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("request failed: {url}")))
}

fn text_of(id: &str) -> String {
    id.rsplit(':').next().unwrap_or(id).to_string()
}

fn parse_page(xml: &str) -> Result<Page> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut page = Page {
        records: Vec::new(),
        token: None,
        complete_size: None,
        response_date: None,
    };

    let mut buf = Vec::new();
    let mut tag = String::new();
    let mut in_record = false;
    let mut in_header = false;
    let mut deleted = false;
    let mut in_token = false;
    let mut in_response_date = false;

    let mut oai_id = String::new();
    let mut title = String::new();
    let mut creators: Vec<String> = Vec::new();
    let mut description = String::new();
    let mut subject = String::new();
    let mut date = String::new();
    let mut rights = String::new();
    let mut url = String::new();
    let mut error: Option<String> = None;
    let mut in_error = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match name.as_str() {
                    "record" => {
                        in_record = true;
                        deleted = false;
                        oai_id.clear();
                        title.clear();
                        creators.clear();
                        description.clear();
                        subject.clear();
                        date.clear();
                        rights.clear();
                        url.clear();
                    }
                    "header" => {
                        in_header = true;
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"status" && attr.value.as_ref() == b"deleted" {
                                deleted = true;
                            }
                        }
                    }
                    "resumptionToken" => {
                        in_token = true;
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"completeListSize" {
                                page.complete_size =
                                    String::from_utf8_lossy(&attr.value).parse().ok();
                            }
                        }
                    }
                    "responseDate" => in_response_date = true,
                    "error" => in_error = true,
                    _ => {}
                }
                tag = name;
            }
            Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if name == "resumptionToken" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"completeListSize" {
                            page.complete_size = String::from_utf8_lossy(&attr.value).parse().ok();
                        }
                    }
                }
                if name == "header" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"status" && attr.value.as_ref() == b"deleted" {
                            deleted = true;
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                let val = t.unescape().unwrap_or_default().to_string();
                if in_error {
                    error = Some(val);
                } else if in_response_date {
                    page.response_date = Some(val);
                } else if in_token {
                    if !val.is_empty() {
                        page.token = Some(val);
                    }
                } else if in_record {
                    match tag.as_str() {
                        "identifier" if in_header => oai_id = text_of(&val),
                        "identifier" => {
                            if val.starts_with("http") && url.is_empty() {
                                url = val;
                            }
                        }
                        "title" => title = val,
                        "creator" => creators.push(val),
                        "description" => {
                            if val.len() > description.len() {
                                description = val;
                            }
                        }
                        "subject" => {
                            if subject.is_empty() {
                                subject = val;
                            }
                        }
                        "date" => {
                            if date.is_empty() {
                                date = val;
                            }
                        }
                        "rights" => rights = val,
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match name.as_str() {
                    "header" => in_header = false,
                    "resumptionToken" => in_token = false,
                    "responseDate" => in_response_date = false,
                    "error" => in_error = false,
                    "record" => {
                        in_record = false;
                        if oai_id.is_empty() {
                            // nothing usable
                        } else if deleted {
                            page.records.push(Record::Deleted(oai_id.clone()));
                        } else {
                            let year = date
                                .get(0..4)
                                .and_then(|y| y.parse::<i64>().ok())
                                .or_else(|| oai_id.split('/').next().and_then(|y| y.parse().ok()))
                                .unwrap_or(0);
                            let final_url = if url.is_empty() {
                                format!("https://eprint.iacr.org/{oai_id}")
                            } else {
                                url.clone()
                            };
                            page.records.push(Record::Live(Paper {
                                id: oai_id.clone(),
                                title: title.trim().to_string(),
                                // One spelling per person, decided on the way in —
                                // see `names::PEOPLE`. Doing it here is what makes
                                // a first-time download correct without a repair.
                                authors: crate::names::canonical_byline(&creators.join("; ")),
                                abstract_: description.trim().to_string(),
                                category: subject.trim().to_string(),
                                date: date.clone(),
                                year,
                                rights: rights.clone(),
                                url: final_url,
                            }));
                        }
                    }
                    _ => {}
                }
                tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => bail!("malformed XML from OAI-PMH endpoint: {e}"),
            _ => {}
        }
        buf.clear();
    }

    if let Some(err) = error {
        // `noRecordsMatch` just means the incremental window was empty.
        if err.contains("no records") || err.to_lowercase().contains("norecordsmatch") {
            return Ok(page);
        }
        bail!("OAI-PMH error: {err}");
    }
    Ok(page)
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Harvest metadata into the local index.
///
/// `from` is an ISO-8601 timestamp for an incremental run; `None` harvests
/// the full archive back to 1996.
/// Refresh the index in the background when it is older than this.
pub const STALE_SECS: u64 = 24 * 3600;

/// Re-request a small overlap window so nothing slips through the cracks.
pub const OVERLAP_SECS: u64 = 2 * 24 * 3600;

/// Where an incremental harvest should start: the **earlier** of the last harvest
/// and the newest record actually indexed, less the re-request window.
///
/// Taking the harvest clock alone opens a hole that never heals. The watermark is
/// the server's `responseDate`, so a harvest that comes back empty still advances
/// it; once it has run ahead of the data, every later run asks about a window that
/// starts after the records it is missing, and `OVERLAP_SECS` is far too short to
/// reach back. That is not hypothetical — it cost this index 2026/1540 to 1575, a
/// week of papers, while `status` reported a harvest minutes old.
///
/// Bounding it by `MAX(papers.date)` makes the request describe the data rather
/// than the clock, so a gap of any size closes itself on the next update. The
/// harvest clock still bounds the other side: a paper dated in the future must not
/// be able to push the window forward.
pub fn incremental_from(last_harvest: Option<&str>, newest: Option<&str>) -> Option<String> {
    let at = |s: Option<&str>| s.and_then(dates::parse_iso);
    let (harvested, newest) = (at(last_harvest), at(newest));
    let start = match (harvested, newest) {
        (Some(h), Some(n)) => h.min(n),
        (Some(h), None) => h,
        // Nothing harvested yet: the caller asks for everything.
        (None, _) => return None,
    };
    Some(dates::format_iso(start - OVERLAP_SECS as i64))
}

pub fn index_age(conn: &Connection) -> Result<Option<i64>> {
    Ok(db::meta_get(conn, KEY_LAST_HARVEST)?
        .and_then(|v| dates::parse_iso(&v))
        .map(|t| (dates::now() - t).max(0)))
}

pub fn run(conn: &mut Connection, from: Option<&str>, quiet: bool, now: &str) -> Result<usize> {
    let agent = agent();
    let mut url = match from {
        Some(f) => format!(
            "{BASE}?verb=ListRecords&metadataPrefix=oai_dc&from={}",
            url_encode(f)
        ),
        None => format!("{BASE}?verb=ListRecords&metadataPrefix=oai_dc"),
    };

    // Cleared on every exit path, including the `?`s below.
    struct Running;
    impl Drop for Running {
        fn drop(&mut self) {
            PROGRESS_RUNNING.store(false, Ordering::Relaxed);
        }
    }
    PROGRESS_SEEN.store(0, Ordering::Relaxed);
    PROGRESS_TOTAL.store(0, Ordering::Relaxed);
    CANCEL.store(false, Ordering::Relaxed);
    PROGRESS_RUNNING.store(true, Ordering::Relaxed);
    let _running = Running;

    let mut seen = 0usize;
    let mut changed = 0usize;
    let mut cancelled = false;
    let mut total: Option<usize> = None;
    let mut response_date: Option<String> = None;
    let mut first = true;

    loop {
        if CANCEL.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        if !first {
            std::thread::sleep(PAGE_DELAY);
        }
        let xml = fetch(&agent, &url).context("fetching OAI-PMH page")?;
        let page = parse_page(&xml)?;
        if first {
            total = page.complete_size;
            response_date = page.response_date.clone();
            first = false;
        }

        let tx = conn.transaction()?;
        for rec in &page.records {
            match rec {
                Record::Live(p) => {
                    db::upsert(&tx, p, now)?;
                    changed += 1;
                }
                Record::Deleted(id) => {
                    db::delete(&tx, id)?;
                }
            }
            seen += 1;
        }
        tx.commit()?;
        PROGRESS_SEEN.store(seen, Ordering::Relaxed);
        PROGRESS_TOTAL.store(total.unwrap_or(0), Ordering::Relaxed);

        if !quiet {
            match total {
                Some(t) if t > 0 => {
                    let pct = (seen as f64 / t as f64 * 100.0).min(100.0);
                    eprint!("\r  harvesting… {seen}/{t} ({pct:.0}%)");
                }
                _ => eprint!("\r  harvesting… {seen}"),
            }
            let _ = std::io::stderr().flush();
        }

        match page.token {
            Some(tok) => {
                url = format!(
                    "{BASE}?verb=ListRecords&resumptionToken={}",
                    url_encode(&tok)
                );
            }
            None => break,
        }
    }

    if !quiet && seen > 0 {
        eprintln!();
    }

    // Record the server's own clock, not ours, so the next incremental run
    // cannot miss records because of clock skew.
    //
    // Skipped when the run was cancelled, and that is the whole safety of being
    // cancellable: the watermark is a clock, and a clock that runs ahead of the
    // data leaves a hole no later `from=` window can reach back into.
    if let Some(rd) = response_date {
        if !cancelled {
            db::meta_set(conn, KEY_LAST_HARVEST, &rd)?;
        }
    }
    let _ = changed;
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_incremental_window_never_outruns_the_data() {
        // The bug this exists for: a harvest that comes back empty still advances
        // the watermark, so the window marched past 2026/1540-1575 and no later run
        // could reach back. The window has to describe the data, not the clock.
        let day = 24 * 3600;
        let harvested = dates::format_iso(30 * day);
        let newest = dates::format_iso(10 * day);
        assert_eq!(
            incremental_from(Some(&harvested), Some(&newest)),
            Some(dates::format_iso(10 * day - OVERLAP_SECS as i64)),
            "the newest record bounds it, not the harvest clock"
        );
        // Up to date: the harvest clock is the earlier of the two and still wins,
        // so the usual case asks about two days rather than everything since.
        let newest = dates::format_iso(40 * day);
        assert_eq!(
            incremental_from(Some(&harvested), Some(&newest)),
            Some(dates::format_iso(30 * day - OVERLAP_SECS as i64)),
            "a paper dated in the future must not push the window forward"
        );
        // An empty index has nothing to bound, and no harvest means a full one.
        assert_eq!(
            incremental_from(Some(&harvested), None),
            Some(dates::format_iso(30 * day - OVERLAP_SECS as i64))
        );
        assert_eq!(incremental_from(None, Some(&newest)), None);
    }
}
