mod bib;
mod config;
mod db;
mod harvest;
mod render;
mod theme;
mod tui;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use db::{Query, Scope};
use render::{Style, StyleOpts};
use std::io::{IsTerminal, Write};
use theme::Theme;
use std::time::{SystemTime, UNIX_EPOCH};

/// Refresh the index in the background when it is older than this.
const STALE_SECS: u64 = 24 * 3600;
/// Never spawn a background refresh more often than this.
const ATTEMPT_COOLDOWN_SECS: u64 = 3600;
/// Re-request a small overlap window so nothing slips through the cracks.
const OVERLAP_SECS: u64 = 2 * 24 * 3600;

#[derive(Parser, Debug)]
#[command(
    name = "eprint",
    version,
    about = "Search the IACR Cryptology ePrint Archive from the command line",
    long_about = "Search the IACR Cryptology ePrint Archive.\n\n\
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
    /// Search titles, authors, abstracts and categories
    Search(SearchArgs),
    /// Interactive full-screen browser
    Browse(BrowseArgs),
    /// Refresh the local index from the ePrint OAI-PMH feed
    Update {
        /// Re-harvest the entire archive instead of only what changed
        #[arg(long)]
        full: bool,
        /// Suppress progress output
        #[arg(long)]
        quiet: bool,
    },
    /// Show one paper in full
    Show {
        /// Paper id, e.g. 2026/1539
        id: String,
    },
    /// Open a paper in your browser
    Open {
        /// Paper id, e.g. 2026/1539
        id: String,
        /// Go straight to the PDF instead of the abstract page
        #[arg(long)]
        pdf: bool,
    },
    /// Show papers that arrived since you last looked
    New {
        /// Maximum results to show
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Show without moving the "last seen" marker
        #[arg(long)]
        peek: bool,
        /// Override the starting point (YYYY-MM-DD, or e.g. 7d)
        #[arg(long)]
        since: Option<String>,
    },
    /// Look up BibTeX citation keys (CryptoBib)
    Bib {
        /// Paper id, e.g. 2015/123. Omit to show database status
        id: Option<String>,
        /// Download or refresh the CryptoBib database
        #[arg(long)]
        update: bool,
        /// Re-download even if unchanged
        #[arg(long, requires = "update")]
        force: bool,
        /// Print the full BibTeX record instead of just the key
        #[arg(long, conflicts_with = "update")]
        entry: bool,
    },
    /// Print the full BibTeX record (same as `bib <id> --entry`)
    #[command(name = "Bib")]
    BibEntry {
        /// Paper id, e.g. 2018/116
        id: String,
    },
    /// Show index statistics
    Status,
    /// Show or create the configuration file
    Config {
        /// Write a commented default config file if none exists
        #[arg(long)]
        init: bool,
    },
}

#[derive(Args, Debug, Default)]
struct BrowseArgs {
    /// Starting query. Edit it live inside the browser with `/`
    query: Vec<String>,
    /// Maximum results to load
    #[arg(short = 'n', long, default_value_t = 500)]
    limit: usize,
    /// Match titles and authors only, ignoring abstracts
    #[arg(short = 't', long)]
    title: bool,
    /// Search scope: all (title, authors, abstract) or title
    #[arg(long, value_name = "all|title", conflicts_with = "title", hide = true)]
    scope: Option<String>,
    /// Colour palette: auto, dark, light or mono
    #[arg(long, value_name = "auto|dark|light|mono", hide = true)]
    theme: Option<String>,
    /// Restrict to a publication year
    #[arg(long)]
    year: Option<i64>,
    /// Only papers on or after this date (YYYY-MM-DD, or e.g. 30d)
    #[arg(long)]
    since: Option<String>,
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
    #[arg(short = 'n', long)]
    limit: Option<usize>,
    /// Restrict to a publication year
    #[arg(long)]
    year: Option<i64>,
    /// Only papers on or after this date (YYYY-MM-DD, or e.g. 30d)
    #[arg(long)]
    since: Option<String>,
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

// ---------- minimal civil-time helpers (no chrono dependency) ----------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let mut secs = days_from_civil(y, m, d) * 86400;
    if b.len() >= 19 {
        secs += num(11, 13)? * 3600 + num(14, 16)? * 60 + num(17, 19)?;
    }
    Some(secs)
}

