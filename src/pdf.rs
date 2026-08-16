//! A local PDF library, filled as a side effect of opening papers.
//!
//! Nothing here fetches a PDF. `eprint.iacr.org` serves them behind a Cloudflare
//! challenge and its `robots.txt` denies `*pdf` to every agent — "Full text PDFs
//! are only available under a license specific to each paper" — so the browser
//! does the fetching and this module only files what the browser saved. Do not
//! add an HTTP client to this file.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long to keep watching for the download after the browser opens. Long
/// enough to read an abstract and decide to save, short enough that a forgotten
/// helper cannot pick up an unrelated download much later.
const WINDOW: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(500);
/// Extensions browsers use for a download still in flight.
const PARTIAL: [&str; 4] = ["crdownload", "download", "part", "tmp"];

/// Where filed PDFs live. `EPRINT_PAPERS_DIR` overrides, matching the
/// `EPRINT_DB`/`EPRINT_CONFIG` convention and keeping tests off real folders.
pub fn library_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("EPRINT_PAPERS_DIR") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::document_dir()
        .or_else(dirs::home_dir)
        .context("could not determine a documents directory")?;
    Ok(base.join("eprint"))
}

/// Everywhere a browser might drop the file. Save dialogs suggest whatever the
/// browser was last pointed at, and no CLI can change that, so the answer is to
/// stop caring: watch the handful of plausible places instead of asking the user
/// to navigate to one. The per-file rules below are what keep this safe.
fn watch_dirs() -> Vec<PathBuf> {
    if let Ok(p) = std::env::var("EPRINT_DOWNLOAD_DIR") {
        return vec![PathBuf::from(p)];
    }
    let mut dirs: Vec<PathBuf> = [dirs::download_dir(), dirs::desktop_dir(), dirs::home_dir()]
        .into_iter()
        .flatten()
        .collect();
    // Ubuntu's default Firefox is a snap, and a confined snap saves here rather
    // than to the real ~/Downloads — so the adopter would sit watching a
    // directory the browser never writes to. Added rather than substituted,
    // because the same machine may also run an unconfined browser.
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("snap/firefox/common/Downloads"));
    }
    dirs.dedup();
    // Also what keeps the snap path out of the list on every other machine.
    dirs.retain(|d| d.is_dir());
    dirs
}

/// The directories named in the save hint, so the message and the behaviour
/// cannot drift apart. The home directory is described rather than named — its
/// basename is the username, which reads as nonsense in a sentence.
pub fn watched_names() -> Vec<String> {
    let home = dirs::home_dir();
    watch_dirs()
        .iter()
        .map(|d| {
            if Some(d) == home.as_ref() {
                "your home folder".to_string()
            } else {
                d.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| d.display().to_string())
            }
        })
        .collect()
}

/// `2026/1523` -> `(2026, 1523)`. Also parses the filename form `2026-1523-…`.
fn split_id(s: &str) -> Option<(i64, i64)> {
    let mut parts = s.splitn(3, ['/', '-']);
    let year = parts.next()?.trim().parse().ok()?;
    let num: String = parts
        .next()?
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    Some((year, num.parse().ok()?))
}

/// Trailing part of the filename: enough of the title to recognise the paper in
/// a file manager, and nothing that needs quoting in a shell.
fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut words = 0;
    for word in title.split_whitespace() {
        let clean: String = word
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if clean.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&clean);
        words += 1;
        if words >= 6 || out.len() > 60 {
            break;
        }
    }
    out
}

/// The canonical filename for a paper. Zero-padded so a directory listing sorts
/// the way a human expects.
pub fn file_name(id: &str, title: &str) -> String {
    let (year, num) = split_id(id).unwrap_or((0, 0));
    let s = slug(title);
    if s.is_empty() {
        format!("{year:04}-{num:04}.pdf")
    } else {
        format!("{year:04}-{num:04}-{s}.pdf")
    }
}

