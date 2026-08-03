//! Who an author is: the several ways the archive spells one person.
//!
//! Three pieces, each with one job, and the boundary exists because this is the
//! part that churned hardest — resist adding a fourth.
//!
//! 1. [`fold_name`] flattens case, punctuation, repeated spaces, accents, and the
//!    digraph spellings of an accented letter (`Damgaard` ≡ `Damgård`).
//! 2. [`PEOPLE`] is a hand-checked table: one spelling per well-published author,
//!    written wherever the archive is inconsistent, plus the strays no rule can
//!    reach. It is data, not a heuristic — see the comment above it.
//! 3. [`author_match`] is the one predicate every caller shares — searching,
//!    badging, watch counts and completion — so they cannot disagree about who
//!    someone is. It matches **one author at a time**.

use crate::db::Connection;
use anyhow::Result;
use std::collections::HashMap;

/// The spelling to show for one person, and the strays it cannot reach on its own.
///
/// A stored name is rewritten to the representative when the two have the same
/// *skeleton* — folded, single-letter words dropped, spaces removed — which covers
/// middle initials, hyphenation, spacing, case, accents and digraphs. Anything
/// else has to be listed as a stray, and a stray is a folded name.
///
/// **Curated on purpose.** Every rule that guesses this from the data was measured
/// and is wrong: expanding a leading initial to the archive's own dominant full
/// name for the surname turns `S. Sree Vivek` into *Srinivas* Vivek and `T-H.
/// Hubert Chan` into *Tony* Chan, two different people, and `M. Barbosa` into
/// "Manul" — the archive's own typo. Grouping by surname plus a prefix-related
/// first name merges `Xiaoyun Wang` with `Xiao Wang` and `Yu Chen` with fifteen
/// other Chens. The skeleton rule above is the one comparison that survived every
/// such test, and it cannot bridge an initial, so initials are listed here.
///
/// Selected as: every author with 25 or more papers whom the archive spells more
/// than one way. The representative is the commonest spelling with its middle
/// initials dropped, preferring a well-used variant that carries the author's real
/// accents. `Ivan Damgaard` is deliberately the ASCII spelling; every other entry
/// keeps its accents.
static PEOPLE: &[(&str, &[&str])] = &[
    ("Brent Waters", &[]),
    ("Ivan Damgaard", &["i damgard", "ivan bjerre damgard"]),
    ("Mihir Bellare", &["m bellare"]),
    ("Debdeep Mukhopadhyay", &[]),
    ("François-Xavier Standaert", &["f x standaert"]),
    ("Palash Sarkar", &[]),
    ("Bart Preneel", &["b preneel"]),
    ("Dan Boneh", &["d boneh"]),
    ("Hwajeong Seo", &[]),
    ("Mridul Nandi", &["m nandi"]),
    ("Amir Moradi", &[]),
    ("Daniel Bernstein", &[]),
    ("Jung Hee Cheon", &[]),
    ("Xiaoyun Wang", &[]),
    ("Ingrid Verbauwhede", &["i verbauwhede"]),
    ("David Naccache", &["d naccache"]),
    ("Yehuda Lindell", &["y lindell"]),
    ("Kenneth Paterson", &["k g paterson"]),
    ("Jesper Buus Nielsen", &["jesper b nielsen"]),
    ("Nigel Smart", &["n p smart", "n smart"]),
    ("David Pointcheval", &["d pointcheval"]),
    ("Qiang Tang", &[]),
    ("Shivam Bhasin", &[]),
    ("Jintai Ding", &[]),
    ("Nico Döttling", &[]),
    ("Kristin Lauter", &[]),
    ("Claude Carlet", &["c carlet"]),
    ("Dario Fiore", &["d fiore"]),
    ("Tim Güneysu", &[]),
    ("C. Pandu Rangan", &[]),
    ("Gregor Leander", &["g leander"]),
    ("David Wu", &[]),
    ("Marc Fischlin", &["m fischlin"]),
    ("Martin Albrecht", &["m r albrecht"]),
    ("Peter Scholl", &["p scholl"]),
    ("Sylvain Guilley", &[]),
    ("Steven Galbraith", &["s d galbraith"]),
    ("Frederik Vercauteren", &["f vercauteren"]),
    ("Subhamoy Maitra", &["s maitra"]),
    ("Dengguo Feng", &[]),
    ("Sujoy Sinha Roy", &[]),
    ("Jean-Sébastien Coron", &[]),
    ("Damien Stehlé", &[]),
    ("Juan Garay", &[]),
    ("Man Ho Au", &[]),
    ("Nir Bitansky", &[]),
    ("Joseph Liu", &[]),
    ("Elisabeth Oswald", &["e oswald"]),
    ("Manoj Prabhakaran", &[]),
    ("Georg Fuchsbauer", &["g fuchsbaur"]),
    ("Bo-Yin Yang", &[]),
    ("Joppe Bos", &[]),
    ("Lejla Batina", &["l batina"]),
    ("Ralf Küsters", &[]),
    ("Victor Shoup", &["v shoup"]),
    ("Léo Ducas", &[]),
    ("Yael Kalai", &["yael tauman kalai"]),
    ("Dominique Schröder", &[]),
    ("Hong-Sheng Zhou", &[]),
    ("Rosario Gennaro", &["r gennaro"]),
    ("Diego Aranha", &[]),
    ("Francisco Rodríguez-Henríquez", &[]),
    ("Matthew Green", &[]),
    ("Vincent Rijmen", &["v rijmen"]),
    ("María Naya-Plasencia", &[]),
    ("Jörn Müller-Quade", &[]),
    ("Michael Scott", &["m scott"]),
    ("Manuel Barbosa", &["m barbosa"]),
    ("Svetla Nikova", &["s nikova"]),
    ("Tal Malkin", &[]),
    ("Benny Pinkas", &["b pinkas"]),
    ("Markku-Juhani Saarinen", &[]),
    ("Russell Lai", &[]),
    ("Gaëtan Leurent", &[]),
    ("Cas Cremers", &[]),
    ("Ron Rothblum", &[]),
    ("Yunlei Zhao", &[]),
    ("Charanjit Jutla", &[]),
    ("Juliane Krämer", &[]),
    ("Paulo Barreto", &["p s l m barreto"]),
    ("Fabrice Benhamouda", &[]),
    ("Fan Zhang", &[]),
    ("Jacques Patarin", &[]),
    ("Benoît Libert", &[]),
    ("Alexander May", &["a may"]),
    ("Léo Perrin", &[]),
    ("Alptekin Küpçü", &[]),
    ("Zhenfeng Zhang", &[]),
    ("Nicolas Courtois", &[]),
    ("Daniel Brown", &[]),
    ("Douglas Stinson", &["d r stinson"]),
    ("Duncan Wong", &[]),
    ("Zhenfei Zhang", &[]),
    ("Bogdan Warinschi", &["b warinschi"]),
    ("Alfred Menezes", &[]),
    ("Ahmad-Reza Sadeghi", &[]),
    ("Sherman Chow", &[]),
    ("S. Sharmila Deva Selvi", &[]),
    ("Chang-An Zhao", &[]),
    ("Nektarios Georgios Tsoutsos", &["nektarios g tsoutsos"]),
    ("Oğuz Yayla", &[]),
    ("Siu-Ming Yiu", &["s m yiu"]),
    ("Giuseppe Persiano", &["g persiano"]),
    ("André Schrottenloher", &[]),
    ("Duong Hieu Phan", &[]),
    ("Angshuman Karmakar", &[]),
    ("Martijn Stam", &["m stam"]),
    ("Reihaneh Safavi-Naini", &["r safavi naini"]),
    ("Sihem Mesnager", &[]),
    ("Tancrède Lepoint", &[]),
    ("Christof Paar", &[]),
    ("Foteini Baldimtsi", &["f baldimtsi"]),
    ("Olivier Pereira", &["o pereira"]),
    ("Adam O'Neill", &[]),
    ("Phillip Rogaway", &["p rogaway"]),
    ("Srdjan Capkun", &[]),
    ("Pedro Moreno-Sanchez", &[]),
    ("Onur Günlü", &[]),
    ("Sri Aravinda Krishnan Thyagarajan", &[]),
    ("Joël Alwen", &[]),
    ("Benjamin Smith", &["b smith"]),
    ("Benedikt Bünz", &[]),
    ("Peter Gaži", &[]),
    ("Chris Brzuska", &["c brzuska"]),
    ("Benjamin Grégoire", &[]),
    ("Wouter Castryck", &["w castryck"]),
    ("Wenling Wu", &[]),
    ("Jean-Pierre Seifert", &["j p seifert"]),
    ("Céline Chevalier", &[]),
    ("Hemanta Maji", &[]),
    ("Emmanuel Prouff", &[]),
    ("Behzad Abdolmaleki", &["b abdolmaleki"]),
    ("Tim Beyne", &[]),
    ("Muhammed Esgin", &[]),
    ("S. Sree Vivek", &[]),
    ("Pooya Farshim", &["p farshim"]),
    ("Karim Baghery", &["k baghery"]),
    ("Ben Fisch", &[]),
    ("Renaud Sirdey", &["r sirdey"]),
    ("Yongbin Zhou", &[]),
    ("Masayuki Abe", &[]),
    ("Philippe Gaborit", &["p gaborit"]),
    ("Jean-Luc Danger", &[]),
    ("Daniel Smith-Tone", &[]),
    ("T-H. Hubert Chan", &[]),
    ("Erkay Savaş", &[]),
    ("Hyunji Kim", &[]),
    ("Ventzislav Nikov", &["v nikov"]),
    ("Marcel Keller", &["m keller"]),
    ("Vasyl Ustimenko", &["v ustimenko"]),
    ("Nadia Heninger", &["n heninger"]),
    ("Emil Simion", &[]),
    ("Björn Tackmann", &[]),
    ("Sonia Belaïd", &[]),
    ("Robert Deng", &[]),
    ("Håvard Raddum", &[]),
    ("Mohammad Reza Aref", &[]),
    ("Thorsten Kleinjung", &["t kleinjung"]),
    ("Jihye Kim", &[]),
    ("Alain Passelègue", &[]),
    ("Oriol Farràs", &[]),
    ("Danilo Gligoroski", &["d gligoroski"]),
    ("S. Dov Gordon", &[]),
    ("Salil Vadhan", &[]),
    ("Oded Goldreich", &["o goldreich"]),
    ("Jérémy Jean", &[]),
    ("Gyeongju Song", &[]),
    ("Michael Klooß", &[]),
    ("Jan-Pieter D'Anvers", &[]),
    ("Sébastien Canard", &[]),
    ("Boris Skoric", &["b skoric"]),
    ("Keting Jia", &[]),
    ("Anderson Nascimento", &[]),
    ("Jean-Charles Faugère", &[]),
    ("Ivica Nikolić", &[]),
    ("Masao Kasahara", &[]),
    ("Sugata Gangopadhyay", &["s gangopadhyay"]),
    ("Thomas Shrimpton", &["t shrimpton"]),
    ("Mark Tehranipoor", &[]),
    ("Matthias Kannwischer", &[]),
    ("Jörg Schwenk", &[]),
    ("Lilya Budaghyan", &["l budaghyan"]),
    ("Yuval Ishai", &["yual ishai"]),
];

