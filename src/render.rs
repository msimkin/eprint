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

// The colourless half of this module now lives in the library, so that something
// which is not a terminal can shorten a byline or wrap a paragraph without linking
// ratatui. Re-exported rather than re-pathed: every `render::wrap`,
// `render::short_authors` and `render::BADGE` in `tui.rs` and `main.rs` still reads
// exactly as it did, and the two renderers stay parallel implementations of one
// layout rather than drifting apart over an import.
pub use eprint::text::{
    fmt_date, full_authors, json_of, loved, short_authors, short_license, wrap, wrap_body,
    wrap_body_count, wrap_count, BADGE, BADGE_W, LOVE_W,
};

const ID_W: usize = 11;

pub fn render_hit(
    out: &mut String,
    hit: &Hit,
    st: &Style,
    show_abstract: bool,
    bib: Option<&(String, bool)>,
    venue: Option<&str>,
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

    // Authors always; date and citation key join once an abstract is open. The
    // venue shows either way, unlike the key: it is what the paper *is* to a reader
    // scanning a listing, where a citation key is something you go and fetch.
    let fav = st.favourite.as_deref();
    let mut meta: Vec<String> = vec![short_authors(&p.authors, fav)];
    if show_abstract && !p.date.is_empty() {
        meta.push(fmt_date(&p.date));
    }
    if let Some(v) = venue {
        meta.push(v.to_string());
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

pub fn render_full(
    out: &mut String,
    p: &Paper,
    st: &Style,
    bib: Option<&(String, bool)>,
    venue: Option<&str>,
) {
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
    if let Some(v) = venue {
        meta.push(v.to_string());
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
