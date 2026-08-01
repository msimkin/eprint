use anyhow::{bail, Context, Result};
use rusqlite::{Connection, ToSql};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// A single ePrint paper as exposed by the OAI-PMH `oai_dc` feed.
#[derive(Debug, Clone)]
pub struct Paper {
    pub id: String,
    pub title: String,
    pub authors: String,
    pub abstract_: String,
    pub category: String,
    pub date: String,
    pub year: i64,
    pub rights: String,
    pub url: String,
}

/// A search result: the paper, plus its title and abstract with the matches
/// marked. Marks are \x01/\x02 rather than ANSI so each front-end picks its own
/// styling and piped output stays clean.
pub struct Hit {
    pub paper: Paper,
    /// The complete abstract with match markers, used by `-a` and by `browse`
    /// when an abstract is expanded.
    pub abstract_hl: String,
    /// The title with match markers.
    pub title_hl: String,
}

/// Which columns a query is matched against. Authors are always included —
/// narrowing means "not the abstract".
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Scope {
    All,
    Title,
}

impl Scope {
    pub fn from_str(s: &str) -> Scope {
        match s.trim().to_ascii_lowercase().as_str() {
            "title" | "titles" => Scope::Title,
            _ => Scope::All,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            // Authors are searched in both modes, so name them explicitly;
            // the "in:" prefix marks this as *where* the query is matched.
            Scope::All => "in: title, authors, abstract",
            Scope::Title => "in: title, authors",
        }
    }
}

/// FTS5 column-filter syntax restricts a whole expression to a column set.
fn scoped(expr: &str, scope: Scope) -> String {
    match scope {
        Scope::All => expr.to_string(),
        Scope::Title => format!("{{title authors}} : ({expr})"),
    }
}

/// Ceiling on how many ids one watch contributes when deciding which rows to badge.
/// Far above any realistic watch, and only a guard against a pathological one.
const WATCH_MATCH_CAP: usize = 50_000;

pub const MARK_START: char = '\x01';
pub const MARK_END: char = '\x02';

pub fn db_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("EPRINT_DB") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::data_dir().context("could not determine a data directory")?;
    Ok(base.join("eprint").join("eprint.db"))
}

pub fn open() -> Result<Connection> {
    let path = db_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS papers (
  id       TEXT PRIMARY KEY,
  title    TEXT NOT NULL DEFAULT '',
  authors  TEXT NOT NULL DEFAULT '',
  abstract TEXT NOT NULL DEFAULT '',
  category TEXT NOT NULL DEFAULT '',
  date     TEXT NOT NULL DEFAULT '',
  year     INTEGER NOT NULL DEFAULT 0,
  rights   TEXT NOT NULL DEFAULT '',
  url      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS papers_year_idx ON papers(year);
CREATE INDEX IF NOT EXISTS papers_date_idx ON papers(date);

CREATE VIRTUAL TABLE IF NOT EXISTS papers_fts USING fts5(
  title, authors, abstract, category,
  content='papers', content_rowid='rowid', tokenize='porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS papers_ai AFTER INSERT ON papers BEGIN
  INSERT INTO papers_fts(rowid, title, authors, abstract, category)
  VALUES (new.rowid, new.title, new.authors, new.abstract, new.category);
END;
CREATE TRIGGER IF NOT EXISTS papers_ad AFTER DELETE ON papers BEGIN
  INSERT INTO papers_fts(papers_fts, rowid, title, authors, abstract, category)
  VALUES ('delete', old.rowid, old.title, old.authors, old.abstract, old.category);
END;
CREATE TRIGGER IF NOT EXISTS papers_au AFTER UPDATE ON papers BEGIN
  INSERT INTO papers_fts(papers_fts, rowid, title, authors, abstract, category)
  VALUES ('delete', old.rowid, old.title, old.authors, old.abstract, old.category);
  INSERT INTO papers_fts(rowid, title, authors, abstract, category)
  VALUES (new.rowid, new.title, new.authors, new.abstract, new.category);
END;

CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);

-- CryptoBib citation keys. At most one 'eprint' and one 'published' row per
-- paper, so the primary key doubles as the "best match wins" constraint.
CREATE TABLE IF NOT EXISTS bib (
  eprint_id TEXT NOT NULL,
  key       TEXT NOT NULL,
  kind      TEXT NOT NULL,
  year      TEXT NOT NULL DEFAULT '',
  entry     TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (eprint_id, kind)
);

-- Saved searches are *not* here: they live in the config file, so that copying it
-- carries a whole setup to another machine. `legacy_watches` reads the table an
-- older build created, once, and then drops it.

-- Scratch space for `browse`'s watched-only filter: the union of every watch's
-- matches, so the filter is one `IN (SELECT …)` instead of thousands of bound
-- parameters. TEMP, so it lives and dies with the connection and never touches
-- the stored schema.
CREATE TEMP TABLE IF NOT EXISTS watched_stage (id TEXT PRIMARY KEY);
"#,
    )?;
    migrate(conn)?;
    Ok(())
}

