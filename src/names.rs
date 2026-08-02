//! Who an author is: the several ways the archive spells one person.
//!
//! Four pieces, each with one job, and the boundary exists because this is the
//! part that churned hardest — resist adding a fifth.
//!
//! 1. [`fold_name`] flattens case, punctuation, repeated spaces and accents.
//! 2. [`expand_name`] writes an umlaut out as its digraph, so `Müller` links both
//!    `Muller` and `Mueller`. Only a name that carries the umlaut can link them,
//!    which is why `Yu` and `Yue` stay apart.
//! 3. The aliases file (`config::aliases`) says what no rule can derive, and wins.
//! 4. [`author_match`] is the one predicate every caller shares — searching,
//!    badging, watch counts and completion — so they cannot disagree about who
//!    someone is. It matches **one author at a time**.

use crate::db::{meta_get, meta_set, Connection, CACHE_VERSION, KEY_NAMES_FOR};

/// Every author spelling to the one that stands for the person, and the same
/// thing pre-folded for [`author_match`], which runs per row of a query and
/// cannot afford to invert a map. Loaded once per process by `db::open`: SQL is
/// built without a `Connection` in hand, and `Watch::query()` has none either.
static CLASSES: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
static SPELLINGS: std::sync::OnceLock<HashMap<String, Vec<String>>> = std::sync::OnceLock::new();

/// Fill both maps. Cheap unless the archive or the aliases file has moved, in
/// which case `author_classes` rebuilds first.
pub fn load(conn: &Connection) {
    if CLASSES.get().is_some() {
        return;
    }
    let classes = author_classes(conn).unwrap_or_default();
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

use anyhow::Result;
use std::collections::HashMap;

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
    // One letter cannot narrow 21,466 names, and answering it means scanning most
    // of the archive on a keypress. The shell shows "a few more letters" instead.
    if needle.len() < 2 {
        return Ok(Vec::new());
    }

    // The same predicate the filter uses, so what is offered and what is found
    // cannot disagree — including its widening to a person's other spellings.
    let mut stmt = conn.prepare("SELECT authors FROM papers WHERE author_match(authors, ?1)")?;
    // Folded once, here, and reused for both passes below. Re-folding a byline per
    // candidate is what made a common needle take minutes: the work was
    // candidates × bylines × names, and every one of those allocated.
    let bylines: Vec<Vec<String>> = stmt
        .query_map([needle.as_str()], |r| r.get::<_, String>(0))?
        .map(|row| {
            row.map(|byline| {
                byline
                    .split(';')
                    .map(|n| n.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|n| !n.is_empty())
                    .collect()
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let words: Vec<&str> = needle.split_whitespace().filter(|w| w.len() > 1).collect();

    // One entry per person, and every spelling of their name the archive uses, so
    // the one that was typed can be offered back.
    let mut spellings: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let mut tally: HashMap<String, i64> = HashMap::new();
    for byline in &bylines {
        for name in byline {
            if !name_matches(name, &words) {
                continue;
            }
            let person = person_of(name);
            *tally.entry(person.clone()).or_insert(0) += 1;
            *spellings
                .entry(person)
                .or_default()
                .entry(name.clone())
                .or_insert(0) += 1;
        }
    }
    // Exact counting below is the expensive pass, so only the candidates that will
    // survive truncation get one. The tally is the cheap ordering that decides who
    // those are.
    let mut people: Vec<String> = spellings.keys().cloned().collect();
    people.sort_by(|a, b| {
        tally
            .get(b)
            .cmp(&tally.get(a))
            .then_with(|| a.cmp(b))
    });
    people.truncate(limit);

    let mut out: Vec<Candidate> = Vec::new();
    for person in people {
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
        let candidate_words: Vec<&str> = folded_value
            .split_whitespace()
            .filter(|w| w.len() > 1)
            .collect();
        let count = bylines
            .iter()
            .filter(|names| names.iter().any(|n| name_matches(n, &candidate_words)))
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
///
/// Each word must begin a word of the name, not merely appear inside one.
/// Anywhere-in-the-string matching made `--author Ishai` return Avishai Wool's
/// twenty papers, and `--author "An Wang"` every Wang in the archive, since "an"
/// hides inside "wang". Typing part of a name still works, which is what
/// completion needs: "boud" begins "boudgoust".
fn name_matches(name: &str, words: &[&str]) -> bool {
    let folded = fold_name(name);
    if words_begin_words(&folded, words) {
        return true;
    }
    SPELLINGS
        .get()
        .and_then(|s| s.get(&folded))
        .is_some_and(|others| {
            others
                .iter()
                .any(|other| words_begin_words(other, words))
        })
}

fn words_begin_words(folded: &str, words: &[&str]) -> bool {
    words
        .iter()
        .all(|w| folded.split_whitespace().any(|part| part.starts_with(w)))
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
}
