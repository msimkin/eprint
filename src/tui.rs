use anyhow::Result;
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;

use crate::db::{self, Hit, Query, Scope, MARK_END, MARK_START};
use crate::render::{full_authors, loved, short_authors, wrap, wrap_body, BADGE, BADGE_W, LOVE_W};
use crate::theme::{Theme, Tone};

/// Which prompt, if any, is taking keystrokes. Was a bool when the query was the
/// only thing you could edit.
#[derive(PartialEq, Clone, Copy)]
enum Editing {
    None,
    Query,
    Date,
    Venue,
}

#[derive(Default, Clone)]
pub struct Filters {
    pub year: Option<i64>,
    pub since: Option<String>,
    pub before: Option<String>,
    pub author: Option<String>,
    pub category: Option<String>,
    pub venue: Option<String>,
    pub venue_year: Option<String>,
    /// The venue filter as the user typed it, for the header — a filter passed as
    /// `--venue` is otherwise invisible from inside the browser. Same reason as
    /// `date_text`.
    pub venue_text: String,
    pub limit: usize,
    pub prefix: bool,
    /// What the user typed for the date, kept verbatim so the header can show it
    /// and `d` can reopen with it. The parsed bounds live in `since`/`before`.
    pub date_text: String,
}

const ID_W: usize = 11;
/// Width of the "❯ " selection marker and the "▸ " expand arrow.
const MARKER_W: usize = 2;
const ARROW_W: usize = 2;
/// Column where title text starts, and therefore where every wrapped
/// continuation, meta and abstract line must line up.
const INDENT: usize = MARKER_W + ARROW_W + ID_W + 2;

struct App {
    query: String,
    hits: Vec<Hit>,
    total: usize,
    selected: usize,
    scroll: usize,
    editing: Editing,
    /// The date prompt's buffer, separate from `query` so cancelling restores.
    date_input: String,
    /// The venue prompt's buffer, for the same reason.
    venue_input: String,
    /// Keyed by paper id so expansion survives re-searching.
    expanded: HashSet<String>,
    status: Option<String>,
    filters: Filters,
    theme: Theme,
    scope: Scope,
    /// eprint id -> (citation key, is_published)
    bib: HashMap<String, (String, bool)>,
    /// Every id in the index matching a saved watch, read once at startup from the
    /// `watch_hits` cache. Badging is then a hash lookup per visible row rather
    /// than a query per search.
    watched: HashSet<String>,
    /// eprint id -> "CRYPTO 2025", the whole table, read once at startup for the
    /// same reason as `watched`: laying out a frame touches every hit, so this has
    /// to be a lookup rather than a query. Unlike `bib` it is not cleared per
    /// search — a venue does not depend on the query.
    venues: HashMap<String, String>,
    /// The unfiltered listing, kept while `w` is on so switching it off restores
    /// instantly instead of re-running a 26,000-row query. Dropped as soon as the
    /// query or a filter changes, since it would no longer be what to return to.
    unfiltered: Option<(Vec<Hit>, usize)>,
    /// `w`: restrict the whole listing, and any query typed into it, to papers
    /// matching a watch.
    watched_only: bool,
    /// The saved searches, from the config file. Re-read when `w` is pressed, so
    /// an edit in another shell is picked up without restarting.
    watch_list: Vec<db::Watch>,
    /// Age of the CryptoBib data in days, when it is old enough to mention.
    bib_stale_days: Option<i64>,
    /// The one name to wrap in hearts, if the config names one.
    favourite: Option<String>,
    /// The query moved and the results have not caught up yet. Set by typing,
    /// cleared by the search the event loop runs once the typing pauses.
    pending_search: bool,
}

/// How long to wait for the next keystroke before searching. Short enough to feel
/// immediate, long enough that a word typed at speed costs one query instead of
/// one per letter — which on a two-character prefix is the difference between
/// 110ms and seven times that.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);

