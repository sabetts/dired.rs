//! Application state and the event loop.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use base64::Engine as _;
use crossterm::event::{self, Event as TermEvent, KeyEvent, KeyEventKind};
use ratatui::backend::Backend;
use ratatui::Terminal;

use crate::buffer::{Buffer, Line, Pos};
use crate::config::Config;
use crate::dired::{Dired, Mark};
use crate::file;
use crate::hexl;
use crate::input::History;
use crate::keymap::{self, Action};
use crate::killring::KillRing;
use crate::search::{Search, SearchDirection};
use crate::theme::Theme;
use crate::ui;

/// What kind of content a buffer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A free text buffer (like Emacs's `*scratch*`).
    Scratch,
    /// The `*Help*` buffer.
    Help,
    /// A Dired directory listing.
    Dired,
    /// A read-only file view.
    File,
    /// A read-only hex viewer (Emacs `hexl-mode`).
    Hexl,
}

/// One buffer.
pub struct Buf {
    pub name: String,
    pub kind: Kind,
    pub buffer: Buffer,
    /// Directory listing state, for `Dired` buffers.
    pub dired: Option<Dired>,
}

/// The minibuffer prompt, if one is active.
#[derive(Debug, Clone)]
pub enum Prompt {
    None,
    SwitchBuffer {
        input: String,
        /// Cursor position within `input`, as a char index.
        cursor: usize,
        selected: usize,
    },
    KillBuffer {
        input: String,
        cursor: usize,
        selected: usize,
    },
}

/// A pending multi-key prefix (`C-x`, `C-c`, or `C-h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Prefix {
    #[default]
    None,
    CtrlX,
    CtrlC,
    Help,
}

/// A numeric argument being built with `C-u`, `M-<digit>`, or `M--`
/// (Emacs's universal argument).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArgState {
    magnitude: u32,
    sign: i32,
    digits: bool,
}

impl ArgState {
    fn value(self) -> i32 {
        let mag = self.magnitude.min(i32::MAX as u32) as i32;
        self.sign * mag
    }
}

/// The kind of the previous editing command, for kill/yank chaining.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LastCommand {
    #[default]
    Other,
    Kill,
    Yank,
}

/// State for cycling through `M-/` (dabbrev) expansions.
#[derive(Debug, Clone, Default)]
struct Dabbrev {
    expansions: Vec<String>,
    index: usize,
}

/// Top-level application.
pub struct App {
    pub config: Config,
    pub theme: Theme,
    pub buffers: Vec<Buf>,
    pub current: usize,
    pub previous: usize,
    pub history: History,
    pub search: Search,
    pub message: Option<String>,
    pub kill_ring: KillRing,
    /// Height (in rows) of the buffer view, from the last draw.
    pub viewport_rows: usize,
    pub should_quit: bool,
    pub prompt: Prompt,
    /// Height of the minibuffer during the current prompt, never shrinking as
    /// the candidate list narrows.
    minibuffer_rows: u16,
    prefix: Prefix,
    numeric_arg: Option<ArgState>,
    quoted: bool,
    last_command: LastCommand,
    last_yank: Option<(Pos, Pos)>,
    dabbrev: Option<Dabbrev>,
    /// Whether kills/copies are written to the system clipboard (OSC 52).
    clipboard: bool,
}

impl App {
    pub fn new(config: Config) -> Self {
        let theme = Theme::for_background(config.ui.background);
        let mut app = App {
            config,
            theme,
            buffers: Vec::new(),
            current: 0,
            previous: 0,
            history: History::default(),
            search: Search::default(),
            message: None,
            kill_ring: KillRing::new(),
            viewport_rows: 20,
            should_quit: false,
            prompt: Prompt::None,
            minibuffer_rows: 1,
            prefix: Prefix::None,
            numeric_arg: None,
            quoted: false,
            last_command: LastCommand::Other,
            last_yank: None,
            dabbrev: None,
            clipboard: true,
        };
        let scratch = app.new_buffer("*scratch*".to_string(), Kind::Scratch, Buffer::new());
        app.buffers[scratch]
            .buffer
            .set_content(scratch_lines().into_iter().map(Line::plain).collect());
        app.current = scratch;
        app.previous = scratch;
        // Open a Dired buffer for the home directory and show it on boot.
        if let Some(home) = home_dir() {
            app.open_dired(home);
        }
        app
    }

    fn new_buffer(&mut self, name: String, kind: Kind, buffer: Buffer) -> usize {
        self.buffers.push(Buf {
            name,
            kind,
            buffer,
            dired: None,
        });
        self.buffers.len() - 1
    }

    // ---- accessors ---------------------------------------------------------

    pub fn current(&self) -> &Buf {
        &self.buffers[self.current]
    }

    pub fn current_mut(&mut self) -> &mut Buf {
        let i = self.current;
        &mut self.buffers[i]
    }

    fn find_buffer(&self, name: &str) -> Option<usize> {
        self.buffers.iter().position(|b| b.name == name)
    }

    pub fn mode_line_name(&self) -> String {
        self.current().name.clone()
    }

    pub fn mode_line_mode(&self) -> String {
        match self.current().kind {
            Kind::Scratch | Kind::File => "Fundamental".to_string(),
            Kind::Help => "Help".to_string(),
            Kind::Dired => "Dired".to_string(),
            Kind::Hexl => "Hexl".to_string(),
        }
    }

    fn switch_to(&mut self, idx: usize) {
        if idx >= self.buffers.len() || idx == self.current {
            return;
        }
        let old = self.current;
        self.previous = old;
        self.current = idx;
        let height = self.viewport_rows.max(1);
        self.current_mut().buffer.ensure_point_visible(height);
    }

    /// Kill a buffer. `*scratch*` cannot be killed.
    fn kill_buffer(&mut self, idx: usize) {
        if idx >= self.buffers.len() {
            return;
        }
        if self.buffers[idx].kind == Kind::Scratch {
            self.message = Some("Cannot kill *scratch*".to_string());
            return;
        }
        self.buffers.remove(idx);
        if self.current > idx {
            self.current -= 1;
        } else if self.current == idx {
            self.current = idx.min(self.buffers.len().saturating_sub(1));
        }
        if self.previous >= self.buffers.len() {
            self.previous = self.buffers.len().saturating_sub(1);
        }
        if self.buffers.is_empty() {
            let scratch = self.new_buffer("*scratch*".to_string(), Kind::Scratch, Buffer::new());
            self.current = scratch;
            self.previous = scratch;
        }
    }

