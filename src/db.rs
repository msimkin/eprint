use anyhow::{bail, Context, Result};
pub use rusqlite::Connection;
use rusqlite::ToSql;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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
    /// when an abstract is expanded. Empty until `hydrate` fills it — see there
    /// for why it is not fetched with the rest of the row.
    pub abstract_hl: String,
    /// The title with match markers. Empty until `hydrate` fills it.
    pub title_hl: String,
    /// The `papers` rowid, carried so `hydrate` can seek straight back to this
    /// row instead of re-running the match.
    pub rowid: i64,
}

/// Which columns a query is matched against. Authors are always included —
/// narrowing means "not the abstract".
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Scope {
    All,
    Title,
}

impl Scope {
    // Deliberately not `FromStr`: that trait returns a `Result`, and this is
    // infallible on purpose — an unreadable `scope` in the config falls back to
    // `All` the same way an unreadable `theme` falls back, rather than failing a
    // command over a typo. The lint only fires now that the enum is library API.
    #[allow(clippy::should_implement_trait)]
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
        Scope::Title => format!("{{title authors_fold}} : ({expr})"),
    }
}

/// Ceiling on how many ids one watch contributes to the cache. Far above any
/// realistic watch, and only a guard against a pathological one.
const WATCH_MATCH_CAP: usize = 50_000;

/// What the cached `watch_hits` was built from: the watch labels, and the harvest
/// it saw. Stored verbatim rather than hashed — a few hundred bytes, and a stale
/// cache should be diagnosable by reading `meta`.
const KEY_CACHE_FOR: &str = "watch_cache_for";
/// Which revision of `names::PEOPLE` the stored bylines were written with. Author
/// names are canonicalised on the way in, so an existing index needs one pass when
/// the table changes.
const KEY_NAMES_FOR: &str = "author_names_for";

/// Bumped when matching changes, so every cache rebuilds once. v5 canonicalises
/// author names in `papers`; v4 recorded which watch matched each paper.
pub(crate) const CACHE_VERSION: &str = "v5";
const KEY_CACHE_HARVEST: &str = "watch_cache_harvest";

pub const MARK_START: char = '\x01';
pub const MARK_END: char = '\x02';

pub fn db_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("EPRINT_DB") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::data_dir().context("could not determine a data directory")?;
    Ok(base.join("eprint").join("eprint.db"))
}

/// How much memory a connection may keep, for callers that are not a laptop.
///
/// The defaults were measured against a 110MB index on a desktop and are right
/// there. They are not right everywhere: 64MB of page cache per connection plus a
/// 256MB mapping is a lot to hold inside a short-lived background task on a phone,
/// and a memory-mapped file whose pages the operating system can make unreadable
/// turns a permission error into a SIGBUS — a crash rather than something a caller
/// can handle. Both are therefore knobs rather than constants, and both keep the
/// values the command-line tool has always used.
#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    /// `cache_size` in KiB, negative as SQLite expects.
    pub cache_kib: i64,
    /// `mmap_size` in bytes; zero disables the mapping.
    pub mmap_bytes: i64,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            cache_kib: -65_536,
            mmap_bytes: 268_435_456,
        }
    }
}

/// Open the index wherever this machine keeps it.
pub fn open() -> Result<Connection> {
    open_at(&db_path()?)
}

/// Open the index at an explicit path, with the usual tuning.
///
/// Separate from [`open`] because [`db_path`] answers a question only a desktop has
/// a convention for: it reads `$EPRINT_DB`, then falls back to the platform data
/// directory, which inside a sandboxed application is a plausible-looking wrong
/// answer. An embedder knows where its own storage is and says so.
pub fn open_at(path: &Path) -> Result<Connection> {
    open_tuned(path, Tuning::default())
}

