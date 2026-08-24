//! The files `eprint` writes on the user's behalf so the desktop can reach it: a
//! periodic updater, and a launcher you can find by typing the name.
//!
//! Same shape as `completions.rs` — every file is built by a pure function so the
//! escaping is testable, and install/remove/probe come in pairs. Nothing here is a
//! daemon: the OS already has one, and launchd and systemd both survive sleep,
//! reboots and crashes in ways a hand-rolled loop with no pid file would not.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// launchd needs a stable label and the app bundle a stable identifier, because
/// removal checks them: without that, `--launcher off` would be an `rm -rf` of
/// whatever happens to sit at that path. `local.` is the conventional prefix for a
/// bundle that belongs to no registered domain.
const SCHED_LABEL: &str = "local.eprint.update";
const LAUNCHER_ID: &str = "local.eprint.launcher";

/// Thirty minutes. ePrint posts in bursts once or twice a day, so this is about
/// noticing a burst within the hour rather than about polling hard.
pub const INTERVAL_SECS: u64 = 30 * 60;

/// Not on `PATH`, and it has lived at this path for a decade of macOS releases.
/// Best-effort: if it ever moves, the bundle is still indexed by Spotlight's own
/// sweep, just not immediately.
/// The launcher's icon: a lattice with one lit node, in the same brass and verdigris
/// as `theme.rs`, with the gold of the watch badge. Embedded rather than drawn,
/// because drawing it needs AppKit and this binary needs neither Swift nor a graphics
/// stack at build or run time — `assets/icon.swift` generates it once, by hand, and
/// the result is committed. Stops at 512 real pixels; see that file for why.
const ICON: &[u8] = include_bytes!("../assets/eprint.icns");

const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/\
                          LaunchServices.framework/Support/lsregister";

// ---------------------------------------------------------------------------
// Where things go
// ---------------------------------------------------------------------------

fn home() -> Result<PathBuf> {
    dirs::home_dir().context("could not determine a home directory")
}

fn plist_path() -> Result<PathBuf> {
    Ok(home()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{SCHED_LABEL}.plist")))
}

fn log_path() -> Result<PathBuf> {
    Ok(home()?
        .join("Library")
        .join("Logs")
        .join("eprint-update.log"))
}

/// `$XDG_CONFIG_HOME` then `~/.config`, spelled out rather than taken from
/// `dirs::config_dir()`, which on macOS answers `~/Library/Application Support` —
/// right for a config file, wrong for a systemd unit.
fn systemd_dir() -> Result<PathBuf> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => home()?.join(".config"),
    };
    Ok(base.join("systemd").join("user"))
}

fn app_path() -> Result<PathBuf> {
    Ok(home()?.join("Applications").join("eprint.app"))
}

fn desktop_path() -> Result<PathBuf> {
    let base = match std::env::var("XDG_DATA_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => home()?.join(".local").join("share"),
    };
    Ok(base.join("applications").join("eprint.desktop"))
}

fn exe() -> Result<PathBuf> {
    std::env::current_exe().context("locating own executable")
}

// ---------------------------------------------------------------------------
// Quoting. Three grammars, none of them forgiving.
// ---------------------------------------------------------------------------

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Wrap in single quotes for `sh`. A single quote inside cannot be escaped, only
/// interrupted, which is what `'\''` does.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `Exec=` is parsed shell-like by every launcher that reads these files, but `%` is
/// the desktop-entry spec's own escape character and has to be doubled or the path
/// silently loses characters.
fn desktop_quote(s: &str) -> String {
    sh_quote(&s.replace('%', "%%"))
}

// ---------------------------------------------------------------------------
// Templates — pure, so the quoting above is unit-testable
// ---------------------------------------------------------------------------