impl App {
    /// The query the current state describes. `terms` is borrowed rather than
    /// owned so callers can hand in a clone and keep mutating `self`.
    fn query_for<'a>(&self, terms: &'a str) -> Query<'a> {
        Query {
            terms,
            year: self.filters.year,
            since: self.filters.since.clone(),
            before: self.filters.before.clone(),
            added_since: None,
            only_watched: self.watched_only,
            venue: self.filters.venue.clone(),
            venue_year: self.filters.venue_year.clone(),
            author: self.filters.author.clone(),
            category: self.filters.category.clone(),
            limit: self.filters.limit,
            scope: self.scope,
            prefix: self.filters.prefix,
        }
    }

    /// Fetch the marked-up title and abstract for the rows about to be drawn.
    ///
    /// Safe to run *after* the heights are known, because marking does not change
    /// how text wraps — `visible_len` skips the markers. That is the whole reason
    /// the layout can cover every hit while only a screenful is hydrated.
    fn hydrate_visible(&mut self, conn: &Connection, first: usize, rows: usize) {
        let terms = self.query.clone();
        let q = self.query_for(&terms);
        let end = (first + rows).min(self.hits.len());
        for i in first..end {
            let _ = db::hydrate(conn, &q, &mut self.hits[i]);
        }
    }

    /// A citation key is only ever drawn on an expanded row, so it is fetched
    /// when a row is expanded rather than for all 26,000 on every keystroke.
    fn expand_selected(&mut self, conn: &Connection) {
        let Some(id) = self.selected_hit().map(|h| h.paper.id.clone()) else {
            return;
        };
        if self.expanded.remove(&id) {
            return;
        }
        if !self.bib.contains_key(&id) {
            if let Ok(Some(entry)) = db::bib_for(conn, &id) {
                self.bib.insert(id.clone(), entry);
            }
        }
        self.expanded.insert(id);
    }

    fn search(&mut self, conn: &Connection) {
        // Whatever `w` off would have restored is stale the moment the query or a
        // filter moves, so the stash is dropped on every search and re-taken by the
        // `w` handler alone.
        self.unfiltered = None;
        let terms = self.query.clone();
        let q = self.query_for(&terms);
        match db::search(conn, &q) {
            Ok(hits) => {
                // Unbounded, so every match is already here and a second
                // `COUNT(*)` over the same expression could only agree with
                // `hits.len()` — it was repeating the whole match to learn
                // nothing. Only a limited listing needs asking.
                self.total = if self.filters.limit == usize::MAX {
                    hits.len()
                } else {
                    db::count_matches(conn, &q).unwrap_or(hits.len())
                };
                // Citation keys are shown only on an expanded row, so they are
                // looked up on expansion. Asking for all of them built an
                // `IN (...)` with 26,000 bind parameters on every keystroke.
                self.bib.clear();
                self.hits = hits;
                self.status = None;
            }
            Err(_) => {
                // Incomplete query while typing (e.g. a lone quote) — keep the
                // previous results rather than flashing an error.
                self.status = Some("…".to_string());
            }
        }
        self.selected = 0;
        self.scroll = 0;
    }

    fn selected_hit(&self) -> Option<&Hit> {
        self.hits.get(self.selected)
    }

    fn stale_hint(&self) -> String {
        match self.bib_stale_days {
            Some(d) => format!("  · CryptoBib unchecked for {d}d, run `eprint bib --update`"),
            None => String::new(),
        }
    }
}

/// Convert FTS5 match markers into styled spans. `open` carries highlight
/// state across a wrapped line break.
fn marked_spans(s: &str, base: Style, hl: Style, open: &mut bool) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut on = *open;
    for c in s.chars() {
        match c {
            MARK_START | MARK_END => {
                if !cur.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(&mut cur),
                        if on { hl } else { base },
                    ));
                }
                on = c == MARK_START;
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, if on { hl } else { base }));
    }
    *open = on;
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    spans
}

/// The widths a hit is laid out in. Derived in one place so the height calculation
/// and the renderer cannot disagree about them.
fn widths(app: &App, hit: &Hit, width: usize) -> (usize, usize, usize) {
    let body_w = width.saturating_sub(INDENT + 2).max(20);
    let fav = app.favourite.as_deref();
    let title_w = if app.watched.contains(&hit.paper.id) {
        body_w.saturating_sub(BADGE_W)
    } else {
        body_w
    };
    let meta_w = if loved(&hit.paper.authors, fav) {
        body_w.saturating_sub(LOVE_W)
    } else {
        body_w
    };
    (body_w, title_w, meta_w)
}

/// How many lines a hit occupies, counted without building any of them. This is
/// what lets the viewport be found without laying out all 26,000 papers, and it
/// **must** agree with `hit_lines` — `hit_lines` debug-asserts that it does.
/// The lines under the title, as text.
///
/// Shared by `hit_height` and `hit_lines` so the two cannot build different
/// strings. They had a copy each, and the whole viewport rests on them agreeing —
/// `hit_lines` debug-asserts it, but only in a debug build.
fn meta_lines(app: &App, hit: &Hit, is_open: bool) -> Vec<String> {
    let p = &hit.paper;
    let fav = app.favourite.as_deref();
    let venue = app.venues.get(&p.id);
    // Surnames while collapsed, so rows stay scannable. Expanding gives the full
    // byline its own line — as `eprint show` does — because appending eight first
    // names to the `·`-joined line buries the date behind them.
    let mut first = if is_open {
        full_authors(&p.authors, fav)
    } else {
        short_authors(&p.authors, fav)
    };
    // Collapsed, the venue rides the byline exactly as it does inline. Expanded,
    // that line is already a list of names, so it joins the trailer instead.
    if !is_open {
        if let Some(v) = venue {
            first.push_str(" · ");
            first.push_str(v);
        }
    }
    let mut out = vec![first];
    if is_open {
        let mut trailer: Vec<String> = Vec::new();
        if !p.date.is_empty() {
            trailer.push(crate::render::fmt_date(&p.date));
        }
        if let Some(v) = venue {
            trailer.push(v.clone());
        }
        if let Some((key, _)) = app.bib.get(&p.id) {
            trailer.push(key.clone());
        }
        if !trailer.is_empty() {
            out.push(trailer.join(" · "));
        }
    }
    out
}