/// The one constructor. Everything else delegates here.
///
/// The pragmas and the `author_match` function are not decoration: an author filter
/// calls that function *from SQL*, so a connection built any other way fails at
/// runtime with "no such function" the first time someone searches by name.
pub fn open_tuned(path: &Path, tuning: Tuning) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // A stale index spawns a detached `update --quiet` child, so a foreground
    // command can meet that writer mid-transaction. WAL keeps readers going, but
    // anything that writes — `meta_set` on the search path, a `watch_hits`
    // rebuild — would otherwise fail instantly rather than wait the few
    // milliseconds the writer actually needs.
    conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
    // SQLite's default page cache is 2MB, against a 110MB index — every broad
    // query evicted the lot, which is why a cold search measured 330ms and the
    // same one warm measured 20ms. The sort below also spills: `browse` orders
    // every match by date, and that temp B-tree belongs in memory.
    conn.pragma_update(None, "cache_size", tuning.cache_kib)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "mmap_size", tuning.mmap_bytes)?;
    // Author matching is one predicate, evaluated per row. It replaced a `fold`
    // function that compared whole bylines, which is why no query folds any more.
    conn.create_scalar_function(
        "author_match",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
            | rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        |ctx| {
            Ok(crate::names::author_match(
                &ctx.get::<String>(0)?,
                &ctx.get::<String>(1)?,
            ))
        },
    )?;
    init(&conn)?;
    Ok(conn)
}

/// The FTS index and the triggers that keep it in step. Its own constant because
/// `migrate` has to recreate it: an FTS5 column list cannot be altered, so moving
/// author search onto `authors_fold` means dropping and rebuilding the table, and
/// one definition is the only way the two cannot drift.
const FTS_SCHEMA: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS papers_fts USING fts5(
  title, authors_fold, abstract, category,
  content='papers', content_rowid='rowid', tokenize='porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS papers_ai AFTER INSERT ON papers BEGIN
  INSERT INTO papers_fts(rowid, title, authors_fold, abstract, category)
  VALUES (new.rowid, new.title, new.authors_fold, new.abstract, new.category);
END;
CREATE TRIGGER IF NOT EXISTS papers_ad AFTER DELETE ON papers BEGIN
  INSERT INTO papers_fts(papers_fts, rowid, title, authors_fold, abstract, category)
  VALUES ('delete', old.rowid, old.title, old.authors_fold, old.abstract, old.category);
END;
CREATE TRIGGER IF NOT EXISTS papers_au AFTER UPDATE ON papers BEGIN
  INSERT INTO papers_fts(papers_fts, rowid, title, authors_fold, abstract, category)
  VALUES ('delete', old.rowid, old.title, old.authors_fold, old.abstract, old.category);
  INSERT INTO papers_fts(rowid, title, authors_fold, abstract, category)
  VALUES (new.rowid, new.title, new.authors_fold, new.abstract, new.category);
END;
"#;

fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS papers (
  id       TEXT PRIMARY KEY,
  title    TEXT NOT NULL DEFAULT '',
  authors  TEXT NOT NULL DEFAULT '',
  -- What FTS5 indexes for author search. `fold_name` of `authors`, written by
  -- `upsert`: the tokenizer folds diacritics but not `ø`, `ß` or `đ`, which are
  -- letters rather than accents, so a prefix query built from a folded needle
  -- could never reach `Rønne` while the index held the raw spelling. Indexing what
  -- the matcher compares makes `author_probe` exact instead of a guess at which
  -- spellings might be in there.
  authors_fold TEXT NOT NULL DEFAULT '',
  abstract TEXT NOT NULL DEFAULT '',
  category TEXT NOT NULL DEFAULT '',
  date     TEXT NOT NULL DEFAULT '',
  year     INTEGER NOT NULL DEFAULT 0,
  rights   TEXT NOT NULL DEFAULT '',
  url      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS papers_year_idx ON papers(year);
CREATE INDEX IF NOT EXISTS papers_date_idx ON papers(date);
-- `added` is what the feed orders and filters by, twice per run; `category` is
-- what the completion list groups by. Without these both were a full scan of
-- every paper plus a temp B-tree, which was 44ms of a 50ms `eprint`.
-- `added` is created in `migrate`, after the column exists.
CREATE INDEX IF NOT EXISTS papers_category_idx ON papers(category);

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

-- Which papers match a saved watch, and which watch matched. Not TEMP and not
-- scratch: the whole point is that it outlives the command, so badging a listing
-- and filtering to watched papers are both indexed lookups rather than a scan of
-- the archive per watch. Recording the label as well as the id is what lets a
-- watch be added or removed without rebuilding the rest, and makes the per-watch
-- counts `eprint watch` prints a GROUP BY instead of a scan each.
-- Pure cache — `watched()` repairs it whenever the watch list or the index has
-- moved, so dropping it costs nothing but the next rebuild.
CREATE TABLE IF NOT EXISTS watch_hits (
  id    TEXT NOT NULL,
  label TEXT NOT NULL,
  PRIMARY KEY (id, label)
);
-- Its index is created in `migrate`, which runs after the old one-column shape
-- has been dropped — there is no `label` to index until then.
"#,
    )?;
    // After the tables it indexes and the triggers reference.
    conn.execute_batch(FTS_SCHEMA)?;
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
    // `watch_hits` gained a `label` column. It is pure cache, so the old shape is
    // dropped and rebuilt rather than migrated.
    let has_id: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('watch_hits') WHERE name = 'label'",
        [],
        |r| r.get(0),
    )?;
    if has_id == 0 {
        conn.execute_batch(
            "DROP TABLE IF EXISTS watch_hits;
             CREATE TABLE watch_hits (
               id    TEXT NOT NULL,
               label TEXT NOT NULL,
               PRIMARY KEY (id, label)
             );",
        )?;
    }
    // Now that `added` is certain to exist. An index is derived, not data, so it
    // needs no version bump — an older database simply gains it here.
    conn.execute_batch("CREATE INDEX IF NOT EXISTS papers_added_idx ON papers(added);")?;
    // Deleting or counting one watch's rows has to be cheap: that is what makes
    // adding and removing a watch incremental rather than a rebuild.
    conn.execute_batch("CREATE INDEX IF NOT EXISTS watch_hits_label_idx ON watch_hits(label);")?;
    let has_entry: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('bib') WHERE name = 'entry'",
        [],
        |r| r.get(0),
    )?;
    if has_entry == 0 {
        conn.execute_batch("ALTER TABLE bib ADD COLUMN entry TEXT NOT NULL DEFAULT '';")?;
    }
    // Author equivalence used to be computed and cached; it is a table in `names`
    // now, applied to the stored byline instead.
    conn.execute_batch("DROP TABLE IF EXISTS author_class;")?;
    fold_authors_for_fts(conn)?;
    canonicalise_authors(conn)?;
    Ok(())
}

