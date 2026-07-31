use anyhow::{Context, Result};
use rusqlite::{Connection, ToSql};
use std::collections::HashMap;
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

/// A search result: the paper plus a snippet showing where the query matched.
/// Snippets use \x01/\x02 as match delimiters so the renderer can decide
/// whether to emit ANSI codes; piped output stays clean.
pub struct Hit {
    pub paper: Paper,
    pub snippet: String,
    /// The complete abstract with the same match markers, used by `-a` so
    /// full-abstract mode highlights matches too.
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
            snippet: String::new(),
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

pub fn count_added_since(conn: &Connection, watermark: &str) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM papers WHERE added > ?1",
        [watermark],
        |r| r.get(0),
    )?;
    Ok(n as usize)
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

pub struct Query<'a> {
    pub terms: &'a str,
    pub year: Option<i64>,
    pub since: Option<String>,
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
        Err(e) => {
            let fallback = fallback_expr(q);
            if fallback.is_empty() {
                return Err(e);
            }
            run_search(conn, q, &fallback)
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
                snippet(papers_fts, 2, char(1), char(2), '…', 16),
                snippet(papers_fts, 0, char(1), char(2), '…', 16),
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
        let abs_snip: String = r.get(9)?;
        let title_snip: String = r.get(10)?;
        let abstract_hl: String = r.get(11)?;
        let title_hl: String = r.get(12)?;
        // Prefer the abstract snippet; fall back to the title when the match
        // was only in the title (snippet() returns a plain prefix otherwise).
        let snippet = if abs_snip.contains(MARK_START) {
            abs_snip
        } else if title_snip.contains(MARK_START) {
            String::new()
        } else {
            abs_snip
        };
        Ok(Hit {
            paper,
            snippet,
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
            snippet: String::new(),
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
