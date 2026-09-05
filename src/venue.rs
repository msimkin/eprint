//! Where a paper was published, read out of its CryptoBib record.
//!
//! `bib.entry` already stores the complete BibTeX record with every `@String`
//! macro inlined, so `booktitle = {Advances in Cryptology - CRYPTO 2025}` is
//! literal text in the index for every published paper. Nothing here downloads
//! anything; it only reads what a CryptoBib refresh already wrote.
//!
//! This lived in the iOS repo, as the generator that built its bundled asset, and
//! was invisible to the terminal although the terminal held the same `bib` table.
//! Moving it down is what lets one `venues` table serve both front-ends — and it
//! is what makes `Query::venue` reachable from here at all, since `filter_sql`
//! has always emitted SQL against a table only the phone created.

/// The venue without its edition: `CRYPTO~2024, Part~III` → `CRYPTO`.
///
/// The year is carried separately, so a venue browses as one series across years
/// rather than as one entry per proceedings volume. Four rules:
///
/// - `, Part~N` is a publisher's volume split, never a different venue.
/// - An ordinal is the edition written out, and it is not always leading:
///   `Post-Quantum Cryptography - 16th International Workshop, PQCrypto`.
/// - Any year-shaped token goes, wherever it sits: trailing (`SCN 04`), leading
///   (`2024 IEEE European Symposium…`), mid-string (`CSF 2024 Computer Security
///   Foundations Symposium`), glued on with an apostrophe (`CRYPTO'97`), or
///   carrying a volume letter (`TCC 2016-A`).
/// - What is left is looked up in [`DISPLAY`].
///
/// An earlier version stripped a year **only** when it equalled the entry's own
/// `year` field, on the reasoning that a venue might legitimately end in a number.
/// That was too cautious and the cost was not eleven papers as estimated but a
/// third of the list: proceedings routinely appear the year after the meeting, so
/// `FSE 2014`, `SAC 2015`, `ICISC 09` and `LATINCRYPT 2017` all survived as
/// separate venues beside the series they belong to. No venue in this corpus has a
/// number in its name — asserted by `cargo test`, which fails if one appears.
fn series(raw: &str) -> String {
    let mut v = raw.replace('~', " ").replace(['{', '}'], "");

    if let Some(at) = v.rfind(", Part ") {
        if v[at + 7..]
            .chars()
            .all(|c| "IVXLC".contains(c) || c.is_whitespace())
        {
            v.truncate(at);
        }
    }

    let kept: Vec<&str> = v
        .split_whitespace()
        .filter(|w| !is_ordinal(w) && !is_year(w))
        // `CRYPTO'97` carries the year inside the word with no space to split on,
        // so the whole-word test above never sees it and the venue would browse as
        // one series per year.
        .map(strip_glued_year)
        .collect();
    let cleaned = kept
        .join(" ")
        .trim_matches(|c: char| c == ',' || c == '-' || c.is_whitespace())
        .to_string();

    DISPLAY
        .iter()
        .find(|(from, _)| *from == cleaned)
        .map(|(_, to)| (*to).to_string())
        .unwrap_or(cleaned)
}

/// `CRYPTO'97` → `CRYPTO`. Only when what follows the apostrophe is itself
/// year-shaped, so a name that legitimately carries one keeps it.
fn strip_glued_year(w: &str) -> &str {
    for sep in ['\'', '\u{2019}', '`'] {
        if let Some((head, tail)) = w.rsplit_once(sep) {
            if !head.is_empty() && is_year(tail) {
                return head;
            }
        }
    }
    w
}

fn is_ordinal(w: &str) -> bool {
    let stem = w.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    !stem.is_empty()
        && stem.chars().all(|c| c.is_ascii_digit())
        && matches!(&w[stem.len()..], "st" | "nd" | "rd" | "th")
}