fn format_iso(epoch: i64) -> String {
    let days = epoch.div_euclid(86400);
    let rem = epoch.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn human_age(secs: i64) -> String {
    if secs < 90 {
        "just now".to_string()
    } else if secs < 5400 {
        format!("{}m old", secs / 60)
    } else if secs < 172800 {
        format!("{}h old", secs / 3600)
    } else {
        format!("{}d old", secs / 86400)
    }
}

/// Accepts `YYYY-MM-DD` or a relative window like `30d` / `6m` / `2y`.
fn resolve_since(s: &str) -> Result<String> {
    let t = s.trim();
    if t.len() >= 8 && t.contains('-') {
        return Ok(t.to_string());
    }
    let (num, unit) = t.split_at(t.len().saturating_sub(1));
    let n: i64 = num
        .parse()
        .with_context(|| format!("could not read --since {s}"))?;
    let days = match unit {
        "d" => n,
        "w" => n * 7,
        "m" => n * 30,
        "y" => n * 365,
        _ => bail!("--since unit must be d, w, m or y (got {unit:?})"),
    };
    Ok(format_iso(now() - days * 86400)
        .chars()
        .take(10)
        .collect())
}

// ---------- index freshness ----------

fn index_age(conn: &rusqlite::Connection) -> Result<Option<i64>> {
    Ok(db::meta_get(conn, harvest::KEY_LAST_HARVEST)?
        .and_then(|v| parse_iso(&v))
        .map(|t| (now() - t).max(0)))
}

/// Kick off `eprint update --quiet` as a detached child and return
/// immediately, so a stale index never delays a search.
fn spawn_background_refresh(conn: &rusqlite::Connection) -> Result<()> {
    let last_attempt = db::meta_get(conn, harvest::KEY_LAST_ATTEMPT)?
        .and_then(|v| parse_iso(&v))
        .unwrap_or(0);
    if now() - last_attempt < ATTEMPT_COOLDOWN_SECS as i64 {
        return Ok(());
    }
    db::meta_set(conn, harvest::KEY_LAST_ATTEMPT, &format_iso(now()))?;

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
            parse_iso(&v).map(|t| format_iso(t - OVERLAP_SECS as i64))
        })
    };
    db::meta_set(&conn, harvest::KEY_LAST_ATTEMPT, &format_iso(now()))?;
    if !quiet {
        match &from {
            Some(f) => eprintln!("Updating index (changes since {})…", &f[..10]),
            None => eprintln!("Harvesting the full archive — this takes a couple of minutes…"),
        }
    }
    let n = harvest::run(&mut conn, from.as_deref(), quiet, &format_iso(now()))?;
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