/// Move author search onto `papers.authors_fold`, for an index whose `papers_fts`
/// was declared over the raw `authors` column.
///
/// The declaration cannot be altered, so the FTS table is dropped and rebuilt. The
/// triggers go first and the backfill happens while they are gone: leaving them in
/// place would reindex all 26,000 rows one at a time on the way past.
fn fold_authors_for_fts(conn: &Connection) -> Result<()> {
    let folded_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('papers') WHERE name = 'authors_fold'",
        [],
        |r| r.get(0),
    )?;
    let folded_index: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('papers_fts') WHERE name = 'authors_fold'",
        [],
        |r| r.get(0),
    )?;
    if folded_column == 1 && folded_index == 1 {
        return Ok(());
    }
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS papers_ai;
         DROP TRIGGER IF EXISTS papers_ad;
         DROP TRIGGER IF EXISTS papers_au;
         DROP TABLE IF EXISTS papers_fts;",
    )?;
    if folded_column == 0 {
        conn.execute_batch("ALTER TABLE papers ADD COLUMN authors_fold TEXT NOT NULL DEFAULT '';")?;
    }
    let mut rows: Vec<(String, String)> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id, authors FROM papers")?;
        let found = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in found {
            rows.push(row?);
        }
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let written = (|| -> Result<()> {
        {
            let mut stmt = conn.prepare("UPDATE papers SET authors_fold = ?2 WHERE id = ?1")?;
            for (id, authors) in &rows {
                stmt.execute(rusqlite::params![id, crate::names::fold_name(authors)])?;
            }
        }
        Ok(())
    })();
    conn.execute_batch(if written.is_ok() {
        "COMMIT"
    } else {
        "ROLLBACK"
    })?;
    written?;
    conn.execute_batch(FTS_SCHEMA)?;
    conn.execute_batch("INSERT INTO papers_fts(papers_fts) VALUES('rebuild');")?;
    Ok(())
}

