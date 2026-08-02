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

/// Ceiling on how many ids one watch contributes to the cache. Far above any
/// realistic watch, and only a guard against a pathological one.
const WATCH_MATCH_CAP: usize = 50_000;

/// What the cached `watch_hits` was built from: the watch labels, and the harvest
/// it saw. Stored verbatim rather than hashed — a few hundred bytes, and a stale
/// cache should be diagnosable by reading `meta`.
const KEY_CACHE_FOR: &str = "watch_cache_for";
/// What the cached `author_class` was built from: the harvest, and the aliases
/// file verbatim. Both change what "the same author" means.
const KEY_NAMES_FOR: &str = "author_class_for";

/// The author classes, loaded once per process by `open()`. `filter_sql` builds
/// SQL without a `Connection` in hand — and `Watch::query()` has none either — so
/// the map lives here rather than travelling through `Query`.
static CLASSES: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
/// The same information the other way round and pre-folded: a folded name to the
/// folded spellings of everyone it is the same person as. Built with `CLASSES`,
/// because `author_match` runs per row and cannot afford to invert a map.
static SPELLINGS: std::sync::OnceLock<HashMap<String, Vec<String>>> = std::sync::OnceLock::new();
/// Bumped when matching changes, so every cache rebuilds once. v4 records which
/// watch matched each paper; v3 and earlier stored ids alone.
const CACHE_VERSION: &str = "v4";
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