/// `ProgramArguments` is an array of separate strings, so the shell never sees this
/// and only XML escaping applies. `env` carries `EPRINT_NOTIFY` plus whatever
/// `EPRINT_*` overrides were in force at install time.
fn plist(exe: &Path, log: &Path, interval: u64, env: &[(String, String)]) -> String {
    let mut vars = String::new();
    for (k, v) in env {
        vars.push_str(&format!(
            "    <key>{}</key><string>{}</string>\n",
            xml_escape(k),
            xml_escape(v)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>update</string>
    <string>--quiet</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
{vars}  </dict>
  <!-- launchd runs a missed interval on wake, which is the whole reason this is a
       LaunchAgent and not a loop: close the lid for two hours and it catches up. -->
  <key>StartInterval</key><integer>{interval}</integer>
  <!-- False on purpose. `config --notify` already posts a banner to prove the thing
       works, and one at every login would be a surprise rather than news. -->
  <key>RunAtLoad</key><false/>
  <key>ProcessType</key><string>Background</string>
  <key>LowPriorityIO</key><true/>
  <key>Nice</key><integer>5</integer>
  <!-- `--quiet` prints nothing, so this file stays empty unless something actually
       broke — which makes "why has nothing arrived" an answerable question. -->
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        label = SCHED_LABEL,
        exe = xml_escape(&exe.display().to_string()),
        log = xml_escape(&log.display().to_string()),
    )
}

/// systemd splits `ExecStart` on whitespace, so a path with a space in it needs the
/// double quotes it understands.
fn systemd_service(exe: &Path, env: &[(String, String)]) -> String {
    let mut vars = String::new();
    for (k, v) in env {
        vars.push_str(&format!("Environment=\"{k}={v}\"\n"));
    }
    format!(
        r#"[Unit]
Description=Refresh the local ePrint index

[Service]
Type=oneshot
{vars}ExecStart="{exe}" update --quiet
"#,
        exe = exe.display()
    )
}

fn systemd_timer(interval: u64) -> String {
    format!(
        r#"[Unit]
Description=Refresh the local ePrint index every {mins} minutes

[Timer]
OnBootSec=2min
OnUnitActiveSec={interval}s
# Catches up after the machine was off, the systemd equivalent of launchd running a
# missed StartInterval on wake.
Persistent=true

[Install]
WantedBy=timers.target
"#,
        mins = interval / 60
    )
}

fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>eprint</string>
  <key>CFBundleDisplayName</key><string>eprint</string>
  <key>CFBundleIdentifier</key><string>{LAUNCHER_ID}</string>
  <key>CFBundleExecutable</key><string>eprint-browse</string>
  <!-- Names Resources/eprint.icns, extension omitted as the format expects. -->
  <key>CFBundleIconFile</key><string>eprint</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <!-- Deliberately NOT LSUIElement. It was set here at first, to stop a Dock icon
       bouncing for an app that opens a terminal window and exits, and it broke the
       one thing the bundle exists for: LaunchServices then flags it `ui-element`,
       i.e. a background agent, and Spotlight will not offer an agent as something to
       launch. With two folders also called `eprint` on this machine, one of those won
       the top slot instead and Spotlight switched to its folder behaviour — "press
       Tab to search", which searches inside the folder and never launches anything.
       The bundle stays indexed either way; being indexed and being launchable are
       different things, and that is the distinction this key gets wrong. A Dock icon
       flashing for a moment is the cheaper cost by far. -->
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
"#,
        version = env!("CARGO_PKG_VERSION")
    )
}

/// The bundle's executable: ask a terminal to run one command line, then get out of
/// the way.
///
/// `update` runs *without* `--quiet`, so its progress line lands in the window that
/// just opened, and `browse` takes the window over after it.
///
/// **No `exec`, and nothing that closes the window.** `browse` runs as a child of the
/// window's shell, so quitting it leaves a working prompt — which is the requirement.
/// What becomes of the window after that is Terminal's own profile setting, and it is
/// the user's to make, not ours.
///
/// Auto-closing the window was tried at length and every route is worse than leaving
/// it alone; `CLAUDE.md` records them so nobody repeats the exercise.
///
/// The command reaches AppleScript through `argv`, never spliced into script source —
/// the same rule as `notify::argv`, and for the same reason. The `--` is load-bearing:
/// without it `osascript` reads a command beginning with `-` as its own option.
fn launcher_script(exe: &Path, terminal_command: Option<&str>) -> String {
    let quoted = sh_quote(&exe.display().to_string());
    // Two shells, so two rounds of quoting. `$exe` is expanded here; the inner quotes
    // are literal so that the shell Terminal starts also sees a quoted path. Without
    // them a home directory containing a space breaks the command inside the new
    // window, where nobody would think to look for it.
    let build = r#"cmd="\"$exe\" update; \"$exe\" browse""#;
    let run = match terminal_command {
        Some(t) => format!("{}\n", t.replace("{cmd}", "\"$cmd\"")),
        None => r#"osascript \
  -e 'on run argv' \
  -e 'tell application "Terminal" to do script (item 1 of argv)' \
  -e 'tell application "Terminal" to activate' \
  -e 'end run' \
  -- "$cmd"
"#
        .to_string(),
    };
    format!(
        "#!/bin/sh\n\
         # Written by `eprint config --launcher on`. Delete the bundle, or run\n\
         # `eprint config --launcher off`, to remove it.\n\
         exe={quoted}\n\
         {build}\n\
         {run}"
    )
}

