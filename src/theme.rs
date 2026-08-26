use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Dark,
    Light,
    Mono,
}

/// Semantic slots rather than colour names, so the two palettes stay in step
/// and the inline renderer and the TUI cannot drift apart.
#[derive(Clone, Copy, PartialEq)]
/// Semantic slots, not colours. The `dark`/`light` palettes are 256-colour
/// indices rather than the 16 ANSI ones: "brass and verdigris" needs *muted*
/// mid-tones, and the 16-colour palette offers no muted anything — only "cyan"
/// and "blue", at whatever brightness the terminal theme happens to pick. The
/// cost is that these no longer follow the user's theme; `mono` still does.
pub enum Tone {
    Id,
    Title,
    Meta,
    Body,
    Url,
    Match,
    Marker,
    Head,
    /// The one author named by `favourite_author`. Dusty rose: warm enough to
    /// read as affection, muted enough to sit beside the brass and verdigris.
    Love,
    /// A paper matching a saved watch. Dark gold (256-colour 136): the one
    /// deliberate exception to the 16-colour rule, because "a bit darker than
    /// the match highlight" is not expressible as a palette index — a theme's
    /// yellow could be anything. Chosen to stay legible on white as well as
    /// black, which the 16-colour brights do not.
    Watch,
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub mode: Mode,
    pub color: bool,
}

/// Best-effort background detection, cheapest signal first.
///
/// `COLORFGBG` is free but set by only a handful of terminals (rxvt, Konsole);
/// Terminal.app, iTerm2 and GNOME Terminal all leave it unset, which is how a
/// light-background user ended up with the dark palette. So when it is missing,
/// ask the terminal directly — see `query_background`.
fn detect_mode() -> Mode {
    if let Ok(v) = std::env::var("COLORFGBG") {
        if let Some(bg) = v.rsplit(';').next() {
            if let Ok(n) = bg.trim().parse::<u8>() {
                return if (7..=15).contains(&n) {
                    Mode::Light
                } else {
                    Mode::Dark
                };
            }
        }
    }
    // Cached for the life of the process: `Theme::resolve` is called more than
    // once per command and the terminal's answer cannot change underneath it.
    static PROBED: std::sync::OnceLock<Option<Mode>> = std::sync::OnceLock::new();
    PROBED.get_or_init(query_background).unwrap_or(Mode::Dark)
}

/// Ask the terminal what colour it is painting behind us (OSC 11), and read the
/// `rgb:RRRR/GGGG/BBBB` it answers with.
///
/// Worth doing where OSC 52 was not: writing the clipboard is unimplemented in
/// VTE, but *querying* colours is supported there and in xterm, kitty, foot,
/// WezTerm, Alacritty and Terminal.app — it is how vim, tmux and bat decide the
/// same question. A terminal that stays silent costs one timeout and falls back
/// to the old assumption.
fn query_background() -> Option<Mode> {
    use std::io::{Read, Write};
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return None;
    }
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let reader = tty.try_clone().ok()?;

    // The reply arrives as ordinary input, so the terminal must not be line
    // buffering or echoing it. Restored by the guard however this returns.
    ratatui::crossterm::terminal::enable_raw_mode().ok()?;
    struct Cooked;
    impl Drop for Cooked {
        fn drop(&mut self) {
            let _ = ratatui::crossterm::terminal::disable_raw_mode();
        }
    }
    let _cooked = Cooked;

    tty.write_all(b"\x1b]11;?\x07").ok()?;
    tty.flush().ok()?;

    // Read on a thread so a terminal that never answers costs a timeout rather
    // than a hang. The thread is left to the process to clean up: it is blocked
    // on a read that will never return, and there is nothing to wait for.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        while out.len() < 64 {
            match reader.read(&mut byte) {
                Ok(1) => {
                    out.push(byte[0]);
                    // Terminals answer with whichever terminator was asked for,
                    // but not all of them are consistent about it.
                    if byte[0] == 0x07 || out.ends_with(b"\x1b\\") {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = tx.send(out);
    });

    let reply = rx
        .recv_timeout(std::time::Duration::from_millis(100))
        .ok()?;
    luminance_of(&String::from_utf8_lossy(&reply)).map(|l| {
        if l > 0.5 {
            Mode::Light
        } else {
            Mode::Dark
        }
    })
}