/// Year-shaped: two or four digits, optionally quoted (`'97`) and optionally with a
/// volume letter (`2016-A`). Four digits are bounded to a plausible range so a
/// number that is not a year survives; two digits are unbounded, because `04` is a
/// year in every venue name here and nothing else.
fn is_year(w: &str) -> bool {
    let w = w.trim_start_matches(['\'', '\u{2019}', '`']);
    let core = w.split_once('-').map_or(w, |(a, b)| {
        if b.len() == 1 && b.chars().all(|c| c.is_ascii_uppercase()) {
            a
        } else {
            w
        }
    });
    if !core.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    match core.len() {
        2 => true,
        4 => core
            .parse::<u32>()
            .is_ok_and(|y| (1970..=2040).contains(&y)),
        _ => false,
    }
}

/// What a venue is called on screen.
///
/// Two rules, both from the tool's owner. **Nothing begins with `IACR`** — the
/// archive is IACR's, so it says nothing, and CryptoBib applies it inconsistently
/// (`IACR TCHES` but plain `CRYPTO`). And **no abbreviation-with-full-stops**:
/// `IACR Trans. Symm. Cryptol.` is a bibliography style, not a name anyone says.
///
/// Conference acronyms are left alone deliberately, and that is not an
/// inconsistency: `CRYPTO` and `ACM CCS` *are* what these are called, while the
/// journals are known by their full titles. Spelling out `TCC` would be pedantry in
/// the one place a reader is scanning for a name they already know.
///
/// Anything absent from this table is shown exactly as CryptoBib wrote it.
const DISPLAY: &[(&str, &str)] = &[
    // Journals: spelled out, and the IACR prefix dropped.
    (
        "IACR Trans. Symm. Cryptol.",
        "Transactions on Symmetric Cryptology",
    ),
    (
        "IACR TCHES",
        "Transactions on Cryptographic Hardware and Embedded Systems",
    ),
    ("CiC", "Communications in Cryptology"),
    ("DCC", "Designs, Codes and Cryptography"),
    ("PoPETs", "Proceedings on Privacy Enhancing Technologies"),
    ("PETS", "Proceedings on Privacy Enhancing Technologies"),
    ("PET", "Proceedings on Privacy Enhancing Technologies"),
    (
        "IEE Proceedings --- Information Security",
        "IET Information Security",
    ),
    ("SIAM Journal on Computing", "SIAM Journal on Computing"),
    // Conferences whose stored name is a sentence rather than a name.
    ("IEEE Symposium on Security and Privacy", "IEEE S&P"),
    (
        "IEEE European Symposium on Security and Privacy",
        "IEEE EuroS&P",
    ),
    ("CSF Computer Security Foundations Symposium", "CSF"),
    (
        "IMA International Conference on Cryptography and Coding",
        "IMA Cryptography and Coding",
    ),
    (
        "Post-Quantum Cryptography - International Workshop, PQCrypto",
        "PQCrypto",
    ),
    ("Progress in Cryptology - VIETCRYPT", "VIETCRYPT"),
    ("ACM PODC", "PODC"),
];

/// The order venues are listed in.
///
/// A paper count is not the answer — it ranks by how much of a venue happens to be
/// on ePrint, which put `SAC` above `IEEE S&P` and `Designs, Codes and Cryptography`
/// above `USENIX Security`. This is a judgement about standing in the field instead,
/// and it is deliberately one editable list rather than a formula: the flagships,
/// then the big security conferences, then the IACR area conferences and workshops,
/// then theory, then the journals, then the rest.
///
/// Anything absent sorts alphabetically after everything here, which is the right
/// default for a long tail nobody is scanning for.
pub const RANK: &[&str] = &[
    "CRYPTO",
    "EUROCRYPT",
    "ASIACRYPT",
    "IEEE S&P",
    "ACM CCS",
    "USENIX Security",
    "NDSS",
    "TCC",
    "PKC",
    "CHES",
    "FSE",
    "ACM STOC",
    "FOCS",
    "SODA",
    "ITCS",
    "ICALP",
    "Journal of Cryptology",
    "Transactions on Symmetric Cryptology",
    "Transactions on Cryptographic Hardware and Embedded Systems",
    "Communications in Cryptology",
    "Proceedings on Privacy Enhancing Technologies",
    "ESORICS",
    "ASIACCS",
    "ACNS",
    "CT-RSA",
    "SAC",
    "SCN",
    "FC",
    "PQCrypto",
    "ITC",
    "ICITS",
    "PODC",
    "IEEE EuroS&P",
    "CSF",
    "LATINCRYPT",
    "INDOCRYPT",
    "ACISP",
    "AFRICACRYPT",
    "Designs, Codes and Cryptography",
    "Journal of Cryptographic Engineering",
    "SIAM Journal on Computing",
    "COSADE",
    "ProvSec",
    "ISC",
    "ICICS",
    "ICISC",
    "IWSEC",
    "CANS",
    "PAIRING",
    "WISA",
];