/// One `<dc:creator>` in the archive holds two people. Keyed on the fold, and the
/// value carries the byline separator, so splitting is just a table entry.
static MUSHED: &[(&str, &str)] = &[(
    "vincenzo iovino abhishek jain",
    "Vincenzo Iovino; Abhishek Jain",
)];

/// What the stored bylines were written with. An existing index is rewritten once
/// when this changes, so editing the table above is enough — there is no second
/// place to remember. FNV-1a, hand-rolled: a hashing crate for six lines would be
/// a tenth dependency.
pub fn table_fingerprint() -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (rep, strays) in PEOPLE {
        for bytes in std::iter::once(rep).chain(strays.iter()) {
            for b in bytes.as_bytes() {
                h = (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    format!("{h:x}")
}

/// [`PEOPLE`] indexed for lookup, built once per process.
struct Table {
    /// skeleton of the representative -> its index
    skeletons: HashMap<String, usize>,
    /// a stray's folded name -> the representative's index
    strays: HashMap<&'static str, usize>,
}

fn table() -> &'static Table {
    static TABLE: std::sync::OnceLock<Table> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut skeletons = HashMap::with_capacity(PEOPLE.len());
        let mut strays = HashMap::new();
        for (i, (rep, aliases)) in PEOPLE.iter().enumerate() {
            skeletons.insert(skeleton(&fold_name(rep)), i);
            for stray in *aliases {
                strays.insert(*stray, i);
            }
        }
        Table { skeletons, strays }
    })
}