/// Pull `rgb:RRRR/GGGG/BBBB` out of an OSC 11 reply and weigh it into one
/// number. Components are hex of any width — 4 digits in practice, but 2 and 1
/// are legal and some terminals use them.
fn luminance_of(reply: &str) -> Option<f32> {
    let rest = reply.split("rgb:").nth(1)?;
    let mut parts = rest.split('/');
    let mut channel = || -> Option<f32> {
        let raw: String = parts
            .next()?
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if raw.is_empty() {
            return None;
        }
        let value = u32::from_str_radix(&raw, 16).ok()?;
        let full = 16u32.pow(raw.len() as u32) - 1;
        Some(value as f32 / full as f32)
    };
    let (r, g, b) = (channel()?, channel()?, channel()?);
    // Rec. 709 luma: green carries most of what the eye reads as brightness.
    Some(0.2126 * r + 0.7152 * g + 0.0722 * b)
}

impl Theme {
    pub fn resolve(name: &str, color: bool) -> Theme {
        let mode = match name.trim().to_ascii_lowercase().as_str() {
            "dark" => Mode::Dark,
            "light" => Mode::Light,
            "mono" | "none" | "plain" => Mode::Mono,
            _ => detect_mode(),
        };
        let mode = if color { mode } else { Mode::Mono };
        Theme { mode, color }
    }