    // ---- dired -------------------------------------------------------------

    /// Open (or switch to) a Dired buffer for `dir`.
    fn open_dired(&mut self, dir: PathBuf) {
        let name = dir.display().to_string();
        if let Some(idx) = self.find_buffer(&name) {
            self.switch_to(idx);
            return;
        }
        match Dired::read(&dir, self.config.ui.show_hidden) {
            Ok(dired) => {
                let lines = dired.render(&self.theme);
                let buffer = Buffer::read_only_from(lines);
                let idx = self.new_buffer(name, Kind::Dired, buffer);
                self.buffers[idx].dired = Some(dired);
                self.goto_first_file(idx);
                self.switch_to(idx);
            }
            Err(err) => {
                self.message = Some(format!("Cannot read {}: {err}", dir.display()));
            }
        }
    }

    /// Open (or switch to) a read-only view of `path`.
    fn open_file(&mut self, path: PathBuf) {
        let name = path.display().to_string();
        if let Some(idx) = self.find_buffer(&name) {
            self.switch_to(idx);
            return;
        }
        match fs::read(&path) {
            Ok(bytes) if file::looks_binary(&bytes) => self.open_hexl(path, bytes),
            Ok(bytes) => {
                let lines = file::to_lines(&bytes);
                let buffer = Buffer::read_only_from(lines);
                let idx = self.new_buffer(name, Kind::File, buffer);
                self.switch_to(idx);
            }
            Err(err) => {
                self.message = Some(format!("Cannot read {}: {err}", path.display()));
            }
        }
    }

    /// Open (or switch to) a read-only hex view of a binary file.
    fn open_hexl(&mut self, path: PathBuf, bytes: Vec<u8>) {
        let name = path.display().to_string();
        if let Some(idx) = self.find_buffer(&name) {
            self.switch_to(idx);
            return;
        }
        let truncated = bytes.len() > hexl::MAX_BYTES;
        let shown = &bytes[..bytes.len().min(hexl::MAX_BYTES)];
        let mut lines = hexl::render(shown);
        if truncated {
            lines.push(Line::plain(""));
            lines.push(Line::plain(format!(
                "  ... file truncated: showing the first {} bytes ...",
                hexl::MAX_BYTES
            )));
        }
        let buffer = Buffer::read_only_from(lines);
        let idx = self.new_buffer(name, Kind::Hexl, buffer);
        self.switch_to(idx);
        if truncated {
            self.message = Some(format!(
                "{} is large; showing the first {} bytes",
                path.display(),
                hexl::MAX_BYTES
            ));
        }
    }

    /// Put point on the first file line's name.
    fn goto_first_file(&mut self, idx: usize) {
        let pos = self.buffers[idx].dired.as_ref().and_then(|d| {
            d.first_file_row()
                .map(|row| Pos::new(row, d.filename_col(row).unwrap_or(0)))
        });
        if let Some(pos) = pos {
            self.buffers[idx].buffer.set_point(pos);
        }
    }

    /// The entry index under point, if point is on a file line.
    fn dired_entry_at_point(&self) -> Option<usize> {
        let buf = self.current();
        let dired = buf.dired.as_ref()?;
        dired.entry_index_at(buf.buffer.point().row)
    }

    /// The next file row in direction `delta` (+1 down, -1 up) from `row`.
    fn next_file_row(&self, row: usize, delta: isize) -> Option<usize> {
        let dired = self.current().dired.as_ref()?;
        let len = dired.len();
        let mut r = row as isize + delta;
        while r >= 0 && (r as usize) < len {
            if dired.entry_index_at(r as usize).is_some() {
                return Some(r as usize);
            }
            r += delta;
        }
        None
    }

    /// Move point to the next/previous file line, landing on its name.
    fn dired_move(&mut self, delta: isize, times: i32) {
        for _ in 0..times {
            let row = self.current().buffer.point().row;
            let Some(target) = self.next_file_row(row, delta) else {
                break;
            };
            let col = self
                .current()
                .dired
                .as_ref()
                .and_then(|d| d.filename_col(target))
                .unwrap_or(0);
            self.current_mut().buffer.set_point(Pos::new(target, col));
        }
    }

    /// Move point to the first or last file line.
    fn dired_goto_edge(&mut self, first: bool) {
        let row = self.current().dired.as_ref().and_then(|d| {
            if first {
                d.first_file_row()
            } else {
                d.first_file_row().map(|_| d.len() - 1)
            }
        });
        if let Some(row) = row {
            let col = self
                .current()
                .dired
                .as_ref()
                .and_then(|d| d.filename_col(row))
                .unwrap_or(0);
            self.current_mut().buffer.set_point(Pos::new(row, col));
        }
    }

    /// Visit the file or directory at point.
    fn dired_visit(&mut self) {
        let Some(i) = self.dired_entry_at_point() else {
            self.message = Some("No file on this line".to_string());
            return;
        };
        let (is_dir, path) = {
            let entry = &self.current().dired.as_ref().unwrap().entries[i];
            (entry.is_dir, entry.path.clone())
        };
        if is_dir {
            self.open_dired(path);
        } else {
            self.open_file(path);
        }
    }

    /// Open the parent directory (`^`).
    fn dired_up(&mut self) {
        let Some(dir) = self.current().dired.as_ref().map(|d| d.dir.clone()) else {
            return;
        };
        match dir.parent() {
            Some(parent) => self.open_dired(parent.to_path_buf()),
            None => self.message = Some("Already at the root directory".to_string()),
        }
    }