/// The filed copy of a paper, if there is one. Matches on the numeric id parsed
/// out of each filename, so zero-padding and any renamed tail both still match.
///
/// Also picks up a PDF the user saved into the library themselves — the browser's
/// save dialog produces `1523.pdf` — and renames it canonically on the way past,
/// so saving straight into the folder works even long after the watcher exited.
/// A bare number is accepted only when the name carries no year of its own: a
/// stem that says `2025-1523` is a different paper and must not be served.
pub fn cached(id: &str) -> Option<PathBuf> {
    let want = split_id(id)?;
    let dir = library_dir().ok()?;
    let mut loose: Option<PathBuf> = None;
    for entry in fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pdf") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if split_id(stem) == Some(want) {
            return Some(path);
        }
        if split_id(stem).is_none()
            && names_paper(stem, want.1)
            && !claims_other_year(stem, want.0, want.1)
            && is_pdf(&path)
        {
            loose = Some(path);
        }
    }
    let path = loose?;
    let dest = dir.join(file_name(id, &title_of(id).unwrap_or_default()));
    match fs::rename(&path, &dest) {
        // A rename inside the folder the user chose for these files, so nothing
        // moves out from under them.
        Ok(()) => Some(dest),
        Err(_) => Some(path),
    }
}

/// Delete a paper's filed copy, returning the path that went. `None` means there
/// was nothing filed for that id — a miss, not a failure.
///
/// The path comes from `cached()`, so it is inside the library by construction;
/// it is re-checked anyway, because this is the only destructive operation in the
/// tool and a future change to `cached` must not be able to aim it at a file the
/// user did not put here.
pub fn remove(id: &str) -> Result<Option<PathBuf>> {
    let Some(path) = cached(id) else {
        return Ok(None);
    };
    let dir = library_dir()?;
    if path.parent() != Some(dir.as_path()) {
        anyhow::bail!(
            "refusing to delete {}: it is not in {}",
            path.display(),
            dir.display()
        );
    }
    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    Ok(Some(path))
}

/// The title is only needed to build a filename, and the caller of `cached` may
/// not have a database handle, so look it up on demand and shrug off failure.
fn title_of(id: &str) -> Option<String> {
    let conn = crate::db::open().ok()?;
    crate::db::get(&conn, id).ok()?.map(|p| p.title)
}

/// Every paper in the library, newest first. The listing counterpart to `cached`,
/// which does the same scan looking for one id.
pub fn library() -> Vec<(String, PathBuf)> {
    let Ok(dir) = library_dir() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<((i64, i64), String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pdf") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some((y, n)) = split_id(stem) {
            out.push(((y, n), format!("{y}/{n}"), path));
        }
    }
    // Numerically, not by string: "2026/674" sorts above "2026/1523" as text.
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, id, path)| (id, path)).collect()
}

/// The filename's own words, for a paper the index has never heard of: the slug
/// read back as a sentence beats showing nothing.
pub fn slug_words(path: &Path) -> String {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return String::new();
    };
    stem.splitn(3, '-').nth(2).unwrap_or("").replace('-', " ")
}

/// True when the filename names a *different* year than the one being asked for,
/// wherever in the name that year sits.
///
/// `split_id` only recognises a year at the very start, so `paper-2025-1523.pdf`
/// slipped past the "never serve 2025/1523 for 2026/1523" rule and was not only
/// opened but renamed to claim it was the 2026 paper. A group equal to the paper
/// number is skipped first: papers numbered 1990-2100 exist, and `1999.pdf` is a
/// paper number, not a year.
fn claims_other_year(stem: &str, want_year: i64, want_num: i64) -> bool {
    stem.split(|c: char| !c.is_ascii_digit())
        .filter(|g| g.len() == 4)
        .filter_map(|g| g.parse::<i64>().ok())
        .filter(|y| *y != want_num)
        .any(|y| (1990..=2100).contains(&y) && y != want_year)
}

/// True when this download plausibly *is* the paper: ePrint serves
/// `/YYYY/NNNN.pdf`, so browsers save `NNNN.pdf`. Requiring the number rules out
/// filing an unrelated download that happened to finish inside the window.
fn names_paper(stem: &str, num: i64) -> bool {
    let digits: Vec<String> = stem
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_start_matches('0').to_string())
        .collect();
    let target = num.to_string();
    digits.iter().any(|d| *d == target)
}

fn is_pdf(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 5];
    f.read_exact(&mut head).is_ok() && &head == b"%PDF-"
}

/// One poll's view of the candidate file, so the next poll can tell whether the
/// browser is still writing to it.
struct Seen {
    path: PathBuf,
    size: u64,
    mtime: SystemTime,
    quiet: u32,
}

