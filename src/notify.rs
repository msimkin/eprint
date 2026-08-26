//! Desktop banners for papers that arrived while nobody was looking.
//!
//! There is no notification crate here for the same reason there is no `chrono` and
//! no base64 crate: every platform already ships a program that does this, and
//! shelling out to it is the whole implementation. The shape is the one
//! `tui::copy_to_clipboard` already uses — a per-platform candidate list, best
//! first, where a missing binary is the next candidate's turn rather than a failure.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;

use crate::{dates, db, render};

/// This module's own watermark, and it must stay its own. `harvest::KEY_LAST_SEEN`
/// is advanced only by the feed actually being *displayed*; consuming it from here
/// would make the next bare `eprint` report "nothing new" about papers the user has
/// so far only glimpsed on a banner. Kept local like `completions.rs`'s
/// `completions_hint` rather than published in `harvest.rs`, because nothing else
/// has any business reading it.
const KEY_NOTIFIED: &str = "notified_through";

/// ePrint posts in bursts, and forty in one morning is ordinary. macOS stacks
/// banners from a single sender, so an uncapped burst is not news but a wall to
/// dismiss by hand; past this many, the remainder becomes one roll-up line.
const ALL_MAX: usize = 5;

/// A sanity bound rather than a policy, matching `BATCH_MAX`. A batch that fills it
/// is reported as `500+` instead of as a number that is quietly wrong.
const NOTIFY_CAP: usize = 500;

/// Titles in this archive run long, and both backends truncate for themselves —
/// but they do it by bytes, which can cut a multi-byte character in half.
const TITLE_MAX: usize = 110;

/// Where a banner that is not about one specific paper lands when clicked: the
/// summary, the roll-up and the `--notify` confirmation all point at the archive
/// itself, so no banner is ever a dead end. Per-paper banners carry the paper's
/// own page instead.
const SITE: &str = "https://eprint.iacr.org";

/// What to announce. `Off` is the default, so nothing here happens to anyone who
/// has not asked for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Off,
    All,
    Summary,
    Watched,
}

impl Mode {
    /// Every accepted spelling, for `--help` text and completion.
    pub const NAMES: [&'static str; 4] = ["off", "all", "summary", "watched"];

    /// `None` for anything unrecognised, so the caller decides what that means: the
    /// config file falls back to `Off` the way an unreadable `theme` falls back to
    /// its default, while `eprint config --notify` refuses outright. A parser that
    /// picked one of those behaviours for both would be wrong somewhere.
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Mode::Off),
            "all" => Some(Mode::All),
            "summary" => Some(Mode::Summary),
            "watched" => Some(Mode::Watched),
            _ => None,
        }
    }

    /// The spelling written back to the config file. Must parse back to `self`.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::All => "all",
            Mode::Summary => "summary",
            Mode::Watched => "watched",
        }
    }
}

/// The programs worth trying, best first.
///
/// `terminal-notifier` is strictly better than `osascript` where it exists: it is
/// attributed to itself rather than to "Script Editor", it takes a subtitle, and
/// `-open` makes the banner click through to the paper. It is also not installed on
/// a stock Mac, which is why `osascript` has to be the one that always works.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Backend {
    TerminalNotifier,
    Osascript,
    NotifySend,
}

fn candidates() -> Vec<Backend> {
    candidates_for(cfg!(target_os = "macos"))
}

/// Split out from the `cfg!` so the Linux list can be tested from macOS, where it
/// is compiled but never reached — the same reason `tui::candidates_for` exists.
fn candidates_for(macos: bool) -> Vec<Backend> {
    if macos {
        vec![Backend::TerminalNotifier, Backend::Osascript]
    } else {
        vec![Backend::NotifySend]
    }
}

/// What to tell someone who has none of them, naming the *package* rather than the
/// binary — the `tui::clipboard_hint` rule. On macOS there is nothing to install,
/// so this only ever fires on a minimal Linux box.
pub fn hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "no notification tool — osascript should be built in"
    } else {
        "no notification tool — sudo apt install libnotify-bin"
    }
}

