//! `diredrs` — a standalone, cross-platform TUI directory editor with
//! Emacs-style editing.

mod app;
mod buffer;
mod config;
mod input;
mod keymap;
mod killring;
mod search;
mod theme;
mod ui;

use std::io::stdout;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// A TUI directory editor with Emacs-style editing.
#[derive(Debug, Parser)]
#[command(name = "diredrs", version, about)]
struct Cli {
    /// Path to the configuration file (defaults to the platform config dir).
    #[arg(short, long)]
    config: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(config::default_path);
    let cfg = config::Config::load(&path)?;

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let mut app = app::App::new(cfg);
    let result = app.run(&mut terminal);

    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();
    result
}
