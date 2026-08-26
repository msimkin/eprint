use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::time::Duration;

use crate::db;

const URL: &str = "https://cryptobib.di.ens.fr/cryptobib/static/files/crypto.bib";
/// Venue names, publishers and editors live here as @String macros; entries in
/// crypto.bib reference them by bare identifier, so a record is not usable on
/// its own until they are substituted in.
const ABBREV_URL: &str = "https://cryptobib.di.ens.fr/cryptobib/static/files/abbrev3.bib";
const UA: &str = concat!(
    "eprint-cli/",
    env!("CARGO_PKG_VERSION"),
    " (CryptoBib client)"
);

pub const KEY_ETAG: &str = "bib_etag";
pub const KEY_UPDATED: &str = "bib_updated";

pub enum Outcome {
    UpToDate,
    Rebuilt {
        entries: usize,
        linked: usize,
        published: usize,
    },
}

// ---------- BibTeX parsing ----------

struct Raw {
    key: String,
    title: String,
    author: String,
    year: String,
    howpublished: String,
    /// Byte range of the complete `@type{...}` record in the source text, so
    /// the raw entry can be stored without duplicating it in memory.
    start: usize,
    end: usize,
}

/// Locate `name = ` case-insensitively at a field boundary; returns the index
/// just past the `=`.
fn find_field(body: &str, name: &str) -> Option<usize> {
    let b = body.as_bytes();
    let n = name.as_bytes();
    if b.len() < n.len() {
        return None;
    }
    for i in 0..=(b.len() - n.len()) {
        if !b[i..i + n.len()].eq_ignore_ascii_case(n) {
            continue;
        }
        let before_ok = i == 0 || b[i - 1].is_ascii_whitespace() || b[i - 1] == b',';
        if !before_ok {
            continue;
        }
        let mut k = i + n.len();
        while k < b.len() && b[k].is_ascii_whitespace() {
            k += 1;
        }
        if k < b.len() && b[k] == b'=' {
            return Some(k + 1);
        }
    }
    None
}

fn read_value(body: &str, start: usize) -> String {
    let b = body.as_bytes();
    let mut i = start;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= b.len() {
        return String::new();
    }
    if b[i] == b'{' {
        let mut depth = 0usize;
        for j in i..b.len() {
            match b[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return body[i + 1..j].to_string();
                    }
                }
                _ => {}
            }
        }
    } else if b[i] == b'"' {
        if let Some(off) = body[i + 1..].find('"') {
            return body[i + 1..i + 1 + off].to_string();
        }
    }
    String::new()
}

fn field(body: &str, name: &str) -> String {
    find_field(body, name)
        .map(|i| read_value(body, i))
        .unwrap_or_default()
}

/// Braces are ASCII, so byte indices are always char boundaries here.
fn parse_entries(text: &str) -> Vec<Raw> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if !(b[i] == b'@' && (i == 0 || b[i - 1] == b'\n')) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && b[j] != b'{' {
            j += 1;
        }
        if j >= b.len() {
            break;
        }
        let open = j;
        let mut depth = 0usize;
        let mut end = b.len();
        while j < b.len() {
            match b[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = j;
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let body = &text[open + 1..end.min(text.len())];
        let key = body.split(',').next().unwrap_or("").trim().to_string();
        if !key.is_empty() {
            out.push(Raw {
                key,
                title: field(body, "title"),
                author: field(body, "author"),
                year: field(body, "year"),
                howpublished: field(body, "howpublished"),
                start: i,
                end: (end + 1).min(text.len()),
            });
        }
        i = end + 1;
    }
    out
}

// ---------- @String macros ----------

