// User config loaded from ~/.config/pr-loop/config.toml (or
// $XDG_CONFIG_HOME/pr-loop/config.toml).
//
// Intentionally kept small and optional — every field has a default, the
// file doesn't need to exist, and CLI flags override config values.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_HUB_PORT: u16 = 10099;
pub const DEFAULT_BIND: &str = "127.0.0.1";

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub hub: HubConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Addresses to bind on. None means use the default (127.0.0.1 only).
    pub bind: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HubConfig {
    pub bind: Option<Vec<String>>,
    pub port: Option<u16>,
}

impl Config {
    /// Addresses the hub should bind on (after applying defaults).
    pub fn hub_binds(&self) -> Vec<String> {
        bind_or_default(self.hub.bind.as_ref())
    }
    /// Port the hub should bind on (after applying defaults).
    pub fn hub_port(&self) -> u16 {
        self.hub.port.unwrap_or(DEFAULT_HUB_PORT)
    }
    /// Addresses a `pr-loop web` instance should bind on.
    pub fn web_binds(&self) -> Vec<String> {
        bind_or_default(self.web.bind.as_ref())
    }
}

fn bind_or_default(v: Option<&Vec<String>>) -> Vec<String> {
    match v {
        Some(list) if !list.is_empty() => list.clone(),
        _ => vec![DEFAULT_BIND.to_string()],
    }
}

/// Resolve the path to the config file, honoring XDG_CONFIG_HOME.
pub fn config_path() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("pr-loop").join("config.toml"));
        }
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/pr-loop/config.toml"))
}

/// Load the config file. Returns `Config::default()` if the file is missing.
/// Warns on parse errors but still returns defaults so the tool doesn't
/// refuse to start because of a broken config.
pub fn load() -> Config {
    let Ok(path) = config_path() else {
        return Config::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    match toml::from_str::<Config>(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "Warning: failed to parse {}: {}. Using defaults.",
                path.display(),
                e
            );
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Config {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn empty_file_is_valid() {
        assert_eq!(parse(""), Config::default());
    }

    #[test]
    fn default_binds() {
        let c = Config::default();
        assert_eq!(c.hub_binds(), vec!["127.0.0.1".to_string()]);
        assert_eq!(c.web_binds(), vec!["127.0.0.1".to_string()]);
        assert_eq!(c.hub_port(), DEFAULT_HUB_PORT);
    }

    #[test]
    fn parses_hub_section_only() {
        let c = parse(
            r#"[hub]
bind = ["127.0.0.1", "100.64.1.2"]
port = 12345
"#,
        );
        assert_eq!(
            c.hub_binds(),
            vec!["127.0.0.1".to_string(), "100.64.1.2".to_string()]
        );
        assert_eq!(c.hub_port(), 12345);
        // web section absent => default
        assert_eq!(c.web_binds(), vec!["127.0.0.1".to_string()]);
    }

    #[test]
    fn parses_web_section_only() {
        let c = parse(
            r#"[web]
bind = ["0.0.0.0"]
"#,
        );
        assert_eq!(c.web_binds(), vec!["0.0.0.0".to_string()]);
        assert_eq!(c.hub_binds(), vec!["127.0.0.1".to_string()]);
        assert_eq!(c.hub_port(), DEFAULT_HUB_PORT);
    }

    #[test]
    fn partial_hub_section() {
        // just port, no bind — bind should fall back to default
        let c = parse("[hub]\nport = 11111\n");
        assert_eq!(c.hub_port(), 11111);
        assert_eq!(c.hub_binds(), vec!["127.0.0.1".to_string()]);
    }

    #[test]
    fn empty_bind_list_falls_back_to_default() {
        let c = parse(
            r#"[hub]
bind = []
"#,
        );
        assert_eq!(c.hub_binds(), vec!["127.0.0.1".to_string()]);
    }

    #[test]
    fn unknown_field_rejected() {
        let r: Result<Config, _> = toml::from_str(
            r#"[hub]
nonsense_field = 1
"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn config_path_prefers_xdg() {
        // SAFETY: tests touching env are single-threaded via serial_test on other
        // tests; these two env vars aren't read by other concurrent tests.
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg"); }
        let p = config_path().unwrap();
        assert_eq!(p, PathBuf::from("/tmp/xdg/pr-loop/config.toml"));
        // restore
        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v); },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME"); },
        }
    }

    #[test]
    fn config_path_falls_back_to_home() {
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_home = std::env::var("HOME").ok();
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::set_var("HOME", "/tmp/home");
        }
        let p = config_path().unwrap();
        assert_eq!(p, PathBuf::from("/tmp/home/.config/pr-loop/config.toml"));
        unsafe {
            if let Some(v) = prev_home { std::env::set_var("HOME", v); }
            if let Some(v) = prev_xdg { std::env::set_var("XDG_CONFIG_HOME", v); }
        }
    }
}