    /// Re-read the current directory listing (`g`), preserving marks by name.
    fn dired_revert(&mut self) {
        let idx = self.current;
        let Some(dir) = self.buffers[idx].dired.as_ref().map(|d| d.dir.clone()) else {
            return;
        };
        let marks: Vec<(String, Mark)> = self.buffers[idx]
            .dired
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .filter(|e| e.mark != Mark::None)
            .map(|e| (e.name.clone(), e.mark))
            .collect();
        match Dired::read(&dir, self.config.ui.show_hidden) {
            Ok(mut dired) => {
                for entry in &mut dired.entries {
                    if let Some((_, mark)) = marks.iter().find(|(n, _)| *n == entry.name) {
                        entry.mark = *mark;
                    }
                }
                let lines = dired.render(&self.theme);
                self.buffers[idx].buffer.set_content(lines);
                self.buffers[idx].dired = Some(dired);
                // Land on the first file line near where point was.
                self.buffers[idx].buffer.beginning_of_buffer();
                self.goto_first_file(idx);
                self.message = Some(format!("Reverted {}", dir.display()));
            }
            Err(err) => {
                self.message = Some(format!("Cannot read {}: {err}", dir.display()));
            }
        }
    }

    /// Kill the current Dired buffer (`q`).
    fn dired_quit(&mut self) {
        let idx = self.current;
        self.kill_buffer(idx);
    }