/// Raw (still unresolved) `@String{name = value}` definitions.
fn parse_strings(text: &str) -> HashMap<String, String> {
    let b = text.as_bytes();
    let mut out = HashMap::new();
    let mut i = 0usize;
    while i < b.len() {
        if !(b[i] == b'@' && (i == 0 || b[i - 1] == b'\n')) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && b[j] != b'{' {
            j += 1;
        }
        if j >= b.len() {
            break;
        }
        let kind = text[i + 1..j].trim();
        let open = j;
        let mut depth = 0usize;
        let mut end = b.len();
        while j < b.len() {
            match b[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = j;
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        if kind.eq_ignore_ascii_case("string") {
            let body = &text[open + 1..end.min(text.len())];
            if let Some((name, value)) = body.split_once('=') {
                out.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        i = end + 1;
    }
    out
}

enum Tok {
    Lit(String),
    Name(String),
    Num(String),
}

/// Split a BibTeX value into `#`-concatenated tokens.
fn value_tokens(s: &str) -> Vec<Tok> {
    let b = s.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0usize;
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'#') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        if b[i] == b'"' {
            let mut depth = 0usize;
            let mut j = i + 1;
            while j < b.len() {
                match b[j] {
                    b'{' => depth += 1,
                    b'}' => depth = depth.saturating_sub(1),
                    b'"' if depth == 0 => break,
                    _ => {}
                }
                j += 1;
            }
            toks.push(Tok::Lit(s[i + 1..j.min(s.len())].to_string()));
            i = j + 1;
        } else if b[i] == b'{' {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            toks.push(Tok::Lit(s[i + 1..j.min(s.len())].to_string()));
            i = j + 1;
        } else if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            toks.push(Tok::Num(s[start..i].to_string()));
        } else {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'-' || b[i] == b':')
            {
                i += 1;
            }
            if i == start {
                i += 1;
                continue;
            }
            toks.push(Tok::Name(s[start..i].to_string()));
        }
    }
    toks
}

/// Expand a value to a literal string, following macro references.
/// `depth` guards against a definition cycle in the source data.
fn expand(value: &str, macros: &HashMap<String, String>, depth: u32) -> String {
    if depth > 12 {
        return String::new();
    }
    let mut out = String::new();
    for t in value_tokens(value) {
        match t {
            Tok::Lit(s) => out.push_str(&s),
            Tok::Num(n) => out.push_str(&n),
            Tok::Name(n) => match macros.get(&n.to_ascii_lowercase()) {
                Some(raw) => out.push_str(&expand(raw, macros, depth + 1)),
                // Unknown macro: keep the identifier rather than silently
                // dropping information.
                None => out.push_str(&n),
            },
        }
    }
    out
}

/// Re-emit one record with every macro reference substituted, so the result
/// compiles on its own without abbrev3.bib.
fn rebuild_entry(entry: &str, macros: &HashMap<String, String>) -> String {
    let b = entry.as_bytes();
    let mut i = 0usize;
    while i < b.len() && b[i] != b'{' {
        i += 1;
    }
    let head = entry[..i].trim();
    let body_end = entry.rfind('}').unwrap_or(entry.len());
    let body = &entry[(i + 1).min(entry.len())..body_end];
    let (key, rest) = match body.split_once(',') {
        Some((k, r)) => (k.trim(), r),
        None => (body.trim(), ""),
    };

    let mut out = format!("{head}{{{key},\n");
    let rb = rest.as_bytes();
    let mut p = 0usize;
    while p < rb.len() {
        while p < rb.len() && (rb[p].is_ascii_whitespace() || rb[p] == b',') {
            p += 1;
        }
        let name_start = p;
        while p < rb.len() && (rb[p].is_ascii_alphanumeric() || rb[p] == b'_') {
            p += 1;
        }
        if p == name_start {
            break;
        }
        let name = &rest[name_start..p];
        while p < rb.len() && rb[p].is_ascii_whitespace() {
            p += 1;
        }
        if p >= rb.len() || rb[p] != b'=' {
            break;
        }
        p += 1;
        // Consume the value: tokens until a top-level comma.
        let val_start = p;
        let mut depth = 0usize;
        let mut in_quote = false;
        while p < rb.len() {
            match rb[p] {
                b'"' if depth == 0 => in_quote = !in_quote,
                b'{' => depth += 1,
                b'}' => depth = depth.saturating_sub(1),
                b',' if depth == 0 && !in_quote => break,
                _ => {}
            }
            p += 1;
        }
        let raw = &rest[val_start..p.min(rest.len())];
        let expanded = expand(raw, macros, 0);
        if expanded.is_empty() {
            continue;
        }
        // Numbers, and BibTeX's twelve built-in month macros, must stay
        // unbraced — bracing a month turns the macro into the literal "aug".
        const MONTHS: [&str; 12] = [
            "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
        ];
        let bare =
            expanded.chars().all(|c| c.is_ascii_digit()) || MONTHS.contains(&expanded.as_str());
        if bare {
            out.push_str(&format!("  {name:<13} = {expanded},\n"));
        } else {
            out.push_str(&format!("  {name:<13} = {{{expanded}}},\n"));
        }
    }
    out.push_str("}\n");
    out
}