/// A folded name with its single-letter words dropped and its spaces closed up:
/// `"ivan b. damgård"` -> `"ivandamgard"`.
///
/// Word boundaries are deliberately gone. The archive writes one Korean or Chinese
/// given name three ways — `Hwajeong Seo`, `Hwa-Jeong Seo`, `HwaJeong Seo` — and no
/// amount of punctuation folding rejoins them. Measured over the whole index, this
/// key merges 108 groups that word-by-word comparison would not, and every one of
/// them is the same person.
fn skeleton(folded: &str) -> String {
    folded
        .split_whitespace()
        .filter(|w| w.chars().count() > 1)
        .collect()
}

/// The spelling this tool shows for whoever this name belongs to, if the table
/// names them.
pub fn canonical(name: &str) -> Option<&'static str> {
    let folded = fold_name(name);
    let t = table();
    t.strays
        .get(folded.as_str())
        .or_else(|| t.skeletons.get(&skeleton(&folded)))
        .map(|&i| PEOPLE[i].0)
}

/// A whole byline with every author it recognises rewritten, and its spacing
/// tidied. What `harvest` stores and what the one-time migration writes, so a
/// listing, a search, a completion candidate and a watch count all see one
/// spelling per person.
pub fn canonical_byline(byline: &str) -> String {
    let mut out = String::with_capacity(byline.len());
    for name in byline.split(';') {
        // The archive's own spacing is not always single: `Ron D.  Rothblum`.
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        if name.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("; ");
        }
        let folded = fold_name(&name);
        match MUSHED.iter().find(|(key, _)| *key == folded) {
            // Back through this function, so the halves are canonicalised like any
            // other name rather than trusted to be spelled well in the table.
            Some((_, split)) => out.push_str(&canonical_byline(split)),
            None => out.push_str(canonical(&name).unwrap_or(&name)),
        }
    }
    out
}

