use anyhow::{anyhow, bail, Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use rusqlite::Connection;
use std::io::Write;
use std::time::Duration;

use crate::db::{self, Paper};

const BASE: &str = "https://eprint.iacr.org/oai";
const UA: &str = concat!("eprint-cli/", env!("CARGO_PKG_VERSION"), " (OAI-PMH metadata harvester)");
/// Politeness delay between successive OAI-PMH pages.
const PAGE_DELAY: Duration = Duration::from_millis(200);

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
                            if attr.key.as_ref() == b"status"
                                && attr.value.as_ref() == b"deleted"
                            {
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
                                .or_else(|| {
                                    oai_id.split('/').next().and_then(|y| y.parse().ok())
                                })
                                .unwrap_or(0);
                            let final_url = if url.is_empty() {
                                format!("https://eprint.iacr.org/{oai_id}")
                            } else {
                                url.clone()
                            };
                            page.records.push(Record::Live(Paper {
                                id: oai_id.clone(),
                                title: title.trim().to_string(),
                                authors: creators.join("; "),
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
pub fn run(conn: &mut Connection, from: Option<&str>, quiet: bool, now: &str) -> Result<usize> {
    let agent = agent();
    let mut url = match from {
        Some(f) => format!(
            "{BASE}?verb=ListRecords&metadataPrefix=oai_dc&from={}",
            url_encode(f)
        ),
        None => format!("{BASE}?verb=ListRecords&metadataPrefix=oai_dc"),
    };

    let mut seen = 0usize;
    let mut changed = 0usize;
    let mut total: Option<usize> = None;
    let mut response_date: Option<String> = None;
    let mut first = true;

    loop {
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
                url = format!("{BASE}?verb=ListRecords&resumptionToken={}", url_encode(&tok));
            }
            None => break,
        }
    }

    if !quiet && seen > 0 {
        eprintln!();
    }

    // Record the server's own clock, not ours, so the next incremental run
    // cannot miss records because of clock skew.
    if let Some(rd) = response_date {
        db::meta_set(conn, KEY_LAST_HARVEST, &rd)?;
    }
    let _ = changed;
    Ok(seen)
}