fn hit_height(app: &App, hit: &Hit, width: usize) -> usize {
    let p = &hit.paper;
    let is_open = app.expanded.contains(&p.id);
    let (body_w, title_w, meta_w) = widths(app, hit, width);
    // Counted from the *unmarked* text on purpose. `visible_len` skips the match
    // markers, so the marked and unmarked forms wrap identically — which is what
    // lets heights be known for all 26,000 hits while only the rows on screen are
    // hydrated. `hit_lines` wraps the marked form and debug-asserts they agree.
    let mut n = crate::render::wrap_count(&p.title, title_w);
    for src in meta_lines(app, hit, is_open) {
        n += crate::render::wrap_count(&src, meta_w);
    }
    if is_open {
        n += crate::render::wrap_body_count(&p.abstract_, body_w);
    }
    n + 1 // the blank line between entries
}

/// As many hints as the terminal can hold, most useful first, with `tail`
/// always kept.
///
/// The line was a fixed string of 123 columns, so on an 80-column terminal a
/// third of it ran off the edge — including `q quit`, which is the one hint
/// nobody can afford to lose. Dropping whole hints from the end beats letting
/// the renderer cut one in half.
fn fit_hints(width: usize, hints: &[&str], tail: &str) -> String {
    const SEP: &str = " · ";
    let mut out = String::from("  ");
    // Reserve the tail before spending anything on the rest.
    let budget = width.saturating_sub(2 + SEP.len() + tail.chars().count());
    let mut used = 0usize;
    for hint in hints {
        let cost = hint.chars().count() + if used == 0 { 0 } else { SEP.len() };
        if used + cost > budget {
            break;
        }
        if used > 0 {
            out.push_str(SEP);
        }
        out.push_str(hint);
        used += cost;
    }
    if used > 0 {
        out.push_str(SEP);
    }
    out.push_str(tail);
    out
}

/// Lay out one hit. Only ever called for rows on screen.
fn hit_lines(app: &App, i: usize, hit: &Hit, width: usize) -> Vec<Line<'static>> {
    let (body_w, _, _) = widths(app, hit, width);
    let pad = " ".repeat(INDENT);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let th = &app.theme;
    let hl = th.style(Tone::Match);
    let meta_s = th.style(Tone::Meta);
    {
        let p = &hit.paper;
        let is_sel = i == app.selected;
        let is_open = app.expanded.contains(&p.id);

        let is_watched = app.watched.contains(&p.id);
        let marker = if is_sel { "❯ " } else { "  " };
        let arrow = if is_open { "▾ " } else { "▸ " };
        let title_src = if hit.title_hl.is_empty() {
            &p.title
        } else {
            &hit.title_hl
        };
        // Matches `render_hit`: only a watched title gives up width, for the
        // badge that trails its last line.
        let title_lines = wrap(
            title_src,
            if is_watched {
                body_w.saturating_sub(BADGE_W)
            } else {
                body_w
            },
        );
        let last_title = title_lines.len() - 1;

        let mut title_s = th.style(Tone::Title);
        if is_sel {
            title_s = title_s.patch(th.style(Tone::Marker));
        }

        let mut topen = false;
        let mut head = vec![
            Span::styled(marker, th.style(Tone::Marker)),
            Span::styled(arrow, meta_s),
            Span::styled(
                format!("{:<w$}", p.id, w = ID_W),
                th.style(if is_watched { Tone::Watch } else { Tone::Id }),
            ),
            Span::raw("  "),
        ];
        head.extend(marked_spans(&title_lines[0], title_s, hl, &mut topen));
        if is_watched && last_title == 0 {
            head.push(Span::styled(format!(" {BADGE}"), th.style(Tone::Watch)));
        }
        lines.push(Line::from(head));
        for (i, cont) in title_lines.iter().enumerate().skip(1) {
            let mut spans = vec![Span::raw(pad.clone())];
            spans.extend(marked_spans(cont, title_s, hl, &mut topen));
            if is_watched && i == last_title {
                spans.push(Span::styled(format!(" {BADGE}"), th.style(Tone::Watch)));
            }
            lines.push(Line::from(spans));
        }

        let fav = app.favourite.as_deref();
        let meta = meta_lines(app, hit, is_open);
        // Same reservation as the inline renderer: a heart may be two cells wide.
        let meta_w = if loved(&p.authors, fav) {
            body_w.saturating_sub(LOVE_W)
        } else {
            body_w
        };
        let love_s = th.style(Tone::Love);
        let mut lopen = false;
        for src in &meta {
            for m in wrap(src, meta_w) {
                let mut spans = vec![Span::raw(pad.clone())];
                spans.extend(marked_spans(&m, meta_s, love_s, &mut lopen));
                lines.push(Line::from(spans));
            }
        }

        let body_s = th.style(Tone::Body);
        if is_open {
            let mut open = false;
            for l in wrap_body(&hit.abstract_hl, body_w) {
                if l.is_empty() {
                    lines.push(Line::raw(""));
                    continue;
                }
                let mut spans = vec![Span::raw(pad.clone())];
                spans.extend(marked_spans(&l, body_s, hl, &mut open));
                lines.push(Line::from(spans));
            }
        }
        // Nothing of the abstract while collapsed. A 16-token FTS snippet used to
        // go here and read as noise; matches show in the title, and in the whole
        // abstract once expanded.

        lines.push(Line::raw(""));
    }
    debug_assert_eq!(
        lines.len(),
        hit_height(app, hit, width),
        "hit_height disagrees with hit_lines — the viewport would drift"
    );
    lines
}

