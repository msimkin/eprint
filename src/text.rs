//! Laying text out, with no opinion about colour.
//!
//! Split out of `render.rs` so that the parts with no terminal in them — wrapping,
//! byline shortening, dates, the machine-readable dump — can be used by a front-end
//! that is not a terminal. `render.rs` is the other half and re-exports all of this,
//! so every existing `render::wrap` / `render::short_authors` call site is unchanged.
//!
//! The wrapping functions and their counting twins live together deliberately: the
//! browser lays out only what is on screen by counting a hit's lines without
//! allocating, and `wrap`/`wrap_count` disagreeing by one line makes the viewport
//! drift from what is drawn. The test at the bottom is what holds them together.

use crate::db::{Hit, MARK_END, MARK_START};
/// Visible width, ignoring ANSI escapes and match markers.
pub fn visible_len(s: &str) -> usize {
    let mut n = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_esc = true;
            continue;
        }
        if in_esc {
            if c == 'm' || c == '\\' {
                in_esc = false;
            }
            continue;
        }
        if c == MARK_START || c == MARK_END {
            continue;
        }
        n += 1;
    }
    n
}

/// Greedy word wrap over unstyled text (markers allowed), so styling can be
/// applied per line afterwards. Shared with the TUI.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let add = if cur.is_empty() { 0 } else { 1 };
        if !cur.is_empty() && visible_len(&cur) + add + visible_len(word) > width {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
        } else {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// How many lines `wrap` would produce, without building any of them. The TUI needs
/// this for every hit on every frame to know where the viewport falls, and doing it
/// by wrapping and counting allocated a `Vec<String>` per paper per keystroke.
pub fn wrap_count(text: &str, width: usize) -> usize {
    let mut lines = 1;
    let mut cur = 0usize;
    for word in text.split_whitespace() {
        let w = visible_len(word);
        let add = if cur == 0 { 0 } else { 1 };
        if cur > 0 && cur + add + w > width {
            lines += 1;
            cur = w;
        } else {
            cur += add + w;
        }
    }
    lines
}

/// Same for `wrap_body`: paragraphs separated by a blank line.
pub fn wrap_body_count(text: &str, width: usize) -> usize {
    let paras = paragraphs(text);
    if paras.is_empty() {
        return 0;
    }
    paras.iter().map(|p| wrap_count(p, width)).sum::<usize>() + paras.len() - 1
}

/// Split on blank lines. Roughly 37% of ePrint abstracts carry real paragraph
/// breaks, which plain whitespace-wrapping would throw away.
pub fn paragraphs(text: &str) -> Vec<String> {
    let norm = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in norm.split('\n') {
        if line.trim().is_empty() {
            if !cur.trim().is_empty() {
                out.push(cur.trim().to_string());
            }
            cur.clear();
        } else {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(line.trim());
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Wrap a body of text, preserving paragraph breaks as blank lines.
pub fn wrap_body(text: &str, width: usize) -> Vec<String> {
    let paras = paragraphs(text);
    if paras.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for (i, p) in paras.iter().enumerate() {
        if i > 0 {
            lines.push(String::new());
        }
        lines.extend(wrap(p, width));
    }
    lines
}

/// Hearts either side of a favourite author, with the name between them marked so
/// the renderer can colour the whole `♥ Name ♥` run. `♥` is East-Asian Ambiguous,
/// so a decorated byline reserves `LOVE_W` extra columns when wrapping — the same
/// concession the watch badge avoids by using a narrow glyph.
pub const LOVE_W: usize = 2;

fn adore(name: &str) -> String {
    format!("{MARK_START}♥ {name} ♥{MARK_END}")
}

/// Case-insensitive substring, the same rule as the `--author` filter, so
/// "simkin" matches "Mark Simkin".
fn is_favourite(name: &str, fav: Option<&str>) -> bool {
    match fav {
        Some(f) => name.to_lowercase().contains(&f.trim().to_lowercase()),
        None => false,
    }
}

/// Does this paper have the favourite author on it at all?
pub fn loved(authors: &str, fav: Option<&str>) -> bool {
    authors.split(';').any(|n| is_favourite(n, fav))
}

/// ISO in, day/month/year out. Storage stays ISO because the index compares dates
/// as fixed-width text in SQL — that is the only reason the comparisons work — so
/// this is the single place the two conventions meet. Anything that is not a date
/// is passed through untouched rather than mangled.
pub fn fmt_date(iso: &str) -> String {
    let d = &iso[..iso.len().min(10)];
    let p: Vec<&str> = d.split('-').collect();
    match p.as_slice() {
        [y, m, day] if y.len() == 4 && m.len() == 2 && day.len() == 2 => {
            format!("{day}/{m}/{y}")
        }
        _ => iso.to_string(),
    }
}

/// Every author, first names included. `short_authors` reduces to surnames so a
/// list of results stays scannable; this is for the single-paper views, where the
/// byline is worth the lines it takes.
pub fn full_authors(authors: &str, fav: Option<&str>) -> String {
    let names: Vec<&str> = authors
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return "—".to_string();
    }
    names
        .iter()
        .map(|n| {
            if is_favourite(n, fav) {
                adore(n)
            } else {
                n.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn short_authors(authors: &str, fav: Option<&str>) -> String {
    let names: Vec<&str> = authors
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return "—".to_string();
    }
    let show = |n: &str| -> String {
        // The last word with letters in it, not simply the last word. The archive
        // carries bylines like "Sohaib ..", whose trailing token would render the
        // author as ".." — which reads as an elision, i.e. as though the tool had
        // dropped names. Falling back to the first real word shows "Sohaib".
        let surname = n
            .split_whitespace()
            .filter(|w| w.chars().any(char::is_alphanumeric))
            .next_back()
            .unwrap_or(n)
            .to_string();
        if is_favourite(n, fav) {
            adore(&surname)
        } else {
            surname
        }
    };
    if names.len() <= 3 {
        return names.iter().map(|n| show(n)).collect::<Vec<_>>().join(", ");
    }
    let mut head: Vec<String> = names.iter().take(2).map(|n| show(n)).collect();
    // A favourite hiding in the tail would be collapsed into "et al.", leaving
    // nothing to decorate on exactly the many-author papers this field is full
    // of — so she takes the last visible slot instead.
    if let Some(f) = names.iter().skip(2).find(|n| is_favourite(n, fav)) {
        head.push(show(f));
    }
    format!("{}, et al.", head.join(", "))
}

pub fn short_license(rights: &str) -> String {
    let r = rights.trim();
    if r.is_empty() {
        return String::new();
    }
    if let Some(rest) = r.split("creativecommons.org/licenses/").nth(1) {
        let mut parts = rest.split('/').filter(|s| !s.is_empty());
        if let Some(code) = parts.next() {
            let ver = parts.next().unwrap_or("");
            let code = code.to_uppercase();
            return if ver.is_empty() {
                format!("CC-{code}")
            } else {
                format!("CC-{code}-{ver}")
            };
        }
    }
    if r.contains("publicdomain") || r.contains("zero") {
        return "CC0".to_string();
    }
    r.chars().take(24).collect()
}
/// The watch badge, and the space it needs: one cell for the glyph plus one to
/// separate it from the title it follows.
///
/// It trails the *title text*, not a fixed column, so only a watched row pays for
/// it: that row's title wraps `BADGE_W` narrower, leaving room on its last line.
/// Unwatched rows keep the full width. Both renderers must agree on this or their
/// layouts diverge.
///
/// U+2731 is East-Asian **Neutral**, i.e. one cell in every terminal. `★` (U+2605)
/// and `◆` (U+25C6) are Ambiguous — counted as one cell here and by ratatui, drawn
/// as two by CJK-aware terminals, which would overrun the line. It is also the
/// *heavy* asterisk: glyph weight is the only sense in which a terminal glyph can
/// be made bigger.
pub const BADGE: &str = "✱";
pub const BADGE_W: usize = 2;

pub fn json_of(hits: &[Hit]) -> String {
    let arr: Vec<serde_json::Value> = hits
        .iter()
        .map(|h| {
            let p = &h.paper;
            serde_json::json!({
                "id": p.id,
                "title": p.title,
                "authors": p.authors.split("; ").filter(|s| !s.is_empty()).collect::<Vec<_>>(),
                "abstract": p.abstract_,
                "category": p.category,
                "date": p.date,
                "year": p.year,
                "license": p.rights,
                "url": p.url,
            })
        })
        .collect();
    serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counters must agree with the wrappers exactly, or the viewport maths
    /// drifts from what is drawn.
    #[test]
    fn counts_match_wrapping() {
        let samples = [
            "Scale, Round, Break: Simple Leakage Attacks on Secret Sharing Schemes",
            "short",
            "",
            "a b c d e f g h i j k l m n o p q r s t u v w x y z 1 2 3 4 5 6 7 8 9",
            "one\n\ntwo paragraphs here\n\nand a third that is quite a lot longer than the others",
        ];
        for w in [20usize, 40, 83, 200] {
            for s in samples {
                assert_eq!(wrap(s, w).len(), wrap_count(s, w), "wrap {s:?} at {w}");
                assert_eq!(
                    wrap_body(s, w).len(),
                    wrap_body_count(s, w),
                    "body {s:?} at {w}"
                );
            }
        }
    }
}
