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
use crate::render::{full_authors, short_authors, wrap, wrap_body};
use crate::theme::{Theme, Tone};

#[derive(Default, Clone)]
pub struct Filters {
    pub year: Option<i64>,
    pub since: Option<String>,
    pub author: Option<String>,
    pub category: Option<String>,
    pub limit: usize,
    pub prefix: bool,
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
    editing: bool,
    /// Keyed by paper id so expansion survives re-searching.
    expanded: HashSet<String>,
    status: Option<String>,
    filters: Filters,
    theme: Theme,
    scope: Scope,
    /// eprint id -> (citation key, is_published)
    bib: HashMap<String, (String, bool)>,
    /// Age of the CryptoBib data in days, when it is old enough to mention.
    bib_stale_days: Option<i64>,
}

impl App {
    fn search(&mut self, conn: &Connection) {
        let q = Query {
            terms: &self.query,
            year: self.filters.year,
            since: self.filters.since.clone(),
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

/// Build the whole scrollable document, plus the starting line index of each
/// hit so selection can drive scrolling.
fn build(app: &App, width: usize) -> (Vec<Line<'static>>, Vec<usize>) {
    let body_w = width.saturating_sub(INDENT + 2).max(20);
    let pad = " ".repeat(INDENT);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let th = &app.theme;
    let hl = th.style(Tone::Match);
    let meta_s = th.style(Tone::Meta);

    for (i, hit) in app.hits.iter().enumerate() {
        starts.push(lines.len());
        let p = &hit.paper;
        let is_sel = i == app.selected;
        let is_open = app.expanded.contains(&p.id);

        let marker = if is_sel { "❯ " } else { "  " };
        let arrow = if is_open { "▾ " } else { "▸ " };
        let title_src = if hit.title_hl.is_empty() {
            &p.title
        } else {
            &hit.title_hl
        };
        let title_lines = wrap(title_src, body_w);

        let mut title_s = th.style(Tone::Title);
        if is_sel {
            title_s = title_s.patch(th.style(Tone::Marker));
        }

        let mut topen = false;
        let mut head = vec![
            Span::styled(marker, th.style(Tone::Marker)),
            Span::styled(arrow, meta_s),
            Span::styled(format!("{:<w$}", p.id, w = ID_W), th.style(Tone::Id)),
            Span::raw("  "),
        ];
        head.extend(marked_spans(&title_lines[0], title_s, hl, &mut topen));
        lines.push(Line::from(head));
        for cont in title_lines.iter().skip(1) {
            let mut spans = vec![Span::raw(pad.clone())];
            spans.extend(marked_spans(cont, title_s, hl, &mut topen));
            lines.push(Line::from(spans));
        }

        // Surnames while collapsed, so rows stay scannable. Expanding gives the
        // full byline its own line — as `eprint show` does — because appending
        // eight first names to the `·`-joined line buries the date behind them.
        let mut meta = vec![if is_open {
            full_authors(&p.authors)
        } else {
            short_authors(&p.authors)
        }];
        if is_open {
            let mut trailer: Vec<String> = Vec::new();
            if !p.date.is_empty() {
                trailer.push(p.date.chars().take(10).collect());
            }
            if let Some((key, _)) = app.bib.get(&p.id) {
                trailer.push(key.clone());
            }
            if !trailer.is_empty() {
                meta.push(trailer.join(" · "));
            }
        }
        for src in &meta {
            for m in wrap(src, body_w) {
                lines.push(Line::from(vec![
                    Span::raw(pad.clone()),
                    Span::styled(m, meta_s),
                ]));
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
        // No snippet while collapsed: a 16-token window out of the middle of an
        // abstract reads as noise. Matches are still highlighted in the title
        // here, and in the whole abstract once expanded.

        lines.push(Line::raw(""));
    }

    if app.hits.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("  No matches.", meta_s)));
    }

    (lines, starts)
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
) -> Result<()> {
    let mut app = App {
        query: initial,
        hits: Vec::new(),
        total: 0,
        selected: 0,
        scroll: 0,
        editing: false,
        expanded: HashSet::new(),
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
            if let Some(d) = app.bib_stale_days {
                modes.push_str(&format!(" · bib {d}d old"));
            }
            let head = if app.editing {
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
            let (lines, starts) = build(&app, width);

            // Keep the selected entry on screen.
            if let Some(&start) = starts.get(app.selected) {
                let end = starts.get(app.selected + 1).copied().unwrap_or(lines.len());
                if start < app.scroll {
                    app.scroll = start;
                } else if end > app.scroll + view_h {
                    app.scroll = end.saturating_sub(view_h);
                }
                if end.saturating_sub(start) > view_h {
                    app.scroll = start;
                }
            }
            let max_scroll = lines.len().saturating_sub(view_h);
            app.scroll = app.scroll.min(max_scroll);

            f.render_widget(
                Paragraph::new(lines).scroll((app.scroll as u16, 0)),
                chunks[1],
            );

            // --- footer ---
            let help = if app.editing {
                "  type to filter · enter accept · ctrl-u clear · esc cancel"
            } else {
                "  j/k move · space expand · t scope · enter open · y url · b key · B entry · / search · q quit"
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

        if app.editing {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => app.editing = false,
                KeyCode::Backspace => {
                    app.query.pop();
                    app.search(&conn);
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.query.clear();
                    app.search(&conn);
                }
                KeyCode::Char(c) => {
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
            KeyCode::Char('/') => app.editing = true,
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(h) = app.selected_hit() {
                    let url = h.paper.url.clone();
                    let _ = crate::open_url(&url);
                    app.status = Some(format!("opened {url}"));
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