    /// Dispatch plain Dired keys. Returns `true` when the key was handled.
    fn handle_dired_key(&mut self, code: crossterm::event::KeyCode) -> bool {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Char('n') | KeyCode::Char(' ') => {
                self.dired_move(1, 1);
                true
            }
            KeyCode::Char('p') => {
                self.dired_move(-1, 1);
                true
            }
            KeyCode::Char('f') | KeyCode::Char('e') | KeyCode::Char('v') | KeyCode::Char('o') => {
                self.dired_visit();
                true
            }
            KeyCode::Char('^') => {
                self.dired_up();
                true
            }
            KeyCode::Char('g') => {
                self.dired_revert();
                true
            }
            KeyCode::Char('q') => {
                self.dired_quit();
                true
            }
            KeyCode::Char('<') => {
                self.dired_goto_edge(true);
                true
            }
            KeyCode::Char('>') => {
                self.dired_goto_edge(false);
                true
            }
            _ => false,
        }
    }

    // ---- the loop ----------------------------------------------------------

    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        loop {
            let size = terminal.size()?;
            let minibuffer = self.minibuffer_height();
            self.viewport_rows = (size.height as usize).saturating_sub(1 + minibuffer as usize);
            self.ensure_scroll_visible();
            terminal.draw(|frame| ui::draw(frame, self))?;
            if self.should_quit {
                break;
            }
            match event::read()? {
                TermEvent::Key(key) => self.handle_key(key),
                TermEvent::Paste(text) => self.handle_paste(text),
                _ => {}
            }
        }
        Ok(())
    }

    fn ensure_scroll_visible(&mut self) {
        let height = self.viewport_rows.max(1);
        self.current_mut().buffer.ensure_point_visible(height);
    }

    // ---- key handling ------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.kind == KeyEventKind::Release {
            return;
        }
        if !matches!(self.prompt, Prompt::None) {
            self.handle_prompt_key(key);
            return;
        }
        if self.search.active {
            self.handle_search_key(key);
            return;
        }
        if self.quoted {
            self.quoted = false;
            self.message = None;
            if let KeyCode::Char(c) = key.code {
                self.current_mut().buffer.insert_char(c);
            } else if key.code == KeyCode::Tab {
                self.current_mut().buffer.insert_char('\t');
            } else if key.code == KeyCode::Enter {
                self.current_mut().buffer.insert_newline();
            }
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if self.handle_argument(key, ctrl) {
            return;
        }

        if self.prefix != Prefix::None {
            self.numeric_arg = None;
            self.handle_prefix(key, ctrl);
            return;
        }

        if ctrl && key.code == KeyCode::Char('x') {
            self.numeric_arg = None;
            self.prefix = Prefix::CtrlX;
            self.message = Some("C-x-".to_string());
            return;
        }
        if ctrl && key.code == KeyCode::Char('c') {
            self.numeric_arg = None;
            self.prefix = Prefix::CtrlC;
            self.message = Some("C-c-".to_string());
            return;
        }
        // C-h (help) prefix. Some terminals report Ctrl+H as Backspace.
        if ctrl && matches!(key.code, KeyCode::Char('h') | KeyCode::Backspace) {
            self.numeric_arg = None;
            self.prefix = Prefix::Help;
            self.message = Some("C-h-".to_string());
            return;
        }

        // Plain keys in a Dired buffer drive Dired commands (n, p, f, ^, g,
        // q, ...) rather than inserting text.
        if self.current().kind == Kind::Dired
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && self.handle_dired_key(key.code)
        {
            self.message = None;
            return;
        }

        self.message = None;
        let action = keymap::translate(key);
        if action == Action::None {
            self.numeric_arg = None;
            if let Some(desc) = keymap::describe(key) {
                self.message = Some(format!("{desc} is undefined"));
            }
        } else {
            self.perform(action);
        }
    }

    /// Handle keys that build the universal/numeric argument. Returns `true`
    /// when the key was consumed.
    fn handle_argument(&mut self, key: KeyEvent, ctrl: bool) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                let arg = self.numeric_arg.get_or_insert(ArgState {
                    magnitude: 1,
                    sign: 1,
                    digits: false,
                });
                arg.magnitude = arg.magnitude.saturating_mul(4);
                self.show_argument();
                true
            }
            KeyCode::Char('-') if alt => {
                match &mut self.numeric_arg {
                    Some(arg) => arg.sign = -arg.sign,
                    None => {
                        self.numeric_arg = Some(ArgState {
                            magnitude: 1,
                            sign: -1,
                            digits: false,
                        })
                    }
                }
                self.show_argument();
                true
            }
            KeyCode::Char('-') if !ctrl && self.numeric_arg.is_some() => {
                if let Some(arg) = &mut self.numeric_arg {
                    arg.sign = -arg.sign;
                }
                self.show_argument();
                true
            }
            KeyCode::Char(d) if d.is_ascii_digit() && alt => {
                self.push_argument_digit(d);
                self.show_argument();
                true
            }
            KeyCode::Char(d) if d.is_ascii_digit() && !ctrl && self.numeric_arg.is_some() => {
                self.push_argument_digit(d);
                self.show_argument();
                true
            }
            _ => false,
        }
    }

    fn push_argument_digit(&mut self, d: char) {
        let n = d.to_digit(10).unwrap_or(0);
        match &mut self.numeric_arg {
            Some(arg) if arg.digits => {
                arg.magnitude = arg.magnitude.saturating_mul(10).saturating_add(n);
            }
            Some(arg) => {
                arg.magnitude = n;
                arg.digits = true;
            }
            None => {
                self.numeric_arg = Some(ArgState {
                    magnitude: n,
                    sign: 1,
                    digits: true,
                })
            }
        }
    }

    fn show_argument(&mut self) {
        if let Some(arg) = self.numeric_arg {
            self.message = Some(format!("C-u {}-", arg.value()));
        }
    }

    fn take_arg(&mut self) -> i32 {
        self.numeric_arg.take().map(ArgState::value).unwrap_or(1)
    }

    fn handle_prefix(&mut self, key: KeyEvent, ctrl: bool) {
        use crossterm::event::KeyCode;
        let prefix = std::mem::take(&mut self.prefix);
        self.message = None;
        match prefix {
            Prefix::None => {}
            Prefix::CtrlX => match key.code {
                KeyCode::Char('c') if ctrl => self.should_quit = true,
                KeyCode::Char('b') => self.start_switch_buffer(),
                KeyCode::Char('k') => self.start_kill_buffer(),
                KeyCode::Char('g') if ctrl => {}
                _ => self.undefined("C-x", key),
            },
            Prefix::CtrlC => match key.code {
                KeyCode::Char('g') if ctrl => {}
                _ => self.undefined("C-c", key),
            },
            Prefix::Help => match key.code {
                KeyCode::Char('b') => self.show_help(),
                KeyCode::Char('g') if ctrl => {}
                _ => self.undefined("C-h", key),
            },
        }
    }

    fn undefined(&mut self, prefix: &str, key: KeyEvent) {
        if let Some(desc) = keymap::describe(key) {
            self.message = Some(format!("{prefix} {desc} is undefined"));
        }
    }

    /// `C-h b`: show the key bindings in a `*Help*` buffer.
    fn show_help(&mut self) {
        let lines: Vec<Line> = help_lines().into_iter().map(Line::plain).collect();
        let idx = match self.find_buffer("*Help*") {
            Some(idx) => {
                self.buffers[idx].buffer.set_content(lines);
                idx
            }
            None => {
                let mut buf = Buffer::read_only_from(lines);
                buf.set_read_only(true);
                self.new_buffer("*Help*".to_string(), Kind::Help, buf)
            }
        };
        self.switch_to(idx);
        self.buffers[idx].buffer.beginning_of_buffer();
    }

    // ---- prompts -----------------------------------------------------------

    fn start_switch_buffer(&mut self) {
        self.prompt = Prompt::SwitchBuffer {
            input: String::new(),
            cursor: 0,
            selected: 0,
        };
    }

    fn start_kill_buffer(&mut self) {
        self.prompt = Prompt::KillBuffer {
            input: String::new(),
            cursor: 0,
            selected: 0,
        };
    }

    fn buffer_candidates(&self, input: &str, include_current: bool) -> Vec<usize> {
        let needle = input.to_lowercase();
        self.buffers
            .iter()
            .enumerate()
            .filter(|(i, b)| {
                (include_current || *i != self.current)
                    && (needle.is_empty() || b.name.to_lowercase().contains(&needle))
            })
            .map(|(i, _)| i)
            .take(5)
            .collect()
    }

    fn switch_candidates(&self, input: &str) -> Vec<usize> {
        self.buffer_candidates(input, false)
    }

    fn kill_candidates(&self, input: &str) -> Vec<usize> {
        self.buffer_candidates(input, true)
    }

    fn prompt_move(&mut self, count: usize, delta: isize) {
        let selected = match &mut self.prompt {
            Prompt::SwitchBuffer { selected, .. } | Prompt::KillBuffer { selected, .. } => selected,
            Prompt::None => return,
        };
        if count == 0 {
            *selected = 0;
            return;
        }
        let next = *selected as isize + delta;
        *selected = next.clamp(0, count as isize - 1) as usize;
    }

    fn prompt_edit<R>(&mut self, f: impl FnOnce(&mut String, &mut usize) -> R) -> Option<R> {
        let (input, cursor) = match &mut self.prompt {
            Prompt::SwitchBuffer { input, cursor, .. }
            | Prompt::KillBuffer { input, cursor, .. } => (input, cursor),
            Prompt::None => return None,
        };
        Some(f(input, cursor))
    }

    fn prompt_reset_selection(&mut self) {
        match &mut self.prompt {
            Prompt::SwitchBuffer { selected, .. } | Prompt::KillBuffer { selected, .. } => {
                *selected = 0
            }
            _ => {}
        }
    }

    fn prompt_insert(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.prompt_insert_str(c.encode_utf8(&mut buf));
    }

    fn prompt_insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self
            .prompt_edit(|input, cursor| {
                let idx = (*cursor).min(input.chars().count());
                input.insert_str(char_to_byte(input, idx), text);
                *cursor = idx + text.chars().count();
            })
            .is_some()
        {
            self.prompt_reset_selection();
            self.last_command = LastCommand::Other;
        }
    }

    fn prompt_delete_backward(&mut self) {
        if self
            .prompt_edit(|input, cursor| {
                if *cursor > 0 {
                    let start = char_to_byte(input, *cursor - 1);
                    let end = char_to_byte(input, *cursor);
                    input.replace_range(start..end, "");
                    *cursor -= 1;
                }
            })
            .is_some()
        {
            self.prompt_reset_selection();
        }
    }

    fn prompt_delete_forward(&mut self) {
        if self
            .prompt_edit(|input, cursor| {
                if *cursor < input.chars().count() {
                    let start = char_to_byte(input, *cursor);
                    let end = char_to_byte(input, *cursor + 1);
                    input.replace_range(start..end, "");
                }
            })
            .is_some()
        {
            self.prompt_reset_selection();
        }
    }

    fn prompt_move_cursor(&mut self, delta: isize) {
        self.prompt_edit(|input, cursor| {
            let len = input.chars().count();
            *cursor = (*cursor as isize + delta).clamp(0, len as isize) as usize;
        });
    }

    fn prompt_cursor_start(&mut self) {
        self.prompt_edit(|_, cursor| *cursor = 0);
    }

    fn prompt_cursor_end(&mut self) {
        self.prompt_edit(|input, cursor| *cursor = input.chars().count());
    }

    fn prompt_kill_line(&mut self) {
        let killed = self.prompt_edit(|input, cursor| {
            let at = char_to_byte(input, (*cursor).min(input.chars().count()));
            let killed = input[at..].to_string();
            input.truncate(at);
            killed
        });
        if let Some(killed) = killed {
            let append = self.last_command == LastCommand::Kill;
            self.kill_ring.push(&killed, append);
            self.prompt_reset_selection();
            self.last_command = LastCommand::Kill;
        }
    }

    fn prompt_yank(&mut self) {
        let text = match self.kill_ring.current() {
            Some(text) => text,
            None => return,
        };
        if self
            .prompt_edit(|input, cursor| {
                let idx = (*cursor).min(input.chars().count());
                input.insert_str(char_to_byte(input, idx), &text);
                *cursor = idx + text.chars().count();
            })
            .is_some()
        {
            self.prompt_reset_selection();
            self.last_command = LastCommand::Yank;
        }
    }

    fn prompt_complete(&mut self) {
        let candidates = match &self.prompt {
            Prompt::SwitchBuffer { input, .. } => self
                .switch_candidates(input)
                .into_iter()
                .map(|i| self.buffers[i].name.clone())
                .collect::<Vec<_>>(),
            Prompt::KillBuffer { input, .. } => self
                .kill_candidates(input)
                .into_iter()
                .map(|i| self.buffers[i].name.clone())
                .collect::<Vec<_>>(),
            Prompt::None => return,
        };
        if candidates.is_empty() {
            return;
        }
        let completion = if candidates.len() == 1 {
            candidates[0].clone()
        } else {
            common_prefix(&candidates)
        };
        if let Prompt::SwitchBuffer {
            input,
            cursor,
            selected,
        }
        | Prompt::KillBuffer {
            input,
            cursor,
            selected,
        } = &mut self.prompt
        {
            *cursor = completion.chars().count();
            *input = completion;
            *selected = 0;
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        match key.code {
            KeyCode::Esc => {
                self.prompt = Prompt::None;
                return;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.prompt = Prompt::None;
                return;
            }
            KeyCode::Enter => {
                self.commit_prompt();
                return;
            }
            KeyCode::Tab => {
                self.prompt_complete();
                return;
            }
            _ => {}
        }
        let count = match &self.prompt {
            Prompt::SwitchBuffer { input, .. } => self.switch_candidates(input).len(),
            Prompt::KillBuffer { input, .. } => self.kill_candidates(input).len(),
            Prompt::None => 0,
        };
        use Action::*;
        let action = keymap::translate(key);
        if action != DabbrevExpand {
            self.dabbrev = Option::None;
        }
        match action {
            Abort => self.prompt = Prompt::None,
            SelfInsert(c) => self.prompt_insert(c),
            DeleteBackward => self.prompt_delete_backward(),
            DeleteForward => self.prompt_delete_forward(),
            BackwardChar => self.prompt_move_cursor(-1),
            ForwardChar => self.prompt_move_cursor(1),
            BeginningOfLine => self.prompt_cursor_start(),
            EndOfLine => self.prompt_cursor_end(),
            KillLine => self.prompt_kill_line(),
            Yank => self.prompt_yank(),
            PrevLine => self.prompt_move(count, -1),
            NextLine => self.prompt_move(count, 1),
            _ => {}
        }
    }

    fn commit_prompt(&mut self) {
        match std::mem::replace(&mut self.prompt, Prompt::None) {
            Prompt::None => {}
            Prompt::SwitchBuffer {
                input, selected, ..
            } => {
                let candidates = self.switch_candidates(&input);
                if let Some(&i) = candidates.get(selected) {
                    self.switch_to(i);
                } else if input.trim().is_empty() {
                    let prev = self.previous;
                    self.switch_to(prev);
                } else if let Some(i) = self.find_buffer(input.trim()) {
                    self.switch_to(i);
                } else {
                    self.message = Some(format!("No such buffer: {}", input.trim()));
                }
                self.history.push(&input);
            }
            Prompt::KillBuffer {
                input, selected, ..
            } => {
                let candidates = self.kill_candidates(&input);
                if let Some(&i) = candidates.get(selected) {
                    self.kill_buffer(i);
                } else if let Some(i) = self.find_buffer(input.trim()) {
                    self.kill_buffer(i);
                } else if input.trim().is_empty() {
                    let cur = self.current;
                    self.kill_buffer(cur);
                } else {
                    self.message = Some(format!("No such buffer: {}", input.trim()));
                }
                self.history.push(&input);
            }
        }
    }

    /// The active prompt as (prompt line, cursor column, candidate list,
    /// selected index).
    pub fn prompt_view(&self) -> Option<(String, usize, Vec<String>, usize)> {
        match &self.prompt {
            Prompt::None => None,
            Prompt::SwitchBuffer {
                input,
                cursor,
                selected,
            } => {
                let names = self
                    .switch_candidates(input)
                    .into_iter()
                    .map(|i| self.buffers[i].name.clone())
                    .collect();
                let prefix = format!(
                    "Switch to buffer (default {}): ",
                    self.previous_buffer_name()
                );
                Some((
                    format!("{prefix}{input}"),
                    prefix.chars().count() + *cursor,
                    names,
                    *selected,
                ))
            }
            Prompt::KillBuffer {
                input,
                cursor,
                selected,
            } => {
                let names = self
                    .kill_candidates(input)
                    .into_iter()
                    .map(|i| self.buffers[i].name.clone())
                    .collect();
                let prefix = format!("Kill buffer (default {}): ", self.mode_line_name());
                Some((
                    format!("{prefix}{input}"),
                    prefix.chars().count() + *cursor,
                    names,
                    *selected,
                ))
            }
        }
    }

    fn previous_buffer_name(&self) -> String {
        self.buffers
            .get(self.previous)
            .map(|b| b.name.clone())
            .unwrap_or_default()
    }

    /// Height of the minibuffer area, expanded to show prompt candidates.
    pub fn minibuffer_height(&mut self) -> u16 {
        let desired = self
            .prompt_view()
            .map(|(_, _, candidates, _)| (1 + candidates.len().min(5)) as u16)
            .unwrap_or(1);
        self.minibuffer_rows = if matches!(self.prompt, Prompt::None) {
            desired
        } else {
            self.minibuffer_rows.max(desired)
        };
        self.minibuffer_rows
    }

    // ---- actions -----------------------------------------------------------

    fn perform(&mut self, action: Action) {
        use Action::*;
        if action != DabbrevExpand {
            self.dabbrev = Option::None;
        }
        let prev = self.last_command;
        self.last_command = LastCommand::Other;
        let n = self.take_arg();
        match action {
            None => {}
            SelfInsert(c) => {
                if self.check_editable() {
                    for _ in 0..n.max(0) {
                        self.current_mut().buffer.insert_char(c);
                    }
                }
            }
            Open => {
                if self.current().kind == Kind::Dired {
                    for _ in 0..n.unsigned_abs().max(1) {
                        self.dired_visit();
                    }
                } else if self.current().buffer.editable() {
                    let buf = self.current_mut();
                    for _ in 0..n.max(0) {
                        buf.buffer.insert_newline();
                    }
                }
            }
            ForwardChar => self.move_chars(n),
            BackwardChar => self.move_chars(-n),
            NextLine => {
                if self.current().kind == Kind::Dired {
                    self.dired_move(1, n.unsigned_abs() as i32);
                } else {
                    self.move_lines(n);
                }
            }
            PrevLine => {
                if self.current().kind == Kind::Dired {
                    self.dired_move(-1, n.unsigned_abs() as i32);
                } else {
                    self.move_lines(-n);
                }
            }
            BeginningOfLine => self.current_mut().buffer.beginning_of_line(),
            EndOfLine => self.current_mut().buffer.end_of_line(),
            ForwardWord => self.move_words(n),
            BackwardWord => self.move_words(-n),
            UpcaseWord => self.case_words(n, true),
            DowncaseWord => self.case_words(n, false),
            DabbrevExpand => self.dabbrev_expand(),
            BeginningOfBuffer => self.current_mut().buffer.beginning_of_buffer(),
            EndOfBuffer => self.current_mut().buffer.end_of_buffer(),
            PageUp => self.page_up(),
            PageDown => self.page_down(),
            DeleteBackward => {
                if self.check_editable() {
                    self.delete_chars(-n);
                }
            }
            DeleteForward => {
                if self.check_editable() {
                    self.delete_chars(n);
                }
            }
            BackwardKillWord => {
                if self.check_editable() {
                    let killed = self.current_mut().buffer.backward_kill_word();
                    self.kill_new(&killed, prev == LastCommand::Kill);
                    self.last_command = LastCommand::Kill;
                }
            }
            KillWord => {
                if self.check_editable() {
                    let killed = self.current_mut().buffer.kill_word();
                    self.kill_new(&killed, prev == LastCommand::Kill);
                    self.last_command = LastCommand::Kill;
                }
            }
            KillLine => {
                if self.check_editable() {
                    let killed = self.current_mut().buffer.kill_line();
                    self.kill_new(&killed, prev == LastCommand::Kill);
                    self.last_command = LastCommand::Kill;
                }
            }
            SetMark => self.current_mut().buffer.set_mark(),
            KillRegion => self.kill_region(prev),
            CopyRegion => self.copy_region(prev),
            Yank => self.yank(),
            YankPop => self.yank_pop(prev),
            HistoryPrev => self.history_prev(),
            HistoryNext => self.history_next(),
            Complete => {
                if self.check_editable() {
                    self.current_mut().buffer.insert_str("    ");
                }
            }
            SearchForward => self.start_search(SearchDirection::Forward),
            SearchBackward => self.start_search(SearchDirection::Backward),
            QuotedInsert => {
                self.quoted = true;
                self.message = Some("C-q-".to_string());
            }
            Abort => {
                self.current_mut().buffer.clear_mark();
                self.message = Option::None;
            }
        }
    }

    /// Ensure the current buffer is editable, setting a message otherwise.
    fn check_editable(&mut self) -> bool {
        if self.current().buffer.editable() {
            true
        } else {
            self.message = Some("Text is read-only".to_string());
            false
        }
    }

    fn move_chars(&mut self, n: i32) {
        for _ in 0..n.abs() {
            if n >= 0 {
                self.current_mut().buffer.forward_char();
            } else {
                self.current_mut().buffer.backward_char();
            }
        }
    }

    fn move_lines(&mut self, n: i32) {
        for _ in 0..n.abs() {
            if n >= 0 {
                self.current_mut().buffer.next_line();
            } else {
                self.current_mut().buffer.prev_line();
            }
        }
    }

    fn move_words(&mut self, n: i32) {
        for _ in 0..n.abs() {
            if n >= 0 {
                self.current_mut().buffer.forward_word();
            } else {
                self.current_mut().buffer.backward_word();
            }
        }
    }

    fn case_words(&mut self, n: i32, up: bool) {
        if !self.check_editable() {
            return;
        }
        for _ in 0..n.abs() {
            match (n >= 0, up) {
                (true, true) => self.current_mut().buffer.upcase_word(),
                (true, false) => self.current_mut().buffer.downcase_word(),
                (false, true) => self.current_mut().buffer.backward_upcase_word(),
                (false, false) => self.current_mut().buffer.backward_downcase_word(),
            }
        }
    }

    fn delete_chars(&mut self, n: i32) {
        for _ in 0..n.abs() {
            if n >= 0 {
                self.current_mut().buffer.delete_forward();
            } else {
                self.current_mut().buffer.delete_backward();
            }
        }
    }

    fn page_lines(&self) -> usize {
        self.viewport_rows.saturating_sub(2).max(1)
    }

    fn page_up(&mut self) {
        let n = self.page_lines();
        self.current_mut().buffer.page_backward(n);
    }

    fn page_down(&mut self) {
        let n = self.page_lines();
        self.current_mut().buffer.page_forward(n);
    }

    fn dabbrev_expand(&mut self) {
        let i = self.current;
        if !self.buffers[i].buffer.editable() {
            return;
        }
        if let Some(prev) = self.dabbrev_current() {
            if self.buffers[i].buffer.word_before_point() == prev {
                self.dabbrev_advance_buffer(i);
                return;
            }
        }
        let prefix = self.buffers[i].buffer.word_before_point();
        if prefix.is_empty() {
            self.dabbrev = None;
            return;
        }
        let candidates = self.buffers[i].buffer.dabbrev_candidates(&prefix);
        let Some(first) = candidates.first().cloned() else {
            self.dabbrev = None;
            self.message = Some(format!("No dabbrev expansion for \"{prefix}\""));
            return;
        };
        self.buffers[i].buffer.replace_word_before_point(&first);
        self.dabbrev = Some(Dabbrev {
            expansions: candidates,
            index: 0,
        });
    }

    fn dabbrev_current(&self) -> Option<String> {
        self.dabbrev
            .as_ref()
            .and_then(|d| d.expansions.get(d.index).cloned())
    }

    fn dabbrev_advance_buffer(&mut self, i: usize) {
        let (next, word) = {
            let d = self.dabbrev.as_ref().unwrap();
            let next = (d.index + 1) % d.expansions.len();
            (next, d.expansions[next].clone())
        };
        self.buffers[i].buffer.replace_word_before_point(&word);
        if let Some(d) = self.dabbrev.as_mut() {
            d.index = next;
        }
    }

    // ---- kill ring ---------------------------------------------------------

    fn kill_new(&mut self, text: &str, append: bool) {
        self.kill_ring.push(text, append);
        if let Some(top) = self.kill_ring.current() {
            self.set_clipboard(&top);
        }
    }

    fn set_clipboard(&self, text: &str) {
        if !self.clipboard {
            return;
        }
        if let Some(sequence) = osc52_sequence(text) {
            let mut out = std::io::stdout();
            let _ = std::io::Write::write_all(&mut out, sequence.as_bytes());
            let _ = std::io::Write::flush(&mut out);
        }
    }

    fn copy_region(&mut self, prev: LastCommand) {
        let i = self.current;
        let Some((start, end)) = self.buffers[i].buffer.region() else {
            self.message = Some("The mark is not set now, so there is no region".to_string());
            return;
        };
        let Some(text) = self.buffers[i].buffer.region_text() else {
            return;
        };
        if start != end {
            self.kill_new(&text, prev == LastCommand::Kill);
            self.last_command = LastCommand::Kill;
        }
    }

    fn kill_region(&mut self, prev: LastCommand) {
        let i = self.current;
        let Some((start, end)) = self.buffers[i].buffer.region() else {
            self.message = Some("The mark is not set now, so there is no region".to_string());
            return;
        };
        let Some(text) = self.buffers[i].buffer.region_text() else {
            return;
        };
        if self.buffers[i].buffer.delete_region(start, end) {
            self.kill_new(&text, prev == LastCommand::Kill);
            self.last_command = LastCommand::Kill;
        } else {
            self.message = Some("Text is read-only".to_string());
        }
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.yank() else {
            self.message = Some("Kill ring is empty".to_string());
            return;
        };
        if !self.check_editable() {
            return;
        }
        let i = self.current;
        let start = self.buffers[i].buffer.point();
        self.buffers[i].buffer.insert_str(&text);
        let end = self.buffers[i].buffer.point();
        self.last_yank = Some((start, end));
        self.last_command = LastCommand::Yank;
    }

    fn yank_pop(&mut self, prev: LastCommand) {
        if prev != LastCommand::Yank {
            self.message = Some("Previous command was not a yank".to_string());
            return;
        }
        let Some((start, end)) = self.last_yank else {
            return;
        };
        let i = self.current;
        if !self.buffers[i].buffer.delete_region(start, end) {
            return;
        }
        let Some(text) = self.kill_ring.yank_pop() else {
            return;
        };
        self.buffers[i].buffer.set_point(start);
        let start = self.buffers[i].buffer.point();
        self.buffers[i].buffer.insert_str(&text);
        let end = self.buffers[i].buffer.point();
        self.last_yank = Some((start, end));
        self.last_command = LastCommand::Yank;
    }

    fn history_prev(&mut self) {
        if let Some(text) = self.history.prev("") {
            let _ = text;
        }
    }

    fn history_next(&mut self) {
        let _ = self.history.next();
    }

    // ---- search ------------------------------------------------------------

    fn start_search(&mut self, direction: SearchDirection) {
        let origin = self.current().buffer.point();
        self.search.start(direction, origin);
    }

    fn search_run(&mut self, advance: bool) {
        let query = self.search.query.clone();
        if query.is_empty() {
            return;
        }
        let direction = self.search.direction;
        let point = self.current().buffer.point();
        let from = if advance {
            step_point(point, direction)
        } else if let Some(origin) = self.search.origin {
            if point == origin && self.search.failed {
                origin
            } else {
                point
            }
        } else {
            point
        };
        let forward = direction == SearchDirection::Forward;
        match self.current().buffer.search(&query, from, forward) {
            Some((pos, _)) => {
                self.current_mut().buffer.set_point(pos);
                self.search.failed = false;
            }
            None => self.search.failed = true,
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('g') if ctrl => self.cancel_search(),
            KeyCode::Char('s') if ctrl => {
                self.search.direction = SearchDirection::Forward;
                self.search_run(true);
            }
            KeyCode::Char('r') if ctrl => {
                self.search.direction = SearchDirection::Backward;
                self.search_run(true);
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.search.query.push(c);
                self.search_run(false);
            }
            KeyCode::Backspace => {
                self.search.query.pop();
                self.search_run(false);
            }
            KeyCode::Enter => self.search.accept(),
            KeyCode::Esc => self.cancel_search(),
            _ => {}
        }
    }

    fn cancel_search(&mut self) {
        if let Some(origin) = self.search.cancel() {
            self.current_mut().buffer.set_point(origin);
        }
    }

    // ---- paste -------------------------------------------------------------

    fn handle_paste(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if !matches!(self.prompt, Prompt::None) {
            let filtered: String = text.chars().filter(|c| *c != '\r').collect();
            self.prompt_insert_str(&filtered);
            self.kill_ring.push(&filtered, false);
            return;
        }
        if !self.current().buffer.editable() {
            self.message = Some("Text is read-only".to_string());
            return;
        }
        let i = self.current;
        self.buffers[i].buffer.insert_str(&text);
        self.kill_ring.push(&text, false);
    }
}