/// Rewrite stored bylines through `names::canonical_byline`, once per revision of
/// the name table.
///
/// `harvest` does this on the way in, so this exists for an index that already
/// exists — and for the next time the table gains an entry. About 600 rows move on
/// a full index; the whole pass is one transaction, which is what makes it cheap
/// (the same updates committed one at a time are two orders of magnitude slower,
/// because each one reindexes the row for FTS and fsyncs).
fn canonicalise_authors(conn: &Connection) -> Result<()> {
    let fingerprint = format!("{CACHE_VERSION} {}", crate::names::table_fingerprint());
    if meta_get(conn, KEY_NAMES_FOR)?.as_deref() == Some(fingerprint.as_str()) {
        return Ok(());
    }
    let mut fixes: Vec<(String, String)> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id, authors FROM papers")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, authors) = row?;
            let canonical = crate::names::canonical_byline(&authors);
            if canonical != authors {
                fixes.push((id, canonical));
            }
        }
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let written = (|| -> Result<()> {
        {
            // Both columns, always: `authors_fold` is what FTS5 indexes, so leaving
            // it behind would file `I. Damgard`'s paper under the old spelling and
            // the probe would never reach it.
            let mut stmt =
                conn.prepare("UPDATE papers SET authors = ?2, authors_fold = ?3 WHERE id = ?1")?;
            for (id, authors) in &fixes {
                stmt.execute([id, authors, &crate::names::fold_name(authors)])?;
            }
        }
        meta_set(conn, KEY_NAMES_FOR, &fingerprint)?;
        Ok(())
    })();
    conn.execute_batch(if written.is_ok() {
        "COMMIT"
    } else {
        "ROLLBACK"
    })?;
    written
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

/// The most recently dated paper: its id and its ePrint timestamp. This is "when
/// the archive last posted", which is a different question from `added`, the
/// wall-clock time this index caught up — and the one worth showing, since a feed
/// that dates itself by when you last ran it tells you nothing you did not know.
pub fn newest(conn: &Connection) -> Result<Option<(String, String)>> {
    let mut stmt = conn.prepare("SELECT id, date FROM papers ORDER BY date DESC LIMIT 1")?;
    let mut rows = stmt.query([])?;
    match rows.next()? {
        Some(r) => Ok(Some((r.get(0)?, r.get(1)?))),
        None => Ok(None),
    }
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
    let placeholders = std::iter::repeat_n("?", ids.len())
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
        "INSERT INTO papers (id,title,authors,authors_fold,abstract,category,date,year,rights,url,added)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(id) DO UPDATE SET
           title=excluded.title, authors=excluded.authors,
           authors_fold=excluded.authors_fold, abstract=excluded.abstract,
           category=excluded.category, date=excluded.date, year=excluded.year,
           rights=excluded.rights, url=excluded.url",
        rusqlite::params![
            p.id,
            p.title,
            p.authors,
            crate::names::fold_name(&p.authors),
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
        "SELECT id,title,authors,abstract,category,date,year,rights,url,rowid
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
            rowid: r.get(9)?,
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
        "SELECT id,title,authors,abstract,category,date,year,rights,url,rowid
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
            rowid: r.get(9)?,
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
    /// How the watch reads on screen. Separate from `label()` on purpose: a list
    /// of `--author` this and `--category` that is command-line syntax being shown
    /// where a sentence belongs, and the flags are storage's business, not the
    /// reader's. Never write this to the config — it does not parse back.
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.terms.trim().is_empty() {
            parts.push(self.terms.clone());
        }
        if let Some(a) = &self.author {
            parts.push(format!("by {a}"));
        }
        if let Some(c) = &self.category {
            parts.push(format!("in {c}"));
        }
        if self.scope == Scope::Title {
            parts.push("titles only".to_string());
        }
        parts.join(" · ")
    }

    /// How the watch is written to the config file, in the same shape the user
    /// typed it, so this has to round-trip through `config`'s parser exactly.
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
    if !uses_fts(q) {
        return collect(None);
    }
    if q.terms.trim().is_empty() {
        return collect(Some(primary_expr(q)));
    }
    two_shot(q, |expr| collect(Some(expr.to_string())))
}

