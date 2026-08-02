mod bib;
mod completions;
mod config;
mod dates;
mod db;
mod harvest;
mod names;
mod pdf;
mod render;
mod theme;
mod tui;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use db::{Query, Scope};
use render::{Style, StyleOpts};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use theme::{Theme, Tone};

/// Refresh the index in the background when it is older than this.
const STALE_SECS: u64 = 24 * 3600;
/// Never spawn a background refresh more often than this.
const ATTEMPT_COOLDOWN_SECS: u64 = 3600;
/// Re-request a small overlap window so nothing slips through the cracks.
const OVERLAP_SECS: u64 = 2 * 24 * 3600;
/// Sanity bound on one arrival batch, for someone coming back after a long
/// absence. Not a display limit: `latest_limit` is a floor, not a cap.
const BATCH_MAX: usize = 500;
/// How many author names one `--author` completion offers. A menu is for choosing
/// from, not for reading: past this the answer is "type another letter".
const AUTHOR_MATCHES: usize = 40;
/// Set on a detached child to make it file a downloaded PDF instead of running a
/// command. An env marker rather than a subcommand, hidden or otherwise, so the
/// CLI surface stays exactly as it was.
const ADOPT_ID_VAR: &str = "EPRINT_ADOPT";
const ADOPT_TITLE_VAR: &str = "EPRINT_ADOPT_TITLE";

#[derive(Parser, Debug)]
#[command(
    name = "eprint",
    version,
    about = "Search the IACR Cryptology ePrint Archive from the command line",
    long_about = "Search the IACR Cryptology ePrint Archive.\n\n\
        `eprint <query>` searches; a bare `eprint` shows the papers that have\n\
        arrived since you last looked.\n\n\
        Metadata comes from the archive's OAI-PMH interface and is cached locally,\n\
        so searches are instant and offline. Full-text PDFs are licensed per paper\n\
        and are opened in your browser rather than downloaded in bulk.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    search: SearchArgs,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Interactive full-screen browser
    Browse(BrowseArgs),
    /// Open a paper's PDF — your local copy once you have one
    Open {
        /// Paper id, e.g. 2026/1539, or just 1539 for this year. Omit to list
        /// the papers you have already downloaded
        id: Option<String>,
        /// Delete downloaded copies instead of opening them, e.g. 1539 1540
        #[arg(long, value_name = "ID", num_args = 1.., conflicts_with = "id")]
        rm: Vec<String>,
    },
    /// Show one paper in full
    Show {
        /// Paper id, e.g. 2026/1539, or just 1539 for this year
        id: String,
    },
    /// Saved searches that mark the papers you care about
    Watch {
        #[command(subcommand)]
        action: Option<WatchCmd>,
    },
    /// Citation keys from CryptoBib — add `--entry` for the whole record
    Bib {
        /// Paper id, e.g. 2015/123 (bare 1539 means this year). Omit for status
        id: Option<String>,
        /// Print the full BibTeX record instead of just the key
        #[arg(long, conflicts_with = "update")]
        entry: bool,
        /// Download or refresh the CryptoBib database
        #[arg(long)]
        update: bool,
        /// Re-download even if unchanged
        #[arg(long, requires = "update")]
        force: bool,
    },
    /// Show index statistics
    Status,
    /// Refresh the local index from the ePrint OAI-PMH feed
    Update {
        /// Re-harvest the entire archive instead of only what changed
        #[arg(long)]
        full: bool,
        /// Suppress progress output
        #[arg(long)]
        quiet: bool,
    },
    /// Show or create the configuration file
    Config {
        /// Write a commented default config file if none exists
        #[arg(long)]
        init: bool,
        /// Open the config file in $EDITOR, creating it first if needed
        #[arg(short = 'e', long, conflicts_with = "init")]
        edit: bool,
        /// Switch on Tab completion by adding one line to your shell's rc file
        #[arg(long)]
        completions: bool,
        /// Write the author aliases file, with suggestions to uncomment. Add
        /// `--edit` to open it afterwards
        #[arg(long)]
        aliases: bool,
    },
    /// Shell completion, hidden because it is plumbing: `completions zsh` prints
    /// the function to install, `completions ids` prints the candidates it offers.
    #[command(hide = true)]
    Completions {
        /// `zsh`, `ids`, `categories`, `watches` or `authors`
        what: String,
        /// Narrows `authors`, whose full list is far too big to offer whole
        needle: Option<String>,
    },
    /// Kept working, kept out of `--help`. Removing it entirely turned
    /// `eprint search NAPs` into a search for the words "search" and "NAPs",
    /// which silently returns junk instead of erroring — a trap that caught its
    /// own author within a day. Hiding it shortens the command list, which was
    /// the point, without punishing muscle memory.
    #[command(hide = true)]
    Search(SearchArgs),
}