/// Watch the download directory and file the paper's PDF when it appears.
///
/// Deliberately silent: this runs detached, behind an `open` the user has already
/// moved on from, so it must never print or fail loudly.
pub fn adopt(id: &str, title: &str) {
    let Some((year, num)) = split_id(id) else {
        return;
    };
    let watching = watch_dirs();
    if watching.is_empty() {
        return;
    }
    let Ok(library) = library_dir() else { return };
    // A file saved straight into the library was put there on purpose, so it
    // needs no freshness test — just the canonical name, which `cached` applies.
    if cached(id).is_some() {
        return;
    }
    let started = SystemTime::now();
    let deadline = started + WINDOW;

    // Remembered between polls so a file is only taken once it stops changing.
    // Two consecutive quiet polls, not one: a writer that pauses for a single
    // poll interval would otherwise look finished and be copied half-written.
    let mut pending: Option<Seen> = None;

    while SystemTime::now() < deadline {
        std::thread::sleep(POLL);
        if cached(id).is_some() {
            return;
        }
        let mut best: Option<(PathBuf, u64, SystemTime)> = None;
        for entry in watching
            .iter()
            .filter_map(|d| fs::read_dir(d).ok())
            .flatten()
            .flatten()
        {
            let path = entry.path();
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if PARTIAL.contains(&ext.to_ascii_lowercase().as_str()) {
                continue;
            }
            if !ext.eq_ignore_ascii_case("pdf") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !names_paper(stem, num) || claims_other_year(stem, year, num) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            // Only files that arrived after we started watching, so an old
            // download of the same paper is never re-filed.
            let fresh = meta.modified().map(|m| m >= started).unwrap_or(false);
            if !fresh {
                continue;
            }
            let mtime = meta.modified().unwrap_or(started);
            if best.as_ref().map(|(_, _, m)| mtime >= *m).unwrap_or(true) {
                best = Some((path, meta.len(), mtime));
            }
        }

        let Some((path, size, mtime)) = best else {
            pending = None;
            continue;
        };
        let quiet = match &pending {
            Some(prev) if prev.path == path && prev.size == size && prev.mtime == mtime => {
                prev.quiet + 1
            }
            _ => 0,
        };
        if quiet >= 2 && size > 0 {
            if !is_pdf(&path) {
                return;
            }
            if fs::create_dir_all(&library).is_err() {
                return;
            }
            // Copy, not rename: moving a file out of someone's Downloads behind
            // their back is worse than a duplicate megabyte. Via a temp name so
            // the library never contains a half-written PDF either.
            let dest = library.join(file_name(id, title));
            let tmp = dest.with_extension("pdf.part");
            if fs::copy(&path, &tmp).is_ok() && fs::rename(&tmp, &dest).is_err() {
                let _ = fs::remove_file(&tmp);
            }
            return;
        }
        pending = Some(Seen {
            path,
            size,
            mtime,
            quiet,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filename_naming_another_year_is_another_paper() {
        // `paper-2025-1523.pdf` was served for 2026/1523 and renamed to claim it
        // was that paper: `split_id` only sees a year at the start of the name.
        assert!(claims_other_year("paper-2025-1523", 2026, 1523));
        assert!(claims_other_year("downloads/2019-77-copy", 2026, 77));
        // The same year, however it is written, is not a claim about another.
        assert!(!claims_other_year("paper-2026-1523", 2026, 1523));
        assert!(!claims_other_year("1523", 2026, 1523));
        // Papers numbered in the 1990s exist; the number is not a year.
        assert!(!claims_other_year("1999", 2026, 1999));
        assert!(!claims_other_year("2026-1999-title", 2026, 1999));
    }

    #[test]
    fn ids_and_filenames_agree() {
        assert_eq!(split_id("2026/1523"), Some((2026, 1523)));
        assert_eq!(split_id("2026-1523-some-title"), Some((2026, 1523)));
        assert_eq!(split_id("nonsense"), None);
        assert_eq!(file_name("2026/1523", "A Title"), "2026-1523-a-title.pdf");
        // No usable title: still a canonical, sortable name.
        assert_eq!(file_name("2026/674", ""), "2026-0674.pdf");
        assert_eq!(file_name("2026/674", "!!! ???"), "2026-0674.pdf");
    }

    #[test]
    fn a_download_has_to_name_the_paper() {
        assert!(names_paper("1523", 1523));
        assert!(names_paper("1523 (1)", 1523));
        assert!(names_paper("0001523", 1523));
        assert!(!names_paper("15230", 1523));
        assert!(!names_paper("thesis", 1523));
    }
}
