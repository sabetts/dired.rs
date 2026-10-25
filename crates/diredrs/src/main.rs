//! `diredrs` — a standalone, cross-platform TUI directory editor with
//! Emacs-style editing.

mod app;
mod buffer;
mod buffer_menu;
mod config;
mod dired;
mod disk;
mod file;
mod hexl;
mod input;
mod keymap;
mod killring;
mod message_log;
mod paste;
mod search;
mod shell;
mod syntax;
mod theme;
mod ui;

use std::io::stdout;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
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
    let mouse = cfg.ui.mouse;

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
    }
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let mut app = app::App::new(cfg);
    let result = app.run(&mut terminal);

    disable_raw_mode().ok();
    if mouse {
        execute!(terminal.backend_mut(), DisableMouseCapture).ok();
    }
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();
    result
}