/// The citation key the archive itself uses. Once CryptoBib is wired in this
/// becomes the fallback, used only when a paper has no published-version entry.
fn bibtex_key(id: &str) -> String {
    format!("cryptoeprint:{id}")
}

/// Is this a Wayland session? Decides only the *order* of the candidates, never
/// which ones are tried, so a wrong guess costs one failed spawn.
fn on_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").map(|s| s == "wayland") == Ok(true)
}

/// The clipboard tools worth trying, best first. There is no single one to pick:
/// macOS has `pbcopy`, X11 has `xclip`/`xsel`, Wayland has `wl-copy`, and Ubuntu
/// ships *none* of the three Linux ones by default.
fn clipboard_candidates() -> Vec<(&'static str, &'static [&'static str])> {
    candidates_for(
        cfg!(target_os = "macos"),
        cfg!(target_os = "windows"),
        on_wayland(),
    )
}

/// Split out from the `cfg!`s so the Linux ordering can be tested from macOS,
/// where it is compiled but never reached.
fn candidates_for(
    macos: bool,
    windows: bool,
    wayland: bool,
) -> Vec<(&'static str, &'static [&'static str])> {
    const XCLIP: (&str, &[&str]) = ("xclip", &["-selection", "clipboard"]);
    const XSEL: (&str, &[&str]) = ("xsel", &["--clipboard", "--input"]);
    const WLCOPY: (&str, &[&str]) = ("wl-copy", &[]);
    if macos {
        vec![("pbcopy", &[])]
    } else if windows {
        vec![("clip", &[])]
    } else if wayland {
        vec![WLCOPY, XCLIP, XSEL]
    } else {
        vec![XCLIP, XSEL, WLCOPY]
    }
}

/// What to tell someone who has none of them, naming the package rather than the
/// binary — "install xclip" is not a command anyone can run.
pub fn clipboard_hint() -> &'static str {
    if on_wayland() {
        "no clipboard tool — sudo apt install wl-clipboard"
    } else {
        "no clipboard tool — sudo apt install xclip"
    }
}

/// Ask the terminal itself to take the text (OSC 52). Written to the controlling
/// terminal rather than stdout, because the TUI owns stdout while this runs.
///
/// This does *not* rescue GNOME Terminal: VTE has never implemented OSC 52
/// (gitlab.gnome.org/GNOME/vte/-/issues/2495, open since 2018). It is here for
/// kitty, WezTerm, foot, Alacritty and tmux, and for sessions over ssh where no
/// local clipboard binary can help.
fn osc52(text: &str) -> bool {
    let payload = base64(text.as_bytes());
    match std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        Ok(mut tty) => write!(tty, "\x1b]52;c;{payload}\x07").is_ok(),
        Err(_) => false,
    }
}

/// Standard base64, because OSC 52 carries its payload that way and pulling in a
/// crate for twenty lines would be the tenth dependency.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(match chunk.len() {
            1 => '=',
            _ => ALPHABET[(n >> 6 & 63) as usize] as char,
        });
        out.push(match chunk.len() {
            3 => ALPHABET[(n & 63) as usize] as char,
            _ => '=',
        });
    }
    out
}