/// The one improvement worth suggesting when posting already works: only
/// `terminal-notifier` can make a click open the paper. An `osascript` banner is
/// attributed to Script Editor and clicking it opens *that*, which reads as a bug
/// rather than as a missing feature — so `config --notify` says so while the user
/// is watching, instead of leaving the first click to explain itself. Linux gets
/// no equivalent: `notify-send` actions (`-A`) block until the banner is dismissed,
/// which on a tray can be indefinitely — an unbounded child, which nothing in this
/// codebase is allowed to be.
pub fn click_hint() -> Option<&'static str> {
    if cfg!(target_os = "macos") && !have("terminal-notifier") {
        Some(
            "clicking a banner opens Script Editor rather than the paper — \
             brew install terminal-notifier to make clicks open the page",
        )
    } else {
        None
    }
}

/// `Command::new` can only answer by failing to spawn; existence is a PATH walk.
fn have(prog: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
        .unwrap_or(false)
}

/// A banner, in the three fields macOS offers. Linux has two, so `subtitle` and
/// `body` are joined there rather than dropped.
struct Banner<'a> {
    title: &'a str,
    subtitle: &'a str,
    body: &'a str,
    /// Followed when the banner is clicked, where the backend can do that at all.
    url: &'a str,
}

/// Build the argument vector for one backend.
///
/// **Text never reaches AppleScript as source.** Titles in this archive carry
/// quotes, backslashes and raw TeX, so splicing one into `-e 'display notification
/// "…"'` is a quoting bug waiting for the right paper. `on run argv` takes it as
/// data instead, which cannot be misread whatever it contains. The `--` is not
/// decoration either: without it `osascript` reads a title beginning with `-` as its
/// own option and exits with a usage message.
fn argv(b: Backend, n: &Banner) -> (&'static str, Vec<String>) {
    match b {
        Backend::Osascript => {
            let mut a: Vec<String> = [
                "-e",
                "on run argv",
                "-e",
                "display notification (item 1 of argv) with title (item 2 of argv) \
                 subtitle (item 3 of argv)",
                "-e",
                "end run",
                "--",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            a.push(n.body.to_string());
            a.push(n.title.to_string());
            a.push(n.subtitle.to_string());
            ("osascript", a)
        }
        Backend::TerminalNotifier => {
            let mut a = vec![
                "-title".to_string(),
                n.title.to_string(),
                "-subtitle".to_string(),
                n.subtitle.to_string(),
                "-message".to_string(),
                n.body.to_string(),
            ];
            if !n.url.is_empty() {
                a.push("-open".to_string());
                a.push(n.url.to_string());
            }
            ("terminal-notifier", a)
        }
        Backend::NotifySend => {
            // Two fields, not three, so the subtitle joins the body rather than
            // being thrown away. `--` for the same reason as above.
            let body = match (n.subtitle.is_empty(), n.body.is_empty()) {
                (true, _) => n.body.to_string(),
                (false, true) => n.subtitle.to_string(),
                (false, false) => format!("{}\n{}", n.subtitle, n.body),
            };
            (
                "notify-send",
                vec![
                    "-a".to_string(),
                    "eprint".to_string(),
                    "--".to_string(),
                    n.title.to_string(),
                    body,
                ],
            )
        }
    }
}

/// First backend that exists and exits cleanly wins. A missing binary is not a
/// failure, just the next one's turn.
fn post(n: &Banner) -> bool {
    for b in candidates() {
        let (prog, args) = argv(b, n);
        let status = std::process::Command::new(prog)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => return true,
            // Not installed, or it failed — either way, try the next one.
            _ => continue,
        }
    }
    false
}

/// Strip everything a banner cannot render.
///
/// A `Hit` from `db::added_since` carries raw text, but one from `db::search` carries
/// `db::MARK_START`/`MARK_END`, and a stray control character reaches a banner as
/// garbage rather than as an error. Whitespace is collapsed in the same pass because
/// the archive's own metadata wraps titles across lines. Truncation counts
/// characters, not bytes, so it can never split one.
fn plain(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 4));
    let mut space = false;
    let mut n = 0;
    for c in s.chars() {
        if c.is_control() || c == db::MARK_START || c == db::MARK_END || c.is_whitespace() {
            // Leading whitespace is dropped, interior runs collapse to one space.
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            n += 1;
            space = false;
        }
        if n >= max {
            out.push('…');
            break;
        }
        out.push(c);
        n += 1;
    }
    out
}