/// Every id in the index matching at least one saved watch.
///
/// Backed by the `watch_hits` table, which is brought up to date first — and only
/// for what actually changed. Adding a watch matches that one watch and inserts
/// its rows; removing one deletes its rows. Only a harvest forces every watch to
/// be matched again, and that happens in the background child that harvested.
/// Nothing here depends on what is on screen, so nothing here belongs on the
/// interactive path: it used to be one whole-index scan per watch on *every*
/// command, 630ms at twenty-three watches.
pub fn watched(conn: &Connection, watches: &[Watch]) -> Result<HashSet<String>> {
    sync_watch_cache(conn, watches)?;
    let mut stmt = conn.prepare("SELECT DISTINCT id FROM watch_hits")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<HashSet<String>, _>>()?)
}

/// How many papers each watch currently marks, straight from the cache.
///
/// `eprint watch` used to run one whole-index count per watch — 1.0s at
/// twenty-three of them, growing by ~44ms each. The rows are already there.
pub fn watch_counts(conn: &Connection, watches: &[Watch]) -> Result<HashMap<String, i64>> {
    sync_watch_cache(conn, watches)?;
    let mut stmt = conn.prepare("SELECT label, COUNT(*) FROM watch_hits GROUP BY label")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<std::result::Result<HashMap<_, _>, _>>()?)
}

/// Which watches matched which of these papers, for the ones that matched at all.
///
/// Deliberately does *not* call `sync_watch_cache`: the only caller is the notifier,
/// which runs at the end of `do_update` where `db::watched` has already brought the
/// cache up to date for the harvest that just finished. The table's primary key is
/// `(id, label)`, so this probe is a covered index scan.
///
/// The alternative — one `search(Watch::query(Some(watermark), n))` per watch — is
/// the shape the cache was built to remove: 630ms at twenty-three watches.
pub fn watch_labels(conn: &Connection, ids: &[String]) -> Result<HashMap<String, Vec<String>>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let holes = vec!["?"; ids.len()].join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT id, label FROM watch_hits WHERE id IN ({holes})"
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, label) = row?;
        out.entry(id).or_default().push(label);
    }
    // Stable order, so a paper matching two watches always names the same one.
    for labels in out.values_mut() {
        labels.sort();
    }
    Ok(out)
}

