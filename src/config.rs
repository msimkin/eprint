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
                author = toks.get(i + 1).map(|v| unquote(v)).filter(|v| !v.is_empty());
                i += 2;
            }
            "--category" | "-c" => {
                category = toks.get(i + 1).map(|v| unquote(v)).filter(|v| !v.is_empty());
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

/// Deliberately tiny `key = value` reader rather than a TOML dependency —
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
