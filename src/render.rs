use crate::db::{Hit, Paper, MARK_END, MARK_START};
use crate::theme::{Theme, Tone};
use std::fmt::Write as _;
use std::io::IsTerminal;

pub struct Style {
    pub theme: Theme,
    /// Terminal renders OSC 8 hyperlinks (iTerm2, Ghostty, kitty, WezTerm, VS Code).
    pub links: bool,
    /// Print bare URLs instead, for terminals that only auto-detect plain text
    /// (notably macOS Terminal.app, which has never supported OSC 8).
    pub bare_urls: bool,
    pub width: usize,
    pub height: usize,
    /// The one name to wrap in hearts, if the config names one.
    pub favourite: Option<String>,
}

/// macOS Terminal.app does not implement OSC 8; it does cmd-click plain URLs.
fn terminal_supports_osc8() -> bool {
    if matches!(std::env::var("TERM").as_deref(), Ok("dumb") | Ok("linux")) {
        return false;
    }
    !matches!(
        std::env::var("TERM_PROGRAM").as_deref(),
        Ok("Apple_Terminal")
    )
}

pub struct StyleOpts {
    pub plain: bool,
    pub force_color: bool,
    /// None = auto-detect
    pub urls: Option<bool>,
    pub theme: String,
    pub favourite: Option<String>,
}

impl Style {
    pub fn detect(o: StyleOpts) -> Style {
        let tty = std::io::stdout().is_terminal();
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let on = o.force_color || (tty && !no_color && !o.plain);
        let (width, height) = terminal_size::terminal_size()
            .map(|(terminal_size::Width(w), terminal_size::Height(h))| (w as usize, h as usize))
            .unwrap_or((100, 40));
        let osc8 = on && terminal_supports_osc8();
        Style {
            favourite: o.favourite,
            theme: Theme::resolve(&o.theme, on),
            links: osc8,
            bare_urls: o.urls.unwrap_or(on && !osc8),
            width: width.clamp(60, 120),
            height: height.max(10),
        }
    }

    fn link(&self, url: &str, text: &str) -> String {
        if self.links {
            format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
        } else {
            text.to_string()
        }
    }

    /// Render one already-wrapped line, converting FTS5 match markers into the
    /// theme's match colour. `open` carries highlight state across a line
    /// break so a match split by wrapping still closes cleanly on each line.
    fn marked(&self, s: &str, base: Tone, hl: Tone, open: &mut bool) -> String {
        if !self.theme.color {
            *open = false;
            return s.replace([MARK_START, MARK_END], "");
        }
        let reset = self.theme.reset();
        let base_code = self.theme.ansi(base);
        let hit_code = self.theme.ansi(hl);
        let mut out = String::new();
        let mut on = *open;
        let mut cur = String::new();
        let flush = |out: &mut String, cur: &mut String, on: bool| {
            if cur.is_empty() {
                return;
            }
            let code = if on { hit_code } else { base_code };
            if code.is_empty() {
                out.push_str(cur);
            } else {
                out.push_str(code);
                out.push_str(cur);
                out.push_str(reset);
            }
            cur.clear();
        };
        for c in s.chars() {
            match c {
                MARK_START | MARK_END => {
                    flush(&mut out, &mut cur, on);
                    on = c == MARK_START;
                }
                _ => cur.push(c),
            }
        }
        flush(&mut out, &mut cur, on);
        *open = on;
        out
    }
}

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

const ID_W: usize = 11;

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