fn normalise_id(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("https://eprint.iacr.org/")
        .trim_start_matches("http://eprint.iacr.org/")
        .trim_end_matches(".pdf")
        .to_string()
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
    let since = match &a.since {
        Some(s) => Some(resolve_since(s)?),
        None => None,
    };
    let scope = effective_scope(a.title, a.scope.as_deref(), cfg);

    let q = Query {
        terms: &terms,
        year: a.year,
        since,
        author: a.author.clone(),
        category: a.category.clone(),
        limit: a.limit.unwrap_or(cfg.limit),
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
    let age = index_age(&conn)?.map(human_age);
    let mut out = String::new();
    render::render_header(&mut out, hits.len(), total, age, scope.label(), st);
    let ids: Vec<String> = hits.iter().map(|h| h.paper.id.clone()).collect();
    let bibs = db::bib_map(&conn, &ids).unwrap_or_default();
    for hit in &hits {
        render::render_hit(&mut out, hit, st, a.abstracts, bibs.get(&hit.paper.id));
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
    let since = match &a.since {
        Some(s) => Some(resolve_since(s)?),
        None => None,
    };
    let cfg = config::load();
    let filters = tui::Filters {
        year: a.year,
        since,
        author: a.author.clone(),
        category: a.category.clone(),
        limit: a.limit,
        prefix: !a.exact,
    };
    let theme = Theme::resolve(
        a.theme.as_deref().unwrap_or(&cfg.theme),
        std::env::var_os("NO_COLOR").is_none(),
    );
    let scope = effective_scope(a.title, a.scope.as_deref(), &cfg);
    let stale = bib_stale_days(&conn)?;
    tui::run(conn, a.query.join(" "), filters, theme, scope, stale)
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
    let Some(t) = parse_iso(&ts) else {
        return Ok(None);
    };
    let days = (now() - t).max(0) / 86400;
    Ok(if days >= BIB_STALE_DAYS {
        Some(days)
    } else {
        None
    })
}

fn do_new(limit: usize, peek: bool, since: Option<&str>) -> Result<()> {
    let cfg = config::load();
    let conn = db::open()?;
    if db::count(&conn)? == 0 {
        drop(conn);
        eprintln!("No local index yet — building it now (one time only).");
        do_update(true, false)?;
        return do_new(limit, peek, since);
    }
    // Refresh first: "what's new" is the one command where stale data is the
    // whole failure mode, so this blocks rather than backgrounding.
    if index_age(&conn)?.map(|s| s > 3600).unwrap_or(true) {
        drop(conn);
        do_update(false, false)?;
        return do_new_inner(limit, peek, since, &cfg);
    }
    drop(conn);
    do_new_inner(limit, peek, since, &cfg)
}

fn do_new_inner(
    limit: usize,
    peek: bool,
    since: Option<&str>,
    cfg: &config::Config,
) -> Result<()> {
    let conn = db::open()?;
    let st = Style::detect(StyleOpts {
        plain: false,
        force_color: false,
        urls: None,
        theme: cfg.theme.clone(),
    });

    let watermark = match since {
        Some(s) => resolve_since(s)?,
        None => db::meta_get(&conn, harvest::KEY_LAST_SEEN)?
            .unwrap_or_else(|| format_iso(now() - 7 * 86400)),
    };

    let hits = db::added_since(&conn, &watermark, limit)?;
    let total = db::count_added_since(&conn, &watermark)?;

    if hits.is_empty() {
        println!(
            "\nNothing new since {}.\n",
            &watermark.chars().take(10).collect::<String>()
        );
    } else {
        let mut out = String::new();
        render::render_header(
            &mut out,
            hits.len(),
            total,
            None,
            &format!("since {}", &watermark.chars().take(10).collect::<String>()),
            &st,
        );
        for hit in &hits {
            render::render_hit(&mut out, hit, &st, false, None);
        }
        page(&out, &st, false)?;
    }

    if !peek {
        db::meta_set(&conn, harvest::KEY_LAST_SEEN, &format_iso(now()))?;
    }
    Ok(())
}

fn do_bib(id: Option<&str>, update: bool, force: bool, want_entry: bool) -> Result<()> {
    let mut conn = db::open()?;

    if update {
        match bib::update(&mut conn, force, false, &format_iso(now()))? {
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

fn do_config(init: bool) -> Result<()> {
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
        Some(p) => println!("  config file    {}  (not created; run `eprint config --init`)", p.display()),
        None => println!("  config file    <could not determine a location>"),
    }
    println!("  theme          {}", cfg.theme);
    println!("  scope          {}", cfg.scope);
    println!("  limit          {}", cfg.limit);
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
    });
    let total = db::count(&conn)?;
    let path = db::db_path()?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let age = index_age(&conn)?
        .map(human_age)
        .unwrap_or_else(|| "never updated".to_string());
    let last = db::meta_get(&conn, harvest::KEY_LAST_HARVEST)?.unwrap_or_else(|| "—".into());
    let newest: String = conn
        .query_row(
            "SELECT id || '  ' || substr(date,1,10) FROM papers ORDER BY date DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "—".into());
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
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => do_search(&cli.search),
        Some(Cmd::Search(a)) => do_search(&a),
        Some(Cmd::Browse(a)) => do_browse(&a),
        Some(Cmd::Update { full, quiet }) => do_update(full, quiet),
        Some(Cmd::New { limit, peek, since }) => do_new(limit, peek, since.as_deref()),
        Some(Cmd::Bib {
            id,
            update,
            force,
            entry,
        }) => do_bib(id.as_deref(), update, force, entry),
        Some(Cmd::BibEntry { id }) => do_bib(Some(&id), false, false, true),
        Some(Cmd::Status) => do_status(),
        Some(Cmd::Config { init }) => do_config(init),
        Some(Cmd::Show { id }) => {
            let conn = db::open()?;
            let st = Style::detect(StyleOpts {
                plain: false,
                force_color: false,
                urls: None,
                theme: config::load().theme,
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
        Some(Cmd::Open { id, pdf }) => {
            let id = normalise_id(&id);
            let url = if pdf {
                format!("https://eprint.iacr.org/{id}.pdf")
            } else {
                format!("https://eprint.iacr.org/{id}")
            };
            open_url(&url)
        }
    }
}
