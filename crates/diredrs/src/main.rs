//! `diredrs` — a standalone, cross-platform TUI directory editor with
//! Emacs-style editing.

use std::io::stdout;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

fn main() -> Result<()> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    loop {
        terminal.draw(|frame| {
            let text = "diredrs — booting up (press q to quit)";
            frame.render_widget(Paragraph::new(text), frame.area());
        })?;
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Char('c') if ctrl => break,
                _ => {}
            }
        }
    }
    Ok(())
}