pub fn render_hit(
    out: &mut String,
    hit: &Hit,
    st: &Style,
    show_abstract: bool,
    bib: Option<&(String, bool)>,
    watched: bool,
) {
    let p = &hit.paper;
    let indent = ID_W + 4;
    let body_width = st.width.saturating_sub(indent + 2);
    let pad = " ".repeat(indent);
    let th = &st.theme;

    // Wrap first, style second — otherwise a style opened on one line is
    // reset on the next and the first line bleeds.
    let title_src = if hit.title_hl.is_empty() {
        &p.title
    } else {
        &hit.title_hl
    };
    // A watched title wraps narrower so its last line has room for the trailing
    // badge; an unwatched one uses the full width.
    let title_lines = wrap(
        title_src,
        if watched {
            body_width.saturating_sub(BADGE_W)
        } else {
            body_width
        },
    );
    let last = title_lines.len() - 1;
    // Watched papers also take Tone::Watch for the id, so the row still reads as
    // watched when the title is long enough to push the badge out of view.
    let id_tone = if watched { Tone::Watch } else { Tone::Id };
    let id_cell = th.paint(id_tone, &format!("{:<w$}", p.id, w = ID_W));
    // Outside the hyperlink: the badge is our annotation, not part of the paper.
    let badge = if watched {
        format!(" {}", th.paint(Tone::Watch, BADGE))
    } else {
        String::new()
    };
    let mut topen = false;
    let first = st.marked(&title_lines[0], Tone::Title, Tone::Match, &mut topen);
    let _ = writeln!(
        out,
        "  {}{}",
        st.link(&p.url, &format!("{id_cell}  {first}")),
        if last == 0 { badge.as_str() } else { "" }
    );
    for (i, line) in title_lines.iter().enumerate().skip(1) {
        let _ = writeln!(
            out,
            "{pad}{}{}",
            st.marked(line, Tone::Title, Tone::Match, &mut topen),
            if i == last { badge.as_str() } else { "" }
        );
    }

    // Authors only in the list; date and citation key join once an abstract
    // is open.
    let fav = st.favourite.as_deref();
    let mut meta: Vec<String> = vec![short_authors(&p.authors, fav)];
    if show_abstract && !p.date.is_empty() {
        meta.push(fmt_date(&p.date));
    }
    if show_abstract {
        if let Some((key, _)) = bib {
            meta.push(key.clone());
        }
    }
    // Hearts may be drawn two cells wide, so a decorated byline gets the columns
    // back before wrapping. Styling is per line, after the wrap, as everywhere.
    let byline = meta.join(" · ");
    let width = if loved(&p.authors, fav) {
        body_width.saturating_sub(LOVE_W)
    } else {
        body_width
    };
    let mut lopen = false;
    for line in wrap(&byline, width) {
        let _ = writeln!(
            out,
            "{pad}{}",
            st.marked(&line, Tone::Meta, Tone::Love, &mut lopen)
        );
    }

    if st.bare_urls {
        let _ = writeln!(out, "{pad}{}", th.paint(Tone::Url, &p.url));
    }

    // Nothing of the abstract unless asked for: a single truncated line is
    // too little to judge a paper by, and `-a` covers the case where you
    // actually want to read it.
    if show_abstract && !hit.abstract_hl.is_empty() {
        let mut open = false;
        for line in wrap_body(&hit.abstract_hl, body_width) {
            if line.is_empty() {
                let _ = writeln!(out);
            } else {
                let _ = writeln!(
                    out,
                    "{pad}{}",
                    st.marked(&line, Tone::Body, Tone::Match, &mut open)
                );
            }
        }
    }
    let _ = writeln!(out);
}

pub fn render_header(
    out: &mut String,
    count: usize,
    total: usize,
    age: Option<String>,
    scope: &str,
    st: &Style,
) {
    let shown = if total > count {
        format!("{count} of {total} results")
    } else {
        format!("{count} result{}", if count == 1 { "" } else { "s" })
    };
    let mut tail = scope.to_string();
    if let Some(a) = age {
        tail.push_str(&format!(" · index {a}"));
    }
    let _ = writeln!(
        out,
        "\n{}  {}\n",
        st.theme.paint(Tone::Head, &shown),
        st.theme.paint(Tone::Meta, &tail)
    );
}

pub fn render_full(out: &mut String, p: &Paper, st: &Style, bib: Option<&(String, bool)>) {
    let width = st.width.saturating_sub(2);
    let th = &st.theme;
    let _ = writeln!(out);
    for line in wrap(&p.title, width) {
        let _ = writeln!(out, "{}", th.paint(Tone::Title, &line));
    }
    let fav = st.favourite.as_deref();
    let byline = full_authors(&p.authors, fav);
    let bw = if loved(&p.authors, fav) {
        width.saturating_sub(LOVE_W)
    } else {
        width
    };
    let mut lopen = false;
    for line in wrap(&byline, bw) {
        let _ = writeln!(
            out,
            "{}",
            st.marked(&line, Tone::Meta, Tone::Love, &mut lopen)
        );
    }
    let _ = writeln!(out);
    let mut meta = vec![format!("ePrint {}", p.id)];
    if !p.category.is_empty() {
        meta.push(p.category.clone());
    }
    if !p.date.is_empty() {
        meta.push(fmt_date(&p.date));
    }
    let lic = short_license(&p.rights);
    if !lic.is_empty() {
        meta.push(lic);
    }
    let _ = writeln!(out, "{}", th.paint(Tone::Meta, &meta.join(" · ")));
    if let Some((key, published)) = bib {
        let note = if *published {
            format!("{key}  (published version)")
        } else {
            key.clone()
        };
        let _ = writeln!(out, "{}", th.paint(Tone::Meta, &note));
    }
    let _ = writeln!(out, "{}", st.link(&p.url, &th.paint(Tone::Url, &p.url)));
    if !p.abstract_.is_empty() {
        let _ = writeln!(out);
        for line in wrap_body(&p.abstract_, width) {
            let _ = writeln!(out, "{line}");
        }
    }
    let _ = writeln!(out);
}

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
