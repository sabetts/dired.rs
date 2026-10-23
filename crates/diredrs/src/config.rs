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
    /// Lines kept above and below point before the view scrolls
    /// (Emacs' `scroll-margin`).
    pub scroll_margin: usize,
    /// Delay in milliseconds before `M-g` shows the window-number overlay.
    pub window_number_delay_ms: u64,
    /// Capture mouse events, including divider dragging.
    pub mouse: bool,
    /// Cursor foreground color (a name like `"black"` or `#rrggbb`).
    pub cursor_fg: String,
    /// Cursor background color.
    pub cursor_bg: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            background: Background::Dark,
            show_hidden: true,
            human_sizes: true,
            scroll_margin: 1,
            window_number_delay_ms: 250,
            mouse: true,
            cursor_fg: "black".to_string(),
            cursor_bg: "yellow".to_string(),
        }
    }
}

/// Default configuration file contents written when none exists.
pub const TEMPLATE: &str = r##"# diredrs configuration

[ui]
background = "dark"     # or "light"
show_hidden = true      # list dotfiles, like ls -a
human_sizes = true      # sizes as K/M/G like ls -h; false for exact bytes
scroll_margin = 1       # lines kept above/below point before the view scrolls
window_number_delay_ms = 250  # M-g overlay delay in milliseconds
mouse = true            # capture mouse events for divider dragging
cursor_fg = "black"     # cursor text color (name or #rrggbb)
cursor_bg = "yellow"    # cursor background color
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
        assert_eq!(cfg.ui.scroll_margin, 1);
        assert_eq!(cfg.ui.window_number_delay_ms, 250);
        assert!(cfg.ui.mouse);
        assert_eq!(cfg.ui.cursor_fg, "black");
        assert_eq!(cfg.ui.cursor_bg, "yellow");
        assert!(TEMPLATE.contains("window_number_delay_ms"));
        assert!(TEMPLATE.contains("mouse"));
        assert!(TEMPLATE.contains("cursor_bg"));
    }

    #[test]
    fn defaults_apply() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.ui.background, Background::Dark);
        assert!(cfg.ui.show_hidden);
        assert!(cfg.ui.human_sizes);
        assert_eq!(cfg.ui.scroll_margin, 1);
        assert_eq!(cfg.ui.window_number_delay_ms, 250);
        assert!(cfg.ui.mouse);
        assert_eq!(cfg.ui.cursor_fg, "black");
        assert_eq!(cfg.ui.cursor_bg, "yellow");
        assert!(TEMPLATE.contains("window_number_delay_ms"));
    }

    #[test]
    fn cursor_colors_are_configurable() {
        let cfg: Config =
            toml::from_str("[ui]\ncursor_fg = \"#ffffff\"\ncursor_bg = \"blue\"\n").unwrap();
        assert_eq!(cfg.ui.cursor_fg, "#ffffff");
        assert_eq!(cfg.ui.cursor_bg, "blue");
    }

    #[test]
    fn overrides_apply() {
        let cfg: Config = toml::from_str(
            "[ui]\nbackground = \"light\"\nshow_hidden = false\nwindow_number_delay_ms = 600\nmouse = false\n",
        )
        .unwrap();
        assert_eq!(cfg.ui.background, Background::Light);
        assert!(!cfg.ui.show_hidden);
        assert_eq!(cfg.ui.window_number_delay_ms, 600);
        assert!(!cfg.ui.mouse);
    }
}