/// `added` records when a paper first entered *this* index, which is what
/// `eprint new` needs — a paper's own date can predate its arrival here, so
/// filtering on that would silently skip late-published submissions.
fn migrate(conn: &Connection) -> Result<()> {
    let has_added: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('papers') WHERE name = 'added'",
        [],
        |r| r.get(0),
    )?;
    if has_added == 0 {
        conn.execute_batch(
            "ALTER TABLE papers ADD COLUMN added TEXT NOT NULL DEFAULT '';
             UPDATE papers SET added = date WHERE added = '';",
        )?;
    }
    let has_entry: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('bib') WHERE name = 'entry'",
        [],
        |r| r.get(0),
    )?;
    if has_entry == 0 {
        conn.execute_batch("ALTER TABLE bib ADD COLUMN entry TEXT NOT NULL DEFAULT '';")?;
    }
    Ok(())
}

pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT v FROM meta WHERE k = ?1")?;
    let mut rows = stmt.query([key])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get(0)?),
        None => None,
    })
}

pub fn meta_set(conn: &Connection, key: &str, val: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(k, v) VALUES (?1, ?2)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        [key, val],
    )?;
    Ok(())
}

pub fn bib_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM bib", [], |r| r.get(0))?)
}

pub fn bib_insert(
    conn: &Connection,
    id: &str,
    key: &str,
    kind: &str,
    year: &str,
    entry: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO bib (eprint_id, key, kind, year, entry) VALUES (?1,?2,?3,?4,?5)
         ON CONFLICT(eprint_id, kind) DO UPDATE SET
           key = excluded.key, year = excluded.year, entry = excluded.entry",
        [id, key, kind, year, entry],
    )?;
    Ok(())
}

/// Full BibTeX record for one paper, preferring the published version.
/// Returns (key, entry text, is_published).
pub fn bib_entry(conn: &Connection, id: &str) -> Result<Option<(String, String, bool)>> {
    let mut stmt = conn.prepare(
        "SELECT key, entry, kind FROM bib WHERE eprint_id = ?1
         ORDER BY CASE kind WHEN 'published' THEN 0 ELSE 1 END LIMIT 1",
    )?;
    let mut rows = stmt.query([id])?;
    Ok(match rows.next()? {
        Some(r) => {
            let key: String = r.get(0)?;
            let entry: String = r.get(1)?;
            let kind: String = r.get(2)?;
            Some((key, entry, kind == "published"))
        }
        None => None,
    })
}

/// Citation key for one paper: the published version when known, otherwise the
/// ePrint entry. The bool reports whether it is a published-version key.
pub fn bib_for(conn: &Connection, id: &str) -> Result<Option<(String, bool)>> {
    let mut stmt = conn.prepare(
        "SELECT key, kind FROM bib WHERE eprint_id = ?1
         ORDER BY CASE kind WHEN 'published' THEN 0 ELSE 1 END LIMIT 1",
    )?;
    let mut rows = stmt.query([id])?;
    Ok(match rows.next()? {
        Some(r) => {
            let key: String = r.get(0)?;
            let kind: String = r.get(1)?;
            Some((key, kind == "published"))
        }
        None => None,
    })
}