/// What a typed `--author` value should be compared as: canonicalised through the
/// same table, then folded.
///
/// Storing the short form would otherwise break the long one — `Yael Kalai` is
/// what is on file, so `--author "Yael Tauman Kalai"` would find nothing, since
/// `tauman` begins no word of the stored name. A partial needle (`damg`) matches no
/// representative and is left exactly as typed, so prefix search and completion are
/// unaffected.
pub fn fold_needle(needle: &str) -> String {
    fold_name(canonical(needle).unwrap_or(needle))
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
    let needle = fold_needle(needle);
    // One letter cannot narrow 21,466 names, and answering it means scanning most
    // of the archive on a keypress. The shell shows "a few more letters" instead.
    if needle.chars().count() < 2 {
        return Ok(Vec::new());
    }

    // The FTS probe narrows the table first; `author_match` still decides. Without
    // it this scanned every paper and called into Rust for each one, which is
    // 60-130ms on a path that runs per keypress.
    let (sql, arg) = match author_probe(&needle) {
        Some(probe) => (
            "SELECT p.authors FROM papers_fts f JOIN papers p ON p.rowid = f.rowid
             WHERE papers_fts MATCH ?1 AND author_match(p.authors, ?2)",
            Some(probe),
        ),
        None => (
            "SELECT authors FROM papers WHERE author_match(authors, ?1)",
            None,
        ),
    };
    let mut stmt = conn.prepare(sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = match &arg {
        Some(probe) => vec![probe, &needle],
        None => vec![&needle],
    };
    let bylines: Vec<Vec<String>> = stmt
        .query_map(params.as_slice(), |r| r.get::<_, String>(0))?
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

    // One entry per *inserted value*, not per stored spelling. Names are canonical
    // on the way in, so mostly one spelling is one person — but two spellings the
    // table does not cover can still deaccent to the same text, and the archive has
    // both `Ivana Klasovita` and `Ivana Klasovitá`. Offering both puts two
    // identical rows in the menu, each under-reporting the papers the other finds.
    struct Group {
        /// How the archive writes this person, preferring an accented spelling.
        spelling: String,
        /// Which of the fetched rows they appear on, so the count is a union
        /// rather than a sum.
        rows: Vec<u32>,
    }
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (row, byline) in bylines.iter().enumerate() {
        for name in byline {
            if !name_matches(name, &words) {
                continue;
            }
            let value = deaccent(name);
            let group = groups.entry(value.clone()).or_insert_with(|| {
                order.push(value.clone());
                Group {
                    spelling: name.clone(),
                    rows: Vec::new(),
                }
            });
            if group.rows.last() != Some(&(row as u32)) {
                group.rows.push(row as u32);
            }
            if group.spelling == value && *name != value {
                group.spelling = name.clone();
            }
        }
    }
    order.sort_by(|a, b| {
        groups[b]
            .rows
            .len()
            .cmp(&groups[a].rows.len())
            .then_with(|| a.cmp(b))
    });
    order.truncate(limit);

    let mut out: Vec<Candidate> = Vec::new();
    for value in order {
        let group = &groups[&value];
        let count = group.rows.len() as i64;
        // Name the person when the offered spelling is not how they are written, so
        // a deaccented candidate still says whose papers it will find.
        let person = if group.spelling == value {
            String::new()
        } else {
            group.spelling.clone()
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
        let folded = ascii_fold(&lower.to_string());
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

/// A name reduced to something comparable: lowercase, ASCII-folded, punctuation
/// and repeated spaces gone, and the digraph spelling of an accented letter
/// collapsed onto the letter.
///
/// Two spellings that differ only in those respects are the same person, and the
/// archive contains plenty of both.
pub fn fold_name(s: &str) -> String {
    collapse_digraphs(&ascii_fold(s))
}

/// Case, accents, punctuation and repeated spaces. Hand-rolled rather than pulled
/// from a crate, in keeping with the rest: the table covers Latin-1 and Latin
/// Extended-A, which is what author names use, and combining marks are dropped so
/// a decomposed "å" folds the same as a composed one.
fn ascii_fold(s: &str) -> String {
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

/// The digraph an accented letter is written as when the character is unavailable,
/// collapsed onto the letter `ascii_fold` already produced: `aa` and `å` both
/// become `a`, `oe` and `ö` both `o`, `ue` and `ü` both `u`.
///
/// `ue` only when a letter follows it. That restriction is the whole rule:
/// collapsing every `ue` merges `Yu Yu` with `Yue Yu`, `Yu Chen` with `Yue Chen`
/// and `Rui Xue` with `Rui Xu` — seven pinyin pairs, all of them word-final, while
/// every German case (`Gueneysu`, `Kuesters`, `Buenz`, `Mueller`) has the digraph
/// mid-word. Measured over the whole index, these three rules merge 31 groups and
/// not one of them is wrong.
///
/// `ae` and `ss` are deliberately absent: they are worth two papers and none
/// respectively, and `ae` would put `Michael` and `Michal` one keystroke apart.
fn collapse_digraphs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            'a' if chars.peek() == Some(&'a') => {
                chars.next();
            }
            'o' if chars.peek() == Some(&'e') => {
                chars.next();
            }
            'u' if chars.peek() == Some(&'e') => {
                let mut after = chars.clone();
                after.next();
                if after.peek().is_some_and(|c| c.is_alphabetic()) {
                    chars.next();
                }
            }
            _ => {}
        }
        out.push(c);
    }
    out
}

/// One author to complete, as offered to the shell.
pub struct Candidate {
    /// What gets inserted. Accent-free, because zsh compares characters and will
    /// not reach `Damgård` from `damga`.
    pub value: String,
    /// How the archive writes this person, when that differs from `value` — so a
    /// deaccented candidate says whose papers it will find. Empty when they agree.
    pub person: String,
    pub papers: i64,
}

/// An FTS5 expression that finds every paper [`author_match`] could accept, and
/// possibly a few more.
///
/// `author_match` is exact but runs per row — 26,000 Rust calls, each splitting a
/// byline and folding every name, which is 70ms of a 108ms author search and the
/// whole cost of a completion keypress. The `authors` column is already indexed by
/// FTS5, whose `unicode61` tokenizer folds diacritics, so `damgard*` finds
/// `Damgård` in a tenth of a millisecond.
///
/// It is a **prefilter only**: whatever it returns is still refined by
/// `author_match`, so per-author and word-boundary semantics do not change. That
/// means it must never exclude a true match, and the reason it cannot is that the
/// column it searches, `papers.authors_fold`, holds the output of this same
/// [`fold_name`] — a prefix of a folded word is a prefix of the indexed token, with
/// nothing left to guess.
///
/// Indexing the raw byline instead is what made this fragile before. `unicode61`
/// folds *diacritics*, so `Damgård` tokenises as `damgard`, but `ø`, `ß`, `đ` and
/// `ł` are letters rather than accented letters and it leaves them alone — a
/// `ronne*` built from a typed `Roenne` reached 9 of Peter Rønne's 16 papers and
/// silently dropped the rest. Enumerating the spellings a fold could have come from
/// does not close that: with `ø`, `æ`, `œ`, `ß`, `đ`, `ł` and `ı` all in play a
/// name like `Boudgoust` has hundreds of candidate spellings.
///
/// `None` when no word survives (a lone initial), so the caller keeps scanning
/// rather than filtering on nothing.
pub fn author_probe(needle: &str) -> Option<String> {
    let words: Vec<String> = fold_needle(needle)
        .split_whitespace()
        .filter(|w| w.chars().count() > 1)
        // A token is matched as a prefix, so anything FTS5 would read as an
        // operator or syntax has to go.
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|w| w.chars().count() > 1)
        .map(|w| format!("{w}*"))
        .collect();
    if words.is_empty() {
        return None;
    }
    Some(format!("{{authors_fold}} : ({})", words.join(" AND ")))
}

/// Does one of this paper's authors match `needle`?
///
/// **Per author, not per byline.** Matching the words against the whole line made
/// `--author "Kasper Damgård"` return six papers, because Kasper Green Larsen and
/// Ivan Damgård write together — two different people, one line of text.
///
/// A name matches when every word of the needle appears in it, in any order,
/// single letters ignored.
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

/// One author name against the needle's words.
///
/// Each word must begin a word of the name, not merely appear inside one.
/// Anywhere-in-the-string matching made `--author Ishai` return Avishai Wool's
/// twenty papers, and `--author "An Wang"` every Wang in the archive, since "an"
/// hides inside "wang". Typing part of a name still works, which is what
/// completion needs: "boud" begins "boudgoust".
///
/// Extra words in the name cost nothing, which is why middle names need no rule of
/// their own: `Yael Kalai` finds `Yael Tauman Kalai`, `Ron Rothblum` finds `Ron D.
/// Rothblum`, and `Angelo Caro` finds `Angelo De Caro`.
fn name_matches(name: &str, words: &[&str]) -> bool {
    let folded = fold_name(name);
    words
        .iter()
        .all(|w| folded.split_whitespace().any(|part| part.starts_with(w)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_unifies_the_archives_spellings() {
        // One person, several ways the archive writes them.
        assert_eq!(fold_name("Ron D.  Rothblum"), fold_name("Ron D. Rothblum"));
        assert_eq!(fold_name("ADI SHAMIR"), fold_name("Adi Shamir"));
        assert_eq!(fold_name("Shamir, Adi"), "shamir adi");
        // Decomposed and pre-composed accents fold alike.
        assert_eq!(fold_name("Damga\u{030a}rd"), fold_name("Damgård"));
    }

    #[test]
    fn a_digraph_is_the_same_letter() {
        // The three rules, each measured over the whole index.
        assert_eq!(fold_name("Ivan Damgaard"), fold_name("Ivan Damgård"));
        assert_eq!(fold_name("Ivan Damgard"), fold_name("Ivan Damgård"));
        assert_eq!(fold_name("Nico Doettling"), fold_name("Nico Döttling"));
        assert_eq!(fold_name("Peter Rønne"), fold_name("Peter Roenne"));
        assert_eq!(fold_name("Ralf Kuesters"), fold_name("Ralf Küsters"));
        assert_eq!(fold_name("Mueller-Quade"), fold_name("Müller-Quade"));
        // `ue` collapses only mid-word: these are pinyin, and seven such pairs are
        // different people. Every German case has the digraph before a letter.
        assert_ne!(fold_name("Yue Chen"), fold_name("Yu Chen"));
        assert_ne!(fold_name("Yue Yu"), fold_name("Yu Yu"));
        assert_ne!(fold_name("Rui Xue"), fold_name("Rui Xu"));
        // Left out on purpose: `ae` would merge these, who are two people.
        assert_ne!(fold_name("Michael Walter"), fold_name("Michal Walter"));
    }

    #[test]
    fn the_table_names_one_person_once() {
        // Middle names, initials, accents and digraphs all reach the same entry.
        for spelling in [
            "Ivan Damgård",
            "Ivan Damgard",
            "Ivan Damgaard",
            "Ivan B. Damgaard",
            "Ivan Bjerre Damgård",
            "I. Damgard",
        ] {
            assert_eq!(canonical(spelling), Some("Ivan Damgaard"), "{spelling}");
        }
        assert_eq!(canonical("Yael Tauman Kalai"), Some("Yael Kalai"));
        assert_eq!(canonical("Yael Tauman-Kalai"), Some("Yael Kalai"));
        assert_eq!(canonical("Ron D. Rothblum"), Some("Ron Rothblum"));
        assert_eq!(canonical("N. P.  Smart"), Some("Nigel Smart"));
        // A hyphen splitting a word is why the key ignores word boundaries.
        assert_eq!(canonical("Hwa-Jeong Seo"), Some("Hwajeong Seo"));
        // One typo, on one paper, for a very well-published author.
        assert_eq!(canonical("Yual Ishai"), Some("Yuval Ishai"));
        // Different people the table must not swallow.
        assert_eq!(canonical("Kasper Damgård"), None);
        assert_eq!(canonical("Guy Rothblum"), None);
        assert_eq!(canonical("Yu Long Chen"), None);
        assert_eq!(canonical("Xiao Wang"), None);
    }

    #[test]
    fn a_byline_is_rewritten_whole() {
        assert_eq!(
            canonical_byline("Ivan Bjerre Damgård; Kasper Green Larsen"),
            "Ivan Damgaard; Kasper Green Larsen"
        );
        // Spacing is tidied even for a name the table does not know.
        assert_eq!(
            canonical_byline("Ron D.  Rothblum;  Cher"),
            "Ron Rothblum; Cher"
        );
        // One creator field holding two people is split.
        assert_eq!(
            canonical_byline("Vincenzo Iovino Abhishek Jain"),
            "Vincenzo Iovino; Abhishek Jain"
        );
        // Idempotent, or the migration would not converge.
        let once = canonical_byline("I. Damgard; Yael Tauman Kalai");
        assert_eq!(canonical_byline(&once), once);
        assert_eq!(once, "Ivan Damgaard; Yael Kalai");
    }

    #[test]
    fn a_needle_is_canonicalised_too() {
        // Storing the short form would otherwise lose the long one.
        assert_eq!(fold_needle("Yael Tauman Kalai"), fold_name("Yael Kalai"));
        assert_eq!(fold_needle("N. P. Smart"), fold_name("Nigel Smart"));
        // A partial name matches no entry and must be left as typed, or prefix
        // search and completion would stop working.
        assert_eq!(fold_needle("damg"), "damg");
        assert_eq!(fold_needle("Kasper Damgård"), "kasper damgard");
    }

    #[test]
    fn the_probe_covers_every_spelling_it_must() {
        // The probe is a prefilter, so it may return too much but never too little.
        let probe = author_probe("Adi Shamir").expect("two usable words");
        assert!(
            probe.starts_with("{authors_fold} : ("),
            "the folded column is what makes this exact: {probe}"
        );
        assert!(probe.contains("adi*") && probe.contains("shamir*"));
        // Punctuation is not FTS5 syntax to be passed through.
        let probe = author_probe("Shamir, Adi").expect("a comma is not a word");
        assert!(!probe.contains(','), "{probe}");
        // Whatever spelling was typed, the term is the folded one — which is what
        // the indexed column holds, so no spelling can hide from it.
        for typed in ["Ivan Damgaard", "Ivan Damgård", "Ivan Damgard"] {
            let probe = author_probe(typed).expect("two usable words");
            assert!(probe.contains("damgard*"), "{typed}: {probe}");
        }
        assert!(author_probe("Roenne").unwrap().contains("ronne*"));
        assert!(author_probe("Rønne").unwrap().contains("ronne*"));
        // Nothing usable means no probe at all, so the caller keeps scanning
        // instead of filtering on an expression that matches nothing.
        assert!(author_probe("J").is_none());
        assert!(author_probe("...").is_none());
        assert!(author_probe("").is_none());
    }

    #[test]
    fn a_name_must_match_one_author_not_a_byline() {
        // The words used to be matched against the whole line, so a paper by
        // Kasper Green Larsen and Ivan Damgård answered to "Kasper Damgård".
        let byline = "Ivan Damgård; Kasper Green Larsen; Sophia Yakoubov";
        assert!(author_match(byline, &fold_needle("Ivan Damgård")));
        assert!(author_match(byline, &fold_needle("Kasper Larsen")));
        assert!(!author_match(byline, &fold_needle("Kasper Damgård")));
        assert!(!author_match(byline, &fold_needle("Sophia Damgård")));
        // Order within a name does not matter, and initials are ignored.
        assert!(author_match(byline, &fold_needle("Damgård, Ivan")));
        assert!(author_match(byline, &fold_needle("Kasper G. Larsen")));
        // A name that is not there is not there.
        assert!(!author_match(byline, &fold_needle("Adi Shamir")));
    }

    #[test]
    fn extra_words_in_a_name_cost_nothing() {
        // Why middle names need no rule: the matcher already ignores them.
        let byline = "Yael Tauman Kalai; Ron D. Rothblum; Angelo De Caro";
        assert!(author_match(byline, &fold_name("Yael Kalai")));
        assert!(author_match(byline, &fold_name("Ron Rothblum")));
        assert!(author_match(byline, &fold_name("Angelo Caro")));
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
    fn the_table_invents_nobody() {
        // Every representative must be spellable from words the archive uses: the
        // check that a typo in the table cannot conjure up a person. Words are
        // taken from the entry itself, so this is really a shape check — no empty
        // entries, no stray that is not a folded name, no duplicate keys.
        let mut seen: HashMap<String, &str> = HashMap::new();
        for (rep, strays) in PEOPLE {
            assert!(!rep.trim().is_empty(), "empty representative");
            assert!(
                rep.split_whitespace().count() >= 2,
                "a representative needs a first and last name: {rep}"
            );
            let key = skeleton(&fold_name(rep));
            if let Some(other) = seen.insert(key.clone(), rep) {
                panic!("{rep} and {other} share the key {key}");
            }
            for stray in *strays {
                assert_eq!(*stray, fold_name(stray), "a stray must be folded: {stray}");
                assert_ne!(
                    skeleton(stray),
                    key,
                    "{stray} needs no entry: the skeleton already reaches {rep}"
                );
                assert_eq!(canonical(stray), Some(*rep), "{stray}");
            }
        }
    }

    #[test]
    fn one_row_per_thing_the_shell_would_insert() {
        // Completion is keyed by the inserted text, so two spellings that deaccent
        // alike cannot produce two identical menu rows. The archive has both
        // `Ivana Klasovita` and `Ivana Klasovitá`, one paper each, and the old build
        // offered them twice over, each row claiming one paper where picking either
        // returns two.
        assert_eq!(deaccent("Ivana Klasovitá"), deaccent("Ivana Klasovita"));
        // The predicate behind the count agrees with the filter the value becomes.
        let needle = fold_needle("Ivana Klasovita");
        let words: Vec<&str> = needle.split_whitespace().collect();
        assert!(name_matches("Ivana Klasovitá", &words));
        assert!(name_matches("Ivana Klasovita", &words));
    }

    #[test]
    fn surnames_split_off_the_end() {
        assert_eq!(
            split_surname("Adi Shamir").unwrap(),
            ("Shamir", "Adi".into())
        );
        assert_eq!(
            split_surname("Ivan Bjerre Damgård").unwrap(),
            ("Damgård", "Ivan Bjerre".into())
        );
        assert!(split_surname("Cher").is_none());
    }
}
