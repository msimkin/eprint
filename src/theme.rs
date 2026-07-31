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
pub enum Tone {
    Id,
    Title,
    Meta,
    Body,
    Url,
    Match,
    Marker,
    Head,
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub mode: Mode,
    pub color: bool,
}

/// Best-effort background detection. `COLORFGBG` is set by a handful of
/// terminals (rxvt, Konsole, some others); macOS Terminal.app and iTerm2 do
/// not set it, so the documented fallback is "assume dark".
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
    Mode::Dark
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
            (Mode::Dark, Tone::Id) => "\x1b[96m",
            (Mode::Dark, Tone::Title) => "\x1b[1m",
            (Mode::Dark, Tone::Meta) => "\x1b[37m",
            (Mode::Dark, Tone::Body) => "",
            (Mode::Dark, Tone::Url) => "\x1b[94m",
            (Mode::Dark, Tone::Marker) => "\x1b[96m",
            (Mode::Dark, Tone::Head) => "\x1b[1m",

            (Mode::Light, Tone::Id) => "\x1b[34m",
            (Mode::Light, Tone::Title) => "\x1b[1m",
            (Mode::Light, Tone::Meta) => "\x1b[90m",
            (Mode::Light, Tone::Body) => "",
            (Mode::Light, Tone::Url) => "\x1b[34m",
            (Mode::Light, Tone::Marker) => "\x1b[34m",
            (Mode::Light, Tone::Head) => "\x1b[1m",

            (Mode::Mono, Tone::Id) => "\x1b[1m",
            (Mode::Mono, Tone::Title) => "\x1b[1m",
            (Mode::Mono, Tone::Meta) => "\x1b[2m",
            (Mode::Mono, Tone::Body) => "",
            (Mode::Mono, Tone::Url) => "\x1b[4m",
            (Mode::Mono, Tone::Marker) => "\x1b[1m",
            (Mode::Mono, Tone::Head) => "\x1b[1m",

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
                Tone::Title | Tone::Id | Tone::Marker | Tone::Head => {
                    s.add_modifier(Modifier::BOLD)
                }
                Tone::Meta => s.add_modifier(Modifier::DIM),
                Tone::Url => s.add_modifier(Modifier::UNDERLINED),
                Tone::Match => s.add_modifier(Modifier::REVERSED),
                Tone::Body => s,
            };
        }
        match (self.mode, t) {
            (Mode::Dark, Tone::Id) => s.fg(Color::LightCyan),
            (Mode::Dark, Tone::Title) => s.add_modifier(Modifier::BOLD),
            (Mode::Dark, Tone::Meta) => s.fg(Color::Gray),
            (Mode::Dark, Tone::Body) => s,
            (Mode::Dark, Tone::Url) => s.fg(Color::LightBlue),
            (Mode::Dark, Tone::Marker) => s.fg(Color::LightCyan).add_modifier(Modifier::BOLD),
            (Mode::Dark, Tone::Head) => s.add_modifier(Modifier::BOLD),

            (Mode::Light, Tone::Id) => s.fg(Color::Blue),
            (Mode::Light, Tone::Title) => s.add_modifier(Modifier::BOLD),
            (Mode::Light, Tone::Meta) => s.fg(Color::DarkGray),
            (Mode::Light, Tone::Body) => s,
            (Mode::Light, Tone::Url) => s.fg(Color::Blue),
            (Mode::Light, Tone::Marker) => s.fg(Color::Blue).add_modifier(Modifier::BOLD),
            (Mode::Light, Tone::Head) => s.add_modifier(Modifier::BOLD),

            (Mode::Mono, _) => s,

            (_, Tone::Match) => s
                .fg(Color::Black)
                .bg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        }
    }
}