/// The user's home directory, falling back to environment variables.
fn home_dir() -> Option<PathBuf> {
    if let Some(dirs) = directories::UserDirs::new() {
        return Some(dirs.home_dir().to_path_buf());
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn step_point(pos: Pos, direction: SearchDirection) -> Pos {
    match direction {
        SearchDirection::Forward => Pos::new(pos.row, pos.col + 1),
        SearchDirection::Backward => Pos::new(pos.row, pos.col.saturating_sub(1)),
    }
}

/// Byte offset of the `idx`-th char in `s`, clamped to the end.
fn char_to_byte(s: &str, idx: usize) -> usize {
    s.char_indices().nth(idx).map(|(i, _)| i).unwrap_or(s.len())
}

fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut end = first.chars().count();
    for item in &items[1..] {
        let limit = first
            .chars()
            .zip(item.chars())
            .take_while(|(a, b)| a == b)
            .count();
        end = end.min(limit);
    }
    first.chars().take(end).collect()
}

/// The OSC 52 escape sequence that sets the system clipboard to `text`.
fn osc52_sequence(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    Some(format!("\u{1b}]52;c;{payload}\u{7}"))
}

/// The introductory text placed in the `*scratch*` buffer at startup.
fn scratch_lines() -> Vec<String> {
    let path = crate::config::default_path();
    [
        "diredrs — a directory editor with Emacs-style editing".to_string(),
        String::new(),
        "Welcome to the *scratch* buffer: a free-text buffer for notes and".to_string(),
        "drafts. Nothing typed here touches the filesystem.".to_string(),
        String::new(),
        "Configuration".to_string(),
        "  By default diredrs reads its config from:".to_string(),
        format!("      {}", path.display()),
        String::new(),
        "  A commented template is written there on first run.".to_string(),
        String::new(),
        "Discovering commands".to_string(),
        "  C-h b          list all key bindings in a *Help* buffer".to_string(),
        "  C-x b          switch buffers".to_string(),
        "  C-x k          kill a buffer".to_string(),
        "  C-x C-c        quit".to_string(),
    ]
    .to_vec()
}

/// The text of the `*Help*` buffer (`C-h b`).
fn help_lines() -> Vec<String> {
    [
        "diredrs key bindings",
        "",
        "Movement",
        "  C-f / C-b      forward / backward character",
        "  C-n / C-p      next / previous line",
        "  M-f / M-b      forward / backward word",
        "  C-a / C-e      beginning / end of line",
        "  M-< / M->      beginning / end of buffer",
        "  C-v / M-v      scroll up / down a screen",
        "  C-s / C-r      incremental search forward / backward",
        "",
        "Editing (in writable buffers)",
        "  RET            insert a newline",
        "  M-u / M-l      upcase / downcase word",
        "  M-/            dabbrev-expand",
        "  C-k            kill to end of line",
        "  M-Backspace    kill word backward",
        "  M-d            kill word forward",
        "  C-d            delete character forward",
        "  C-SPC          set mark",
        "  C-w / M-w      kill / copy region",
        "  C-y / M-y      yank / yank-pop",
        "  C-q            quoted insert",
        "",
        "Buffers",
        "  C-x b          switch buffer",
        "  C-x k          kill buffer",
        "  C-x C-c        quit",
        "",
        "Help",
        "  C-h b          describe bindings (this buffer)",
        "  C-g            abort the current command",
        "",
        "Arguments",
        "  C-u            numeric argument (4); repeat to multiply (C-u C-u = 16)",
        "  M-<digit>      start a numeric argument, e.g. M-3",
        "  M--            negative argument",
        "",
        "Notes",
        "  C-w / M-w / C-k mirror kills to the system clipboard (OSC 52).",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn press(app: &mut App, c: char) {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "diredrs-app-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn boot_has_a_scratch_buffer() {
        let app = App::new(Config::default());
        assert!(app.buffers.iter().any(|b| b.kind == Kind::Scratch));
    }

    #[test]
    fn dired_navigation_and_quit() {
        let dir = temp_dir("nav");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();

        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        assert_eq!(app.current().kind, Kind::Dired);
        let first = app
            .current()
            .dired
            .as_ref()
            .unwrap()
            .first_file_row()
            .unwrap();
        assert_eq!(app.current().buffer.point().row, first);

        press(&mut app, 'n');
        assert_eq!(app.current().buffer.point().row, first + 1);
        press(&mut app, 'p');
        assert_eq!(app.current().buffer.point().row, first);

        // `q` kills the Dired buffer.
        let name = app.current().name.clone();
        press(&mut app, 'q');
        assert!(app.find_buffer(&name).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dired_ret_visits_directory() {
        let dir = temp_dir("visit");
        std::fs::create_dir(dir.join("sub")).unwrap();

        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        // Point is on the single entry (the subdirectory); RET opens it.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.current().name, dir.join("sub").display().to_string());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opens_text_file_read_only() {
        let dir = temp_dir("file");
        let path = dir.join("note.txt");
        std::fs::write(&path, b"hello\nworld\n").unwrap();

        let mut app = App::new(Config::default());
        app.open_file(path.clone());
        assert_eq!(app.current().kind, Kind::File);
        assert!(!app.current().buffer.editable());
        assert_eq!(app.current().buffer.line(0).text(), "hello");
        assert_eq!(app.current().buffer.len(), 2);

        // Editing a file view is refused.
        let before = app.current().buffer.line(0).text();
        press(&mut app, 'x');
        assert_eq!(app.current().buffer.line(0).text(), before);
        assert_eq!(app.message.as_deref(), Some("Text is read-only"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opens_binary_file_in_hexl() {
        let dir = temp_dir("hex");
        let path = dir.join("data.bin");
        std::fs::write(&path, [0x00u8, 0x01, 0x02, 0xff]).unwrap();

        let mut app = App::new(Config::default());
        app.open_file(path.clone());
        assert_eq!(app.current().kind, Kind::Hexl);
        assert!(!app.current().buffer.editable());
        assert!(app
            .current()
            .buffer
            .line(0)
            .text()
            .starts_with("00000000: 0001 02ff"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_cx_cc_quits_in_scratch() {
        let mut app = App::new(Config::default());
        // Focus the scratch buffer (home Dired may be current).
        if let Some(i) = app.buffers.iter().position(|b| b.kind == Kind::Scratch) {
            app.switch_to(i);
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }
}