pub fn open() -> Result<Connection> {
    let path = db_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // A stale index spawns a detached `update --quiet` child, so a foreground
    // command can meet that writer mid-transaction. WAL keeps readers going, but
    // anything that writes — `meta_set` on the search path, a `watch_hits`
    // rebuild — would otherwise fail instantly rather than wait the few
    // milliseconds the writer actually needs.
    conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
    // `fold` is the same normalisation the completion list groups by, exposed to
    // SQL so an author filter matches every spelling shown as one person. A
    // function rather than a stored column: no migration, no backfill, and no way
    // for the two to drift. It costs one call per row of a query that already
    // scans the table.
    conn.create_scalar_function(
        "fold",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
            | rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        |ctx| Ok(fold_name(&ctx.get::<String>(0)?)),
    )?;
    conn.create_scalar_function(
        "author_match",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
            | rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        |ctx| {
            Ok(author_match(
                &ctx.get::<String>(0)?,
                &ctx.get::<String>(1)?,
            ))
        },
    )?;
    init(&conn)?;
    // Rebuilt here only when the archive or the aliases file has moved; otherwise
    // this is one small query.
    if CLASSES.get().is_none() {
        let classes = author_classes(&conn).unwrap_or_default();
        let mut groups: HashMap<&str, Vec<String>> = HashMap::new();
        for (name, canonical) in &classes {
            let group = groups.entry(canonical.as_str()).or_default();
            for n in [name.as_str(), canonical.as_str()] {
                let folded = fold_name(n);
                if !group.contains(&folded) {
                    group.push(folded);
                }
            }
        }
        let mut spellings: HashMap<String, Vec<String>> = HashMap::new();
        for group in groups.values() {
            for member in group {
                spellings.insert(member.clone(), group.clone());
            }
        }
        let _ = SPELLINGS.set(spellings);
        let _ = CLASSES.set(classes);
    }
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

-- Which spellings of an author name are the same person. Derived: the rules in
-- `build_author_classes` plus whatever the aliases file adds or vetoes, rebuilt
-- when either the archive or that file moves.
CREATE TABLE IF NOT EXISTS author_class (
  name      TEXT PRIMARY KEY,
  canonical TEXT NOT NULL
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
    // Deleting or counting one watch's rows has to be cheap: that is what makes
    // adding and removing a watch incremental rather than a rebuild.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS watch_hits_label_idx ON watch_hits(label);",
    )?;
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

/// Candidates for completing `--author`: names containing `needle`, each offered
/// both in full and as a bare surname, commonest first.
///
/// Two forms because zsh completes on a prefix and people start a name from either
/// end. "boudg" then finds the surname `Boudgoust`, "kath" finds `Katharina
/// Boudgoust`, and the shell filters to whichever fits what was typed. Both are
/// valid filters, since `--author` matches every word of the name in any order.
///
/// The needle is not optional: the whole author list is 21,000 names and over a
/// megabyte, and narrowing first is what makes this cheap enough for a keypress.
pub fn authors_matching(conn: &Connection, needle: &str, limit: usize) -> Result<Vec<Candidate>> {
    let needle = fold_name(needle);
    if needle.is_empty() {
        return Ok(Vec::new());
    }

    // The same predicate the filter uses, so what is offered and what is found
    // cannot disagree — including its widening to a person's other spellings.
    let mut stmt = conn.prepare("SELECT authors FROM papers WHERE author_match(authors, ?1)")?;
    let bylines: Vec<String> = stmt
        .query_map([needle.as_str()], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let words: Vec<&str> = needle.split_whitespace().filter(|w| w.len() > 1).collect();

    // One entry per person, and every spelling of their name the archive uses, so
    // the one that was typed can be offered back.
    let mut spellings: HashMap<String, HashMap<String, i64>> = HashMap::new();
    for byline in &bylines {
        for name in byline.split(';') {
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            if name.is_empty() || !name_matches(&name, &words) {
                continue;
            }
            *spellings
                .entry(person_of(&name))
                .or_default()
                .entry(name)
                .or_insert(0) += 1;
        }
    }

    let mut out: Vec<Candidate> = Vec::new();
    for person in spellings.keys().cloned().collect::<Vec<_>>() {
        // Offer a spelling that can actually be typed: accents removed, and of the
        // spellings the archive uses, one that contains what was typed. zsh keeps
        // only candidates starting with the typed text and compares characters, so
        // an accented candidate is one the shell silently discards.
        let usable = spellings
            .get(&person)
            .filter(|_| true)
            .and_then(|s| {
                s.iter()
                    .filter(|(n, _)| fold_name(&deaccent(n)).contains(&needle))
                    .max_by_key(|(n, c)| (**c, std::cmp::Reverse((*n).clone())))
                    .map(|(n, _)| n.clone())
            })
            .unwrap_or_else(|| person.clone());
        let value = deaccent(&usable);
        // The number is what picking this candidate actually returns — counted
        // with the same predicate the filter uses, over the papers already in
        // hand. Counting the person's class instead would under-report: choosing
        // `Ivan Damgard` also finds `Ivan Bjerre Damgård`.
        let folded_value = fold_name(&value);
        let count = bylines
            .iter()
            .filter(|b| author_match(b, &folded_value))
            .count() as i64;
        // Name the person when the offered spelling is not how they are usually
        // written, so it is clear what the watch will actually follow.
        let person = if fold_name(&person) == folded_value {
            String::new()
        } else {
            person.clone()
        };
        if let Some((surname, rest)) = split_surname(&value) {
            out.push(Candidate {
                value: format!("{surname}, {rest}"),
                person: person.clone(),
                papers: count,
            });
        }
        out.push(Candidate {
            value,
            person,
            papers: count,
        });
    }

    // zsh keeps only what starts with the typed text, so a name matching in the
    // middle — "Sullivan" for "ivan" — is dead weight, and sorting it last stops
    // it from eating the budget before the truncation.
    out.retain(|c| fold_name(&c.value).contains(&needle));
    out.sort_by(|a, b| {
        let starts = |c: &Candidate| !fold_name(&c.value).starts_with(&needle);
        starts(a)
            .cmp(&starts(b))
            .then_with(|| b.papers.cmp(&a.papers))
            .then_with(|| a.value.cmp(&b.value))
    });
    out.truncate(limit);
    Ok(out)
}

/// Names that look like one person but cannot be proven to be, as
/// `canonical = other, other` lines for the aliases file.
///
/// Suggestions only — the rules merge what they can show, and everything here is
/// a judgement the tool is not entitled to make on its own: a shared surname with
/// a matching first initial, or one name's words being a subset of another's
/// (`Ivan Damgård` and `Ivan Bjerre Damgård`). Written commented out.
pub fn alias_suggestions(conn: &Connection) -> Result<Vec<String>> {
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut stmt = conn.prepare("SELECT authors FROM papers")?;
    for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
        for name in row?.split(';') {
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            if !name.is_empty() {
                *counts.entry(name).or_insert(0) += 1;
            }
        }
    }
    // Only spellings the rules left apart are worth suggesting.
    let person = |n: &str| person_of(n);
    let mut by_surname: HashMap<String, Vec<String>> = HashMap::new();
    for name in counts.keys() {
        if let Some(surname) = fold_name(name).split_whitespace().next_back() {
            by_surname
                .entry(surname.to_string())
                .or_default()
                .push(name.clone());
        }
    }

    // An abbreviation of a name, not merely a name starting with the same letter:
    // `I. Damgard` abbreviates `Ivan Damgård`, but `Yu Wang` and `Yang Wang` are
    // two people who happen to share a Y. Requiring one side to be a bare initial
    // took the suggestion list from 1,943 lines to something reviewable.
    let abbreviates = |short: &str, long: &str| -> bool {
        let (s, l) = (fold_name(short), fold_name(long));
        let (mut sw, mut lw) = (s.split_whitespace(), l.split_whitespace());
        match (sw.next(), lw.next()) {
            (Some(a), Some(b)) => {
                a.len() == 1 && b.starts_with(a) && sw.next_back() == lw.next_back()
            }
            _ => false,
        }
    };
    let words = |n: &str| -> Vec<String> {
        fold_name(n)
            .split_whitespace()
            .filter(|w| w.len() > 1)
            .map(|w| w.to_string())
            .collect()
    };
    let mut out: Vec<String> = Vec::new();
    for (_, mut group) in by_surname {
        if group.len() < 2 {
            continue;
        }
        group.sort_by_key(|n| (-counts[n], n.clone()));
        let mut taken: Vec<String> = Vec::new();
        for anchor in &group {
            if taken.contains(anchor) {
                continue;
            }
            let mut others: Vec<String> = Vec::new();
            for other in &group {
                if other == anchor || taken.contains(other) || person(other) == person(anchor) {
                    continue;
                }
                let (aw, ow) = (words(anchor), words(other));
                // At least a first name and a surname, or `G. Stütz` — which
                // reduces to its surname — would be suggested as an alias for
                // every Stutz in the archive.
                let subset = aw.len() >= 2 && aw.iter().all(|w| ow.contains(w));
                if subset || abbreviates(other, anchor) || abbreviates(anchor, other) {
                    others.push(other.clone());
                    taken.push(other.clone());
                }
            }
            if !others.is_empty() {
                taken.push(anchor.clone());
                out.push(format!("{anchor} = {}", others.join(", ")));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The spelling that stands for whoever this name belongs to.
fn person_of(name: &str) -> String {
    CLASSES
        .get()
        .and_then(|c| c.get(name))
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

/// `"Adi Shamir"` -> `("Shamir", "Adi")`. `None` for a single-word name.
fn split_surname(name: &str) -> Option<(&str, String)> {
    let mut words: Vec<&str> = name.split_whitespace().collect();
    let surname = words.pop()?;
    if words.is_empty() {
        return None;
    }
    Some((surname, words.join(" ")))
}

/// The name with its accents removed but its case and spacing intact, so it can
/// be *offered* as a candidate: `Damgård, Ivan` -> `Damgard, Ivan`.
///
/// zsh keeps only candidates starting with what was typed, and it compares
/// characters — `damga` cannot reach `Damgård`, so the busiest spelling of the
/// name silently vanished from the menu while two rare ones survived. The
/// accent-free rendering inserts cleanly and is the same filter, since matching
/// folds both sides.
pub fn deaccent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let lower = ch.to_lowercase().next().unwrap_or(ch);
        let folded = fold_name(&lower.to_string());
        if folded.is_empty() || folded == lower.to_string() {
            out.push(ch);
        } else if ch.is_uppercase() {
            out.push_str(&folded.to_uppercase());
        } else {
            out.push_str(&folded);
        }
    }
    out
}

/// The *other* spelling convention: an umlaut written as a digraph, as it is
/// written when the character is unavailable. Folded afterwards, so this and
/// `fold_name` produce comparable keys.
///
/// Deliberately not the inverse of folding — collapsing `ue` to `u` would merge
/// `Yue` with `Yu` and `Xue` with `Xu`, who are different people. Expanding fires
/// only on a name that actually carries the umlaut, so it is evidence rather than
/// guesswork: `Müller` links `Muller` through one key and `Mueller` through the
/// other, while `Yu` and `Yue` share neither.
pub fn expand_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            'ä' | 'Ä' => out.push_str("ae"),
            'ö' | 'Ö' | 'ø' | 'Ø' => out.push_str("oe"),
            'ü' | 'Ü' => out.push_str("ue"),
            'å' | 'Å' => out.push_str("aa"),
            'æ' | 'Æ' => out.push_str("ae"),
            'ß' => out.push_str("ss"),
            _ => out.push(ch),
        }
    }
    fold_name(&out)
}

/// A name reduced to something comparable: lowercase, ASCII-folded, punctuation
/// and repeated spaces gone. Two spellings that differ only in those respects are
/// the same person, and the archive contains plenty of both.
///
/// Hand-rolled rather than pulled from a crate, in keeping with the rest: the
/// table covers Latin-1 and Latin Extended-A, which is what author names use, and
/// combining marks are dropped so a decomposed "å" folds the same as a composed
/// one.
pub fn fold_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for ch in s.chars().flat_map(|c| c.to_lowercase()) {
        // A decomposed letter has already contributed its base character.
        if ('\u{0300}'..='\u{036f}').contains(&ch) {
            continue;
        }
        let mapped: Option<&str> = match ch {
            'à'..='å' | 'ā' | 'ă' | 'ą' => Some("a"),
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => Some("c"),
            'ď' | 'đ' => Some("d"),
            'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => Some("e"),
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => Some("g"),
            'ĥ' | 'ħ' => Some("h"),
            'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => Some("i"),
            'ĵ' => Some("j"),
            'ķ' => Some("k"),
            'ĺ' | 'ļ' | 'ľ' | 'ł' => Some("l"),
            'ñ' | 'ń' | 'ņ' | 'ň' => Some("n"),
            'ò'..='ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => Some("o"),
            'ŕ' | 'ŗ' | 'ř' => Some("r"),
            'ś' | 'ŝ' | 'ş' | 'š' => Some("s"),
            'ţ' | 'ť' | 'ŧ' => Some("t"),
            'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => Some("u"),
            'ŵ' => Some("w"),
            'ý' | 'ÿ' | 'ŷ' => Some("y"),
            'ź' | 'ż' | 'ž' => Some("z"),
            'æ' => Some("ae"),
            'œ' => Some("oe"),
            'ß' => Some("ss"),
            _ => None,
        };
        match mapped {
            Some(rep) => {
                out.push_str(rep);
                space = false;
            }
            None if ch.is_alphanumeric() => {
                out.push(ch);
                space = false;
            }
            // Punctuation and whitespace both collapse to a single separator, so
            // "Ron D.  Rothblum" and "Ron D. Rothblum" fold alike.
            None => {
                if !space {
                    out.push(' ');
                    space = true;
                }
            }
        }
    }
    out.trim_end().to_string()
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

/// One author to complete, as offered to the shell.
pub struct Candidate {
    /// What gets inserted. Accent-free, because zsh compares characters and will
    /// not reach `Damgård` from `damga`.
    pub value: String,
    /// How the archive usually writes this person, when that differs from
    /// `value` — so a rarer spelling says whose papers it will find. Empty when
    /// the two agree.
    pub person: String,
    pub papers: i64,
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

/// Does one of this paper's authors match `needle`?
///
/// **Per author, not per byline.** Matching the words against the whole line made
/// `--author "Kasper Damgård"` return six papers, because Kasper Green Larsen and
/// Ivan Damgård write together — two different people, one line of text.
///
/// A name matches when every word of the needle appears in it, in any order,
/// single letters ignored; or when another spelling of the same person does, so
/// `Damgård` finds the papers filed as `Damgaard`.
pub fn author_match(byline: &str, needle: &str) -> bool {
    let words: Vec<&str> = needle.split_whitespace().filter(|w| w.len() > 1).collect();
    if words.is_empty() {
        // Nothing usable — a lone initial — so fall back to the literal text
        // rather than matching every paper in the archive.
        let needle = needle.trim();
        return !needle.is_empty() && fold_name(byline).contains(needle);
    }
    byline.split(';').any(|name| name_matches(name, &words))
}

/// One author name against the needle's words, including the person's other
/// spellings.
fn name_matches(name: &str, words: &[&str]) -> bool {
    let folded = fold_name(name);
    if words.iter().all(|w| folded.contains(w)) {
        return true;
    }
    SPELLINGS
        .get()
        .and_then(|s| s.get(&folded))
        .is_some_and(|others| {
            others
                .iter()
                .any(|other| words.iter().all(|w| other.contains(w)))
        })
}

/// The author classes, cached in `author_class` and rebuilt when the archive or
/// the aliases file moves. Every consumer — searching, badging, completion — goes
/// through here, so they cannot disagree about who is who.
pub fn author_classes(conn: &Connection) -> Result<HashMap<String, String>> {
    let fingerprint = format!(
        "{CACHE_VERSION}\n{}\n{}",
        meta_get(conn, crate::harvest::KEY_LAST_HARVEST)?.unwrap_or_default(),
        crate::config::aliases()
            .iter()
            .map(|(a, b, same)| format!("{a}{}{b}", if *same { "=" } else { "!=" }))
            .collect::<Vec<_>>()
            .join("\n")
    );
    if meta_get(conn, KEY_NAMES_FOR)?.as_deref() != Some(fingerprint.as_str()) {
        let classes = build_author_classes(conn)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let written = (|| -> Result<()> {
            conn.execute("DELETE FROM author_class", [])?;
            {
                let mut stmt = conn
                    .prepare("INSERT OR REPLACE INTO author_class (name, canonical) VALUES (?1,?2)")?;
                // Only the names that actually stand for someone else are worth
                // storing; a name that is its own canonical is the default.
                for (name, canonical) in classes.iter().filter(|(n, c)| n != c) {
                    stmt.execute([name, canonical])?;
                }
            }
            meta_set(conn, KEY_NAMES_FOR, &fingerprint)?;
            Ok(())
        })();
        conn.execute_batch(if written.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        written?;
    }
    let mut stmt = conn.prepare("SELECT name, canonical FROM author_class")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<std::result::Result<HashMap<_, _>, _>>()?)
}

/// Every author spelling in the index, mapped to the one spelling that stands for
/// the person. Built once and cached in `author_class`.
///
/// Two keys per name — `fold_name` and `expand_name` — joined transitively, so
/// `Ivan Damgård`, `Ivan Damgard` and `Ivan Damgaard` become one person while
/// `Yu Chen` and `Yue Chen` stay two. Measured over 20,071 distinct spellings:
/// 28 groups merged, none of them wrong. Collapsing digraphs instead would merge
/// 43 and get 11 wrong, all Chinese pinyin.
///
/// The aliases file is applied afterwards so it can override in both directions:
/// `A = B` says the rules missed one, `A != B` says they overreached.
fn build_author_classes(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut stmt = conn.prepare("SELECT authors FROM papers")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    for row in rows {
        for name in row?.split(';') {
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            if !name.is_empty() {
                *counts.entry(name).or_insert(0) += 1;
            }
        }
    }

    // Union-find over the names, keyed by both spellings of each.
    let names: Vec<String> = counts.keys().cloned().collect();
    let index: HashMap<&str, usize> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();
    let mut parent: Vec<usize> = (0..names.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let union = |parent: &mut [usize], a: usize, b: usize| {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    };
    let mut owner: HashMap<String, usize> = HashMap::new();
    for (i, name) in names.iter().enumerate() {
        for key in [fold_name(name), expand_name(name)] {
            match owner.get(&key) {
                Some(&j) => union(&mut parent, i, j),
                None => {
                    owner.insert(key, i);
                }
            }
        }
    }

    // `A = B` joins them. Names the archive has never used are still recorded, so
    // a watch written against one resolves to the class.
    let aliases = crate::config::aliases();
    let mut extra: Vec<String> = Vec::new();
    let mut split: Vec<(String, String)> = Vec::new();
    for (canonical, other, same) in &aliases {
        if *same {
            for n in [canonical, other] {
                if !index.contains_key(n.as_str()) && !extra.contains(n) {
                    extra.push(n.clone());
                }
            }
        } else {
            split.push((canonical.clone(), other.clone()));
        }
    }
    let mut names = names;
    for n in extra {
        names.push(n);
        parent.push(parent.len());
    }
    let index: HashMap<&str, usize> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();
    for (canonical, other, same) in &aliases {
        if !*same {
            continue;
        }
        if let (Some(&a), Some(&b)) = (index.get(canonical.as_str()), index.get(other.as_str())) {
            union(&mut parent, a, b);
        }
    }

    // Group, then pick the spelling the archive uses most as the one to show.
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..names.len() {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }
    let mut out: HashMap<String, String> = HashMap::new();
    for members in groups.values() {
        let canonical = members
            .iter()
            .max_by_key(|&&i| (counts.get(&names[i]).copied().unwrap_or(0), std::cmp::Reverse(i)))
            .map(|&i| names[i].clone())
            .unwrap_or_default();
        for &i in members {
            out.insert(names[i].clone(), canonical.clone());
        }
    }
    // `A != B` is applied last: it wins over anything the rules or the merges did.
    for (a, b) in split {
        // The vetoed spelling becomes its own canonical; the other keeps whatever
        // it had, which is either itself or a class the veto says nothing about.
        if out.contains_key(&b) {
            out.insert(b.clone(), b.clone());
        }
        let _ = &a;
    }
    Ok(out)
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

/// Bring `watch_hits` in line with the watch list, doing as little as possible.
///
/// The labels the cache was built from are recorded in `meta`, so the work is the
/// difference between that list and this one: labels that went are deleted, labels
/// that arrived are matched. A changed harvest invalidates everything, since a
/// revised paper can start or stop matching a watch without being new.
fn sync_watch_cache(conn: &Connection, watches: &[Watch]) -> Result<()> {
    let harvest = meta_get(conn, crate::harvest::KEY_LAST_HARVEST)?.unwrap_or_default();
    let stale_index = meta_get(conn, KEY_CACHE_HARVEST)?.as_deref() != Some(harvest.as_str());
    let covered: Vec<String> = match (stale_index, meta_get(conn, KEY_CACHE_FOR)?) {
        (false, Some(list)) if list.starts_with(CACHE_VERSION) => list
            .split('\n')
            .skip(1)
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
        meta_set(conn, KEY_CACHE_FOR, &watch_fingerprint(watches))?;
        meta_set(conn, KEY_CACHE_HARVEST, &harvest)?;
        Ok(())
    })();
    conn.execute_batch(if written.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
    written
}

/// The labels the cache covers, behind a version tag. Bumping the tag invalidates
/// every cache, which is what a change to *how* matching works requires.
fn watch_fingerprint(watches: &[Watch]) -> String {
    // The author classes are part of what a watch *means*: if `Damgård` starts
    // matching `Damgaard`, the cached rows for that watch are wrong. Folding the
    // class fingerprint in here keeps badges and searches agreeing.
    let classes = CLASSES.get().map(|c| c.len()).unwrap_or(0);
    std::iter::once(format!("{CACHE_VERSION} names:{classes}"))
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
        sql.push_str(" AND p.id IN (SELECT id FROM watch_hits)");
    }
    // Every word of the name has to appear, but the order does not: nobody should
    // have to know whether the archive stored "Katharina Boudgoust" or "Boudgoust
    // Katharina" to watch a person. One `LIKE` per word, ANDed. A single word
    // behaves exactly as it did before, which is the common case.
    //
    // Punctuation is dropped from each word, which is what lets completion offer a
    // name surname-first — "Shamir, Adi" has to be the same filter as "Adi Shamir",
    // since the comma is there only so the candidate starts with what was typed.
    if let Some(a) = &q.author {
        let folded = fold_name(a);
        // Single letters are dropped: an initial as its own `LIKE '%d%'` matches
        // almost every byline in the archive, and dropping it is also what makes
        // "Ron D. Rothblum" and "Ron Rothblum" — the same person, filed both ways —
        // one filter rather than two.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_unifies_the_archives_spellings() {
        // One person, four ways the archive writes them.
        assert_eq!(fold_name("Ron D.  Rothblum"), fold_name("Ron D. Rothblum"));
        assert_eq!(fold_name("Ivan Damgård"), fold_name("Ivan Damgard"));
        assert_eq!(fold_name("ADI SHAMIR"), fold_name("Adi Shamir"));
        assert_eq!(fold_name("Shamir, Adi"), "shamir adi");
        // Decomposed and pre-composed accents fold alike.
        assert_eq!(fold_name("Damga\u{030a}rd"), fold_name("Damgård"));
        // Distinct people stay distinct: `aa` is a transliteration, not an accent,
        // and folding it would merge unrelated names.
        assert_ne!(fold_name("Ivan Damgaard"), fold_name("Ivan Damgård"));
    }

    #[test]
    fn expansion_is_evidence_not_guesswork() {
        // A name carrying the umlaut links both conventions: `Müller` shares a key
        // with `Muller` through folding and with `Mueller` through expansion.
        assert_eq!(fold_name("Müller"), fold_name("Muller"));
        assert_eq!(expand_name("Müller"), fold_name("Mueller"));
        assert_eq!(expand_name("Damgård"), fold_name("Damgaard"));
        assert_eq!(expand_name("Rønne"), fold_name("Roenne"));
        // The reverse rule — collapsing `ue` to `u` — would merge these, and they
        // are different people. Expanding cannot, because there is no umlaut to
        // expand: that is the whole reason for doing it this way round.
        assert_ne!(expand_name("Yue Chen"), expand_name("Yu Chen"));
        assert_ne!(fold_name("Yue Chen"), fold_name("Yu Chen"));
        assert_ne!(expand_name("Xue Liu"), expand_name("Xu Liu"));
        // Expansion leaves a name with no umlaut exactly as folding does.
        assert_eq!(expand_name("Adi Shamir"), fold_name("Adi Shamir"));
    }

    #[test]
    fn a_name_must_match_one_author_not_a_byline() {
        // The words used to be matched against the whole line, so a paper by
        // Kasper Green Larsen and Ivan Damgård answered to "Kasper Damgård".
        let byline = "Ivan Damgård; Kasper Green Larsen; Sophia Yakoubov";
        assert!(author_match(byline, &fold_name("Ivan Damgård")));
        assert!(author_match(byline, &fold_name("Kasper Larsen")));
        assert!(!author_match(byline, &fold_name("Kasper Damgård")));
        assert!(!author_match(byline, &fold_name("Sophia Damgård")));
        // Order within a name does not matter, and initials are ignored.
        assert!(author_match(byline, &fold_name("Damgård, Ivan")));
        assert!(author_match(byline, &fold_name("Kasper G. Larsen")));
        // A name that is not there is not there.
        assert!(!author_match(byline, &fold_name("Adi Shamir")));
    }

    #[test]
    fn an_accent_does_not_hide_a_name() {
        // zsh keeps only candidates starting with what was typed, and compares
        // characters: "damga" cannot reach "Damgård". The accent-free rendering
        // can be typed, keeps its capitals, and is the same filter.
        assert_eq!(deaccent("Damgård, Ivan"), "Damgard, Ivan");
        assert_eq!(deaccent("Nico Döttling"), "Nico Dottling");
        assert_eq!(deaccent("Peter B. Rønne"), "Peter B. Ronne");
        // Nothing to strip, nothing to change — including the punctuation and
        // spacing that `fold_name` would have flattened.
        assert_eq!(deaccent("Ron D.  Rothblum"), "Ron D.  Rothblum");
        assert_eq!(deaccent("Adi Shamir"), "Adi Shamir");
    }

    #[test]
    fn surnames_split_off_the_end() {
        assert_eq!(split_surname("Adi Shamir").unwrap(), ("Shamir", "Adi".into()));
        assert_eq!(
            split_surname("Ivan Bjerre Damgård").unwrap(),
            ("Damgård", "Ivan Bjerre".into())
        );
        assert!(split_surname("Cher").is_none());
    }

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
        assert_eq!(w.describe(), "zk · by Adi Shamir · in Foundations · titles only");
    }
}