    /// ANSI escape prefix for the inline renderer.
    pub fn ansi(&self, t: Tone) -> &'static str {
        if !self.color {
            return "";
        }
        match (self.mode, t) {
            // Dark backgrounds: bright variants, never the dim end of the
            // palette (blue 4 and dark-gray 8 are unreadable on black).
            (Mode::Dark, Tone::Id) => "\x1b[38;5;66m",
            (Mode::Dark, Tone::Title) => "\x1b[1m",
            (Mode::Dark, Tone::Meta) => "\x1b[38;5;102m",
            (Mode::Dark, Tone::Body) => "",
            (Mode::Dark, Tone::Url) => "\x1b[4;38;5;103m",
            (Mode::Dark, Tone::Marker) => "\x1b[38;5;66m",
            (Mode::Dark, Tone::Head) => "\x1b[1m",
            (Mode::Dark, Tone::Watch) => "\x1b[1;38;5;136m",
            (Mode::Dark, Tone::Love) => "\x1b[38;5;131m",

            (Mode::Light, Tone::Id) => "\x1b[38;5;30m",
            (Mode::Light, Tone::Title) => "\x1b[1m",
            (Mode::Light, Tone::Meta) => "\x1b[38;5;240m",
            (Mode::Light, Tone::Body) => "",
            (Mode::Light, Tone::Url) => "\x1b[4;38;5;61m",
            (Mode::Light, Tone::Marker) => "\x1b[38;5;30m",
            (Mode::Light, Tone::Head) => "\x1b[1m",
            (Mode::Light, Tone::Watch) => "\x1b[1;38;5;136m",
            (Mode::Light, Tone::Love) => "\x1b[38;5;95m",

            (Mode::Mono, Tone::Id) => "\x1b[1m",
            (Mode::Mono, Tone::Title) => "\x1b[1m",
            (Mode::Mono, Tone::Meta) => "\x1b[2m",
            (Mode::Mono, Tone::Body) => "",
            (Mode::Mono, Tone::Url) => "\x1b[4m",
            (Mode::Mono, Tone::Marker) => "\x1b[1m",
            (Mode::Mono, Tone::Head) => "\x1b[1m",
            (Mode::Mono, Tone::Watch) => "\x1b[1m",
            // No colour to give, and none needed: a heart is legible as itself.
            (Mode::Mono, Tone::Love) => "",

            // Matches set both foreground and background, so they keep their
            // contrast whatever the terminal background is.
            (Mode::Mono, Tone::Match) => "\x1b[7m",
            (_, Tone::Match) => "\x1b[30;103m",
        }
    }

    pub fn reset(&self) -> &'static str {
        if self.color {
            "\x1b[0m"
        } else {
            ""
        }
    }

    pub fn paint(&self, t: Tone, s: &str) -> String {
        let code = self.ansi(t);
        if code.is_empty() || s.is_empty() {
            s.to_string()
        } else {
            format!("{code}{s}{}", self.reset())
        }
    }

    /// ratatui equivalent, for the TUI.
    pub fn style(&self, t: Tone) -> Style {
        let s = Style::default();
        if !self.color {
            return match t {
                Tone::Title | Tone::Id | Tone::Marker | Tone::Head | Tone::Watch | Tone::Love => {
                    s.add_modifier(Modifier::BOLD)
                }
                Tone::Meta => s.add_modifier(Modifier::DIM),
                Tone::Url => s.add_modifier(Modifier::UNDERLINED),
                Tone::Match => s.add_modifier(Modifier::REVERSED),
                Tone::Body => s,
            };
        }
        match (self.mode, t) {
            (Mode::Dark, Tone::Id) => s.fg(Color::Indexed(66)),
            (Mode::Dark, Tone::Title) => s.add_modifier(Modifier::BOLD),
            (Mode::Dark, Tone::Meta) => s.fg(Color::Indexed(102)),
            (Mode::Dark, Tone::Body) => s,
            (Mode::Dark, Tone::Url) => s.fg(Color::Indexed(103)).add_modifier(Modifier::UNDERLINED),
            (Mode::Dark, Tone::Marker) => s.fg(Color::Indexed(66)).add_modifier(Modifier::BOLD),
            (Mode::Dark, Tone::Head) => s.add_modifier(Modifier::BOLD),
            (Mode::Dark, Tone::Watch) => s.fg(Color::Indexed(136)).add_modifier(Modifier::BOLD),
            (Mode::Dark, Tone::Love) => s.fg(Color::Indexed(131)),

            (Mode::Light, Tone::Id) => s.fg(Color::Indexed(30)),
            (Mode::Light, Tone::Title) => s.add_modifier(Modifier::BOLD),
            (Mode::Light, Tone::Meta) => s.fg(Color::Indexed(240)),
            (Mode::Light, Tone::Body) => s,
            (Mode::Light, Tone::Url) => s.fg(Color::Indexed(61)).add_modifier(Modifier::UNDERLINED),
            (Mode::Light, Tone::Marker) => s.fg(Color::Indexed(30)).add_modifier(Modifier::BOLD),
            (Mode::Light, Tone::Head) => s.add_modifier(Modifier::BOLD),
            (Mode::Light, Tone::Watch) => s.fg(Color::Indexed(136)).add_modifier(Modifier::BOLD),
            (Mode::Light, Tone::Love) => s.fg(Color::Indexed(95)),

            (Mode::Mono, _) => s,

            (_, Tone::Match) => s
                .fg(Color::Black)
                .bg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_shapes_terminals_actually_reply_with() {
        // xterm/VTE: 16-bit components, BEL-terminated.
        let white = luminance_of("\x1b]11;rgb:ffff/ffff/ffff\x07").unwrap();
        assert!(white > 0.99, "white was {white}");
        let black = luminance_of("\x1b]11;rgb:0000/0000/0000\x07").unwrap();
        assert!(black < 0.01, "black was {black}");
        // 8-bit components, ST-terminated — also legal.
        let same = luminance_of("\x1b]11;rgb:ff/ff/ff\x1b\\").unwrap();
        assert!(
            (same - white).abs() < 0.01,
            "width should not change the value"
        );
    }

    #[test]
    fn picks_the_palette_the_background_calls_for() {
        let dark = luminance_of("rgb:1c1c/1c1c/1c1c").unwrap();
        let light = luminance_of("rgb:ffff/fff8/f0f0").unwrap();
        assert!(dark <= 0.5, "a near-black background must read as dark");
        assert!(light > 0.5, "an off-white background must read as light");
        // Solarized light and dark, the classic pair that must not collide.
        assert!(luminance_of("rgb:fdfd/f6f6/e3e3").unwrap() > 0.5);
        assert!(luminance_of("rgb:0000/2b2b/3636").unwrap() <= 0.5);
    }

    #[test]
    fn nonsense_is_declined_rather_than_guessed() {
        assert!(luminance_of("").is_none());
        assert!(luminance_of("\x1b]11;?\x07").is_none());
        assert!(
            luminance_of("rgb:ffff/ffff").is_none(),
            "two channels is not a colour"
        );
    }
}