// ---------- normalisation ----------

fn norm_title(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // drop a LaTeX control word
            while matches!(chars.peek(), Some(d) if d.is_ascii_alphabetic()) {
                chars.next();
            }
            out.push(' ');
        } else if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn surnames(authors: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in authors.split(" and ") {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let name = if let Some((last, _)) = p.split_once(',') {
            norm_title(last)
        } else {
            p.split_whitespace()
                .last()
                .map(norm_title)
                .unwrap_or_default()
        };
        if !name.is_empty() {
            out.push(name);
        }
    }
    out
}

/// `Cryptology ePrint Archive, Report 2025/001` -> `2025/001`
fn eprint_id_of(howpublished: &str) -> Option<String> {
    let b = howpublished.as_bytes();
    for i in 0..b.len() {
        if !b[i].is_ascii_digit() {
            continue;
        }
        let year_end = i + 4;
        if year_end + 1 < b.len()
            && b[i..year_end].iter().all(|c| c.is_ascii_digit())
            && b[year_end] == b'/'
            && b[year_end + 1].is_ascii_digit()
        {
            let mut j = year_end + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            return Some(howpublished[i..j].to_string());
        }
    }
    None
}

// ---------- fetch ----------

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(300))
        .user_agent(UA)
        .build()
}

/// Returns None when the server reports the cached copy is still current.
fn download(etag: Option<&str>, quiet: bool) -> Result<Option<(String, Option<String>)>> {
    let agent = agent();
    let mut req = agent.get(URL);
    if let Some(tag) = etag {
        req = req.set("If-None-Match", tag);
    }
    let resp = match req.call() {
        Ok(r) => r,
        // Defensive: ureq only maps >= 400 to Err, but be explicit anyway.
        Err(ureq::Error::Status(304, _)) => return Ok(None),
        Err(e) => return Err(anyhow!(e)).context("fetching crypto.bib"),
    };
    // 304 is a success status as far as ureq is concerned, so it arrives here
    // with an empty body. Treating that as a download would erase the table.
    if resp.status() == 304 {
        return Ok(None);
    }
    let new_etag = resp.header("ETag").map(|s| s.to_string());
    let total: usize = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Progress needs a terminal to rewrite the line; piped output would
    // otherwise collect hundreds of copies of it.
    let show = !quiet && std::io::stderr().is_terminal() && total > 0;
    let mut reader = resp.into_reader();
    let mut buf = Vec::with_capacity(total.max(1 << 20));
    let mut chunk = vec![0u8; 1 << 16];
    let mut read_total = 0usize;
    let mut next_tick = 0usize;
    loop {
        let n = reader.read(&mut chunk).context("reading crypto.bib")?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        read_total += n;
        if show && read_total >= next_tick {
            next_tick = read_total + (2 << 20);
            eprint!(
                "\r  downloading crypto.bib… {:.0}% ({:.0} MB)",
                read_total as f64 / total as f64 * 100.0,
                read_total as f64 / 1e6
            );
            let _ = std::io::stderr().flush();
        }
    }
    if show {
        eprintln!(
            "\r  downloaded crypto.bib ({:.0} MB)   ",
            read_total as f64 / 1e6
        );
    }
    Ok(Some((String::from_utf8_lossy(&buf).into_owned(), new_etag)))
}

// ---------- build ----------

