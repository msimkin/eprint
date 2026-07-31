use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    /// auto | dark | light | mono
    pub theme: String,
    /// all | title
    pub scope: String,
    pub limit: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "auto".into(),
            scope: "all".into(),
            limit: 20,
        }
    }
}

pub const TEMPLATE: &str = r#"# eprint configuration

# Colour palette.
#   auto   pick from the terminal background when it can be determined,
#          otherwise assume a dark background
#   dark   for dark terminal backgrounds
#   light  for light terminal backgrounds
#   mono   no colour, only bold / dim / reverse
theme = "auto"

# Default search scope: "all" (title, authors and abstract) or
# "title" (title and authors only).
scope = "all"

# Default number of results for `eprint search`.
limit = 20
"#;

pub fn path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("EPRINT_CONFIG") {
        return Some(PathBuf::from(p));
    }
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("eprint").join("config.toml"));
        }
    }
    dirs::home_dir().map(|h| h.join(".config").join("eprint").join("config.toml"))
}

/// Deliberately tiny `key = value` reader rather than a TOML dependency —
/// the whole config is four scalar settings.
pub fn load() -> Config {
    let mut c = Config::default();
    let Some(p) = path() else { return c };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return c;
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim().trim_matches(['"', '\'']).to_string();
        match k.trim() {
            "theme" => c.theme = v,
            "scope" => c.scope = v,
            "limit" => {
                if let Ok(n) = v.parse::<usize>() {
                    if n > 0 {
                        c.limit = n;
                    }
                }
            }
            _ => {}
        }
    }
    c
}

pub fn init() -> Result<(PathBuf, bool)> {
    let p = path().context("could not determine a config directory")?;
    if p.exists() {
        return Ok((p, false));
    }
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&p, TEMPLATE).with_context(|| format!("writing {}", p.display()))?;
    Ok((p, true))
}
