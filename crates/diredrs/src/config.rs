//! Standalone TOML configuration.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::theme::Background;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Which face variant to use: `"dark"` (default) or `"light"`.
    pub background: Background,
    /// List dotfiles, like `ls -a`.
    pub show_hidden: bool,
    /// Show file sizes in human-readable units (K, M, G), like `ls -h`.
    pub human_sizes: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            background: Background::Dark,
            show_hidden: true,
            human_sizes: true,
        }
    }
}

/// Default configuration file contents written when none exists.
pub const TEMPLATE: &str = r##"# diredrs configuration

[ui]
background = "dark"     # or "light"
show_hidden = true      # list dotfiles, like ls -a
human_sizes = true      # sizes as K/M/G like ls -h; false for exact bytes
"##;

impl Config {
    /// Load configuration from `path`, writing a template if it does not exist.
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(path, TEMPLATE)
                .with_context(|| format!("writing template config to {}", path.display()))?;
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config from {}", path.display()))?;
        Ok(config)
    }
}

/// The default location of the configuration file for this platform.
pub fn default_path() -> PathBuf {
    if let Some(proj) = directories::ProjectDirs::from("", "", "diredrs") {
        proj.config_dir().join("diredrs.toml")
    } else {
        PathBuf::from("diredrs.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_template() {
        let cfg: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(cfg.ui.background, Background::Dark);
        assert!(cfg.ui.show_hidden);
        assert!(cfg.ui.human_sizes);
    }

    #[test]
    fn defaults_apply() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.ui.background, Background::Dark);
        assert!(cfg.ui.show_hidden);
        assert!(cfg.ui.human_sizes);
    }

    #[test]
    fn overrides_apply() {
        let cfg: Config =
            toml::from_str("[ui]\nbackground = \"light\"\nshow_hidden = false\n").unwrap();
        assert_eq!(cfg.ui.background, Background::Light);
        assert!(!cfg.ui.show_hidden);
    }
}