/// `7 new papers`, and `500+` once the query cap is full rather than a number that
/// is quietly wrong.
fn summary_line(n: usize) -> String {
    if n >= NOTIFY_CAP {
        return format!("{NOTIFY_CAP}+ new papers");
    }
    format!("{n} new paper{}", if n == 1 { "" } else { "s" })
}

/// One banner per paper, then a roll-up for whatever the cap cut off. `why` supplies
/// the body — the matching watch in `Watched` mode, the category otherwise.
fn announce_each(hits: &[db::Hit], why: impl Fn(&db::Hit) -> String) -> usize {
    let mut posted = 0;
    for hit in hits.iter().take(ALL_MAX) {
        let p = &hit.paper;
        let title = plain(&p.title, TITLE_MAX);
        // The heart is deliberately absent: `favourite_author` is something the
        // terminal does, and a banner is not the terminal.
        let subtitle = format!("{} · {}", p.id, render::short_authors(&p.authors, None));
        let body = why(hit);
        if post(&Banner {
            title: &title,
            subtitle: &subtitle,
            body: &body,
            url: &p.url,
        }) {
            posted += 1;
        }
    }
    let extra = hits.len().saturating_sub(ALL_MAX);
    if extra > 0 {
        let body = format!(
            "+{extra} more new paper{}",
            if extra == 1 { "" } else { "s" }
        );
        if post(&Banner {
            title: "eprint",
            subtitle: "",
            body: &body,
            url: SITE,
        }) {
            posted += 1;
        }
    }
    posted
}

/// Confirm, on screen, that notifications work — posted by `eprint config --notify`
/// while the user is watching. That is the point of it: macOS asks permission the
/// first time anything posts a banner, and the moment to be asked is now, not
/// silently at three in the morning.
pub fn confirm(mode: Mode) -> bool {
    post(&Banner {
        title: "eprint",
        subtitle: "",
        body: &format!("notifications are on ({})", mode.label()),
        // So the very first banner is also the click-through test.
        url: SITE,
    })
}

