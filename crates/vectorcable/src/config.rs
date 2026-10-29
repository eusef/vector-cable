//! The local config file: the Night Ops choice, saved when toggled, and the
//! rig control relay's port, set by hand.
//! `$XDG_CONFIG_HOME/vectorcable/config` (or `~/.config/...`, `%APPDATA%` on Windows),
//! one `key = value` per line.

use std::path::{Path, PathBuf};

use crate::theme::ThemeName;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub theme: Option<ThemeName>,
    /// `rig_port = N`: where the relay listens on 127.0.0.1 when 4532 is taken.
    pub rig_port: Option<u16>,
}

pub fn path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("vectorcable").join("config"))
}

pub fn parse(text: &str) -> Config {
    let mut c = Config::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "theme" => c.theme = ThemeName::parse(v.trim()),
                "rig_port" => c.rig_port = v.trim().parse().ok().filter(|p| *p != 0),
                _ => {}
            }
        }
    }
    c
}

pub fn render(c: &Config) -> String {
    let theme = match c.theme {
        Some(ThemeName::NightOps) => "night-ops",
        _ => "standard",
    };
    let mut out = format!("# VectorCable settings\ntheme = {theme}\n");
    if let Some(p) = c.rig_port {
        out.push_str(&format!("rig_port = {p}\n"));
    }
    out
}

pub fn load_from(path: &Path) -> Config {
    std::fs::read_to_string(path)
        .map(|t| parse(&t))
        .unwrap_or_default()
}

pub fn save_to(path: &Path, c: &Config) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, render(c))
}

pub fn load() -> Config {
    path().map(|p| load_from(&p)).unwrap_or_default()
}

pub fn save(c: &Config) -> std::io::Result<()> {
    match path() {
        Some(p) => save_to(&p, c),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn night_ops_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("vectorcable-config-{}", std::process::id()));
        let file = dir.join("config");
        assert_eq!(load_from(&file), Config::default());
        let c = Config {
            theme: Some(ThemeName::NightOps),
            rig_port: None,
        };
        save_to(&file, &c).unwrap();
        assert_eq!(load_from(&file), c);
        assert_eq!(parse("theme = standard").theme, Some(ThemeName::Standard));
        assert_eq!(parse("# nothing\n").theme, None);
        let c = Config {
            theme: Some(ThemeName::Standard),
            rig_port: Some(4533),
        };
        save_to(&file, &c).unwrap();
        assert_eq!(load_from(&file), c);
        assert_eq!(parse("rig_port = 0").rig_port, None);
        assert_eq!(parse("rig_port = many").rig_port, None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
