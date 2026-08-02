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
use crate::render::{
    full_authors, loved, short_authors, wrap, wrap_body, BADGE, BADGE_W, LOVE_W,
};
use crate::theme::{Theme, Tone};

/// Which prompt, if any, is taking keystrokes. Was a bool when the query was the
/// only thing you could edit.
#[derive(PartialEq, Clone, Copy)]
enum Editing {
    None,
    Query,
    Date,
}

#[derive(Default, Clone)]
pub struct Filters {
    pub year: Option<i64>,
    pub since: Option<String>,
    pub before: Option<String>,
    pub author: Option<String>,
    pub category: Option<String>,
    pub limit: usize,
    pub prefix: bool,
    /// What the user typed for the date, kept verbatim so the header can show it
    /// and `d` can reopen with it. The parsed bounds live in `since`/`before`.
    pub date_text: String,
}

const ID_W: usize = 11;
/// Per-watch ceiling when staging the watched-only filter. Generous enough that
/// no realistic watch is truncated, bounded so a pathological one cannot stage
/// the whole archive.
const WATCH_STAGE_CAP: usize = 20_000;
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
    /// Keyed by paper id so expansion survives re-searching.
    expanded: HashSet<String>,
    status: Option<String>,
    filters: Filters,
    theme: Theme,
    scope: Scope,
    /// eprint id -> (citation key, is_published)
    bib: HashMap<String, (String, bool)>,
    /// Ids on screen that match a saved watch.
    watched: HashSet<String>,
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
}