pub fn update(conn: &mut Connection, force: bool, quiet: bool, now: &str) -> Result<Outcome> {
    // Only send the validator when we actually have a usable table to keep.
    let have_rows = db::bib_count(conn)? > 0;
    let etag = if force || !have_rows {
        None
    } else {
        db::meta_get(conn, KEY_ETAG)?
    };

    let Some((text, new_etag)) = download(etag.as_deref(), quiet)? else {
        return Ok(Outcome::UpToDate);
    };

    // Records reference venue/publisher macros by name, so fetch the
    // definitions too and inline them; otherwise a copied entry will not
    // compile on its own.
    if !quiet {
        eprint!("  fetching abbreviations…");
        let _ = std::io::stderr().flush();
    }
    let abbrev = agent()
        .get(ABBREV_URL)
        .call()
        .context("fetching abbrev3.bib")?
        .into_string()
        .context("reading abbrev3.bib")?;
    let macros = parse_strings(&abbrev);

    if !quiet {
        eprint!(" parsing…");
        let _ = std::io::stderr().flush();
    }
    let entries = parse_entries(&text);
    // Never replace a populated table with nothing; a truncated or unexpected
    // response should fail loudly rather than quietly destroy the index.
    if entries.is_empty() {
        return Err(anyhow!(
            "crypto.bib parsed to zero entries — refusing to overwrite existing data"
        ));
    }

    // Published entries indexed by normalised title.
    let mut by_title: HashMap<String, Vec<(usize, Vec<String>, i64)>> = HashMap::new();
    let mut eprints: Vec<(String, usize)> = Vec::new();
    for (idx, e) in entries.iter().enumerate() {
        if e.title.is_empty() {
            continue;
        }
        if e.key.starts_with("EPRINT:") {
            if let Some(id) = eprint_id_of(&e.howpublished) {
                eprints.push((id, idx));
            }
        } else {
            let year = e.year.trim().parse::<i64>().unwrap_or(0);
            by_title.entry(norm_title(&e.title)).or_default().push((
                idx,
                surnames(&e.author),
                year,
            ));
        }
    }

    if !quiet {
        eprint!(" matching…");
        let _ = std::io::stderr().flush();
    }

    let tx = conn.transaction()?;
    tx.execute("DELETE FROM bib", [])?;
    let mut linked = 0usize;
    let mut published = 0usize;
    for (id, idx) in &eprints {
        let e = &entries[*idx];
        db::bib_insert(
            &tx,
            id,
            &e.key,
            "eprint",
            &e.year,
            &rebuild_entry(&text[e.start..e.end], &macros),
        )?;
        linked += 1;

        // A shared author surname guards against title collisions between
        // unrelated papers; without it precision drops noticeably.
        let want = surnames(&e.author);
        let eyear = e.year.trim().parse::<i64>().unwrap_or(0);
        if let Some(cands) = by_title.get(&norm_title(&e.title)) {
            let mut best: Option<(i64, usize)> = None;
            for (pidx, psn, pyear) in cands {
                if !psn.iter().any(|s| want.contains(s)) {
                    continue;
                }
                let dist = if eyear > 0 && *pyear > 0 {
                    (pyear - eyear).abs()
                } else {
                    99
                };
                best = match best {
                    Some(b)
                        if (b.0, entries[b.1].key.as_str())
                            <= (dist, entries[*pidx].key.as_str()) =>
                    {
                        Some(b)
                    }
                    _ => Some((dist, *pidx)),
                };
            }
            if let Some((_, pidx)) = best {
                let p = &entries[pidx];
                db::bib_insert(
                    &tx,
                    id,
                    &p.key,
                    "published",
                    &p.year,
                    &rebuild_entry(&text[p.start..p.end], &macros),
                )?;
                published += 1;
            }
        }
    }
    if let Some(tag) = &new_etag {
        db::meta_set(&tx, KEY_ETAG, tag)?;
    }
    db::meta_set(&tx, KEY_UPDATED, now)?;
    tx.commit()?;

    if !quiet {
        eprintln!();
    }
    Ok(Outcome::Rebuilt {
        entries: entries.len(),
        linked,
        published,
    })
}