/// The venue a stored BibTeX record names, or nothing.
///
/// `booktitle` first, then `journal`: a record carries one or the other, and an
/// `@InProceedings` that also has a journal field is naming where the conference
/// version appeared, not the paper's venue.
///
/// Uses `bib::field`, which matches braces, is case-insensitive and respects field
/// boundaries. The iOS generator carried its own weaker copy — a plain
/// `find("booktitle ")` that could hit a substring of another field name — and that
/// copy is what this replaces.
pub fn of_entry(entry: &str) -> Option<(String, String)> {
    let raw = match crate::bib::field(entry, "booktitle") {
        s if !s.is_empty() => s,
        _ => crate::bib::field(entry, "journal"),
    };
    if raw.is_empty() {
        return None;
    }
    let s = series(&raw);
    if s.is_empty() {
        None
    } else {
        Some((s, year_of(entry).unwrap_or_default()))
    }
}

/// The record's own year — the proceedings year, which is not always the year the
/// meeting happened.
///
/// Read out of the entry text rather than taken from `bib.year`, and that is not
/// belt-and-braces: CryptoBib writes the year **unbraced** (`year = 2024,`) and
/// `bib::read_value` reads only `{...}` and `"..."` values, so that column is empty
/// for every row in the table. Parsing here keeps this module right either way.
fn year_of(entry: &str) -> Option<String> {
    let digits: String = crate::bib::field(entry, "year")
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.len() == 4 {
        return Some(digits);
    }
    for line in entry.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("year") {
            let digits: String = rest
                .trim_start_matches(|c: char| c == '=' || c.is_whitespace() || c == '{')
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if digits.len() == 4 {
                return Some(digits);
            }
        }
    }
    None
}

/// Where a venue sorts in a list of venues; unranked ones come last.
pub fn rank_of(venue: &str) -> usize {
    RANK.iter().position(|v| *v == venue).unwrap_or(RANK.len())
}