fn desktop_entry(exe: &Path) -> String {
    let q = desktop_quote(&exe.display().to_string());
    format!(
        r#"[Desktop Entry]
Type=Application
Version=1.0
Name=eprint
GenericName=ePrint archive browser
Comment=Search and browse the IACR Cryptology ePrint Archive
# Terminal=true hands this to the user's own terminal, so there is no list of
# terminal emulators to keep up to date here. The trailing `exec $SHELL` is what
# leaves a usable prompt after `browse` exits, the same as the macOS launcher:
# without it the emulator closes the window the moment the command finishes.
# The path arrives as $0 rather than inside the command, because the command is
# already single-quoted and a single-quoted path within it does not parse.
Exec=sh -c '"$0" update; "$0" browse; exec "${{SHELL:-/bin/sh}}"' {q}
Terminal=true
Categories=Science;Education;
Keywords=eprint;iacr;crypto;cryptography;papers;preprint;
"#
    )
}

// ---------------------------------------------------------------------------
// The environment a scheduled harvest should run in
// ---------------------------------------------------------------------------

/// `EPRINT_NOTIFY`, plus any `EPRINT_*` override that was in force at install time.
///
/// Carrying those forward is what keeps the scheduler honest: install while pointed
/// at a scratch index with `EPRINT_DB` and the agent uses the scratch index, exactly
/// as every other command would. Without it, `--notify` under a test harness would
/// quietly schedule a job against the real database.
fn scheduled_env() -> Vec<(String, String)> {
    let mut env = vec![(crate::NOTIFY_VAR.to_string(), "1".to_string())];
    for key in ["EPRINT_DB", "EPRINT_CONFIG", "EPRINT_PAPERS_DIR"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                env.push((key.to_string(), v));
            }
        }
    }
    env
}