/// Batch form for result lists, so rendering does not issue a query per row.
/// Titles for a set of ids, in one query. `pdf::title_of` opens the database per
/// call, which is fine for one paper and wrong for a whole library.
pub fn titles(conn: &Connection, ids: &[String]) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let slots = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT id, title FROM papers WHERE id IN ({slots})");
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn ToSql> = ids.iter().map(|s| s as &dyn ToSql).collect();
    let rows = stmt.query_map(params.as_slice(), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, title) = row?;
        out.insert(id, title);
    }
    Ok(out)
}

/// The categories actually present in the index, with how many papers carry each,
/// commonest first. Read from the data rather than hard-coded: the archive owns
/// this list, so a category it adds should appear here without a release. Papers
/// with no category at all are left out — there is nothing to type for them.
pub fn categories(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT category, COUNT(*) FROM papers
         WHERE category <> '' GROUP BY category ORDER BY 2 DESC, 1",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn bib_map(conn: &Connection, ids: &[String]) -> Result<HashMap<String, (String, bool)>> {
    let mut out = HashMap::new();
    if ids.is_empty() || bib_count(conn)? == 0 {
        return Ok(out);
    }
    let placeholders = std::iter::repeat("?")
        .take(ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT eprint_id, key, kind FROM bib WHERE eprint_id IN ({placeholders})
         ORDER BY CASE kind WHEN 'published' THEN 1 ELSE 0 END"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn ToSql> = ids.iter().map(|s| s as &dyn ToSql).collect();
    let rows = stmt.query_map(params.as_slice(), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    // 'published' sorts last, so it overwrites the ePrint fallback.
    for row in rows {
        let (id, key, kind) = row?;
        out.insert(id, (key, kind == "published"));
    }
    Ok(out)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM papers", [], |r| r.get(0))?)
}

/// `now` is stored as the first-seen timestamp; an update leaves it untouched.
pub fn upsert(conn: &Connection, p: &Paper, now: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO papers (id,title,authors,abstract,category,date,year,rights,url,added)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(id) DO UPDATE SET
           title=excluded.title, authors=excluded.authors, abstract=excluded.abstract,
           category=excluded.category, date=excluded.date, year=excluded.year,
           rights=excluded.rights, url=excluded.url",
        rusqlite::params![
            p.id,
            p.title,
            p.authors,
            p.abstract_,
            p.category,
            p.date,
            p.year,
            p.rights,
            p.url,
            now
        ],
    )?;
    Ok(())
}

/// Papers that arrived in the index after `watermark`, newest first.
pub fn added_since(conn: &Connection, watermark: &str, limit: usize) -> Result<Vec<Hit>> {
    let mut stmt = conn.prepare(
        "SELECT id,title,authors,abstract,category,date,year,rights,url
         FROM papers WHERE added > ?1
         ORDER BY added DESC, date DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![watermark, limit as i64], |r| {
        let paper = row_to_paper(r, 0)?;
        let abstract_hl = paper.abstract_.clone();
        let title_hl = paper.title.clone();
        Ok(Hit {
            paper,
            abstract_hl,
            title_hl,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The most recently *arrived* papers, whatever the watermark says. The floor under
/// a bare `eprint`: when nothing is new there is still something to look at.
pub fn recent_arrivals(conn: &Connection, limit: usize) -> Result<Vec<Hit>> {
    let mut stmt = conn.prepare(
        "SELECT id,title,authors,abstract,category,date,year,rights,url
         FROM papers ORDER BY added DESC, date DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| {
        let paper = row_to_paper(r, 0)?;
        let abstract_hl = paper.abstract_.clone();
        let title_hl = paper.title.clone();
        Ok(Hit {
            paper,
            abstract_hl,
            title_hl,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM papers WHERE id = ?1", [id])?;
    Ok(())
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Paper>> {
    let mut stmt = conn.prepare(
        "SELECT id,title,authors,abstract,category,date,year,rights,url
         FROM papers WHERE id = ?1",
    )?;
    let mut rows = stmt.query([id])?;
    Ok(match rows.next()? {
        Some(r) => Some(row_to_paper(r, 0)?),
        None => None,
    })
}

fn row_to_paper(r: &rusqlite::Row, o: usize) -> rusqlite::Result<Paper> {
    Ok(Paper {
        id: r.get(o)?,
        title: r.get(o + 1)?,
        authors: r.get(o + 2)?,
        abstract_: r.get(o + 3)?,
        category: r.get(o + 4)?,
        date: r.get(o + 5)?,
        year: r.get(o + 6)?,
        rights: r.get(o + 7)?,
        url: r.get(o + 8)?,
    })
}

/// A saved search. Mirrors the fields of `Query` that make sense to persist:
/// no limit (the caller decides how much to show) and no year, which would
/// date a standing watch.
#[derive(Clone, Debug)]
pub struct Watch {
    pub id: i64,
    pub terms: String,
    pub author: Option<String>,
    pub category: Option<String>,
    pub scope: Scope,
}

impl Watch {
    /// How the watch reads back to the user, in the same shape they typed it —
    /// and also how it is written to the config file, so this has to round-trip
    /// through `config`'s parser exactly.
    pub fn label(&self) -> String {
        // A flag value containing a space must come back quoted, or reading the
        // line again splits `--author Dan Boneh` into author `Dan` plus a stray
        // query term `Boneh`. Terms keep whatever quoting they already carry,
        // because FTS5 reads `"a b"` as a phrase.
        let arg = |v: &str| {
            if v.contains(char::is_whitespace) && !v.starts_with('"') {
                format!("\"{v}\"")
            } else {
                v.to_string()
            }
        };
        let mut parts: Vec<String> = Vec::new();
        if !self.terms.trim().is_empty() {
            parts.push(self.terms.clone());
        }
        if let Some(a) = &self.author {
            parts.push(format!("--author {}", arg(a)));
        }
        if let Some(c) = &self.category {
            parts.push(format!("--category {}", arg(c)));
        }
        if self.scope == Scope::Title {
            parts.push("--title".to_string());
        }
        parts.join(" ")
    }

    /// The watch as a query over papers that arrived after `watermark`. Pass
    /// `None` to match the whole index.
    pub fn query<'a>(&'a self, watermark: Option<&str>, limit: usize) -> Query<'a> {
        Query {
            terms: &self.terms,
            year: None,
            since: None,
            before: None,
            added_since: watermark.map(|w| w.to_string()),
            only_watched: false,
            author: self.author.clone(),
            category: self.category.clone(),
            limit,
            scope: self.scope,
            prefix: true,
        }
    }
}

/// Just the ids a query matches, skipping the `highlight()` work — used when the
/// result feeds another query rather than the screen.
fn matching_ids(conn: &Connection, q: &Query) -> Result<Vec<String>> {
    let collect = |expr: Option<String>| -> Result<Vec<String>> {
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        let head = match expr {
            Some(e) => {
                args.push(Box::new(e));
                "papers_fts f JOIN papers p ON p.rowid = f.rowid WHERE papers_fts MATCH ?1"
            }
            None => "papers p WHERE 1=1",
        };
        let filters = filter_sql(q, &mut args);
        args.push(Box::new(q.limit as i64));
        let sql = format!(
            "SELECT p.id FROM {head}{filters} ORDER BY p.date DESC LIMIT ?{}",
            args.len()
        );
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    };
    if q.terms.trim().is_empty() {
        return collect(None);
    }
    // Same verbatim-then-quoted fallback as `search`.
    match collect(Some(primary_expr(q))) {
        Ok(ids) => Ok(ids),
        Err(e) => {
            let fallback = fallback_expr(q);
            if fallback.is_empty() {
                return Err(e);
            }
            collect(Some(fallback))
        }
    }
}

/// Fill `watched_stage` with every id matching some watch, and report how many.
/// Cheap to repeat, so callers can re-stage whenever the watch list may have
/// changed rather than tracking that themselves.
pub fn stage_watched(conn: &Connection, watches: &[Watch], cap_per_watch: usize) -> Result<usize> {
    conn.execute("DELETE FROM watched_stage", [])?;
    for w in watches {
        // A watch that no longer parses contributes nothing rather than
        // emptying the filter.
        if let Ok(ids) = matching_ids(conn, &w.query(None, cap_per_watch)) {
            let mut stmt =
                conn.prepare("INSERT OR IGNORE INTO watched_stage (id) VALUES (?1)")?;
            for id in ids {
                stmt.execute([id])?;
            }
        }
    }
    Ok(conn.query_row("SELECT COUNT(*) FROM watched_stage", [], |r| r.get::<_, i64>(0))? as usize)
}

/// Which of `ids` match at least one saved watch.
///
/// One query per watch over the whole index, intersected in memory — *not* one
/// query per watch per chunk of on-screen ids. The chunked version cost 66 queries
/// per watch on a full 26,000-paper listing and dominated the search; this is ten
/// queries whatever the listing size, because a watch's match set is small (an
/// author is tens of papers, a broad term a few thousand ids).
pub fn watched_ids(
    conn: &Connection,
    ids: &[String],
    watches: &[Watch],
) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    if ids.is_empty() || watches.is_empty() {
        return Ok(out);
    }
    let want: HashSet<&String> = ids.iter().collect();
    for w in watches {
        // A watch with a broken expression must not break the listing it is
        // annotating, so a failed match just contributes nothing.
        if let Ok(matched) = matching_ids(conn, &w.query(None, WATCH_MATCH_CAP)) {
            for id in matched {
                if want.contains(&id) {
                    out.insert(id);
                }
            }
        }
    }
    Ok(out)
}

/// Watches used to live in this file, one row per saved search. They are settings,
/// not index data, so they moved to the config file where a single copy carries a
/// whole setup to another machine. This reads whatever an older build left behind,
/// once, so `main` can write it out and then call `drop_legacy_watches`.
pub fn legacy_watches(conn: &Connection) -> Result<Vec<Watch>> {
    let present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='watches'",
        [],
        |r| r.get(0),
    )?;
    if present == 0 {
        return Ok(Vec::new());
    }
    let mut stmt =
        conn.prepare("SELECT id,terms,author,category,scope FROM watches ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        let author: String = r.get(2)?;
        let category: String = r.get(3)?;
        let scope: String = r.get(4)?;
        Ok(Watch {
            id: r.get(0)?,
            terms: r.get(1)?,
            author: Some(author).filter(|s| !s.is_empty()),
            category: Some(category).filter(|s| !s.is_empty()),
            scope: Scope::from_str(&scope),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Only after the config has been written, so a failed migration can be retried.
pub fn drop_legacy_watches(conn: &Connection) -> Result<()> {
    conn.execute_batch("DROP TABLE IF EXISTS watches;")?;
    Ok(())
}

pub struct Query<'a> {
    pub terms: &'a str,
    pub year: Option<i64>,
    pub since: Option<String>,
    /// **Exclusive** upper bound: the first day after the period asked for. Stored
    /// dates are full timestamps, so an inclusive `<=` on a date-only string would
    /// exclude the whole of that final day.
    pub before: Option<String>,
    /// Filters on when the paper entered *this* index, not its own date, so
    /// `new` can ask a watch "anything since I last looked?".
    pub added_since: Option<String>,
    /// Restricts the query to papers matching some watch. Requires a preceding
    /// `stage_watched()` on the same connection to fill `watched_stage`.
    pub only_watched: bool,
    pub author: Option<String>,
    pub category: Option<String>,
    pub limit: usize,
    pub scope: Scope,
    /// Treat bare terms as prefixes so partial words match.
    pub prefix: bool,
}

const FTS_OPS: [&str; 4] = ["AND", "OR", "NOT", "NEAR"];

/// Split on whitespace while keeping "quoted phrases" intact.
fn split_tokens(s: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        if c == '"' {
            quoted = !quoted;
            cur.push(c);
        } else if c.is_whitespace() && !quoted {
            if !cur.is_empty() {
                toks.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks
}

/// FTS5 matches whole tokens, so `bone` never finds `Boneh` — stemming only
/// covers suffix variants like signature/signatures. Append `*` to bare terms
/// so a partial word behaves the way a search box is expected to. Operators,
/// column filters, parenthesised groups and explicit `*` are left alone.
fn add_prefix(terms: &str) -> String {
    split_tokens(terms)
        .into_iter()
        .map(|t| {
            let upper = t.to_ascii_uppercase();
            if FTS_OPS.contains(&upper.as_str())
                || t.ends_with('*')
                || t.contains(':')
                || t.contains('(')
                || t.contains(')')
                || t.contains('{')
                || t.contains('}')
                || t.starts_with('-')
                || t.starts_with('^')
            {
                return t;
            }
            // A single character prefix would match most of the index.
            if t.trim_matches('"').chars().count() < 2 {
                return t;
            }
            format!("{t}*")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// FTS5 rejects some punctuation in bare queries (e.g. `zero-knowledge`).
/// Rather than guess, we try the user's query verbatim first — preserving
/// operators like AND/OR/NOT, quoted phrases and `prefix*` — and only fall
/// back to quoting each token if SQLite reports a parse error.
fn quote_terms(terms: &str, prefix: bool) -> String {
    terms
        .split_whitespace()
        .map(|t| {
            let clean = t.replace('"', "");
            if clean.is_empty() {
                String::new()
            } else if prefix && clean.chars().count() >= 2 {
                format!("\"{clean}\"*")
            } else {
                format!("\"{clean}\"")
            }
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn primary_expr(q: &Query) -> String {
    let base = if q.prefix {
        add_prefix(q.terms)
    } else {
        q.terms.to_string()
    };
    scoped(&base, q.scope)
}

fn fallback_expr(q: &Query) -> String {
    scoped(&quote_terms(q.terms, q.prefix), q.scope)
}

pub fn search(conn: &Connection, q: &Query) -> Result<Vec<Hit>> {
    if q.terms.trim().is_empty() {
        return browse(conn, q);
    }
    match run_search(conn, q, &primary_expr(q)) {
        Ok(hits) => Ok(hits),
        Err(_) => {
            // Nothing survives quoting (e.g. a lone `"`), so there is no second
            // shot to take. Say that plainly instead of forwarding SQLite's
            // "unterminated string: Error code 1" at the user. Checking the
            // quoted tokens rather than the whole expression matters: under
            // `--scope title` the wrapper makes even an empty query non-empty.
            if quote_terms(q.terms, q.prefix).trim().is_empty() {
                bail!("could not parse query: {}", q.terms);
            }
            // The fallback keeps its source attached: if *that* fails it is more
            // likely a real database problem than a syntax one.
            run_search(conn, q, &fallback_expr(q))
                .with_context(|| format!("could not parse query: {}", q.terms))
        }
    }
}

fn filter_sql(q: &Query, args: &mut Vec<Box<dyn ToSql>>) -> String {
    let mut sql = String::new();
    if let Some(y) = q.year {
        args.push(Box::new(y));
        sql.push_str(&format!(" AND p.year = ?{}", args.len()));
    }
    if let Some(s) = &q.since {
        args.push(Box::new(s.clone()));
        sql.push_str(&format!(" AND p.date >= ?{}", args.len()));
    }
    if let Some(s) = &q.before {
        args.push(Box::new(s.clone()));
        sql.push_str(&format!(" AND p.date < ?{}", args.len()));
    }
    // `>` not `>=`, to agree with `added_since()` — the watermark is the last
    // moment already seen, not the first unseen one.
    if let Some(s) = &q.added_since {
        args.push(Box::new(s.clone()));
        sql.push_str(&format!(" AND p.added > ?{}", args.len()));
    }
    if q.only_watched {
        sql.push_str(" AND p.id IN (SELECT id FROM watched_stage)");
    }
    if let Some(a) = &q.author {
        args.push(Box::new(format!("%{}%", a.to_lowercase())));
        sql.push_str(&format!(" AND lower(p.authors) LIKE ?{}", args.len()));
    }
    if let Some(c) = &q.category {
        args.push(Box::new(format!("%{}%", c.to_lowercase())));
        sql.push_str(&format!(" AND lower(p.category) LIKE ?{}", args.len()));
    }
    sql
}

fn run_search(conn: &Connection, q: &Query, match_expr: &str) -> Result<Vec<Hit>> {
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(match_expr.to_string())];
    let filters = filter_sql(q, &mut args);
    args.push(Box::new(q.limit as i64));
    let limit_idx = args.len();

    let sql = format!(
        "SELECT p.id,p.title,p.authors,p.abstract,p.category,p.date,p.year,p.rights,p.url,
                highlight(papers_fts, 2, char(1), char(2)),
                highlight(papers_fts, 0, char(1), char(2))
         FROM papers_fts f JOIN papers p ON p.rowid = f.rowid
         WHERE papers_fts MATCH ?1{filters}
         ORDER BY p.date DESC
         LIMIT ?{limit_idx}"
    );

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        let paper = row_to_paper(r, 0)?;
        let abstract_hl: String = r.get(9)?;
        let title_hl: String = r.get(10)?;
        Ok(Hit {
            paper,
            abstract_hl,
            title_hl,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Total number of matches, ignoring the display limit, so the header can
/// say "20 of 147 results".
pub fn count_matches(conn: &Connection, q: &Query) -> Result<usize> {
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let sql = if q.terms.trim().is_empty() {
        args.clear();
        let filters = filter_sql(q, &mut args);
        format!("SELECT COUNT(*) FROM papers p WHERE 1=1{filters}")
    } else {
        args.push(Box::new(primary_expr(q)));
        let filters = filter_sql(q, &mut args);
        format!(
            "SELECT COUNT(*) FROM papers_fts f JOIN papers p ON p.rowid = f.rowid
             WHERE papers_fts MATCH ?1{filters}"
        )
    };
    let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let n: i64 = match conn.query_row(&sql, refs.as_slice(), |r| r.get(0)) {
        Ok(n) => n,
        Err(_) if !q.terms.trim().is_empty() => {
            // Same verbatim-then-quoted fallback as `search`.
            let mut args2: Vec<Box<dyn ToSql>> = vec![Box::new(fallback_expr(q))];
            let filters = filter_sql(q, &mut args2);
            let sql2 = format!(
                "SELECT COUNT(*) FROM papers_fts f JOIN papers p ON p.rowid = f.rowid
                 WHERE papers_fts MATCH ?1{filters}"
            );
            let refs2: Vec<&dyn ToSql> = args2.iter().map(|b| b.as_ref()).collect();
            conn.query_row(&sql2, refs2.as_slice(), |r| r.get(0))?
        }
        Err(e) => return Err(e.into()),
    };
    Ok(n as usize)
}

/// No query terms: plain filtered listing, newest first.
fn browse(conn: &Connection, q: &Query) -> Result<Vec<Hit>> {
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let filters = filter_sql(q, &mut args);
    args.push(Box::new(q.limit as i64));
    let limit_idx = args.len();
    let sql = format!(
        "SELECT p.id,p.title,p.authors,p.abstract,p.category,p.date,p.year,p.rights,p.url
         FROM papers p WHERE 1=1{filters}
         ORDER BY p.date DESC LIMIT ?{limit_idx}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        let paper = row_to_paper(r, 0)?;
        let abstract_hl = paper.abstract_.clone();
        let title_hl = paper.title.clone();
        Ok(Hit {
            paper,
            abstract_hl,
            title_hl,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
