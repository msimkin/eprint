use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::db::{Scope, Watch};

#[derive(Clone, Debug)]
pub struct Config {
    /// auto | dark | light | mono
    pub theme: String,
    /// all | title
    pub scope: String,
    pub limit: usize,
    /// The *minimum* a bare `eprint` shows. A batch bigger than this is shown
    /// whole; a quiet day is topped up with recent arrivals so the feed is never
    /// empty. Not a cap — that is what `-n` is for.
    pub latest_limit: usize,
    /// Saved searches, in file order. They live here rather than in the index so
    /// that copying this one file to another machine copies your whole setup.
    pub watches: Vec<Watch>,
    /// An author to mark with a heart wherever their name appears in a byline.
    /// Matched case-insensitively as a substring, like `--author`. Undocumented
    /// on purpose; it lives in the config so no name is ever committed.
    pub favourite_author: Option<String>,
    /// off | all | summary | watched. Stored verbatim and validated where it is
    /// used, like `theme` and `scope`: this parser never fails, so an unreadable
    /// value has to fall back rather than error.
    pub notify: String,
    /// How to open a terminal for the desktop launcher, with `{cmd}` standing in
    /// for the command line to run. `None` means Terminal.app on macOS and the
    /// desktop's own choice on Linux, which is what almost everyone wants.
    pub terminal_command: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "auto".into(),
            scope: "all".into(),
            limit: 20,
            latest_limit: 10,
            watches: Vec::new(),
            favourite_author: None,
            notify: "off".into(),
            terminal_command: None,
        }
    }
}

pub const TEMPLATE: &str = r#"# eprint configuration

# Colour palette.
#   auto   pick from the terminal background when it can be determined,
#          otherwise assume a dark background
#   dark   for dark terminal backgrounds
#   light  for light terminal backgrounds
#   mono   no colour, only bold / dim / reverse
theme = "auto"

# Default search scope: "all" (title, authors and abstract) or
# "title" (title and authors only).
scope = "all"

# Default number of results for `eprint search`.
limit = 20

# The fewest papers a bare `eprint` will show. If more than this arrived in the
# last batch you get the whole batch; if fewer, the list is topped up with recent
# arrivals so it is never empty. A cap is `-n`, not this.
latest_limit = 10

# Desktop notifications for papers that arrive while you are not looking.
#   off      no notifications (the default)
#   all      one banner per new paper, then a roll-up once a burst gets long
#   summary  a single banner saying how many arrived
#   watched  only papers matching a `watch` line below, naming the watch
# `eprint config --notify <mode>` writes this line *and* installs the background
# updater that produces the notifications, so prefer it to editing by hand.
notify = "off"

# Saved searches. Papers matching one are marked with a gold ✱ wherever papers are
# listed, and `w` in `browse` filters to them. One `watch` line each, written
# exactly as you would type it after `eprint watch add`:
#
#   watch = "lattice OR LWE"
#   watch = --author Boneh
#   watch = zk --category "Public-key"
#   watch = "proof of work" --title
#
# `eprint watch add` and `eprint watch rm` edit these lines for you, and the rest
# of this file is left alone when they do.

"#;

/// Split a watch line the way a shell would, keeping "quoted phrases" intact.
/// The quotes are *kept* on query terms, because FTS5 needs them to mean a phrase,
/// and stripped from flag values, where they are only shell syntax.
fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').to_string()
}

/// Parse one `watch = …` value. Accepts exactly what `eprint watch add` accepts,
/// which is also what `Watch::label()` writes, so a round trip is lossless.
/// Returns `None` for a line that would watch nothing.
/// Does this label survive being written and read back?
///
/// `Watch::label()` and `parse_watch()` are meant to be exact inverses, and a
/// value that breaks that is written to the file and then read back as something
/// else — or as nothing at all. Two ways it happened: a query term containing a
/// line break left a stray line in the file and truncated the watch, and a term
/// of literally `--title` was stored as a flag and vanished on the next read.
pub fn round_trips(label: &str) -> bool {
    if label.contains(['\n', '\r']) {
        return false;
    }
    parse_watch(0, label).map(|w| w.label()).as_deref() == Some(label)
}