/// `-n 0` is not "no limit", it is a query that can only return nothing, so
/// reject it at parse time rather than printing "No matches."
fn positive(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(0) => Err("must be at least 1".to_string()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}

#[derive(Subcommand, Debug)]
enum WatchCmd {
    /// Save a search. Same query syntax and filters as `eprint search`
    Add {
        /// Query terms. Supports "quoted phrases", AND/OR/NOT and prefix*
        query: Vec<String>,
        /// Watch an author — quote a full name: --author "Katharina Boudgoust"
        #[arg(long)]
        author: Option<String>,
        /// Watch an IACR category
        #[arg(long)]
        category: Option<String>,
        /// Match titles and authors only, ignoring abstracts
        #[arg(short = 't', long)]
        title: bool,
    },
    /// Remove a watch by the number `eprint watch` shows
    Rm {
        /// Watch number
        id: Option<i64>,
        /// Remove every watch
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// List saved watches (the default)
    List,
}

#[derive(Args, Debug, Default)]
struct BrowseArgs {
    /// Starting query. Edit it live inside the browser with `/`
    query: Vec<String>,
    /// Maximum results to load. Everything, unless you say otherwise
    #[arg(short = 'n', long, value_parser = positive)]
    limit: Option<usize>,
    /// Match titles and authors only, ignoring abstracts
    #[arg(short = 't', long)]
    title: bool,
    /// Search scope: all (title, authors, abstract) or title
    #[arg(long, value_name = "all|title", conflicts_with = "title", hide = true)]
    scope: Option<String>,
    /// Colour palette: auto, dark, light or mono
    #[arg(long, value_name = "auto|dark|light|mono", hide = true)]
    theme: Option<String>,
    /// Date or range: 2024, 04/2024, 28/04/2024, 2023..2024, ..2020, or 30d
    #[arg(long, value_name = "RANGE")]
    date: Option<String>,
    /// Superseded by `--date`, kept so old habits and scripts keep working.
    #[arg(long, hide = true)]
    since: Option<String>,
    /// Superseded by `--date 2024`, kept for the same reason.
    #[arg(long, hide = true)]
    year: Option<i64>,
    /// Filter by author name (substring match)
    #[arg(long)]
    author: Option<String>,
    /// Filter by IACR category (substring match)
    #[arg(long)]
    category: Option<String>,
    /// Match whole words only, disabling automatic prefix matching
    #[arg(long, hide = true)]
    exact: bool,
}

#[derive(Args, Debug, Default)]
struct SearchArgs {
    /// Query terms. Supports "quoted phrases", AND/OR/NOT and prefix*
    query: Vec<String>,
    /// Maximum results to show
    #[arg(short = 'n', long, value_parser = positive)]
    limit: Option<usize>,
    /// Date or range: 2024, 04/2024, 28/04/2024, 2023..2024, ..2020, or 30d
    #[arg(long, value_name = "RANGE")]
    date: Option<String>,
    /// Filter by author name (substring match)
    #[arg(long)]
    author: Option<String>,
    /// Filter by IACR category (substring match)
    #[arg(long)]
    category: Option<String>,
    /// Match titles and authors only, ignoring abstracts
    #[arg(short = 't', long)]
    title: bool,
    /// Include full abstracts (omitted by default)
    #[arg(short = 'a', long)]
    abstracts: bool,

    /// Superseded by `--date`, kept so old habits and scripts keep working.
    #[arg(long, hide = true)]
    since: Option<String>,
    /// Superseded by `--date 2024`, kept for the same reason.
    #[arg(long, hide = true)]
    year: Option<i64>,

    // --- Rarely needed; functional but kept out of --help to keep it short.
    /// Search scope: all (title, authors, abstract) or title
    #[arg(long, value_name = "all|title", conflicts_with = "title", hide = true)]
    scope: Option<String>,
    /// Colour palette: auto, dark, light or mono (normally set in the config file)
    #[arg(long, value_name = "auto|dark|light|mono", hide = true)]
    theme: Option<String>,
    /// Match whole words only, disabling automatic prefix matching
    #[arg(long, hide = true)]
    exact: bool,
    /// Emit JSON
    #[arg(long, hide = true)]
    json: bool,
    /// Disable colour and hyperlinks
    #[arg(long, hide = true)]
    plain: bool,
    /// Force colour even when piped (e.g. into `less -R`)
    #[arg(long, conflicts_with = "plain", hide = true)]
    color: bool,
    /// Print a bare URL under each result
    #[arg(long, hide = true)]
    urls: bool,
    /// Never print bare URLs
    #[arg(long, conflicts_with = "urls", hide = true)]
    no_urls: bool,
    /// Do not pipe long output through a pager
    #[arg(long, hide = true)]
    no_pager: bool,
    /// Skip the automatic background index refresh
    #[arg(long, hide = true)]
    no_update: bool,
}

impl SearchArgs {
    /// A bare `eprint`: no query and nothing narrowed down, so the output is just
    /// the newest papers. Filters count as a search even without query terms —
    /// `--author x` is something you asked for, not a glance at the feed.
    fn is_latest(&self) -> bool {
        self.query.join(" ").trim().is_empty()
            && self.year.is_none()
            && self.date.is_none()
            && self.since.is_none()
            && self.author.is_none()
            && self.category.is_none()
    }
}

// ---------- minimal civil-time helpers (no chrono dependency) ----------











// ---------- index freshness ----------

fn index_age(conn: &rusqlite::Connection) -> Result<Option<i64>> {
    Ok(db::meta_get(conn, harvest::KEY_LAST_HARVEST)?
        .and_then(|v| dates::parse_iso(&v))
        .map(|t| (dates::now() - t).max(0)))
}

/// Kick off `eprint update --quiet` as a detached child and return
/// immediately, so a stale index never delays a search.
fn spawn_background_refresh(conn: &rusqlite::Connection) -> Result<()> {
    let last_attempt = db::meta_get(conn, harvest::KEY_LAST_ATTEMPT)?
        .and_then(|v| dates::parse_iso(&v))
        .unwrap_or(0);
    if dates::now() - last_attempt < ATTEMPT_COOLDOWN_SECS as i64 {
        return Ok(());
    }
    db::meta_set(conn, harvest::KEY_LAST_ATTEMPT, &dates::format_iso(dates::now()))?;

    let exe = std::env::current_exe().context("locating own executable")?;
    let _ = std::process::Command::new(exe)
        .args(["update", "--quiet"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    Ok(())
}

fn do_update(full: bool, quiet: bool) -> Result<()> {
    let mut conn = db::open()?;
    let from = if full {
        None
    } else {
        db::meta_get(&conn, harvest::KEY_LAST_HARVEST)?.and_then(|v| {
            dates::parse_iso(&v).map(|t| dates::format_iso(t - OVERLAP_SECS as i64))
        })
    };
    db::meta_set(&conn, harvest::KEY_LAST_ATTEMPT, &dates::format_iso(dates::now()))?;
    if !quiet {
        match &from {
            Some(f) => eprintln!("Updating index (changes since {})…", &f[..10]),
            None => eprintln!("Harvesting the full archive — this takes a couple of minutes…"),
        }
    }
    let n = harvest::run(&mut conn, from.as_deref(), quiet, &dates::format_iso(dates::now()))?;
    // New papers invalidate the watch cache. Rebuilding here keeps the cost inside
    // the update — usually the detached background child — rather than surprising
    // whichever command runs next.
    let _ = db::watched(&conn, &watches(&conn));
    if !quiet {
        let total = db::count(&conn)?;
        if n == 0 {
            eprintln!("Already up to date — {total} papers indexed.");
        } else {
            eprintln!("Done — {n} records processed, {total} papers indexed.");
        }
    }
    Ok(())
}

fn style_for(a: &SearchArgs, cfg: &config::Config) -> Style {
    let urls = if a.urls {
        Some(true)
    } else if a.no_urls {
        Some(false)
    } else {
        None
    };
    Style::detect(StyleOpts {
        plain: a.plain || a.json,
        force_color: a.color && !a.json,
        urls,
        theme: a.theme.clone().unwrap_or_else(|| cfg.theme.clone()),
        favourite: cfg.favourite_author.clone(),
    })
}

/// CLI flag beats config file beats built-in default.
fn effective_scope(title_flag: bool, cli: Option<&str>, cfg: &config::Config) -> Scope {
    if title_flag {
        return Scope::Title;
    }
    Scope::from_str(cli.unwrap_or(&cfg.scope))
}


/// Print, or pipe through a pager when the output would not fit on screen.
/// `less -RFX` keeps colour, exits immediately if the output fits, and leaves
/// the results in scrollback afterwards rather than clearing them.
fn page(text: &str, st: &Style, no_pager: bool) -> Result<()> {
    let fits = text.lines().count() + 1 <= st.height;
    if no_pager || fits || !std::io::stdout().is_terminal() {
        print!("{text}");
        let _ = std::io::stdout().flush();
        return Ok(());
    }

    let spec = std::env::var("EPRINT_PAGER")
        .or_else(|_| std::env::var("PAGER"))
        .unwrap_or_else(|_| "less".to_string());
    let mut parts = spec.split_whitespace();
    let prog = match parts.next() {
        Some(p) => p,
        None => {
            print!("{text}");
            return Ok(());
        }
    };
    let extra: Vec<&str> = parts.collect();
    let mut cmd = std::process::Command::new(prog);
    if extra.is_empty() && prog.ends_with("less") {
        cmd.arg("-RFX");
    } else {
        cmd.args(&extra);
    }
    cmd.stdin(std::process::Stdio::piped());

    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(mut sin) = child.stdin.take() {
                // Ignore EPIPE: quitting the pager early is normal.
                let _ = sin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            Ok(())
        }
        Err(_) => {
            print!("{text}");
            let _ = std::io::stdout().flush();
            Ok(())
        }
    }
}

pub fn open_url(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("launching {opener}"))?;
    Ok(())
}

/// Open a paper the way the user means it: the filed PDF if we have one, else the
/// browser — and in that case start watching for the download, because opening a
/// paper is the only signal needed that it is worth keeping.
///
/// The archive serves PDFs behind a Cloudflare challenge and denies `*pdf` in
/// `robots.txt`, so the browser does the fetching and we only file the result.
/// `announce` is false when the caller owns the screen. `browse` draws every cell
/// itself, so anything printed here lands on top of the listing — the save hint
/// used to appear as broken text across the selected paper's title. The TUI shows
/// the same words in its status line instead; the filing behaviour is identical,
/// which is the point of both front-ends coming through here.
fn open_paper(conn: &rusqlite::Connection, id: &str, announce: bool) -> Result<()> {
    if let Some(path) = pdf::cached(id) {
        return open_url(&path.to_string_lossy());
    }
    // Straight to the PDF: the landing page is a detour, and `eprint show` already
    // holds the metadata it would have shown.
    open_url(&format!("https://eprint.iacr.org/{id}.pdf"))?;
    let title = db::get(conn, id)?.map(|p| p.title).unwrap_or_default();
    spawn_adopter(id, &title);
    if announce {
        completions::nudge_completions(conn);
        // stderr, so piping stays clean.
        eprintln!("{}", save_hint());
    }
    Ok(())
}

/// The papers already downloaded, id, title and file, newest first.
fn library_listing() -> Vec<(String, String, PathBuf)> {
    let files = pdf::library();
    let ids: Vec<String> = files.iter().map(|(id, _)| id.clone()).collect();
    let titles = db::open()
        .ok()
        .and_then(|c| db::titles(&c, &ids).ok())
        .unwrap_or_default();
    files
        .into_iter()
        .map(|(id, path)| {
            // The real title beats the filename slug; the slug is the fallback for
            // a paper the index has never seen.
            let t = titles
                .get(&id)
                .cloned()
                .unwrap_or_else(|| pdf::slug_words(&path));
            (id, t, path)
        })
        .collect()
}

/// Decimal MB with one place, matching `status` and the CryptoBib download rather
/// than introducing a second size convention.
fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

fn do_library() -> Result<()> {
    let papers = library_listing();
    if papers.is_empty() {
        println!("\nNo papers downloaded yet — `eprint open <id>` and save the PDF.\n");
        return Ok(());
    }
    let th = Theme::resolve(&config::load().theme, std::io::stdout().is_terminal());
    println!();
    // Sizes are here because "which of these should I drop?" is the question that
    // comes before `open --rm`.
    let mut total = 0u64;
    for (id, title, path) in &papers {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        total += size;
        println!(
            "  {}  {}  {}",
            th.paint(Tone::Id, &format!("{id:<9}")),
            th.paint(Tone::Meta, &format!("{:>8}", mb(size))),
            title
        );
    }
    println!(
        "\n  {}\n",
        th.paint(
            Tone::Meta,
            &format!(
                "{} paper{}, {} in {}",
                papers.len(),
                if papers.len() == 1 { "" } else { "s" },
                mb(total),
                pdf::library_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            )
        )
    );
    Ok(())
}

/// `open --rm`: drop filed copies. Unprompted on purpose — naming ids is already
/// explicit, and opening the paper again fetches it back — but each file that goes
/// is named, so a deletion is never silent.
fn do_forget(ids: &[String]) -> Result<()> {
    let th = Theme::resolve(&config::load().theme, std::io::stdout().is_terminal());
    println!();
    let mut gone = 0usize;
    let mut freed = 0u64;
    for raw in ids {
        let id = normalise_id(raw);
        let size = pdf::cached(&id)
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        match pdf::remove(&id)? {
            Some(path) => {
                gone += 1;
                freed += size;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                println!(
                    "  {}  removed {}",
                    th.paint(Tone::Id, &format!("{id:<9}")),
                    th.paint(Tone::Meta, &name)
                );
            }
            None => println!(
                "  {}  {}",
                th.paint(Tone::Id, &format!("{id:<9}")),
                th.paint(Tone::Meta, "no downloaded copy")
            ),
        }
    }
    let summary = if gone == 0 {
        "nothing removed".to_string()
    } else {
        format!(
            "{gone} paper{} removed, {} freed",
            if gone == 1 { "" } else { "s" },
            mb(freed)
        )
    };
    println!("\n  {}\n", th.paint(Tone::Meta, &summary));
    Ok(())
}



/// Printed on the first open of a paper. Deliberately does *not* ask the user to
/// navigate anywhere: the browser suggests whatever folder it last used, so the
/// hint names the places already being watched instead.
fn save_hint() -> String {
    let names = pdf::watched_names();
    let places = match names.split_last() {
        None => return "save the PDF and it will be kept".to_string(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    };
    format!("⌘S anywhere in {places} and it will be kept")
}

/// Detached child that files the PDF once the browser has saved it. Same recipe
/// as `spawn_background_refresh`, and like it the child is fire-and-forget: the
/// caller must not wait, so `browse` stays interactive and the shell prompt
/// returns immediately.
fn spawn_adopter(id: &str, title: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .env(ADOPT_ID_VAR, id)
        .env(ADOPT_TITLE_VAR, title)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// ePrint ids are `YYYY/NNN` and nothing else. Checked before anything is opened:
/// the alternative is handing a typo to the browser, which looks like the tool
/// working right up until the page 404s.
fn valid_id(id: &str) -> bool {
    match id.split_once('/') {
        Some((y, n)) => {
            y.len() == 4
                && y.bytes().all(|b| b.is_ascii_digit())
                && !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

fn normalise_id(raw: &str) -> String {
    let id = raw
        .trim()
        .trim_start_matches("https://eprint.iacr.org/")
        .trim_start_matches("http://eprint.iacr.org/")
        .trim_end_matches(".pdf");
    // A bare number is the year-less half of an id, as printed in announcements
    // and mailing-list posts; assume the current year. Even a four-digit input
    // is read this way — per-year submission counts passed 2000 in 2024, so it
    // is a plausible paper number and guessing "year" would be wrong as often.
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        let (y, _, _) = dates::civil_from_days(dates::now().div_euclid(86400));
        return format!("{y}/{id}");
    }
    id.to_string()
}

fn do_search(a: &SearchArgs) -> Result<()> {
    let cfg = config::load();
    let conn = db::open()?;
    let st = style_for(a, &cfg);

    // First run: an empty index makes searching pointless, so harvest inline.
    if db::count(&conn)? == 0 {
        drop(conn);
        eprintln!("No local index yet — building it now (one time only).");
        do_update(true, false)?;
        return do_search_inner(a, &st, &cfg);
    }

    let age = index_age(&conn)?;
    if !a.no_update && age.map(|s| s as u64 > STALE_SECS).unwrap_or(true) {
        let _ = spawn_background_refresh(&conn);
    }
    drop(conn);
    do_search_inner(a, &st, &cfg)
}

fn do_search_inner(a: &SearchArgs, st: &Style, cfg: &config::Config) -> Result<()> {
    let conn = db::open()?;
    let terms = a.query.join(" ");
    let (since, before) = dates::date_window(&a.date, &a.since)?;
    let scope = effective_scope(a.title, a.scope.as_deref(), cfg);

    let q = Query {
        terms: &terms,
        year: a.year,
        since,
        before,
        added_since: None,
        only_watched: false,
        author: a.author.clone(),
        category: a.category.clone(),
        limit: a.limit.unwrap_or(if a.is_latest() {
            cfg.latest_limit
        } else {
            cfg.limit
        }),
        scope,
        prefix: !a.exact,
    };

    let hits = db::search(&conn, &q)?;

    if a.json {
        println!("{}", render::json_of(&hits));
        return Ok(());
    }

    if hits.is_empty() {
        println!("\nNo matches.\n");
        return Ok(());
    }

    let total = db::count_matches(&conn, &q)?;
    let age = index_age(&conn)?.map(dates::human_age);
    let mut out = String::new();
    render::render_header(&mut out, hits.len(), total, age, scope.label(), st);
    let ids: Vec<String> = hits.iter().map(|h| h.paper.id.clone()).collect();
    let bibs = db::bib_map(&conn, &ids).unwrap_or_default();
    let watched = db::watched(&conn, &watches(&conn)).unwrap_or_default();
    for hit in &hits {
        let w = watched.contains(&hit.paper.id);
        render::render_hit(&mut out, hit, st, a.abstracts, bibs.get(&hit.paper.id), w);
    }
    page(&out, st, a.no_pager)
}

fn do_browse(a: &BrowseArgs) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        bail!("`browse` is interactive; use `eprint search` when piping output");
    }
    let conn = db::open()?;
    if db::count(&conn)? == 0 {
        drop(conn);
        eprintln!("No local index yet — building it now (one time only).");
        do_update(true, false)?;
        return do_browse(a);
    }
    // Same non-blocking freshness check as `search`.
    if index_age(&conn)?.map(|s| s as u64 > STALE_SECS).unwrap_or(true) {
        let _ = spawn_background_refresh(&conn);
    }
    let (since, before) = dates::date_window(&a.date, &a.since)?;
    let cfg = config::load();
    let filters = tui::Filters {
        year: a.year,
        since,
        before,
        date_text: a.date.clone().or_else(|| a.since.clone()).unwrap_or_default(),
        author: a.author.clone(),
        category: a.category.clone(),
        // No limit means no limit: laying out only the visible rows made loading
        // the whole archive cost nothing measurable.
        limit: a.limit.unwrap_or(usize::MAX),
        prefix: !a.exact,
    };
    let theme = Theme::resolve(
        a.theme.as_deref().unwrap_or(&cfg.theme),
        std::env::var_os("NO_COLOR").is_none(),
    );
    let scope = effective_scope(a.title, a.scope.as_deref(), &cfg);
    let stale = bib_stale_days(&conn)?;
    let watch_list = watches(&conn);
    tui::run(conn, a.query.join(" "), filters, theme, scope, stale, watch_list)
}

/// Days since the CryptoBib data was refreshed, but only once that exceeds
/// the threshold — `None` means "recent enough to say nothing".
const BIB_STALE_DAYS: i64 = 30;

fn bib_stale_days(conn: &rusqlite::Connection) -> Result<Option<i64>> {
    if db::bib_count(conn)? == 0 {
        return Ok(None);
    }
    let Some(ts) = db::meta_get(conn, bib::KEY_UPDATED)? else {
        return Ok(None);
    };
    let Some(t) = dates::parse_iso(&ts) else {
        return Ok(None);
    };
    let days = (dates::now() - t).max(0) / 86400;
    Ok(if days >= BIB_STALE_DAYS {
        Some(days)
    } else {
        None
    })
}

/// No query and no filters is not an empty search, it is "what is new?". Shared by
/// the bare invocation and the hidden `search` alias so the two cannot diverge.
fn query_or_feed(a: &SearchArgs) -> Result<()> {
    if a.is_latest() {
        do_feed(a)
    } else {
        do_search(a)
    }
}

/// A bare `eprint`: the papers that have arrived since you last looked. Refreshes
/// in the background like `search` rather than blocking — this is the most-typed
/// invocation, and the batch replay below means a slightly stale answer is shown
/// again next time rather than lost.
fn do_feed(a: &SearchArgs) -> Result<()> {
    let cfg = config::load();
    let conn = db::open()?;
    if db::count(&conn)? == 0 {
        drop(conn);
        eprintln!("No local index yet — building it now (one time only).");
        do_update(true, false)?;
        return do_feed(a);
    }
    if !a.no_update && index_age(&conn)?.map(|s| s as u64 > STALE_SECS).unwrap_or(true) {
        let _ = spawn_background_refresh(&conn);
    }
    drop(conn);
    do_feed_inner(a, &cfg)
}

fn do_feed_inner(a: &SearchArgs, cfg: &config::Config) -> Result<()> {
    let conn = db::open()?;
    let st = style_for(a, cfg);
    // `latest_limit` is a *floor*, not a cap: a quiet day still shows something, and
    // a big batch is shown whole rather than truncated to a number that has nothing
    // to do with how much arrived. `-n` overrides it as an exact count.
    let floor = a.limit.unwrap_or(cfg.latest_limit);

    let watermark = db::meta_get(&conn, harvest::KEY_LAST_SEEN)?
        .unwrap_or_else(|| dates::format_iso(dates::now() - 7 * 86400));

    let day = |ts: &str| render::fmt_date(ts);

    // No cap on the batch itself, only a sanity bound for the case where someone
    // returns from a long absence.
    let mut hits = db::added_since(&conn, &watermark, BATCH_MAX)?;
    let fresh_count = hits.len();
    // The window whose papers are on screen: the fresh diff normally, the
    // remembered one when there is no fresh diff to show.
    let mut window = watermark.clone();
    let mut replayed = false;

    // ePrint posts in bursts, so most runs find nothing. Rather than report an
    // empty diff, show the last one again until the archive actually moves.
    if hits.is_empty() {
        if let Some(prev) = db::meta_get(&conn, harvest::KEY_NEW_BATCH)? {
            let again = db::added_since(&conn, &prev, BATCH_MAX)?;
            if !again.is_empty() {
                hits = again;
                window = prev;
                replayed = true;
            }
        }
    }

    // Still short of the floor — either nothing is new or the batch was tiny — so
    // top up with the most recent arrivals. They are ordered the same way, so the
    // new ones stay at the top and the rest are simply context.
    let topped_up = hits.len() < floor;
    if topped_up {
        hits = db::recent_arrivals(&conn, floor)?;
    }
    // With `-n`, the number given wins outright, batch or not.
    if let Some(n) = a.limit {
        hits.truncate(n);
    }

    if hits.is_empty() {
        // Only reachable on an empty index, which the caller has already handled.
        println!("\nNothing to show yet — try `eprint update`.\n");
        return Ok(());
    }

    // The two dates in this header deliberately mean different things. A count of
    // new papers is *about* your last look, so it is dated by it. "Nothing new",
    // dated by your last look, only ever says "you ran this recently" — the useful
    // answer there is when the archive itself last posted.
    let posted = db::newest(&conn)?.map(|(_, date)| day(&date));

    // Order matters: a topped-up listing is no longer "the last batch", even if a
    // replay is what it was topped up from, so that case is reported first.
    let label = if topped_up {
        match (fresh_count, &posted) {
            (0, Some(p)) => format!("nothing new since {p}"),
            // Only with no papers at all, which the caller has already handled.
            (0, None) => "nothing new".to_string(),
            (n, _) => format!("{n} new since {}", day(&watermark)),
        }
    } else if replayed {
        // The batch's own newest paper, not the window that produced it: the window
        // start is another "when you last ran it" date, and a late-published paper
        // (recent arrival, older date) would make the index-wide answer name a date
        // no paper on screen carries.
        let batch = hits
            .iter()
            .map(|h| h.paper.date.as_str())
            .max()
            .map(day)
            .unwrap_or_else(|| day(&window));
        format!("last batch, from {batch} · nothing new yet")
    } else {
        format!("since {}", day(&window))
    };

    // A stale index looks exactly like a quiet archive once the header is dated by
    // the archive, so say which it is. Only when stale: the feed is the most-typed
    // command and this is the one listing that has always kept its header short.
    let age = index_age(&conn)?
        .filter(|s| *s as u64 > STALE_SECS)
        .map(dates::human_age);

    if a.json {
        println!("{}", render::json_of(&hits));
        return Ok(());
    }
    let mut out = String::new();
    render::render_header(&mut out, hits.len(), hits.len(), age, &label, &st);
    let ids: Vec<String> = hits.iter().map(|h| h.paper.id.clone()).collect();
    let bibs = db::bib_map(&conn, &ids).unwrap_or_default();
    let watched = db::watched(&conn, &watches(&conn)).unwrap_or_default();
    for hit in &hits {
        let w = watched.contains(&hit.paper.id);
        render::render_hit(&mut out, hit, &st, a.abstracts, bibs.get(&hit.paper.id), w);
    }
    page(&out, &st, a.no_pager)?;

    // Remember a *fresh* diff so later runs can replay it; a replay needs no
    // pointer write, since the pointer already names that window, and a topped-up
    // listing is not a batch at all.
    if fresh_count > 0 {
        db::meta_set(&conn, harvest::KEY_NEW_BATCH, &watermark)?;
    }
    db::meta_set(&conn, harvest::KEY_LAST_SEEN, &dates::format_iso(dates::now()))?;
    Ok(())
}

/// The saved searches, from the config file — plus the one-time move of anything an
/// older build left in the index. Everything that reads watches goes through here,
/// so the migration cannot be missed by one code path.
fn watches(conn: &rusqlite::Connection) -> Vec<db::Watch> {
    let from_config = config::load().watches;
    if !from_config.is_empty() {
        return from_config;
    }
    let legacy = db::legacy_watches(conn).unwrap_or_default();
    if legacy.is_empty() {
        return legacy;
    }
    let labels: Vec<String> = legacy.iter().map(|w| w.label()).collect();
    match config::set_watches(&labels) {
        Ok(path) => {
            // Only drop the table once the config is safely written, so a failure
            // here leaves the watches recoverable and the move is retried.
            let _ = db::drop_legacy_watches(conn);
            eprintln!(
                "moved {} watch{} into {} — they travel with that file now",
                labels.len(),
                if labels.len() == 1 { "" } else { "es" },
                path.display()
            );
            config::load().watches
        }
        Err(_) => legacy,
    }
}

fn do_watch(action: Option<WatchCmd>) -> Result<()> {
    let conn = db::open()?;
    match action.unwrap_or(WatchCmd::List) {
        WatchCmd::Add {
            query,
            author,
            category,
            title,
        } => {
            let terms = query.join(" ");
            // Blank means absent: `--author ''` is the user saying nothing, and
            // storing it as an empty filter would save a watch that matches every
            // paper in the archive.
            let author = author.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            let category = category
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if terms.trim().is_empty() && author.is_none() && category.is_none() {
                bail!("nothing to watch — give query terms, --author or --category");
            }
            let cfg = config::load();
            let scope = effective_scope(title, None, &cfg);
            let new = db::Watch {
                id: 0,
                terms,
                author,
                category,
                scope,
            }
            .label();
            // The file is line-based and the label has to parse back to this same
            // watch. Anything that would not is refused here rather than written
            // and silently misread later.
            if !config::round_trips(&new) {
                if new.contains(['\n', '\r']) {
                    bail!("a watch cannot contain a line break");
                }
                bail!("{new:?} would not read back the same way — try quoting it");
            }
            let mut labels: Vec<String> = watches(&conn).iter().map(|w| w.label()).collect();
            if labels.contains(&new) {
                // Not a failure: the state the user asked for is the state they
                // have. Say so and show the list, rather than exiting non-zero
                // over a no-op.
                println!("\n  already watching that — nothing to add");
                println!();
                return list_watches(&conn);
            }
            labels.push(new);
            config::set_watches(&labels)?;
            println!();
            list_watches(&conn)?;
        }
        WatchCmd::Rm { id, all } => {
            let existing = watches(&conn);
            if all {
                let n = existing.len();
                config::set_watches(&[])?;
                println!("\n  removed {n} watch{}\n", if n == 1 { "" } else { "es" });
            } else {
                let Some(id) = id else {
                    bail!("which watch? give a number from `eprint watch`, or --all");
                };
                // Numbers are positions in the file, so they close up after a
                // removal — which is what `eprint watch` shows you next.
                if id < 1 || id as usize > existing.len() {
                    bail!("no watch {id} — `eprint watch` lists them");
                }
                let labels: Vec<String> = existing
                    .iter()
                    .filter(|w| w.id != id)
                    .map(|w| w.label())
                    .collect();
                config::set_watches(&labels)?;
                println!();
                list_watches(&conn)?;
            }
        }
        WatchCmd::List => {
            println!();
            list_watches(&conn)?;
        }
    }
    Ok(())
}

fn list_watches(conn: &rusqlite::Connection) -> Result<()> {
    let watches = watches(conn);
    // Every write to the watch list passes through here on its way to being
    // printed, so this is where the cache is brought up to date — which for an
    // added or removed watch is that one watch's rows, not a rebuild.
    let counts = db::watch_counts(conn, &watches).unwrap_or_default();
    if watches.is_empty() {
        println!("  No watches yet. Save one with:\n");
        println!("    eprint watch add \"lattice OR LWE\"");
        println!("    eprint watch add --author Boneh");
        println!("    eprint watch add --author \"Katharina Boudgoust\"   # quote a full name\n");
        println!("  Matching papers are then marked ✱ wherever they appear.\n");
        return Ok(());
    }
    for w in &watches {
        // The index-wide count is the useful sanity check on a new watch: it
        // says "this expression does match things" before you wait a day for
        // `new` to prove it the hard way. Read from the cache, which already
        // knows which papers this watch marks — counting them here meant one
        // whole-index scan per watch, and a second per `eprint watch`.
        let total = counts.get(&w.label()).copied().unwrap_or(0);
        println!("  {:<3} {:<44} {total} in the index", w.id, w.describe());
    }
    println!("\n  matches are marked ✱ in search, `new` and `browse` · `w` in browse filters to them");
    if let Some(p) = config::path() {
        // Naming the file is the point of keeping them there: copy it and the
        // whole setup follows.
        println!("  stored in {}\n", p.display());
    } else {
        println!();
    }
    Ok(())
}

fn do_bib(id: Option<&str>, update: bool, force: bool, want_entry: bool) -> Result<()> {
    let mut conn = db::open()?;

    if update {
        match bib::update(&mut conn, force, false, &dates::format_iso(dates::now()))? {
            bib::Outcome::UpToDate => {
                eprintln!("CryptoBib is already up to date.");
            }
            bib::Outcome::Rebuilt {
                entries,
                linked,
                published,
            } => {
                eprintln!(
                    "Parsed {entries} entries — {linked} ePrint papers linked, \
                     {published} with a published version ({:.0}%).",
                    if linked > 0 {
                        published as f64 / linked as f64 * 100.0
                    } else {
                        0.0
                    }
                );
            }
        }
        if id.is_none() {
            return Ok(());
        }
    }

    if let Some(raw) = id {
        let pid = normalise_id(raw);
        if db::bib_count(&conn)? == 0 {
            bail!("no CryptoBib data yet — run `eprint bib --update`");
        }
        if want_entry {
            match db::bib_entry(&conn, &pid)? {
                Some((_, entry, _)) if !entry.is_empty() => println!("{entry}"),
                Some(_) => bail!("entry text missing — run `eprint bib --update`"),
                None => bail!("{pid} is not in CryptoBib"),
            }
            warn_if_stale(&conn)?;
            return Ok(());
        }
        match db::bib_for(&conn, &pid)? {
            Some((key, published)) => {
                println!();
                println!("  {key}");
                println!(
                    "  {}",
                    if published {
                        "published version"
                    } else {
                        "ePrint preprint (no published version found)"
                    }
                );
                println!();
            }
            None => {
                println!("\n  cryptoeprint:{pid}\n  not in CryptoBib; using the archive's own key\n");
            }
        }
        warn_if_stale(&conn)?;
        return Ok(());
    }

    let total = db::bib_count(&conn)?;
    let updated = db::meta_get(&conn, bib::KEY_UPDATED)?.unwrap_or_else(|| "never".into());
    let published: i64 = conn.query_row(
        "SELECT COUNT(*) FROM bib WHERE kind = 'published'",
        [],
        |r| r.get(0),
    )?;
    let eprints: i64 = conn.query_row(
        "SELECT COUNT(*) FROM bib WHERE kind = 'eprint'",
        [],
        |r| r.get(0),
    )?;
    println!();
    if total == 0 {
        println!("  no CryptoBib data — run `eprint bib --update`");
    } else {
        println!("  ePrint papers linked   {eprints}");
        println!("  with published version {published}");
        match bib_stale_days(&conn)? {
            Some(d) => println!("  last updated           {updated}  ({d} days ago)"),
            None => println!("  last updated           {updated}"),
        }
    }
    println!();
    warn_if_stale(&conn)?;
    Ok(())
}

fn warn_if_stale(conn: &rusqlite::Connection) -> Result<()> {
    if let Some(d) = bib_stale_days(conn)? {
        eprintln!("note: CryptoBib data is {d} days old — run `eprint bib --update` to refresh");
    }
    Ok(())
}

/// `$VISUAL`/`$EDITOR` may carry arguments (`code -w`, `emacsclient -nw`), so the
/// first word is the program and the rest are passed through.
fn edit_config() -> Result<()> {
    // Nothing to edit until the file exists; write the commented template so the
    // editor opens on the settings and their explanations rather than a blank buffer.
    let (path, created) = config::init()?;
    if created {
        println!("wrote {}", path.display());
    }
    edit_file(&path)
}

/// Hand a file to the user's editor. Shared by `--edit` and `--aliases --edit`,
/// which open different files.
fn edit_file(path: &std::path::Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut words = editor.split_whitespace();
    let prog = words.next().unwrap_or("vi");
    let status = std::process::Command::new(prog)
        .args(words)
        .arg(path)
        .status()
        .with_context(|| format!("launching {prog} (set $EDITOR to choose another editor)"))?;
    if !status.success() {
        bail!("{prog} exited with {status}");
    }
    Ok(())
}






/// `config --aliases`: start the author aliases file off with everything the
/// rules could not decide for themselves.
///
/// Suggestions are written commented out. Merging two names is a claim about
/// people, and the tool has no business making it silently — but it does know
/// which names look alike, and finding them by hand across 20,000 spellings is
/// not a reasonable thing to ask.
fn write_aliases(open_after: bool) -> Result<()> {
    let path = config::aliases_path().context("could not determine the config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let conn = db::open()?;
    let suggestions = names::alias_suggestions(&conn)?;
    // Only what the file does not mention yet, so this can be run again after the
    // archive grows without disturbing anything already decided.
    let fresh: Vec<&String> = suggestions
        .iter()
        .filter(|s| {
            let name = s.split(" = ").next().unwrap_or("");
            !name.is_empty() && !existing.contains(name)
        })
        .collect();
    let mut out = existing.clone();
    if out.is_empty() {
        out.push_str(
            "# Author aliases: spellings of one person that the tool cannot prove
             # are the same. Accents, punctuation, spacing and the umlaut/digraph
             # pair are handled automatically and need no entry here.
             #
             #   Yuval Ishai  = Yual Ishai, Y. Ishai
             #   Yu Chen     != Yue Chen        # never the same person
             #
             # Below: names that look alike. Uncomment the ones that are right.

",
        );
    } else if !fresh.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n# Further suggestions:\n");
    }
    for s in &fresh {
        out.push_str(&format!("# {s}\n"));
    }
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    println!();
    println!("  {} suggestion{} written to {}",
        fresh.len(),
        if fresh.len() == 1 { "" } else { "s" },
        path.display());
    // Naming the command that opens *this* file: `--edit` alone opens the config
    // file, which is a different file and used to be promised here by mistake.
    if open_after {
        println!("  uncomment the ones that are right\n");
        return edit_file(&path);
    }
    println!("  uncomment the ones that are right — `eprint config --aliases --edit` opens it\n");
    Ok(())
}

fn do_config(init: bool, edit: bool, completions: bool, aliases: bool) -> Result<()> {
    if aliases {
        return write_aliases(edit);
    }
    if completions {
        return completions::install_completions();
    }
    if edit {
        return edit_config();
    }
    if init {
        let (p, created) = config::init()?;
        println!();
        if created {
            println!("  wrote {}", p.display());
        } else {
            println!("  {} already exists — left untouched", p.display());
        }
        println!();
        return Ok(());
    }
    let cfg = config::load();
    let path = config::path();
    let exists = path.as_ref().map(|p| p.exists()).unwrap_or(false);
    println!();
    match &path {
        Some(p) if exists => println!("  config file    {}", p.display()),
        Some(p) => println!("  config file    {}  (not created yet)", p.display()),
        None => println!("  config file    <could not determine a location>"),
    }
    println!("  theme          {}", cfg.theme);
    println!("  scope          {}", cfg.scope);
    println!("  limit          {}", cfg.limit);
    println!("  latest_limit   {}", cfg.latest_limit);
    println!(
        "  watches        {} ({})",
        cfg.watches.len(),
        if cfg.watches.is_empty() {
            "eprint watch add \"topic\"".to_string()
        } else {
            "eprint watch".to_string()
        }
    );
    println!(
        "  completions   {}",
        if completions::completions_installed() {
            "on".to_string()
        } else {
            "off  (eprint config --completions)".to_string()
        }
    );
    match config::aliases_path() {
        Some(p) if p.exists() => println!(
            "  author aliases {}  ({} in use)",
            p.display(),
            config::aliases().len()
        ),
        Some(p) => println!(
            "  author aliases {}  (not created yet — eprint config --aliases)",
            p.display()
        ),
        None => {}
    }
    if path.is_some() {
        println!("\n  edit with `eprint config --edit`");
    }
    println!();
    Ok(())
}

fn do_status() -> Result<()> {
    let conn = db::open()?;
    let st = Style::detect(StyleOpts {
        plain: false,
        force_color: false,
        urls: None,
        theme: config::load().theme,
        favourite: config::load().favourite_author,
    });
    let total = db::count(&conn)?;
    let path = db::db_path()?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let age = index_age(&conn)?
        .map(dates::human_age)
        .unwrap_or_else(|| "never updated".to_string());
    // Both go through the same formatter as everything else, so `status` cannot be
    // the one place still speaking ISO at the user.
    let last = db::meta_get(&conn, harvest::KEY_LAST_HARVEST)?
        .map(|v| render::fmt_date(&v))
        .unwrap_or_else(|| "—".into());
    // Same "newest paper" the feed dates itself by, so the two cannot disagree.
    let newest = match db::newest(&conn)? {
        Some((id, date)) => format!("{id}  {}", render::fmt_date(&date)),
        None => "—".to_string(),
    };
    let _ = &st;
    println!();
    println!("  papers indexed  {total}");
    println!("  newest entry    {newest}");
    println!("  last harvest    {last}  ({age})");
    println!("  database        {}  ({:.1} MB)", path.display(), size as f64 / 1e6);
    println!();
    Ok(())
}

fn main() {
    quiet_broken_pipe();
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// `eprint watch | head -3` used to end in a Rust panic and a backtrace notice:
/// Rust ignores SIGPIPE, so the first `println!` after the reader leaves fails,
/// and a failed print panics. Closing a pipe early is what `head` is *for*, so it
/// leaves quietly instead. Resetting SIGPIPE itself would mean a `libc`
/// dependency for three lines; every other panic still reports as before.
fn quiet_broken_pipe() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let broken = info
            .payload()
            .downcast_ref::<String>()
            .map(|s| s.contains("Broken pipe"))
            .unwrap_or(false);
        if broken {
            std::process::exit(0);
        }
        previous(info);
    }));
}

fn real_main() -> Result<()> {
    // Checked before argument parsing: this process was spawned to wait for a
    // download, not to run a command.
    if let Ok(id) = std::env::var(ADOPT_ID_VAR) {
        let title = std::env::var(ADOPT_TITLE_VAR).unwrap_or_default();
        pdf::adopt(&id, &title);
        return Ok(());
    }
    let cli = Cli::parse();
    match cli.cmd {
        None => query_or_feed(&cli.search),
        Some(Cmd::Search(a)) => query_or_feed(&a),
        Some(Cmd::Browse(a)) => do_browse(&a),
        Some(Cmd::Update { full, quiet }) => do_update(full, quiet),
        Some(Cmd::Watch { action }) => do_watch(action),
        Some(Cmd::Bib {
            id,
            update,
            force,
            entry,
        }) => do_bib(id.as_deref(), update, force, entry),
        Some(Cmd::Status) => do_status(),
        Some(Cmd::Config {
            init,
            edit,
            completions,
            aliases,
        }) => do_config(init, edit, completions, aliases),
        Some(Cmd::Show { id }) => {
            let conn = db::open()?;
            let st = Style::detect(StyleOpts {
                plain: false,
                force_color: false,
                urls: None,
                theme: config::load().theme,
                favourite: config::load().favourite_author,
            });
            let id = normalise_id(&id);
            match db::get(&conn, &id)? {
                Some(p) => {
                    let key = db::bib_for(&conn, &p.id).unwrap_or(None);
                    let mut out = String::new();
                    render::render_full(&mut out, &p, &st, key.as_ref());
                    page(&out, &st, false)
                }
                None => bail!("no paper {id} in the local index (try `eprint update`)"),
            }
        }
        Some(Cmd::Open { id, rm }) if !rm.is_empty() => {
            debug_assert!(id.is_none(), "clap declares --rm as conflicting with id");
            do_forget(&rm)
        }
        Some(Cmd::Open { id, .. }) => match id {
            Some(id) => {
                let id = normalise_id(&id);
                if !valid_id(&id) {
                    bail!("{id:?} is not a paper id — they look like 2026/1539, or just 1539");
                }
                let conn = db::open()?;
                open_paper(&conn, &id, true)
            }
            // No id: answer "what do I have?" rather than erroring. Works
            // everywhere, including shells with no completion installed.
            None => do_library(),
        },
        Some(Cmd::Completions { what, needle }) => completions::do_completions(&what, needle.as_deref()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case here is a bug that shipped, so the tests are the bug list.
    #[test]
    fn dates_reject_what_they_cannot_read() {
        // A typo used to default each unreadable component and answer a question
        // nobody asked: "2024-o6-01" quietly became 2024-01-01.
        for bad in ["2024-o6-01", "not-a-date", "2024-xx-yy", "abc-def-ghi"] {
            assert!(dates::parse_bound(bad, false).is_err(), "{bad} should not parse");
        }
        // Out-of-range components were already caught; keep them caught.
        assert!(dates::parse_bound("2024-13-01", false).is_err());
        assert!(dates::parse_bound("2024-06-99", false).is_err());
    }

    #[test]
    fn dates_still_accept_what_they_should() {
        assert_eq!(dates::parse_bound("2024-06-15", false).unwrap(), "2024-06-15");
        assert_eq!(dates::parse_bound("28/04/2024", false).unwrap(), "2024-04-28");
        assert_eq!(dates::parse_bound("2024", false).unwrap(), "2024-01-01");
        // The upper bound is the day *after* the period: stored dates are
        // timestamps, so an inclusive `<=` would drop the final day.
        assert_eq!(dates::parse_bound("2024", true).unwrap(), "2025-01-01");
        assert_eq!(dates::parse_bound("28/04/2024", true).unwrap(), "2024-04-29");
        assert_eq!(dates::parse_bound("02/2024", true).unwrap(), "2024-03-01");
    }

    #[test]
    fn ranges_must_run_forwards() {
        assert!(dates::parse_range("2024..2020").is_err());
        assert!(dates::parse_range("2020..2024").is_ok());
        // A single period is both ends of itself, and must stay valid.
        assert!(dates::parse_range("2024").is_ok());
        let (from, till) = dates::parse_range("2020..2024").unwrap();
        assert_eq!(from.unwrap(), "2020-01-01");
        assert_eq!(till.unwrap(), "2025-01-01");
    }

    #[test]
    fn ids_are_year_slash_number() {
        for good in ["2026/1539", "1996/1", "2026/0001"] {
            assert!(valid_id(good), "{good} should be an id");
        }
        // "2026/1523extra" was handed to the browser as a URL.
        for bad in ["2026/1523extra", "abc", "2026/", "/1523", "26/15", "2026-1539"] {
            assert!(!valid_id(bad), "{bad} should not be an id");
        }
    }

    #[test]
    fn a_bare_number_means_this_year() {
        let (y, _, _) = dates::civil_from_days(dates::now().div_euclid(86400));
        assert_eq!(normalise_id("1539"), format!("{y}/1539"));
        assert_eq!(normalise_id("2019/17"), "2019/17");
    }
}