/// The whole pass: diff against this module's watermark, post, advance it.
///
/// Returns how many banners were delivered, which is only ever used for a message —
/// nothing decides anything on it.
pub fn announce(conn: &Connection, mode: Mode, watches: &[db::Watch]) -> Result<usize> {
    if mode == Mode::Off {
        return Ok(0);
    }
    // Stamped from before the query, so a paper arriving mid-pass is announced next
    // time rather than skipped.
    let now = dates::format_iso(dates::now());

    let Some(since) = db::meta_get(conn, KEY_NOTIFIED)? else {
        // The first run announces nothing. Anything else means that switching
        // notifications on produces a banner for every paper in the default
        // seven-day window — or, on a fresh index, for the entire archive.
        db::meta_set(conn, KEY_NOTIFIED, &now)?;
        return Ok(0);
    };

    let hits = db::added_since(conn, &since, NOTIFY_CAP)?;
    let posted = match mode {
        Mode::Off => 0,
        Mode::Summary => {
            if hits.is_empty() {
                0
            } else {
                usize::from(post(&Banner {
                    title: "eprint",
                    subtitle: "",
                    body: &summary_line(hits.len()),
                    url: SITE,
                }))
            }
        }
        Mode::All => announce_each(&hits, |h| plain(&h.paper.category, TITLE_MAX)),
        Mode::Watched => {
            // The cache is keyed by `Watch::label()`, the config form; a banner wants
            // the sentence, which is what every other watch display uses.
            let described: HashMap<String, String> =
                watches.iter().map(|w| (w.label(), w.describe())).collect();
            let ids: Vec<String> = hits.iter().map(|h| h.paper.id.clone()).collect();
            let by_id = db::watch_labels(conn, &ids)?;
            let matched: Vec<db::Hit> = hits
                .into_iter()
                .filter(|h| by_id.contains_key(&h.paper.id))
                .collect();
            announce_each(&matched, |h| {
                by_id
                    .get(&h.paper.id)
                    .and_then(|labels| labels.first())
                    .and_then(|l| described.get(l))
                    .cloned()
                    .unwrap_or_default()
            })
        }
    };

    // Advanced whether or not anything was posted, and whether or not the backend
    // worked. A watermark that only moved on success would re-announce the same
    // batch every thirty minutes on a machine with no notifier installed.
    db::meta_set(conn, KEY_NOTIFIED, &now)?;
    Ok(posted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip() {
        for name in Mode::NAMES {
            let m = Mode::parse(name).expect("a documented name must parse");
            assert_eq!(m.label(), name, "label() must be the inverse of parse()");
        }
        // Case and stray whitespace are what a hand-edited config file produces.
        assert_eq!(Mode::parse("  Summary "), Some(Mode::Summary));
        // Unrecognised is *not* silently a mode: the caller decides.
        assert_eq!(Mode::parse("verbose"), None);
        assert_eq!(Mode::parse(""), None);
    }

    #[test]
    fn plain_strips_markers_and_control_characters() {
        let marked = format!("Lattice {}LWE{} attack", db::MARK_START, db::MARK_END);
        assert_eq!(plain(&marked, 100), "Lattice LWE attack");
        assert_eq!(
            plain("  wrapped\n  title\t here ", 100),
            "wrapped title here"
        );
        assert_eq!(plain("", 100), "");
    }

    #[test]
    fn plain_never_splits_a_character() {
        // Every char here is multi-byte, so a byte-wise truncation would panic or
        // emit a broken sequence.
        let s = "Krzysztof Pietrzak и Даниэль Вихс — Ωμέγα";
        let cut = plain(s, 10);
        assert!(cut.chars().count() <= 11, "{cut:?}");
        assert!(cut.ends_with('…'));
        // And a string exactly at the limit keeps every character, no ellipsis.
        assert_eq!(plain("abcde", 5), "abcde");
    }

    #[test]
    fn summary_counts_read_as_english() {
        assert_eq!(summary_line(1), "1 new paper");
        assert_eq!(summary_line(7), "7 new papers");
        assert_eq!(summary_line(0), "0 new papers");
        // A full query cap is not a count.
        assert_eq!(summary_line(NOTIFY_CAP), "500+ new papers");
    }

    #[test]
    fn linux_candidates_are_reachable_from_macos() {
        assert_eq!(
            candidates_for(true),
            vec![Backend::TerminalNotifier, Backend::Osascript]
        );
        assert_eq!(candidates_for(false), vec![Backend::NotifySend]);
    }

    #[test]
    fn osascript_takes_text_as_arguments_not_as_source() {
        let n = Banner {
            title: r#"On the "Hardness" of \LWE"#,
            subtitle: "2026/1540 · Boneh",
            body: "Public-key cryptography",
            url: "https://eprint.iacr.org/2026/1540",
        };
        let (prog, args) = argv(Backend::Osascript, &n);
        assert_eq!(prog, "osascript");
        // The text appears only after `--`, never inside a `-e` statement.
        let sep = args.iter().position(|a| a == "--").expect("needs a --");
        for stmt in &args[..sep] {
            assert!(
                !stmt.contains("Hardness"),
                "text spliced into script source"
            );
        }
        assert!(args[sep + 1..].contains(&n.title.to_string()));
    }

    #[test]
    fn terminal_notifier_gets_the_click_url() {
        // Clicking a banner opened Script Editor for want of this: the summary and
        // roll-up banners carried no URL, so even the backend that can click
        // through had nowhere to go.
        let n = Banner {
            title: "eprint",
            subtitle: "",
            body: "7 new papers",
            url: SITE,
        };
        let (prog, args) = argv(Backend::TerminalNotifier, &n);
        assert_eq!(prog, "terminal-notifier");
        let at = args.iter().position(|a| a == "-open").expect("needs -open");
        assert_eq!(args[at + 1], SITE);

        // And no dangling `-open` with nowhere to go — terminal-notifier would
        // read the next flag as its value.
        let bare = Banner {
            title: "eprint",
            subtitle: "",
            body: "b",
            url: "",
        };
        let (_, args) = argv(Backend::TerminalNotifier, &bare);
        assert!(!args.contains(&"-open".to_string()));
    }

    #[test]
    fn notify_send_folds_the_subtitle_into_the_body() {
        let n = Banner {
            title: "T",
            subtitle: "S",
            body: "B",
            url: "",
        };
        let (prog, args) = argv(Backend::NotifySend, &n);
        assert_eq!(prog, "notify-send");
        assert_eq!(args.last().unwrap(), "S\nB");
        // A title that begins with a dash must not be read as an option.
        assert!(args.contains(&"--".to_string()));
    }
}