fn parse_watch(id: i64, value: &str) -> Option<Watch> {
    // The file is named `.toml`, which invites wrapping the whole value in quotes
    // — `watch = "--author Boudgoust"`. Left alone that parses as a phrase query
    // for the literal text and matches nothing, silently. Unwrap it when the
    // inside carries a flag, which leaves a genuine phrase like
    // `watch = "proof of work"` untouched.
    let value = match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) if inner.starts_with('-') || inner.contains(" --") => inner,
        _ => value,
    };
    let toks = tokens(value);
    let mut terms: Vec<String> = Vec::new();
    let mut author = None;
    let mut category = None;
    let mut scope = Scope::All;
    let mut i = 0;
    while i < toks.len() {
        match toks[i].as_str() {
            "--author" | "-a" => {
                // Written as a name reads, so a watch saved from the shell's
                // `Shamir, Adi` candidate does not sit among two dozen `First Last`.
                author = toks
                    .get(i + 1)
                    .map(|v| crate::names::person_form(&unquote(v)))
                    .filter(|v| !v.is_empty());
                i += 2;
            }
            "--category" | "-c" => {
                category = toks
                    .get(i + 1)
                    .map(|v| unquote(v))
                    .filter(|v| !v.is_empty());
                i += 2;
            }
            "--title" | "-t" => {
                scope = Scope::Title;
                i += 1;
            }
            other => {
                terms.push(other.to_string());
                i += 1;
            }
        }
    }
    let terms = terms.join(" ");
    if terms.trim().is_empty() && author.is_none() && category.is_none() {
        return None;
    }
    Some(Watch {
        id,
        terms,
        author,
        category,
        scope,
    })
}

/// Rewrite just the `watch` lines, leaving every other line — including comments
/// and settings this build does not know about — exactly where it was.
pub fn set_watches(labels: &[String]) -> Result<PathBuf> {
    let (path, _) = init()?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    let mut written = false;
    for line in text.lines() {
        let is_watch = line
            .split_once('=')
            .map(|(k, _)| k.trim() == "watch")
            .unwrap_or(false);
        if !is_watch {
            out.push(line.to_string());
            continue;
        }
        // The block goes back where the first one was, so hand-ordered files keep
        // their shape instead of having watches migrate to the bottom.
        if !written {
            out.extend(labels.iter().map(|l| format!("watch = {l}")));
            written = true;
        }
    }
    if !written && !labels.is_empty() {
        if !out.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
            out.push(String::new());
        }
        out.extend(labels.iter().map(|l| format!("watch = {l}")));
    }
    let mut body = out.join("\n");
    body.push('\n');
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Set one scalar key, leaving the rest of the file exactly as it was.
///
/// Line-surgical for the same reason `set_watches` is: this file is hand-editable
/// and `config --edit` invites exactly that, so comments and keys this build has
/// never heard of have to survive being written through.
pub fn set_scalar(key: &str, value: &str) -> Result<PathBuf> {
    let (path, _) = init()?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, rewrite_scalar(&text, key, value))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The pure half, so the placement rules above are testable without a filesystem.
fn rewrite_scalar(text: &str, key: &str, value: &str) -> String {
    let line = format!("{key} = \"{value}\"");
    let names = |l: &str, k: &str| l.split('=').next().map(|n| n.trim() == k).unwrap_or(false);
    let mut out: Vec<String> = Vec::new();
    let mut written = false;
    for l in text.lines() {
        if names(l, key) {
            // In place, so a key sitting under its own explanatory comment stays
            // under it. Any later duplicate is dropped rather than left to shadow.
            if !written {
                out.push(line.clone());
                written = true;
            }
            continue;
        }
        out.push(l.to_string());
    }
    if !written {
        // Before the watch block, never after it: `set_watches` re-emits that block
        // wherever its first line was, and a scalar appended below the watches would
        // end up inside it the next time a watch is added.
        let at = out.iter().position(|l| names(l, "watch"));
        match at {
            Some(i) => {
                out.insert(i, String::new());
                out.insert(i, line);
            }
            None => {
                if !out.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                    out.push(String::new());
                }
                out.push(line);
            }
        }
    }
    let mut body = out.join("\n");
    body.push('\n');
    body
}

pub fn path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("EPRINT_CONFIG") {
        return Some(PathBuf::from(p));
    }
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("eprint").join("config.toml"));
        }
    }
    dirs::home_dir().map(|h| h.join(".config").join("eprint").join("config.toml"))
}