impl App {
    fn search(&mut self, conn: &Connection) {
        let q = Query {
            terms: &self.query,
            year: self.filters.year,
            since: self.filters.since.clone(),
            before: self.filters.before.clone(),
            added_since: None,
            only_watched: self.watched_only,
            only_listed: false,
            author: self.filters.author.clone(),
            category: self.filters.category.clone(),
            limit: self.filters.limit,
            scope: self.scope,
            prefix: self.filters.prefix,
        };
        match db::search(conn, &q) {
            Ok(hits) => {
                self.total = db::count_matches(conn, &q).unwrap_or(hits.len());
                let ids: Vec<String> = hits.iter().map(|h| h.paper.id.clone()).collect();
                self.bib = db::bib_map(conn, &ids).unwrap_or_default();
                self.watched = db::watched_ids(conn, &ids, &self.watch_list).unwrap_or_default();
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
            Some(d) => format!("  · bib data {d}d old, run `eprint bib --update`"),
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
fn hit_height(app: &App, hit: &Hit, width: usize) -> usize {
    let p = &hit.paper;
    let is_open = app.expanded.contains(&p.id);
    let fav = app.favourite.as_deref();
    let (body_w, title_w, meta_w) = widths(app, hit, width);
    let title_src = if hit.title_hl.is_empty() {
        &p.title
    } else {
        &hit.title_hl
    };
    let mut n = crate::render::wrap_count(title_src, title_w);
    let byline = if is_open {
        full_authors(&p.authors, fav)
    } else {
        short_authors(&p.authors, fav)
    };
    n += crate::render::wrap_count(&byline, meta_w);
    if is_open {
        let mut trailer: Vec<String> = Vec::new();
        if !p.date.is_empty() {
            trailer.push(crate::render::fmt_date(&p.date));
        }
        if let Some((key, _)) = app.bib.get(&p.id) {
            trailer.push(key.clone());
        }
        if !trailer.is_empty() {
            n += crate::render::wrap_count(&trailer.join(" · "), meta_w);
        }
        n += crate::render::wrap_body_count(&hit.abstract_hl, body_w);
    }
    n + 1 // the blank line between entries
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

        // Surnames while collapsed, so rows stay scannable. Expanding gives the
        // full byline its own line — as `eprint show` does — because appending
        // eight first names to the `·`-joined line buries the date behind them.
        let fav = app.favourite.as_deref();
        let mut meta = vec![if is_open {
            full_authors(&p.authors, fav)
        } else {
            short_authors(&p.authors, fav)
        }];
        if is_open {
            let mut trailer: Vec<String> = Vec::new();
            if !p.date.is_empty() {
                trailer.push(crate::render::fmt_date(&p.date));
            }
            if let Some((key, _)) = app.bib.get(&p.id) {
                trailer.push(key.clone());
            }
            if !trailer.is_empty() {
                meta.push(trailer.join(" · "));
            }
        }
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

fn copy_to_clipboard(text: &str) -> bool {
    let (prog, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else if cfg!(target_os = "windows") {
        ("clip", &[])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };
    match std::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(mut sin) = child.stdin.take() {
                let _ = sin.write_all(text.as_bytes());
            }
            child.wait().map(|s| s.success()).unwrap_or(false)
        }
        Err(_) => false,
    }
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
        expanded: HashSet::new(),
        watched: HashSet::new(),
        watched_only: false,
        watch_list,
        favourite: crate::config::load().favourite_author,
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

            let width = chunks[1].width as usize;
            let view_h = chunks[1].height as usize;
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
            if let Some(d) = app.bib_stale_days {
                modes.push_str(&format!(" · bib {d}d old"));
            }
            let head = if app.editing == Editing::Date {
                Line::from(vec![
                    Span::styled("  date ", th.style(Tone::Meta)),
                    Span::styled(app.date_input.clone(), th.style(Tone::Title)),
                    Span::styled("▏", th.style(Tone::Marker)),
                ])
            } else if app.editing == Editing::Query {
                Line::from(vec![
                    Span::styled("  search ", th.style(Tone::Meta)),
                    Span::styled(app.query.clone(), th.style(Tone::Title)),
                    Span::styled("▏", th.style(Tone::Marker)),
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

            // --- results ---
            // Heights only: counting is cheap enough to do for every hit on every
            // frame, where laying them all out is not. This is what removed the
            // entry limit.
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
            let total = acc;

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
            app.scroll = app.scroll.min(total.saturating_sub(view_h));

            // Lay out only what the viewport can show, starting from the hit the
            // scroll offset falls inside.
            let first = starts.partition_point(|&s| s <= app.scroll).saturating_sub(1);
            let intra = app.scroll - starts.get(first).copied().unwrap_or(0);
            let mut lines: Vec<Line<'static>> = Vec::new();
            let mut i = first;
            while i < app.hits.len() && lines.len() < view_h + intra {
                lines.extend(hit_lines(&app, i, &app.hits[i], width));
                i += 1;
            }
            if app.hits.is_empty() {
                lines.push(Line::raw(""));
                lines.push(Line::from(Span::styled(
                    "  No matches.",
                    app.theme.style(Tone::Meta),
                )));
            }

            // `intra` is at most one entry's height, so this cast is safe. Passing
            // the absolute line offset here was the old bug: past ~65,535 lines the
            // u16 truncated and scrolling landed somewhere arbitrary.
            f.render_widget(
                Paragraph::new(lines).scroll((intra as u16, 0)),
                chunks[1],
            );

            // --- footer ---
            let help = match app.editing {
                Editing::Date => {
                    "  2024 · 04/2024 · 28/04/2024 · 2023..2024 · 30d · enter apply · empty clears · esc cancel"
                }
                Editing::Query => "  type to filter · enter accept · ctrl-u clear · esc cancel",
                Editing::None => {
                    "  j/k move · space expand · t scope · d date · w watched · enter open · y url · b key · B entry · / search · q quit"
                }
            };
            let foot = match &app.status {
                Some(s) => Line::from(Span::styled(format!("  {s}"), th.style(Tone::Id))),
                None => Line::from(Span::styled(help, th.style(Tone::Meta))),
            };
            f.render_widget(Paragraph::new(foot), chunks[2]);
        })?;

        let ev = event::read()?;
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
                        match crate::parse_range(&typed) {
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

        if app.editing == Editing::Query {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc | KeyCode::Enter => app.editing = Editing::None,
                // Ctrl-C has to work here too. Without this arm it fell through to
                // `Char(c)` and typed a "c" — an app that swallows Ctrl-C is worse
                // than one that ignores the key.
                KeyCode::Char('c') if ctrl => break,
                KeyCode::Backspace => {
                    app.query.pop();
                    app.search(&conn);
                }
                KeyCode::Char('u') if ctrl => {
                    app.query.clear();
                    app.search(&conn);
                }
                // Only unmodified characters are text; every other chord (ctrl-d,
                // ctrl-a, alt-x …) is ignored rather than inserted as its letter.
                KeyCode::Char(c) if !ctrl => {
                    app.query.push(c);
                    app.search(&conn);
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
            KeyCode::Char(' ') | KeyCode::Tab => {
                if let Some(h) = app.selected_hit() {
                    let id = h.paper.id.clone();
                    if !app.expanded.remove(&id) {
                        app.expanded.insert(id);
                    }
                }
            }
            KeyCode::Char('a') => {
                if app.expanded.is_empty() {
                    app.expanded = app.hits.iter().map(|h| h.paper.id.clone()).collect();
                } else {
                    app.expanded.clear();
                }
            }
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
                    app.search(&conn);
                } else {
                    // Re-stage on every switch-on, so watches added in another
                    // shell since this session started are picked up.
                    app.watch_list = crate::config::load().watches;
                    match db::stage_watched(&conn, &app.watch_list, WATCH_STAGE_CAP) {
                        Ok(0) => {
                            app.status =
                                Some("no watches yet — `eprint watch add \"topic\"`".to_string())
                        }
                        Ok(_) => {
                            app.watched_only = true;
                            app.search(&conn);
                        }
                        Err(_) => app.status = Some("could not read your watches".to_string()),
                    }
                }
            }
            KeyCode::Char('/') => app.editing = Editing::Query,
            KeyCode::Char('d') => {
                app.date_input = app.filters.date_text.clone();
                app.editing = Editing::Date;
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
                    let _ = crate::open_paper(&conn, &id);
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
                    let note = if published { " (published version)" } else { "" };
                    app.status = Some(if copy_to_clipboard(&key) {
                        format!("copied {key}{note}{}", app.stale_hint())
                    } else {
                        "could not reach the clipboard".to_string()
                    });
                }
            }
            KeyCode::Char('B') => {
                let id = app.selected_hit().map(|h| h.paper.id.clone());
                if let Some(id) = id {
                    app.status = Some(match db::bib_entry(&conn, &id) {
                        Ok(Some((key, entry, published))) if !entry.is_empty() => {
                            let note = if published { " (published version)" } else { "" };
                            if copy_to_clipboard(&entry) {
                                format!("copied BibTeX entry {key}{note}{}", app.stale_hint())
                            } else {
                                "could not reach the clipboard".to_string()
                            }
                        }
                        Ok(Some(_)) => {
                            "entry text missing — run `eprint bib --update`".to_string()
                        }
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
                        "could not reach the clipboard".to_string()
                    });
                }
            }
            _ => {}
        }
    }

    Ok(())
}