fn write_bytes(path: &Path, body: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

fn write_file(path: &Path, body: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

// ---------------------------------------------------------------------------
// The periodic updater
// ---------------------------------------------------------------------------

pub fn scheduler_installed() -> bool {
    if cfg!(target_os = "macos") {
        plist_path().map(|p| p.exists()).unwrap_or(false)
    } else {
        systemd_dir()
            .map(|d| d.join("eprint-update.timer").exists())
            .unwrap_or(false)
    }
}

/// `every 30 min`, for the one line `status` and `config` print about it.
pub fn interval_label(secs: u64) -> String {
    if secs % 3600 == 0 {
        let h = secs / 3600;
        return format!("every {h} hour{}", if h == 1 { "" } else { "s" });
    }
    format!("every {} min", secs / 60)
}

pub fn install_scheduler(interval: u64) -> Result<String> {
    let exe = exe()?;
    let env = scheduled_env();
    if cfg!(target_os = "macos") {
        let path = plist_path()?;
        let log = log_path()?;
        if let Some(dir) = log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        write_file(&path, &plist(&exe, &log, interval, &env))?;
        load_launch_agent(&path)?;
        Ok(format!("{} ({})", path.display(), interval_label(interval)))
    } else {
        let dir = systemd_dir()?;
        write_file(
            &dir.join("eprint-update.service"),
            &systemd_service(&exe, &env),
        )?;
        write_file(&dir.join("eprint-update.timer"), &systemd_timer(interval))?;
        enable_systemd_timer()?;
        Ok(format!(
            "{} ({})",
            dir.join("eprint-update.timer").display(),
            interval_label(interval)
        ))
    }
}

pub fn remove_scheduler() -> Result<String> {
    if cfg!(target_os = "macos") {
        let path = plist_path()?;
        if !path.exists() {
            return Ok("no scheduled updater was installed".to_string());
        }
        // Unloaded before the file goes, or launchd keeps running a job whose
        // definition no longer exists.
        if let Some(uid) = uid() {
            let _ = run(
                "launchctl",
                &["bootout".into(), format!("gui/{uid}/{SCHED_LABEL}")],
            );
        }
        let _ = run("launchctl", &["unload".into(), path.display().to_string()]);
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        Ok(path.display().to_string())
    } else {
        let dir = systemd_dir()?;
        let timer = dir.join("eprint-update.timer");
        if !timer.exists() {
            return Ok("no scheduled updater was installed".to_string());
        }
        let _ = run(
            "systemctl",
            &[
                "--user".into(),
                "disable".into(),
                "--now".into(),
                "eprint-update.timer".into(),
            ],
        );
        let _ = std::fs::remove_file(&timer);
        let _ = std::fs::remove_file(dir.join("eprint-update.service"));
        let _ = run("systemctl", &["--user".into(), "daemon-reload".into()]);
        Ok(timer.display().to_string())
    }
}

/// launchd's own uid, via `id -u`. Reading it directly would mean a `libc`
/// dependency, which this crate has already declined once for three lines
/// (`quiet_broken_pipe`).
fn uid() -> Option<String> {
    let out = std::process::Command::new("id").arg("-u").output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `bootstrap` is the documented modern spelling and `load -w` the one that still
/// works everywhere. Trying both costs one failed spawn and means neither a new nor
/// an old macOS is the one that breaks.
fn load_launch_agent(path: &Path) -> Result<()> {
    let p = path.display().to_string();
    if let Some(uid) = uid() {
        // Any previous copy has to go first; bootstrap refuses a label already loaded.
        let _ = run(
            "launchctl",
            &["bootout".into(), format!("gui/{uid}/{SCHED_LABEL}")],
        );
        if run(
            "launchctl",
            &["bootstrap".into(), format!("gui/{uid}"), p.clone()],
        )
        .is_ok()
        {
            return Ok(());
        }
    }
    run("launchctl", &["load".into(), "-w".into(), p]).context("launchctl would not load the agent")
}

fn enable_systemd_timer() -> Result<()> {
    // A machine with no user systemd — a container, a non-systemd distribution — gets
    // told what to add instead of having its crontab edited behind its back. Same
    // choice `install_completions` makes for a shell it has no function for.
    if run("systemctl", &["--user".into(), "daemon-reload".into()]).is_err() {
        // `format!` inline captures take a bare identifier, not a path.
        let marker = crate::NOTIFY_VAR;
        bail!(
            "no user systemd here, so the timer cannot be started.\n  \
             The unit files are written; add this to your crontab instead:\n    \
             */{} * * * * {marker}=1 {} update --quiet",
            INTERVAL_SECS / 60,
            exe()?.display()
        );
    }
    run(
        "systemctl",
        &[
            "--user".into(),
            "enable".into(),
            "--now".into(),
            "eprint-update.timer".into(),
        ],
    )
    .context("systemctl could not enable the timer")
}

fn run(prog: &str, args: &[String]) -> Result<()> {
    let status = std::process::Command::new(prog)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("running {prog}"))?;
    if !status.success() {
        bail!("{prog} {} failed", args.join(" "));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The launcher
// ---------------------------------------------------------------------------

pub fn launcher_installed() -> bool {
    let path = if cfg!(target_os = "macos") {
        app_path()
    } else {
        desktop_path()
    };
    path.map(|p| p.exists()).unwrap_or(false)
}

pub fn install_launcher(terminal_command: Option<&str>) -> Result<String> {
    let exe = exe()?;
    if cfg!(target_os = "macos") {
        let app = app_path()?;
        let macos = app.join("Contents").join("MacOS");
        write_file(&app.join("Contents").join("Info.plist"), &info_plist())?;
        write_bytes(
            &app.join("Contents").join("Resources").join("eprint.icns"),
            ICON,
        )?;
        let script = macos.join("eprint-browse");
        write_file(&script, &launcher_script(&exe, terminal_command))?;
        make_executable(&script)?;
        // Two nudges, because a bundle nobody can find is the whole feature failing
        // quietly. `lsregister` tells LaunchServices the bundle exists at all;
        // `mdimport` asks Spotlight to index it rather than waiting for its own
        // sweep. Both are best-effort — indexing is asynchronous either way, which
        // is why the caller says it may take a moment.
        let _ = run(LSREGISTER, &["-f".into(), app.display().to_string()]);
        let _ = run("/usr/bin/mdimport", &[app.display().to_string()]);
        Ok(app.display().to_string())
    } else {
        let path = desktop_path()?;
        write_file(&path, &desktop_entry(&exe))?;
        if let Some(dir) = path.parent() {
            let _ = run("update-desktop-database", &[dir.display().to_string()]);
        }
        Ok(path.display().to_string())
    }
}

pub fn remove_launcher() -> Result<String> {
    if cfg!(target_os = "macos") {
        let app = app_path()?;
        if !app.exists() {
            return Ok("no launcher was installed".to_string());
        }
        // Checked, not assumed. `cached()` can only ever return paths inside the
        // library and `pdf::remove` still re-checks that; a recursive delete of a
        // path built from `$HOME` deserves at least as much.
        let info =
            std::fs::read_to_string(app.join("Contents").join("Info.plist")).unwrap_or_default();
        if !info.contains(LAUNCHER_ID) {
            bail!(
                "{} was not created by eprint — remove it by hand",
                app.display()
            );
        }
        std::fs::remove_dir_all(&app).with_context(|| format!("removing {}", app.display()))?;
        Ok(app.display().to_string())
    } else {
        let path = desktop_path()?;
        if !path.exists() {
            return Ok("no launcher was installed".to_string());
        }
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        if !body.contains("Name=eprint") {
            bail!(
                "{} was not created by eprint — remove it by hand",
                path.display()
            );
        }
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        Ok(path.display().to_string())
    }
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path that would break each grammar in turn.
    fn awkward() -> PathBuf {
        PathBuf::from("/Users/o'brien/My Tools & Bits/<eprint>/100% eprint")
    }

    #[test]
    fn plist_escapes_xml_metacharacters() {
        let out = plist(
            &awkward(),
            Path::new("/tmp/log"),
            1800,
            &[("EPRINT_NOTIFY".into(), "1".into())],
        );
        assert!(
            out.contains("Bits &amp; Bits") || out.contains("Tools &amp; Bits"),
            "{out}"
        );
        assert!(out.contains("&lt;eprint&gt;"));
        // A raw `&` or `<` anywhere in a value makes the file unparseable, and
        // launchd's only report is that the job never runs.
        for line in out.lines().filter(|l| l.contains("<string>")) {
            let inner = line
                .split("<string>")
                .nth(1)
                .unwrap_or("")
                .split("</string>")
                .next()
                .unwrap_or("");
            assert!(!inner.contains('&') || inner.contains("&amp;"), "{line}");
        }
        assert!(out.contains("<integer>1800</integer>"));
    }

    #[test]
    fn the_launcher_leaves_a_working_shell() {
        let out = launcher_script(&awkward(), None);
        assert!(out.starts_with("#!/bin/sh\n"), "{out}");
        // `exec` would replace the window's shell, so quitting `browse` would leave a
        // dead `[Process completed]` window instead of a prompt. That was the bug.
        assert!(!out.contains("exec "), "{out}");
        // Nothing here closes the window, and nothing waits around to. Every attempt
        // at that misfired; see CLAUDE.md.
        assert!(!out.contains("close "), "{out}");
        assert!(!out.contains("shellExitAction"), "{out}");
        assert!(!out.contains("busy of"), "{out}");
        // The command is data in `argv`, never part of the AppleScript source.
        assert!(out.contains("do script (item 1 of argv)"), "{out}");
        assert!(out.contains(r#"-- "$cmd""#), "{out}");
        for line in out.lines().filter(|l| l.contains("do script")) {
            assert!(
                !line.contains("brien"),
                "path leaked into script source: {line}"
            );
        }
        // The apostrophe must interrupt the single-quoting, not end it.
        assert!(out.contains(r"/Users/o'\''brien/"), "{out}");
    }

    #[test]
    fn launcher_script_honours_a_custom_terminal() {
        let out = launcher_script(Path::new("/bin/eprint"), Some("ghostty -e {cmd}"));
        assert!(out.contains(r#"ghostty -e "$cmd""#), "{out}");
        assert!(!out.contains("osascript"));
    }

    #[test]
    fn desktop_entry_doubles_percent_and_quotes_spaces() {
        let out = desktop_entry(&awkward());
        let exec = out
            .lines()
            .find(|l| l.starts_with("Exec="))
            .expect("needs an Exec line");
        // `%` is the desktop-entry escape character; a bare one eats what follows.
        assert!(exec.contains("100%% eprint"), "{exec}");
        assert!(exec.contains(r"o'\''brien"), "{exec}");
        assert!(out.contains("Terminal=true"));
        // A prompt has to survive `browse`, the same as on macOS — otherwise the
        // emulator closes the window the instant the command finishes.
        // With a fallback: `$SHELL` is not guaranteed to be set in a desktop
        // session, and `exec ""` would just fail.
        assert!(exec.contains(r#"exec "${SHELL:-/bin/sh}""#), "{exec}");
        // The path is a positional parameter, not part of the command string. It was
        // interpolated into it once, and a single-quoted path inside an already
        // single-quoted `sh -c` argument does not parse at all — which would have
        // broken every Linux launcher whose path contains a space.
        assert!(exec.contains(r#"'"$0" update; "$0" browse"#), "{exec}");
        let cmd = exec.split('\'').nth(1).expect("a quoted command string");
        assert!(
            !cmd.contains("brien"),
            "path leaked into the command: {cmd}"
        );
        // The path is the last field, quoted on its own.
        assert!(
            exec.ends_with('\''),
            "path must be the final quoted field: {exec}"
        );
        // One line, or the whole entry is malformed.
        assert_eq!(out.lines().filter(|l| l.starts_with("Exec=")).count(), 1);
    }

    #[test]
    fn systemd_quotes_a_path_with_spaces() {
        let out = systemd_service(&awkward(), &[("EPRINT_NOTIFY".into(), "1".into())]);
        assert!(
            out.contains(r#"ExecStart="/Users/o'brien/My Tools"#),
            "{out}"
        );
        assert!(out.contains(r#"Environment="EPRINT_NOTIFY=1""#));
        let timer = systemd_timer(1800);
        assert!(timer.contains("OnUnitActiveSec=1800s"));
        assert!(timer.contains("every 30 minutes"));
        assert!(timer.contains("Persistent=true"));
    }

    #[test]
    fn interval_reads_as_english() {
        assert_eq!(interval_label(1800), "every 30 min");
        assert_eq!(interval_label(3600), "every 1 hour");
        assert_eq!(interval_label(7200), "every 2 hours");
    }

    #[test]
    fn the_bundle_is_launchable_not_an_agent() {
        let out = info_plist();
        // `LSUIElement` makes LaunchServices flag the bundle `ui-element`, and
        // Spotlight will not offer a background agent as something to launch — it
        // fell behind two same-named folders and Tab-to-search took over. Being
        // launchable is the whole point of the bundle.
        assert!(!out.contains("<key>LSUIElement</key>"), "{out}");
        assert!(!out.contains("<key>LSBackgroundOnly</key>"), "{out}");
        // Both version keys: LaunchServices synthesises a version from whatever it
        // finds, and the reference bundles all carry CFBundleVersion.
        assert!(out.contains("<key>CFBundleVersion</key>"), "{out}");
        assert!(
            out.contains("<key>CFBundleShortVersionString</key>"),
            "{out}"
        );
    }

    #[test]
    fn the_bundle_declares_the_icon_it_ships() {
        // `CFBundleIconFile` names the file without its extension, so the two halves
        // can drift apart silently and the bundle just shows a blank page icon.
        assert!(info_plist().contains("<key>CFBundleIconFile</key><string>eprint</string>"));
        // A real icns, not an empty or truncated include.
        assert!(ICON.len() > 10_000, "icon is {} bytes", ICON.len());
        assert_eq!(&ICON[..4], b"icns", "not an icns file");
    }

    #[test]
    fn the_bundle_carries_the_identifier_removal_checks_for() {
        // `remove_launcher` refuses to delete a bundle whose Info.plist lacks this,
        // so the two must not drift apart.
        assert!(info_plist().contains(LAUNCHER_ID));
    }
}