/// the whole config is a handful of scalars plus the watch lines.
pub fn load() -> Config {
    let mut c = Config::default();
    let Some(p) = path() else { return c };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return c;
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        // Watch lines keep their quotes: `"lattice OR LWE"` is an FTS phrase, and
        // stripping the quotes would silently turn it into three loose words.
        let raw = v.trim().to_string();
        let v = raw.trim_matches(['"', '\'']).to_string();
        match k.trim() {
            "theme" => c.theme = v,
            "scope" => c.scope = v,
            "limit" => {
                if let Ok(n) = v.parse::<usize>() {
                    if n > 0 {
                        c.limit = n;
                    }
                }
            }
            "latest_limit" => {
                if let Ok(n) = v.parse::<usize>() {
                    if n > 0 {
                        c.latest_limit = n;
                    }
                }
            }
            // Repeated, unlike every other key: each line is one saved search.
            // Numbered by position, which is what `eprint watch rm <n>` takes.
            "favourite_author" => {
                c.favourite_author = Some(v).filter(|s| !s.trim().is_empty());
            }
            "notify" => c.notify = v,
            "terminal_command" => {
                c.terminal_command = Some(v).filter(|s| !s.trim().is_empty());
            }
            "watch" => {
                let next = c.watches.len() as i64 + 1;
                if let Some(w) = parse_watch(next, &raw) {
                    c.watches.push(w);
                }
            }
            _ => {}
        }
    }
    c
}

pub fn init() -> Result<(PathBuf, bool)> {
    let p = path().context("could not determine a config directory")?;
    if p.exists() {
        return Ok((p, false));
    }
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&p, TEMPLATE).with_context(|| format!("writing {}", p.display()))?;
    Ok((p, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scalar_is_replaced_where_it_already_sits() {
        let before = "# eprint configuration\n\n# Colour palette.\ntheme = \"dark\"\n\nnotify = \"off\"\nlimit = 20\n";
        let after = rewrite_scalar(before, "notify", "watched");
        assert!(after.contains("notify = \"watched\""));
        assert!(!after.contains("notify = \"off\""));
        // Everything else, comments included, is exactly where it was.
        assert!(after.contains("# Colour palette.\ntheme = \"dark\""));
        assert!(after.contains("limit = 20"));
        assert_eq!(before.lines().count(), after.lines().count());
    }

    #[test]
    fn a_new_scalar_lands_above_the_watch_block() {
        // `set_watches` re-emits the watch block wherever its first line was, so a
        // scalar appended below the watches would be swallowed by it next time.
        let before = "theme = \"dark\"\n\nwatch = lattice\nwatch = --author Boneh\n";
        let after = rewrite_scalar(before, "notify", "all");
        let lines: Vec<&str> = after.lines().collect();
        let scalar = lines.iter().position(|l| l.starts_with("notify")).unwrap();
        let first_watch = lines.iter().position(|l| l.starts_with("watch")).unwrap();
        assert!(scalar < first_watch, "{after}");
        assert_eq!(lines.iter().filter(|l| l.starts_with("watch")).count(), 2);
    }

    #[test]
    fn a_new_scalar_is_appended_when_there_are_no_watches() {
        let after = rewrite_scalar("theme = \"dark\"\n", "notify", "summary");
        assert!(after.ends_with("notify = \"summary\"\n"), "{after}");
        assert!(after.contains("theme = \"dark\""));
    }

    #[test]
    fn a_duplicate_key_is_collapsed_rather_than_left_to_shadow() {
        // `load()` takes the last value it sees, so leaving a stale duplicate below
        // the rewritten one would make the write appear to do nothing.
        let after = rewrite_scalar(
            "notify = \"off\"\nlimit = 5\nnotify = \"all\"\n",
            "notify",
            "summary",
        );
        assert_eq!(after.matches("notify").count(), 1, "{after}");
        assert!(after.contains("notify = \"summary\""));
    }

    #[test]
    fn what_is_written_is_what_load_reads_back() {
        // The two halves of this file have to agree: `load()` strips the quotes that
        // `rewrite_scalar` adds.
        for mode in ["off", "all", "summary", "watched"] {
            let text = rewrite_scalar("", "notify", mode);
            let line = text.lines().find(|l| l.starts_with("notify")).unwrap();
            let (_, v) = line.split_once('=').unwrap();
            assert_eq!(v.trim().trim_matches(['"', '\'']), mode);
        }
    }

    #[test]
    fn the_template_declares_the_keys_load_understands() {
        // A key documented in the template but missing from `load` is invisible; one
        // in `load` but missing from the template is undiscoverable.
        for key in ["theme", "scope", "limit", "latest_limit", "notify"] {
            assert!(
                TEMPLATE
                    .lines()
                    .any(|l| l.split('=').next().map(str::trim) == Some(key)),
                "{key} is not in the template"
            );
        }
        let c = load_from(TEMPLATE);
        assert_eq!(c.notify, "off", "the template's default must be off");
    }

    /// `load()` reads a path; this is the same match on text, for the test above.
    fn load_from(text: &str) -> Config {
        let mut c = Config::default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim().trim_matches(['"', '\'']).to_string();
            if k.trim() == "notify" {
                c.notify = v;
            }
        }
        c
    }
}