/// Bring `watch_hits` in line with the watch list, doing as little as possible.
///
/// The labels the cache was built from are recorded in `meta`, so the work is the
/// difference between that list and this one: labels that went are deleted, labels
/// that arrived are matched. A changed harvest invalidates everything, since a
/// revised paper can start or stop matching a watch without being new.
fn sync_watch_cache(conn: &Connection, watches: &[Watch]) -> Result<()> {
    let harvest = meta_get(conn, crate::harvest::KEY_LAST_HARVEST)?.unwrap_or_default();
    let stale_index = meta_get(conn, KEY_CACHE_HARVEST)?.as_deref() != Some(harvest.as_str());
    // The stored fingerprint carries the class fingerprint on its first lines; the
    // watch labels follow. A change to either invalidates what is cached.
    let prefix = watch_fingerprint(conn, &[]);
    let covered: Vec<String> = match (stale_index, meta_get(conn, KEY_CACHE_FOR)?) {
        (false, Some(list)) if list.starts_with(&prefix) => list
            .strip_prefix(&prefix)
            .unwrap_or("")
            .split('\n')
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
        // No usable cache: every watch is "new" and the table starts empty.
        _ => {
            conn.execute("DELETE FROM watch_hits", [])?;
            Vec::new()
        }
    };

    let wanted: Vec<String> = watches.iter().map(|w| w.label()).collect();
    let gone: Vec<&String> = covered.iter().filter(|l| !wanted.contains(l)).collect();
    let added: Vec<&Watch> = watches
        .iter()
        .filter(|w| !covered.contains(&w.label()))
        .collect();
    if gone.is_empty() && added.is_empty() && !covered.is_empty() {
        // Already current, and the common case: three meta reads and nothing else.
        return Ok(());
    }

    // Matching happens outside the write transaction: it is the slow part, and
    // holding a write lock across it would block the background refresh.
    let mut fresh: Vec<(String, String)> = Vec::new();
    for w in &added {
        let label = w.label();
        // A watch that no longer parses contributes nothing rather than breaking
        // the listing it is meant to annotate.
        if let Ok(ids) = matching_ids(conn, &w.query(None, WATCH_MATCH_CAP)) {
            fresh.extend(ids.into_iter().map(|id| (id, label.clone())));
        }
    }

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let written = (|| -> Result<()> {
        for label in &gone {
            conn.execute("DELETE FROM watch_hits WHERE label = ?1", [label])?;
        }
        {
            let mut stmt =
                conn.prepare("INSERT OR IGNORE INTO watch_hits (id, label) VALUES (?1, ?2)")?;
            for (id, label) in &fresh {
                stmt.execute([id, label])?;
            }
        }
        meta_set(conn, KEY_CACHE_FOR, &watch_fingerprint(conn, watches))?;
        meta_set(conn, KEY_CACHE_HARVEST, &harvest)?;
        Ok(())
    })();
    conn.execute_batch(if written.is_ok() {
        "COMMIT"
    } else {
        "ROLLBACK"
    })?;
    written
}