/// First candidate that exists and exits cleanly wins; a missing binary is not a
/// failure, just the next one's turn.
fn copy_to_clipboard(text: &str) -> bool {
    for (prog, args) in clipboard_candidates() {
        let spawned = std::process::Command::new(prog)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            // Not installed — try the next one rather than giving up.
            Err(_) => continue,
        };
        if let Some(mut sin) = child.stdin.take() {
            if sin.write_all(text.as_bytes()).is_err() {
                continue;
            }
            // Closed here rather than at the end of the loop body: wl-copy and
            // xclip both wait for EOF before taking ownership of the selection.
            drop(sin);
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    osc52(text)
}

struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

pub fn run(
    conn: Connection,
    initial: String,
    filters: Filters,
    theme: Theme,
    scope: Scope,
    bib_stale_days: Option<i64>,
    watch_list: Vec<db::Watch>,
) -> Result<()> {
    let mut app = App {
        query: initial,
        hits: Vec::new(),
        total: 0,
        selected: 0,
        scroll: 0,
        editing: Editing::None,
        date_input: String::new(),
        venue_input: String::new(),
        expanded: HashSet::new(),
        watched: db::watched(&conn, &watch_list).unwrap_or_default(),
        venues: db::venue_all(&conn).unwrap_or_default(),
        unfiltered: None,
        watched_only: false,
        watch_list,
        favourite: crate::config::load().favourite_author,
        pending_search: false,
        status: None,
        filters,
        theme,
        scope,
        bib: HashMap::new(),
        bib_stale_days,
    };
    app.search(&conn);

    // Restore the terminal even if we panic mid-draw.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        hook(info);
    }));

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let _guard = Guard;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    loop {
        // The viewport is worked out before drawing rather than inside the draw
        // closure, because the rows it lands on have to be hydrated first and that
        // needs `&mut app`. The layout below is `Length(2) / Min(1) / Length(1)`
        // over the full area, so these two are exactly what the closure will see.
        let size = term.size()?;
        let width = size.width as usize;
        let view_h = (size.height as usize).saturating_sub(3);

        // --- results: heights for every hit, spans for the visible few ---
        // Counting is cheap enough to do for every hit on every frame, where
        // laying them all out is not. This is what removed the entry limit.
        let heights: Vec<usize> = app
            .hits
            .iter()
            .map(|h| hit_height(&app, h, width))
            .collect();
        let mut starts: Vec<usize> = Vec::with_capacity(heights.len() + 1);
        let mut acc = 0usize;
        for h in &heights {
            starts.push(acc);
            acc += h;
        }
        let total_lines = acc;

        // Keep the selected entry on screen.
        if let Some(&start) = starts.get(app.selected) {
            let end = start + heights[app.selected];
            if start < app.scroll {
                app.scroll = start;
            } else if end > app.scroll + view_h {
                app.scroll = end.saturating_sub(view_h);
            }
            if heights[app.selected] > view_h {
                app.scroll = start;
            }
        }
        app.scroll = app.scroll.min(total_lines.saturating_sub(view_h));

        // Lay out only what the viewport can show, starting from the hit the
        // scroll offset falls inside.
        let first = starts
            .partition_point(|&s| s <= app.scroll)
            .saturating_sub(1);
        let intra = app.scroll - starts.get(first).copied().unwrap_or(0);

        // Fetch the marked-up text for just those rows. Done here, after the
        // heights, because marking cannot change them — see `hit_height`.
        let mut visible = 0usize;
        let mut used = 0usize;
        while first + visible < app.hits.len() && used < view_h + intra {
            used += heights[first + visible];
            visible += 1;
        }
        app.hydrate_visible(&conn, first, visible);

        let mut lines: Vec<Line<'static>> = Vec::new();
        for i in first..(first + visible).min(app.hits.len()) {
            lines.extend(hit_lines(&app, i, &app.hits[i], width));
        }
        if app.hits.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  No matches.",
                app.theme.style(Tone::Meta),
            )));
        }

        term.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(area);

            let th = &app.theme;

            // --- header ---
            let count = if app.total > app.hits.len() {
                format!("{} of {}", app.hits.len(), app.total)
            } else {
                format!("{}", app.hits.len())
            };
            let position = if app.hits.is_empty() {
                "0/0".to_string()
            } else {
                format!("{}/{}", app.selected + 1, app.hits.len())
            };
            let mut modes = format!("   [{}]  {} · {}", position, count, app.scope.label());
            if app.watched_only {
                modes.push_str(" · watched only");
            }
            // A date filter used to be invisible once you were inside, whether it
            // came from `--date` or from `d`.
            if !app.filters.date_text.is_empty() {
                modes.push_str(&format!(" · {}", app.filters.date_text));
            }
            if !app.filters.venue_text.is_empty() {
                modes.push_str(&format!(" · {}", app.filters.venue_text));
            }
            if let Some(d) = app.bib_stale_days {
                modes.push_str(&format!(" · bib unchecked {d}d"));
            }
            let head = if app.editing == Editing::Date {
                Line::from(vec![
                    Span::styled("  date ", th.style(Tone::Meta)),
                    Span::styled(app.date_input.clone(), th.style(Tone::Title)),
                    Span::styled("▏", th.style(Tone::Marker)),
                ])
            } else if app.editing == Editing::Venue {
                Line::from(vec![
                    Span::styled("  venue ", th.style(Tone::Meta)),
                    Span::styled(app.venue_input.clone(), th.style(Tone::Title)),
                    Span::styled("▏", th.style(Tone::Marker)),
                ])
            } else if app.editing == Editing::Query {
                Line::from(vec![
                    Span::styled("  search ", th.style(Tone::Meta)),
                    Span::styled(app.query.clone(), th.style(Tone::Title)),
                    Span::styled("▏", th.style(Tone::Marker)),
                    // The listing below is one query behind until the debounce
                    // fires; say so rather than let it look like a wrong answer.
                    Span::styled(
                        if app.pending_search { "  …" } else { "" },
                        th.style(Tone::Meta),
                    ),
                ])
            } else {
                let q = if app.query.is_empty() {
                    "(all papers)".to_string()
                } else {
                    app.query.clone()
                };
                Line::from(vec![
                    Span::styled("  eprint ", th.style(Tone::Marker)),
                    Span::styled(q, th.style(Tone::Title)),
                    Span::styled(modes, th.style(Tone::Meta)),
                ])
            };
            f.render_widget(Paragraph::new(vec![head, Line::raw("")]), chunks[0]);

            // `intra` is at most one entry's height, so this cast is safe. Passing
            // the absolute line offset here was the old bug: past ~65,535 lines the
            // u16 truncated and scrolling landed somewhere arbitrary.
            f.render_widget(Paragraph::new(lines).scroll((intra as u16, 0)), chunks[1]);

            // --- footer ---
            let help = match app.editing {
                Editing::Date => fit_hints(
                    width,
                    // Enough of the grammar to show its shape, then what the
                    // keys do — a narrow terminal should lose a fourth example
                    // before it loses "enter apply".
                    &[
                        "2024",
                        "2023..2024",
                        "30d",
                        "enter apply",
                        "empty clears",
                        "04/2024",
                        "28/04/2024",
                    ],
                    "esc cancel",
                ),
                Editing::Venue => fit_hints(
                    width,
                    &[
                        "CRYPTO",
                        "CRYPTO 2025",
                        "enter apply",
                        "empty clears",
                        "prefixes work",
                    ],
                    "esc cancel",
                ),
                Editing::Query => fit_hints(
                    width,
                    &["type to filter", "enter accept", "ctrl-u clear"],
                    "esc cancel",
                ),
                Editing::None => fit_hints(
                    width,
                    &[
                        "j/k move",
                        "space expand",
                        "/ search",
                        "enter open",
                        "y url",
                        "b key",
                        "B entry",
                        "t scope",
                        "d date",
                        "v venue",
                        "w watched",
                    ],
                    "q quit",
                ),
            };
            let foot = match &app.status {
                Some(s) => Line::from(Span::styled(format!("  {s}"), th.style(Tone::Id))),
                None => Line::from(Span::styled(help, th.style(Tone::Meta))),
            };
            f.render_widget(Paragraph::new(foot), chunks[2]);
        })?;

        // Blocking while idle, so a still screen costs nothing; bounded while a
        // search is owed, so a burst of typing collapses into one query rather
        // than one per keystroke. The old unconditional `event::read()` meant
        // every queued keystroke ran its own full search, and they stacked.
        let ev = if app.pending_search {
            if event::poll(DEBOUNCE)? {
                event::read()?
            } else {
                app.pending_search = false;
                app.search(&conn);
                continue;
            }
        } else {
            event::read()?
        };
        let key = match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => k,
            _ => continue,
        };

        if app.editing == Editing::Date {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('c') if ctrl => break,
                KeyCode::Esc => {
                    app.editing = Editing::None;
                    app.status = None;
                }
                KeyCode::Enter => {
                    let typed = app.date_input.trim().to_string();
                    if typed.is_empty() {
                        // An empty prompt is how you clear the filter.
                        app.filters.since = None;
                        app.filters.before = None;
                        app.filters.date_text.clear();
                        app.editing = Editing::None;
                        app.search(&conn);
                    } else {
                        // The same parser the flag uses, so the grammar cannot
                        // drift between `--date` and `d`.
                        match crate::dates::parse_range(&typed) {
                            Ok((since, before)) => {
                                app.filters.since = since;
                                app.filters.before = before;
                                app.filters.date_text = typed;
                                app.editing = Editing::None;
                                app.search(&conn);
                            }
                            // Stay in the prompt so the mistake can be corrected
                            // rather than retyped.
                            Err(e) => app.status = Some(format!("{e}")),
                        }
                    }
                }
                KeyCode::Backspace => {
                    app.date_input.pop();
                }
                KeyCode::Char('u') if ctrl => app.date_input.clear(),
                KeyCode::Char(c) if !ctrl => app.date_input.push(c),
                _ => {}
            }
            continue;
        }

        if app.editing == Editing::Venue {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('c') if ctrl => break,
                KeyCode::Esc => {
                    app.editing = Editing::None;
                    app.status = None;
                }
                KeyCode::Enter => {
                    let typed = app.venue_input.trim().to_string();
                    if typed.is_empty() {
                        // An empty prompt is how you clear the filter.
                        app.filters.venue = None;
                        app.filters.venue_year = None;
                        app.filters.venue_text.clear();
                        app.editing = Editing::None;
                        app.search(&conn);
                    } else {
                        // The same grammar and the same resolver the flag uses, so
                        // `--venue` and `v` cannot drift apart.
                        let (name, year) = eprint::venue::parse_filter(&typed);
                        match db::resolve_venue(&conn, &name) {
                            Ok(Some(v)) => {
                                app.filters.venue_text = match &year {
                                    Some(y) => format!("{v} {y}"),
                                    None => v.clone(),
                                };
                                app.filters.venue = Some(v);
                                app.filters.venue_year = year;
                                app.editing = Editing::None;
                                app.search(&conn);
                            }
                            // Stay in the prompt so the mistake can be corrected
                            // rather than retyped.
                            Ok(None) => app.status = Some(format!("no venue matching {name:?}")),
                            Err(e) => app.status = Some(format!("{e}")),
                        }
                    }
                }
                KeyCode::Backspace => {
                    app.venue_input.pop();
                }
                KeyCode::Char('u') if ctrl => app.venue_input.clear(),
                KeyCode::Char(c) if !ctrl => app.venue_input.push(c),
                _ => {}
            }
            continue;
        }

        if app.editing == Editing::Query {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                // Leaving the prompt must not leave the listing behind: if the
                // debounce has not fired yet, settle it here.
                KeyCode::Esc | KeyCode::Enter => {
                    app.editing = Editing::None;
                    if app.pending_search {
                        app.pending_search = false;
                        app.search(&conn);
                    }
                }
                // Ctrl-C has to work here too. Without this arm it fell through to
                // `Char(c)` and typed a "c" — an app that swallows Ctrl-C is worse
                // than one that ignores the key.
                KeyCode::Char('c') if ctrl => break,
                // Each of these only marks the query dirty. The search itself runs
                // from the event loop once the keystrokes stop.
                KeyCode::Backspace => {
                    app.query.pop();
                    app.pending_search = true;
                }
                KeyCode::Char('u') if ctrl => {
                    app.query.clear();
                    app.pending_search = true;
                }
                // Only unmodified characters are text; every other chord (ctrl-d,
                // ctrl-a, alt-x …) is ignored rather than inserted as its letter.
                KeyCode::Char(c) if !ctrl => {
                    app.query.push(c);
                    app.pending_search = true;
                }
                _ => {}
            }
            continue;
        }

        app.status = None;
        let last = app.hits.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Char('j') | KeyCode::Down => app.selected = (app.selected + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => app.selected = app.selected.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => app.selected = 0,
            KeyCode::Char('G') | KeyCode::End => app.selected = last,
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.selected = (app.selected + 5).min(last);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.selected = app.selected.saturating_sub(5);
            }
            KeyCode::PageDown => app.selected = (app.selected + 10).min(last),
            KeyCode::PageUp => app.selected = app.selected.saturating_sub(10),
            KeyCode::Char(' ') | KeyCode::Tab => app.expand_selected(&conn),
            KeyCode::Char('t') => {
                app.scope = match app.scope {
                    Scope::All => Scope::Title,
                    Scope::Title => Scope::All,
                };
                app.search(&conn);
            }
            KeyCode::Char('w') => {
                if app.watched_only {
                    app.watched_only = false;
                    // Switching off returns to a listing we already had, so restore
                    // it rather than asking the database for it again.
                    match app.unfiltered.take() {
                        Some((hits, total)) => {
                            app.hits = hits;
                            app.total = total;
                            app.selected = 0;
                            app.scroll = 0;
                        }
                        None => app.search(&conn),
                    }
                } else {
                    // Re-read on every switch-on, so a watch added in another shell
                    // since this session started is picked up. The cache rebuilds
                    // itself if that list has changed; otherwise this is a lookup.
                    app.watch_list = crate::config::load().watches;
                    app.watched = db::watched(&conn, &app.watch_list).unwrap_or_default();
                    if app.watched.is_empty() {
                        app.status =
                            Some("no watches yet — `eprint watch add \"topic\"`".to_string());
                    } else {
                        let prev = (std::mem::take(&mut app.hits), app.total);
                        app.watched_only = true;
                        app.search(&conn);
                        app.unfiltered = Some(prev);
                    }
                }
            }
            KeyCode::Char('/') => app.editing = Editing::Query,
            KeyCode::Char('d') => {
                app.date_input = app.filters.date_text.clone();
                app.editing = Editing::Date;
                app.status = None;
            }
            KeyCode::Char('v') => {
                app.venue_input = app.filters.venue_text.clone();
                app.editing = Editing::Venue;
                app.status = None;
            }
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(h) = app.selected_hit() {
                    let id = h.paper.id.clone();
                    // Same reflex as `eprint open`: the filed PDF when we have
                    // one, otherwise the browser plus a detached watcher. The
                    // status line carries the save hint, since the TUI owns the
                    // screen and cannot print to it.
                    let local = crate::pdf::cached(&id);
                    let _ = crate::open_paper(&conn, &id, false);
                    app.status = Some(match local {
                        Some(p) => format!(
                            "opened {}",
                            p.file_name()
                                .map(|f| f.to_string_lossy().to_string())
                                .unwrap_or_else(|| p.to_string_lossy().to_string())
                        ),
                        None => format!("opened {id}.pdf — {}", crate::save_hint()),
                    });
                }
            }
            KeyCode::Char('b') => {
                if let Some(h) = app.selected_hit() {
                    let (key, published) = match app.bib.get(&h.paper.id) {
                        Some((k, p)) => (k.clone(), *p),
                        None => (bibtex_key(&h.paper.id), false),
                    };
                    let note = if published {
                        " (published version)"
                    } else {
                        ""
                    };
                    app.status = Some(if copy_to_clipboard(&key) {
                        format!("copied {key}{note}{}", app.stale_hint())
                    } else {
                        clipboard_hint().to_string()
                    });
                }
            }
            KeyCode::Char('B') => {
                let id = app.selected_hit().map(|h| h.paper.id.clone());
                if let Some(id) = id {
                    app.status = Some(match db::bib_entry(&conn, &id) {
                        Ok(Some((key, entry, published))) if !entry.is_empty() => {
                            let note = if published {
                                " (published version)"
                            } else {
                                ""
                            };
                            if copy_to_clipboard(&entry) {
                                format!("copied BibTeX entry {key}{note}{}", app.stale_hint())
                            } else {
                                clipboard_hint().to_string()
                            }
                        }
                        Ok(Some(_)) => "entry text missing — run `eprint bib --update`".to_string(),
                        _ => format!("{id} is not in CryptoBib; no entry to copy"),
                    });
                }
            }
            KeyCode::Char('y') => {
                if let Some(h) = app.selected_hit() {
                    let url = h.paper.url.clone();
                    app.status = Some(if copy_to_clipboard(&url) {
                        format!("copied {url}")
                    } else {
                        clipboard_hint().to_string()
                    });
                }
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-rolled, so pinned to the RFC 4648 vectors — a padding slip here would
    // corrupt whatever OSC 52 handed the terminal, silently.
    #[test]
    fn base64_matches_the_standard_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "base64({input:?})");
        }
    }

    #[test]
    fn base64_survives_non_ascii() {
        // A citation key can carry an accented author name.
        assert_eq!(base64("Grégoire".as_bytes()), "R3LDqWdvaXJl");
    }

    #[test]
    fn every_platform_offers_a_clipboard_candidate() {
        assert!(!clipboard_candidates().is_empty());
    }

    // The Linux arms never run on macOS, so this is the only thing standing
    // between them and shipping untested.
    #[test]
    fn linux_tries_the_session_s_own_tool_first() {
        let wayland: Vec<&str> = candidates_for(false, false, true)
            .iter()
            .map(|(p, _)| *p)
            .collect();
        assert_eq!(wayland, ["wl-copy", "xclip", "xsel"]);

        let x11: Vec<&str> = candidates_for(false, false, false)
            .iter()
            .map(|(p, _)| *p)
            .collect();
        assert_eq!(x11, ["xclip", "xsel", "wl-copy"]);

        // Whichever session, all three remain reachable: guessing wrong costs a
        // failed spawn, not a failed copy.
        for probe in [true, false] {
            let names: Vec<&str> = candidates_for(false, false, probe)
                .iter()
                .map(|(p, _)| *p)
                .collect();
            for tool in ["wl-copy", "xclip", "xsel"] {
                assert!(names.contains(&tool), "{tool} missing when wayland={probe}");
            }
        }
    }

    #[test]
    fn mac_and_windows_use_their_builtin() {
        assert_eq!(candidates_for(true, false, false)[0].0, "pbcopy");
        assert_eq!(candidates_for(false, true, false)[0].0, "clip");
    }
}