/// Split a typed venue into the series and an optional year.
///
/// `"CRYPTO 2025"` -> `("CRYPTO", Some("2025"))`, `"CRYPTO"` -> `("CRYPTO", None)`.
/// One flag carrying both parts, the way `--date` carries both ends of a range, and
/// one grammar shared by `--venue` and the browser's `v` — the same reason the date
/// prompt calls `dates::parse_range` rather than growing its own parser.
///
/// Only a *trailing* four-digit token counts, so `IEEE S&P` keeps its name and a
/// venue is never split on a number in the middle of it.
pub fn parse_filter(s: &str) -> (String, Option<String>) {
    let s = s.trim();
    if let Some((head, tail)) = s.rsplit_once(char::is_whitespace) {
        if tail.len() == 4 && tail.chars().all(|c| c.is_ascii_digit()) && !head.trim().is_empty() {
            return (head.trim().to_string(), Some(tail.to_string()));
        }
    }
    (s.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four stripping rules, one case each from the corpus that produced them.
    #[test]
    fn an_edition_is_not_a_venue() {
        assert_eq!(series("CRYPTO~2024, Part~III"), "CRYPTO");
        assert_eq!(series("SCN 04"), "SCN");
        assert_eq!(series("CRYPTO'97"), "CRYPTO");
        assert_eq!(series("TCC 2016-A"), "TCC");
        assert_eq!(
            series("Post-Quantum Cryptography - 16th International Workshop, PQCrypto"),
            "PQCrypto"
        );
        assert_eq!(
            series("2024 IEEE European Symposium on Security and Privacy"),
            "IEEE EuroS&P"
        );
    }

    /// `{{IACR} {TCHES}}` must not read as "IACR". This is the case the iOS
    /// generator's own weaker `field()` was written for, and the reason `of_entry`
    /// borrows `bib::field` rather than matching to the first `}`.
    #[test]
    fn a_protected_acronym_survives() {
        let entry = "@Article{X,\n  journal = {{IACR} {TCHES}},\n  year = {2025},\n}";
        assert_eq!(
            of_entry(entry),
            Some((
                "Transactions on Cryptographic Hardware and Embedded Systems".into(),
                "2025".into()
            ))
        );
    }

    #[test]
    fn booktitle_wins_over_journal() {
        let entry = "@InProceedings{X,\n  booktitle = {CRYPTO~2025},\n  journal = {DCC},\n}";
        assert_eq!(of_entry(entry), Some(("CRYPTO".into(), String::new())));
        assert_eq!(of_entry("@Misc{X,\n  title = {No venue},\n}"), None);
        // The unbraced form CryptoBib actually writes, which `bib.year` misses.
        let bare = "@InProceedings{X,\n  booktitle = {CRYPTO~2024},\n  year = 2024,\n}";
        assert_eq!(of_entry(bare), Some(("CRYPTO".into(), "2024".into())));
    }

    /// A year-shaped token is not always a year. Both halves shipped wrong once.
    #[test]
    fn year_shaped_but_not_a_year() {
        assert!(is_year("2016") && is_year("04") && is_year("'97") && is_year("2016-A"));
        assert!(!is_year("1969") && !is_year("2041") && !is_year("123") && !is_year("A"));
        assert!(is_ordinal("16th") && is_ordinal("1st") && !is_ordinal("th") && !is_ordinal("16"));
    }

    /// The two naming rules the display table exists to enforce. These were
    /// assertions over the generated asset in the iOS repo; they belong beside the
    /// table that decides them.
    #[test]
    fn display_names_follow_the_house_rules() {
        for (_, to) in DISPLAY {
            assert!(
                !to.starts_with("IACR"),
                "{to} begins with IACR — the archive is IACR's, so it says nothing"
            );
            assert!(
                !to.contains(". "),
                "{to} is an abbreviation-with-full-stops, not a name anyone says"
            );
            assert!(
                !to.chars().any(|c| c.is_ascii_digit()),
                "{to} carries a number — an edition survived the stripping"
            );
        }
    }

    #[test]
    fn ranking_puts_the_flagships_first_and_strangers_last() {
        assert_eq!(rank_of("CRYPTO"), 0);
        assert!(rank_of("EUROCRYPT") < rank_of("ACM CCS"));
        assert_eq!(rank_of("Some Workshop Nobody Ranked"), RANK.len());
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    /// The grammar `--venue` and the browser's `v` share. Only a *trailing*
    /// four-digit token is a year, so a venue keeps a number in the middle of its
    /// name and `IEEE S&P` keeps its own.
    #[test]
    fn a_trailing_year_splits_and_nothing_else_does() {
        assert_eq!(
            parse_filter("CRYPTO 2025"),
            ("CRYPTO".into(), Some("2025".into()))
        );
        assert_eq!(parse_filter("  CRYPTO  "), ("CRYPTO".into(), None));
        assert_eq!(parse_filter("IEEE S&P"), ("IEEE S&P".into(), None));
        assert_eq!(
            parse_filter("IEEE S&P 2025"),
            ("IEEE S&P".into(), Some("2025".into()))
        );
        // A bare year is a venue nobody has, not a yearless filter — splitting it
        // would silently turn `--venue 2025` into "every venue in 2025".
        assert_eq!(parse_filter("2025"), ("2025".into(), None));
        assert_eq!(parse_filter("ACM CCS 25"), ("ACM CCS 25".into(), None));
    }
}