/// The labels the cache covers, behind a version tag. Bumping the tag invalidates
/// every cache, which is what a change to *how* matching works requires — and
/// canonicalising author names is such a change, so `CACHE_VERSION` moves with the
/// name table.
fn watch_fingerprint(_conn: &Connection, watches: &[Watch]) -> String {
    std::iter::once(CACHE_VERSION.to_string())
        .chain(watches.iter().map(|w| w.label()))
        .collect::<Vec<_>>()
        .join("\n")
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
    /// Restricts the query to papers matching some watch, via the `watch_hits`
    /// cache. Call `watched()` first on the same connection so the cache is fresh.
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

/// Add the author prefilter to an FTS expression, so the index narrows the table
/// before `filter_sql`'s exact predicate looks at what is left. An author filter
/// with no query terms becomes an FTS query in its own right — see `uses_fts`.
fn with_author(q: &Query, terms: String) -> String {
    let Some(probe) = q.author.as_deref().and_then(crate::names::author_probe) else {
        return terms;
    };
    match terms.trim().is_empty() {
        true => probe,
        false => format!("({terms}) AND ({probe})"),
    }
}

/// Whether a query can go through the FTS index at all: it needs terms, or an
/// author whose name yields a probe. Everything else is a plain filtered listing.
fn uses_fts(q: &Query) -> bool {
    !q.terms.trim().is_empty()
        || q.author
            .as_deref()
            .and_then(crate::names::author_probe)
            .is_some()
}

/// Run an FTS query the user's way, and if SQLite rejects it as a parse error,
/// again with every token quoted.
///
/// One function because it used to be three — `search`, `count_matches` and
/// `matching_ids` each had their own copy, and a header could disagree with the
/// results it was counting if only one of them was changed.
fn two_shot<T>(q: &Query, run: impl Fn(&str) -> Result<T>) -> Result<T> {
    match run(&primary_expr(q)) {
        Ok(found) => Ok(found),
        Err(first) => {
            let fallback = fallback_expr(q);
            if fallback.trim().is_empty() {
                return Err(first);
            }
            run(&fallback)
        }
    }
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
    with_author(q, scoped(&base, q.scope))
}

fn fallback_expr(q: &Query) -> String {
    with_author(q, scoped(&quote_terms(q.terms, q.prefix), q.scope))
}

pub fn search(conn: &Connection, q: &Query) -> Result<Vec<Hit>> {
    if !uses_fts(q) {
        return browse(conn, q);
    }
    if q.terms.trim().is_empty() {
        // An author filter alone: the probe is the whole expression, and there is
        // no user text that could fail to parse.
        return run_search(conn, q, &primary_expr(q));
    }
    // Nothing survives quoting (e.g. a lone `"`), so there is no second shot to
    // take. Say that plainly instead of forwarding SQLite's "unterminated string:
    // Error code 1" at the user. Checking the quoted tokens rather than the whole
    // expression matters: under `--scope title` the wrapper makes even an empty
    // query non-empty.
    if quote_terms(q.terms, q.prefix).trim().is_empty()
        && run_search(conn, q, &primary_expr(q)).is_err()
    {
        bail!("could not parse query: {}", q.terms);
    }
    two_shot(q, |expr| run_search(conn, q, expr))
        .with_context(|| format!("could not parse query: {}", q.terms))
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
        sql.push_str(" AND p.id IN (SELECT id FROM watch_hits)");
    }
    // One predicate, defined in `names`: every word of the name must begin a word
    // of a single author's name, in any order. See that module for why it is per
    // author rather than per byline.
    if let Some(a) = &q.author {
        let folded = crate::names::fold_needle(a);
        args.push(Box::new(folded));
        sql.push_str(&format!(" AND author_match(p.authors, ?{})", args.len()));
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

    // No `highlight()` here, deliberately. It re-tokenises the whole abstract per
    // row, and `browse` runs unbounded: on a two-character query that is 26,000
    // abstracts and 1.25s per keystroke, against 110ms without. The marked-up
    // strings are fetched by `hydrate` for the handful of rows on screen.
    let sql = format!(
        "SELECT p.id,p.title,p.authors,p.abstract,p.category,p.date,p.year,p.rights,p.url,
                f.rowid
         FROM papers_fts f JOIN papers p ON p.rowid = f.rowid
         WHERE papers_fts MATCH ?1{filters}
         ORDER BY p.date DESC
         LIMIT ?{limit_idx}"
    );

    let mut stmt = conn.prepare_cached(&sql)?;
    let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        let paper = row_to_paper(r, 0)?;
        Ok(Hit {
            paper,
            abstract_hl: String::new(),
            title_hl: String::new(),
            rowid: r.get(9)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Fill in `title_hl` and `abstract_hl` for one row that a search already
/// returned.
///
/// Split out of the bulk query because `highlight()` costs far more than the
/// rest of the row put together — measured on the real index, a two-character
/// query matching 26,556 papers takes 1250ms with it and 110ms without. Doing it
/// per visible row instead is free: FTS5 honours a rowid *equality* constraint as
/// a seek (`VIRTUAL TABLE INDEX 0:=M4`), so thirty of these measure 0ms.
///
/// Note `rowid IN (...)` does **not** get that treatment — FTS5 evaluates the
/// whole match and then filters, which measured 840ms. One query per row is the
/// fast shape here, counterintuitive as that looks.
pub fn hydrate(conn: &Connection, q: &Query, hit: &mut Hit) -> Result<()> {
    if !hit.title_hl.is_empty() || !hit.abstract_hl.is_empty() {
        return Ok(()); // already done, or a non-matching listing
    }
    if !uses_fts(q) {
        hit.title_hl = hit.paper.title.clone();
        hit.abstract_hl = hit.paper.abstract_.clone();
        return Ok(());
    }
    let rowid = hit.rowid;
    let fetch = |expr: &str| -> Result<(String, String)> {
        let mut stmt = conn.prepare_cached(
            "SELECT highlight(papers_fts, 0, char(1), char(2)),
                    highlight(papers_fts, 2, char(1), char(2))
             FROM papers_fts
             WHERE papers_fts MATCH ?1 AND rowid = ?2",
        )?;
        Ok(stmt.query_row(rusqlite::params![expr, rowid], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?)
    };
    // The same two-shot the search used, so the expression that produced this row
    // is the one that marks it.
    match two_shot(q, fetch) {
        Ok((title_hl, abstract_hl)) => {
            hit.title_hl = title_hl;
            hit.abstract_hl = abstract_hl;
        }
        // Leave them empty rather than fail a render: both front-ends fall back
        // to the unmarked text, which only costs the highlighting.
        Err(_) => {
            hit.title_hl = hit.paper.title.clone();
            hit.abstract_hl = hit.paper.abstract_.clone();
        }
    }
    Ok(())
}

/// Hydrate a whole slice — what the non-interactive `search` wants, since it
/// prints every row it fetched.
pub fn hydrate_all(conn: &Connection, q: &Query, hits: &mut [Hit]) {
    for hit in hits.iter_mut() {
        let _ = hydrate(conn, q, hit);
    }
}

/// Total number of matches, ignoring the display limit, so the header can
/// say "20 of 147 results".
pub fn count_matches(conn: &Connection, q: &Query) -> Result<usize> {
    if !uses_fts(q) {
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        let filters = filter_sql(q, &mut args);
        let sql = format!("SELECT COUNT(*) FROM papers p WHERE 1=1{filters}");
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let n: i64 = conn.query_row(&sql, refs.as_slice(), |r| r.get(0))?;
        return Ok(n as usize);
    }
    if q.terms.trim().is_empty() {
        let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(primary_expr(q))];
        let filters = filter_sql(q, &mut args);
        let sql = format!(
            "SELECT COUNT(*) FROM papers_fts f JOIN papers p ON p.rowid = f.rowid
             WHERE papers_fts MATCH ?1{filters}"
        );
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let n: i64 = conn.query_row(&sql, refs.as_slice(), |r| r.get(0))?;
        return Ok(n as usize);
    }
    let count = |expr: &str| -> Result<i64> {
        let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(expr.to_string())];
        let filters = filter_sql(q, &mut args);
        let sql = format!(
            "SELECT COUNT(*) FROM papers_fts f JOIN papers p ON p.rowid = f.rowid
             WHERE papers_fts MATCH ?1{filters}"
        );
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        Ok(conn.query_row(&sql, refs.as_slice(), |r| r.get(0))?)
    };
    Ok(two_shot(q, count)? as usize)
}

/// No query terms: plain filtered listing, newest first.
fn browse(conn: &Connection, q: &Query) -> Result<Vec<Hit>> {
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let filters = filter_sql(q, &mut args);
    args.push(Box::new(q.limit as i64));
    let limit_idx = args.len();
    let sql = format!(
        "SELECT p.id,p.title,p.authors,p.abstract,p.category,p.date,p.year,p.rights,p.url,
                p.rowid
         FROM papers p WHERE 1=1{filters}
         ORDER BY p.date DESC LIMIT ?{limit_idx}"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        let paper = row_to_paper(r, 0)?;
        // Nothing matched, so there is nothing to mark: these are already their
        // own "highlighted" form and need no hydration.
        let abstract_hl = paper.abstract_.clone();
        let title_hl = paper.title.clone();
        Ok(Hit {
            paper,
            abstract_hl,
            title_hl,
            rowid: r.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watches_read_one_way_and_store_another() {
        let w = Watch {
            id: 1,
            terms: "zk".into(),
            author: Some("Adi Shamir".into()),
            category: Some("Foundations".into()),
            scope: Scope::Title,
        };
        // Storage has to round-trip through the config parser, so flag values
        // containing a space stay quoted.
        assert_eq!(
            w.label(),
            "zk --author \"Adi Shamir\" --category Foundations --title"
        );
        // Display is a sentence and deliberately does not round-trip.
        assert_eq!(
            w.describe(),
            "zk · by Adi Shamir · in Foundations · titles only"
        );
    }
}
