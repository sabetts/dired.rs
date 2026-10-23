//! Application state and the event loop.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::Engine as _;
use crossterm::event::{self, Event as TermEvent, KeyEvent, KeyEventKind};
use ratatui::backend::Backend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use crate::buffer::{transpose_chars_at, transpose_words_at, Buffer, Color, Line, Pos, Style};
use crate::config::Config;
use crate::dired::{Dired, Mark, SortKey};
use crate::file;
use crate::hexl;
use crate::input::History;
use crate::keymap::{self, Action};
use crate::killring::KillRing;
use crate::paste;
use crate::search::{Search, SearchDirection};
use crate::syntax;
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
    /// Editable filename state, for Wdired buffers.
    dired_edit: Option<DiredEdit>,
    /// Offset column width, for `Hexl` buffers (see `hexl_highlights`).
    pub hexl_width: Option<usize>,
    /// Monotonic stamp of the most recent visit, for buffer-list ordering.
    last_visited: u64,
}

#[derive(Debug, Clone)]
struct DiredEdit {
    /// The original listing, restored when changes are aborted.
    old_lines: Vec<Line>,
    old_point: Pos,
    /// Edited names, one per `Dired.entries` item.
    names: Vec<String>,
}

/// A window's saved view of its buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowView {
    pub point: Pos,
    pub scroll: usize,
    pub scroll_sub: usize,
    pub width: usize,
}

/// A stable identifier for a window.
pub type WindowId = u64;

/// One window: a buffer plus its saved view.
#[derive(Debug, Clone)]
pub struct Window {
    pub id: WindowId,
    pub buffer: usize,
    pub view: WindowView,
}

/// How a window is divided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    /// Top and bottom.
    Below,
    /// Left and right.
    Right,
}

/// A direction for `M-g M-f/b/p/n` window movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowDirection {
    Left,
    Right,
    Up,
    Down,
}

/// The binary tree describing the window layout.
#[derive(Debug, Clone)]
pub enum WindowTree {
    Leaf(WindowId),
    Split {
        axis: SplitAxis,
        first_weight: u16,
        second_weight: u16,
        first: Box<WindowTree>,
        second: Box<WindowTree>,
    },
}

/// A split boundary that can be dragged with the mouse.
#[derive(Debug, Clone)]
pub(crate) struct Divider {
    pub axis: SplitAxis,
    pub rect: Rect,
    pub container: Rect,
    pub(crate) path: Vec<usize>,
}

#[derive(Debug, Clone)]
struct DividerDrag {
    axis: SplitAxis,
    container: Rect,
    path: Vec<usize>,
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
        /// `C-x 4 b`: switch the buffer in the other window.
        other_window: bool,
    },
    KillBuffer {
        input: String,
        cursor: usize,
        selected: usize,
    },
    /// Find a file (`C-x C-f`) or open a Dired buffer (`C-x d`).
    FindFile {
        input: String,
        cursor: usize,
        /// `true` for `C-x d` (a directory listing), `false` for a file.
        dired: bool,
        /// Highlighted completion candidate, moved with Up/Down. `None` means
        /// the typed `input` is used as-is (the default).
        selected: Option<usize>,
    },
    /// Read a destination for the marked files, or the file at point (`R`
    /// renames/moves; `C` copies when `copy` is true).
    Rename {
        input: String,
        cursor: usize,
        /// Highlighted completion candidate (a path), moved with Up/Down.
        selected: Option<usize>,
        copy: bool,
    },
    /// Read a file extension; every file ending in it is marked (`* .`).
    MarkExtension {
        input: String,
        cursor: usize,
    },
    /// Read a directory name to create (`+`).
    CreateDirectory {
        input: String,
        cursor: usize,
        /// Highlighted completion candidate (a path), moved with Up/Down.
        selected: Option<usize>,
    },
    /// A `yes-or-no-p` confirmation: type the answer, then `RET`.
    Confirm {
        message: String,
        input: String,
        cursor: usize,
        action: PendingAction,
    },
}

/// A deferred action awaiting confirmation.
#[derive(Debug, Clone)]
pub enum PendingAction {
    Flagged,
    Marked,
    Current(PathBuf),
    /// Continue deleting after the user confirmed a non-empty directory.
    Delete {
        paths: Vec<PathBuf>,
        index: usize,
        deleted: usize,
        errors: Vec<String>,
        all: bool,
    },
    /// Continue a Wdired commit after confirming an overwrite.
    WdiredCommit(WdiredCommit),
}

/// Rename operations and pending overwrite confirmations for Wdired.
#[derive(Debug, Clone)]
pub struct WdiredCommit {
    /// Entry index, source path, and target path for each changed name.
    renames: Vec<(usize, PathBuf, PathBuf)>,
    /// Positions in `renames` whose target already exists.
    conflicts: Vec<usize>,
    next_conflict: usize,
}

#[derive(Debug, Clone)]
struct RenameWork {
    /// Index of the corresponding entry in the old listing.
    plan: usize,
    original: PathBuf,
    current: PathBuf,
    target: PathBuf,
}

#[derive(Debug, Clone)]
struct DiredReload {
    dir: PathBuf,
    sort: SortKey,
    marks: Vec<(String, Mark)>,
    preferred: Option<String>,
    fallback: Vec<String>,
    default_point: Pos,
}

/// A pending multi-key prefix (`C-x`, `C-c`, `C-h`, or Dired's `*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Prefix {
    #[default]
    None,
    CtrlX,
    /// `C-x 4` (`C-x 4 b`).
    CtrlX4,
    /// `M-g` (window movement and numbering).
    MetaG,
    CtrlC,
    Help,
    /// Dired's `*` marking prefix (`* !`, `* .`).
    Star,
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

/// How long after a detected paste we keep folding queued keys into it, so a
/// paste delivered in chunks is not interpreted as commands.
const PASTE_LATCH: Duration = Duration::from_millis(50);

/// How long to wait before echoing an incomplete prefix key (`C-c-`), like
/// Emacs's `echo-keystrokes` (default one second).
const PREFIX_ECHO_DELAY: Duration = Duration::from_secs(1);

/// How long to show the other end of a copied region, like Emacs's
/// `indicate-copied-region`.
const COPY_FLASH: Duration = Duration::from_millis(220);

/// How long `read-answer` shows its "Please answer ..." message after an
/// unrecognized reply before asking again.
const CONFIRM_FLASH: Duration = Duration::from_secs(2);

/// Top-level application.
pub struct App {
    pub config: Config,
    pub theme: Theme,
    /// Colors used to draw the point, independent of the text under it.
    pub cursor_style: Style,
    pub buffers: Vec<Buf>,
    pub current: usize,
    pub previous: usize,
    pub(crate) windows: Vec<Window>,
    pub(crate) active_window: WindowId,
    pub(crate) window_tree: WindowTree,
    /// Buffer areas from the last draw, used for directional movement.
    window_areas: Vec<(WindowId, Rect)>,
    dividers: Vec<Divider>,
    divider_drag: Option<DividerDrag>,
    /// Whether the idle `M-g` number overlay is showing.
    show_window_numbers: bool,
    next_window_id: WindowId,
    /// Monotonic counter for the most recent buffer visit, for list ordering.
    visit_seq: u64,
    pub history: History,
    pub search: Search,
    pub message: Option<String>,
    pub kill_ring: KillRing,
    /// Height (in rows) of the buffer view, from the last draw.
    pub viewport_rows: usize,
    /// Width (in columns) of the buffer view, from the last draw.
    pub viewport_cols: usize,
    pub should_quit: bool,
    pub prompt: Prompt,
    /// Height of the minibuffer during the current prompt, never shrinking as
    /// the candidate list narrows.
    minibuffer_rows: u16,
    prefix: Prefix,
    /// A pending prefix key hint (`C-c-`), shown after an idle delay the way
    /// Emacs's `echo-keystrokes` does.
    prefix_echo: Option<(String, Instant)>,
    /// A confirmation waiting behind an invalid-answer message: the prompt to
    /// restore, and when to restore it if no key arrives first.
    confirm_flash: Option<(Prompt, Instant)>,
    /// Pending `M-w` indication: (position to restore, deadline).
    copy_flash: Option<(Pos, Instant)>,
    numeric_arg: Option<ArgState>,
    quoted: bool,
    last_command: LastCommand,
    last_yank: Option<(Pos, Pos)>,
    /// Mark within the active minibuffer input, as a char index.
    prompt_mark: Option<usize>,
    /// Range of the last prompt yank, for `M-y` (`yank-pop`).
    prompt_last_yank: Option<(usize, usize)>,
    /// True after `C-x` in a prompt, waiting for `C-x` (`C-x C-x`).
    prompt_cx: bool,
    dabbrev: Option<Dabbrev>,
    /// Whether kills/copies are written to the system clipboard (OSC 52).
    clipboard: bool,
    /// While set, key events are treated as part of an in-progress paste (a
    /// Windows paste delivered as individual keys; see `handle_term_event`).
    pasting_until: Option<Instant>,
    /// Whether Dired shows sizes human-readably (`ls -h`); `ui.human_sizes`.
    human_sizes: bool,
    /// Rows of padding kept above/below point before scrolling.
    scroll_margin: usize,
}

impl App {
    pub fn new(config: Config) -> Self {
        let theme = Theme::for_background(config.ui.background);
        let human_sizes = config.ui.human_sizes;
        let scroll_margin = config.ui.scroll_margin;
        let cursor_style = Style::fg(Color::parse(&config.ui.cursor_fg).unwrap_or(Color::Black))
            .bg(Color::parse(&config.ui.cursor_bg).unwrap_or(Color::Yellow));
        let mut app = App {
            config,
            theme,
            cursor_style,
            buffers: Vec::new(),
            current: 0,
            previous: 0,
            windows: Vec::new(),
            active_window: 0,
            window_tree: WindowTree::Leaf(0),
            window_areas: Vec::new(),
            dividers: Vec::new(),
            divider_drag: None,
            show_window_numbers: false,
            next_window_id: 1,
            visit_seq: 0,
            history: History::default(),
            search: Search::default(),
            message: None,
            kill_ring: KillRing::new(),
            viewport_rows: 20,
            viewport_cols: 80,
            should_quit: false,
            prompt: Prompt::None,
            minibuffer_rows: 1,
            prefix: Prefix::None,
            prefix_echo: None,
            confirm_flash: None,
            copy_flash: None,
            numeric_arg: None,
            quoted: false,
            last_command: LastCommand::Other,
            last_yank: None,
            prompt_mark: None,
            prompt_last_yank: None,
            prompt_cx: false,
            dabbrev: None,
            clipboard: true,
            pasting_until: None,
            human_sizes,
            scroll_margin,
        };
        let scratch = app.new_buffer("*scratch*".to_string(), Kind::Scratch, Buffer::new());
        app.buffers[scratch]
            .buffer
            .set_content(scratch_lines().into_iter().map(Line::plain).collect());
        app.current = scratch;
        app.previous = scratch;
        let id = app.next_window_id;
        app.next_window_id += 1;
        app.windows.push(Window {
            id,
            buffer: scratch,
            view: capture_view(&app.buffers[scratch].buffer),
        });
        app.active_window = id;
        app.window_tree = WindowTree::Leaf(id);
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
            dired_edit: None,
            hexl_width: None,
            last_visited: 0,
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
        if let Some(i) = self.buffers.iter().position(|b| b.name == name) {
            return Some(i);
        }
        // Fall back to a case-insensitive match.
        let lower = name.to_lowercase();
        self.buffers
            .iter()
            .position(|b| b.name.to_lowercase() == lower)
    }

    pub fn mode_line_name(&self) -> String {
        self.mode_line_name_for(self.current)
    }

    #[cfg(test)]
    pub fn mode_line_mode(&self) -> String {
        self.mode_line_mode_for(self.current)
    }

    pub fn mode_line_name_for(&self, idx: usize) -> String {
        self.buffers[idx].name.clone()
    }

    pub fn mode_line_mode_for(&self, idx: usize) -> String {
        match self.buffers[idx].kind {
            Kind::Scratch | Kind::File => "Fundamental".to_string(),
            Kind::Help => "Help".to_string(),
            // Emacs shows the listing order in the mode name, e.g. "Dired by name".
            Kind::Dired if self.buffers[idx].dired_edit.is_some() => "Editable Dired".to_string(),
            Kind::Dired => match self.buffers[idx].dired.as_ref() {
                Some(d) => format!("Dired by {}", d.sort.label()),
                None => "Dired".to_string(),
            },
            Kind::Hexl => "Hexl".to_string(),
        }
    }

    pub(crate) fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub(crate) fn window_tree(&self) -> &WindowTree {
        &self.window_tree
    }

    pub(crate) fn active_window_id(&self) -> WindowId {
        self.active_window
    }

    pub(crate) fn set_window_areas(&mut self, areas: Vec<(WindowId, Rect)>) {
        self.window_areas = areas;
    }

    pub(crate) fn set_dividers(&mut self, dividers: Vec<Divider>) {
        self.dividers = dividers;
    }

    pub(crate) fn show_window_numbers(&self) -> bool {
        self.show_window_numbers
    }

    /// The displayed number for a window: 1-9, then 0 for the tenth window.
    pub(crate) fn window_number(&self, id: WindowId) -> Option<u8> {
        let index = self.window_ids().iter().position(|window| *window == id)?;
        match index {
            0..=8 => Some(index as u8 + 1),
            9 => Some(0),
            _ => None,
        }
    }

    fn window_ids(&self) -> Vec<WindowId> {
        let mut ids = Vec::with_capacity(self.windows.len());
        window_ids(&self.window_tree, &mut ids);
        ids
    }

    fn save_active_view(&mut self) {
        let view = capture_view(&self.buffers[self.current].buffer);
        if let Some(window) = self.windows.iter_mut().find(|w| w.id == self.active_window) {
            window.buffer = self.current;
            window.view = view;
        }
    }

    fn note_buffer_visit(&mut self, idx: usize) {
        self.visit_seq += 1;
        self.buffers[idx].last_visited = self.visit_seq;
    }

    /// Column ranges to highlight in the hex view, so the byte under point is
    /// shown in both its hex and ASCII forms (like `hexl-mode`). Empty for
    /// non-hex buffers or when point is not over a byte.
    pub fn hexl_highlights(&self) -> Vec<(Pos, usize)> {
        let buf = self.current();
        if buf.kind != Kind::Hexl {
            return Vec::new();
        }
        let Some(width) = buf.hexl_width else {
            return Vec::new();
        };
        let point = buf.buffer.point();
        let line = buf.buffer.line(point.row).text();
        match hexl::byte_at(&line, point.col, width) {
            Some((hex_col, ascii_col)) => vec![
                (Pos::new(point.row, hex_col), 2),
                (Pos::new(point.row, ascii_col), 1),
            ],
            None => Vec::new(),
        }
    }

    fn switch_to(&mut self, idx: usize) {
        if idx >= self.buffers.len() || idx == self.current {
            return;
        }
        self.save_active_view();
        let old = self.current;
        self.previous = old;
        self.current = idx;
        if let Some(window) = self.windows.iter_mut().find(|w| w.id == self.active_window) {
            window.buffer = idx;
        }
        self.note_buffer_visit(idx);
        let cols = self.viewport_cols;
        let height = self.viewport_rows.max(1);
        let margin = self.scroll_margin;
        let buf = &mut self.buffers[idx].buffer;
        buf.set_width(cols);
        buf.ensure_point_visible(height, margin);
    }

    fn select_window(&mut self, id: WindowId) {
        if id == self.active_window {
            return;
        }
        let Some(pos) = self.windows.iter().position(|w| w.id == id) else {
            return;
        };
        self.save_active_view();
        let (buffer, view) = {
            let window = &self.windows[pos];
            (window.buffer, window.view)
        };
        if buffer != self.current {
            self.previous = self.current;
        }
        self.active_window = id;
        self.current = buffer;
        restore_view(&mut self.buffers[buffer].buffer, view);
        self.note_buffer_visit(buffer);
    }

    fn split_active_window(&mut self, axis: SplitAxis) {
        let too_small = match axis {
            SplitAxis::Below => self.viewport_rows < 3,
            SplitAxis::Right => self.viewport_cols < 4,
        };
        if too_small {
            self.message = Some("Window too small to split".to_string());
            return;
        }
        self.save_active_view();
        let old = self.active_window;
        let new_id = self.next_window_id;
        self.next_window_id += 1;
        self.windows.push(Window {
            id: new_id,
            buffer: self.current,
            view: capture_view(&self.buffers[self.current].buffer),
        });
        if !split_leaf(&mut self.window_tree, old, axis, new_id) {
            self.windows.pop();
        }
    }

    fn delete_active_window(&mut self) {
        if self.windows.len() <= 1 {
            self.message = Some("Only one window".to_string());
            return;
        }
        let old = self.active_window;
        let before = self.window_ids();
        let pos = before.iter().position(|id| *id == old).unwrap_or(0);
        let tree = std::mem::replace(&mut self.window_tree, WindowTree::Leaf(old));
        self.window_tree = remove_leaf(tree, old).unwrap_or(WindowTree::Leaf(old));
        self.windows.retain(|w| w.id != old);
        let after = self.window_ids();
        let Some(new_id) = after.get(pos).copied().or_else(|| after.last().copied()) else {
            return;
        };
        let (buffer, view) = {
            let window = self.windows.iter().find(|w| w.id == new_id).unwrap();
            (window.buffer, window.view)
        };
        self.active_window = new_id;
        self.current = buffer;
        restore_view(&mut self.buffers[buffer].buffer, view);
        self.note_buffer_visit(buffer);
    }

    fn delete_other_windows(&mut self) {
        if self.windows.len() <= 1 {
            self.message = Some("Only one window".to_string());
            return;
        }
        let active = self.active_window;
        self.save_active_view();
        self.windows.retain(|w| w.id == active);
        self.window_tree = WindowTree::Leaf(active);
    }

    fn other_window(&mut self) {
        let ids = self.window_ids();
        if ids.len() <= 1 {
            self.message = Some("Only one window".to_string());
            return;
        }
        let pos = ids
            .iter()
            .position(|id| *id == self.active_window)
            .unwrap_or(0);
        self.select_window(ids[(pos + 1) % ids.len()]);
    }

    fn next_window_id(&self) -> Option<WindowId> {
        let ids = self.window_ids();
        if ids.len() <= 1 {
            return None;
        }
        let pos = ids
            .iter()
            .position(|id| *id == self.active_window)
            .unwrap_or(0);
        Some(ids[(pos + 1) % ids.len()])
    }

    fn focus_window_number(&mut self, number: u8) {
        let index = if number == 0 {
            9
        } else {
            usize::from(number.saturating_sub(1))
        };
        if let Some(id) = self.window_ids().get(index).copied() {
            self.select_window(id);
        } else {
            self.message = Some(format!("No window {number}"));
        }
    }

    fn focus_direction(&mut self, direction: WindowDirection) {
        let Some(active) = self
            .window_areas
            .iter()
            .find(|(id, _)| *id == self.active_window)
            .map(|(_, area)| *area)
        else {
            self.message = Some("No window in that direction".to_string());
            return;
        };
        let active_left = u32::from(active.x);
        let active_top = u32::from(active.y);
        let active_right = active_left + u32::from(active.width);
        let active_bottom = active_top + u32::from(active.height);

        let mut best: Option<(u32, WindowId)> = None;
        for (id, area) in &self.window_areas {
            if *id == self.active_window {
                continue;
            }
            let left = u32::from(area.x);
            let top = u32::from(area.y);
            let right = left + u32::from(area.width);
            let bottom = top + u32::from(area.height);
            let (in_direction, primary, perpendicular) = match direction {
                WindowDirection::Right if left >= active_right => (
                    true,
                    left - active_right,
                    interval_gap(top, bottom, active_top, active_bottom),
                ),
                WindowDirection::Left if right <= active_left => (
                    true,
                    active_left - right,
                    interval_gap(top, bottom, active_top, active_bottom),
                ),
                WindowDirection::Down if top >= active_bottom => (
                    true,
                    top - active_bottom,
                    interval_gap(left, right, active_left, active_right),
                ),
                WindowDirection::Up if bottom <= active_top => (
                    true,
                    active_top - bottom,
                    interval_gap(left, right, active_left, active_right),
                ),
                _ => (false, 0, 0),
            };
            if !in_direction {
                continue;
            }
            let score = primary + perpendicular * 2;
            if best.is_none_or(|(best_score, _)| score < best_score) {
                best = Some((score, *id));
            }
        }
        if let Some((_, id)) = best {
            self.select_window(id);
        } else {
            self.message = Some(format!(
                "No window {}",
                match direction {
                    WindowDirection::Left => "to the left",
                    WindowDirection::Right => "to the right",
                    WindowDirection::Up => "above",
                    WindowDirection::Down => "below",
                }
            ));
        }
    }

    fn sensible_split_axis(&self) -> SplitAxis {
        if self.viewport_cols > self.viewport_rows {
            SplitAxis::Right
        } else {
            SplitAxis::Below
        }
    }

    fn switch_buffer_in_other_window(&mut self, idx: usize) {
        if idx >= self.buffers.len() {
            return;
        }
        if self.windows.len() == 1 {
            self.split_active_window(self.sensible_split_axis());
        }
        let Some(other) = self.next_window_id() else {
            return;
        };
        self.save_active_view();
        let old = self.current;
        let view = capture_view(&self.buffers[idx].buffer);
        if let Some(window) = self.windows.iter_mut().find(|w| w.id == other) {
            if window.buffer != idx {
                window.buffer = idx;
                window.view = view;
            }
        }
        self.previous = old;
        self.select_window(other);
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
        let active_killed = self.current == idx;
        self.save_active_view();
        self.buffers.remove(idx);
        for window in &mut self.windows {
            if window.buffer > idx {
                window.buffer -= 1;
            } else if window.buffer == idx {
                window.buffer = usize::MAX;
            }
        }
        let fallback = idx.min(self.buffers.len().saturating_sub(1));
        if self.current > idx {
            self.current -= 1;
        } else if self.current == idx {
            self.current = fallback;
        }
        if self.previous > idx {
            self.previous -= 1;
        } else if self.previous == idx {
            self.previous = fallback;
        }
        let fallback_view = capture_view(&self.buffers[fallback].buffer);
        for window in &mut self.windows {
            if window.buffer == usize::MAX {
                window.buffer = fallback;
                window.view = fallback_view;
            }
        }
        if let Some(window) = self.windows.iter_mut().find(|w| w.id == self.active_window) {
            window.buffer = self.current;
            if active_killed {
                window.view = fallback_view;
            }
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
        match Dired::read(&dir, self.config.ui.show_hidden, self.human_sizes) {
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
                let raw = file::to_lines(&bytes);
                let lines = match syntax::detect(&path) {
                    Some(lang) => syntax::highlight(lang, &raw, &self.theme.syntax),
                    None => raw.into_iter().map(Line::plain).collect(),
                };
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
        let width = hexl::offset_width(shown.len());
        let buffer = Buffer::read_only_from(lines);
        let idx = self.new_buffer(name, Kind::Hexl, buffer);
        self.buffers[idx].hexl_width = Some(width);
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

    /// Put point on the name of the entry called `name`, if it is listed.
    fn dired_goto_name(&mut self, idx: usize, name: &str) {
        let pos = self.buffers[idx].dired.as_ref().and_then(|d| {
            d.entries.iter().position(|e| e.name == name).map(|i| {
                let row = d.row_of(i);
                Pos::new(row, d.filename_col(row).unwrap_or(0))
            })
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

    /// Move point `times` lines down (`delta` +1) or up (-1), like Emacs'
    /// `dired-next-line`: move by physical line, then land on the file name
    /// when the target line is a file (so the header line is reachable).
    fn dired_move(&mut self, delta: isize, times: i32) {
        let idx = self.current;
        for _ in 0..times {
            let last = self.buffers[idx].buffer.last_row() as isize;
            let target = (self.buffers[idx].buffer.point().row as isize + delta).clamp(0, last);
            let target = target as usize;
            // Land on the file name, or at the line start on a non-file line
            // (the header) like `dired-move-to-filename`.
            let col = self.buffers[idx]
                .dired
                .as_ref()
                .and_then(|d| d.filename_col(target))
                .unwrap_or(0);
            self.buffers[idx].buffer.set_point(Pos::new(target, col));
        }
    }

    /// Move to the next (`delta` +1) or previous (-1) directory line (`>` /
    /// `<`), like Emacs' `dired-next-dirline` / `dired-prev-dirline`. Point
    /// stays put when there is no such line.
    fn dired_dir_line(&mut self, delta: isize) {
        let idx = self.current;
        let target = {
            let Some(d) = self.buffers[idx].dired.as_ref() else {
                return;
            };
            let len = d.entries.len() as isize;
            let row = self.buffers[idx].buffer.point().row;
            let start = match d.entry_index_at(row) {
                Some(i) => i as isize + delta,
                None if delta > 0 => 0,
                None => len - 1,
            };
            let mut r = start;
            let mut found = None;
            while r >= 0 && r < len {
                if d.entries[r as usize].is_dir {
                    found = Some(r as usize);
                    break;
                }
                r += delta;
            }
            found
        };
        if let Some(i) = target {
            let (row, col) = {
                let d = self.buffers[idx].dired.as_ref().unwrap();
                let row = d.row_of(i);
                (row, d.filename_col(row).unwrap_or(0))
            };
            self.buffers[idx].buffer.set_point(Pos::new(row, col));
        }
    }

    /// The parent path when point is on the synthetic `..` row.
    fn dired_parent_at_point(&self) -> Option<PathBuf> {
        let buf = self.current();
        let dired = buf.dired.as_ref()?;
        (dired.parent_row() == Some(buf.buffer.point().row))
            .then(|| dired.parent_path().map(Path::to_path_buf))
            .flatten()
    }

    /// Visit the file or directory at point.
    fn dired_visit(&mut self) {
        if let Some(parent) = self.dired_parent_at_point() {
            self.open_dired(parent);
            return;
        }
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
        let point = self.buffers[idx].buffer.point();
        let Some((dir, sort, names, point_index)) = self.buffers[idx].dired.as_ref().map(|d| {
            let names: Vec<String> = d.entries.iter().map(|e| e.name.clone()).collect();
            (d.dir.clone(), d.sort, names, d.entry_index_at(point.row))
        }) else {
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
        let preferred = point_index.map(|index| names[index].clone());
        let fallback = point_index
            .map(|index| {
                names[index + 1..]
                    .iter()
                    .chain(names[..index].iter().rev())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let reload = DiredReload {
            dir,
            sort,
            marks,
            preferred,
            fallback,
            default_point: point,
        };
        if self.reload_dired(idx, reload) {
            self.message = Some(format!(
                "Reverted {}",
                self.buffers[idx].dired.as_ref().unwrap().dir.display()
            ));
        }
    }

    /// Re-read `idx`'s directory, applying `marks`, preserving its sort order,
    /// and placing point on the first name in `preferred` or `fallback`.
    fn reload_dired(&mut self, idx: usize, reload: DiredReload) -> bool {
        let DiredReload {
            dir,
            sort,
            marks,
            preferred,
            fallback,
            default_point,
        } = reload;
        match Dired::read(&dir, self.config.ui.show_hidden, self.human_sizes) {
            Ok(mut dired) => {
                for entry in &mut dired.entries {
                    if let Some((_, mark)) = marks.iter().find(|(n, _)| *n == entry.name) {
                        entry.mark = *mark;
                    }
                }
                dired.set_sort(sort);
                let target_row = preferred.into_iter().chain(fallback).find_map(|name| {
                    dired
                        .entries
                        .iter()
                        .position(|e| e.name == name)
                        .map(|i| dired.row_of(i))
                });
                let target = target_row
                    .map(|row| Pos::new(row, dired.filename_col(row).unwrap_or(0)))
                    .unwrap_or(default_point);
                let lines = dired.render(&self.theme);
                self.buffers[idx].buffer.set_content(lines);
                self.buffers[idx].dired = Some(dired);
                self.buffers[idx].buffer.set_point(target);
                true
            }
            Err(err) => {
                self.message = Some(format!("Cannot read {}: {err}", dir.display()));
                false
            }
        }
    }

    // ---- wdired ------------------------------------------------------------

    fn wdired_active(&self) -> bool {
        self.current().dired_edit.is_some()
    }

    fn wdired_toggle(&mut self) {
        if self.wdired_active() {
            self.wdired_commit();
        } else {
            self.wdired_enter();
        }
    }

    fn wdired_enter(&mut self) {
        let idx = self.current;
        if self.buffers[idx].kind != Kind::Dired || self.buffers[idx].dired_edit.is_some() {
            return;
        }
        let theme = self.theme;
        let Some((old_lines, names, old_point)) = self.buffers[idx].dired.as_ref().map(|d| {
            (
                d.render(&theme),
                d.entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
                self.buffers[idx].buffer.point(),
            )
        }) else {
            self.message = Some("Not a Dired buffer".to_string());
            return;
        };
        let lines = self.buffers[idx]
            .dired
            .as_ref()
            .map(|d| d.render_with_names(&theme, &names))
            .unwrap_or_default();
        self.buffers[idx].buffer.set_content(lines);
        self.buffers[idx].buffer.set_point(old_point);
        self.buffers[idx].dired_edit = Some(DiredEdit {
            old_lines,
            old_point,
            names,
        });
        self.wdired_clamp_point();
        self.message =
            Some("Press C-c C-c or C-x C-q when finished; C-c ESC aborts changes".to_string());
    }

    fn wdired_abort(&mut self) {
        let idx = self.current;
        let Some(edit) = self.buffers[idx].dired_edit.take() else {
            self.message = Some("Not editing file names".to_string());
            return;
        };
        self.buffers[idx].buffer.set_content(edit.old_lines);
        self.buffers[idx].buffer.set_point(edit.old_point);
        self.message = Some("Changes aborted".to_string());
    }

    fn wdired_commit(&mut self) {
        let idx = self.current;
        let Some(edit) = self.buffers[idx].dired_edit.clone() else {
            self.message = Some("Not editing file names".to_string());
            return;
        };
        let Some(dired) = self.buffers[idx].dired.as_ref() else {
            return;
        };
        let dir = dired.dir.clone();
        let mut renames = Vec::new();
        for (i, name) in edit.names.iter().enumerate() {
            if *name == dired.entries[i].name {
                continue;
            }
            if let Err(err) = wdired_validate_name(name) {
                self.message = Some(err);
                return;
            }
            renames.push((i, dired.entries[i].path.clone(), dir.join(name)));
        }
        if renames.is_empty() {
            self.buffers[idx].dired_edit = None;
            self.buffers[idx].buffer.set_content(edit.old_lines);
            self.buffers[idx].buffer.set_point(edit.old_point);
            self.message = Some("(No changes to be performed)".to_string());
            return;
        }

        let mut targets = HashMap::new();
        for (position, (_, _, target)) in renames.iter().enumerate() {
            if targets.insert(wdired_path_key(target), position).is_some() {
                self.message = Some("Trying to rename 2 files to the same name".to_string());
                return;
            }
        }

        let sources: HashSet<String> = renames
            .iter()
            .map(|(_, old, _)| wdired_path_key(old))
            .collect();
        let conflicts: Vec<usize> = renames
            .iter()
            .enumerate()
            .filter(|(_, (_, old, target))| {
                wdired_target_exists(target)
                    && !wdired_same_file(old, target)
                    && !sources.contains(&wdired_path_key(target))
            })
            .map(|(position, _)| position)
            .collect();
        let commit = WdiredCommit {
            renames,
            conflicts,
            next_conflict: 0,
        };
        self.ask_wdired_conflict(commit);
    }

    fn ask_wdired_conflict(&mut self, commit: WdiredCommit) {
        if let Some(&position) = commit.conflicts.get(commit.next_conflict) {
            let target = commit.renames[position].2.display();
            self.prompt = Prompt::Confirm {
                message: format!("Overwrite {}? (yes or no) ", target),
                input: String::new(),
                cursor: 0,
                action: PendingAction::WdiredCommit(commit),
            };
        } else {
            self.run_wdired_renames(commit);
        }
    }

    fn run_wdired_renames(&mut self, commit: WdiredCommit) {
        let idx = self.current;
        let Some((dir, sort, old_names, old_marks, point_index, default_point)) =
            self.buffers[idx].dired.as_ref().map(|d| {
                let entries = &d.entries;
                (
                    d.dir.clone(),
                    d.sort,
                    entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
                    entries
                        .iter()
                        .map(|e| (e.name.clone(), e.path.clone(), e.mark))
                        .collect::<Vec<_>>(),
                    d.entry_index_at(self.buffers[idx].buffer.point().row),
                    self.buffers[idx].buffer.point(),
                )
            })
        else {
            return;
        };

        let overwrite_keys: HashSet<String> = commit
            .conflicts
            .iter()
            .map(|position| wdired_path_key(&commit.renames[*position].2))
            .collect();
        let mut pending: Vec<RenameWork> = commit
            .renames
            .iter()
            .map(|(plan, old, target)| RenameWork {
                plan: *plan,
                original: old.clone(),
                current: old.clone(),
                target: target.clone(),
            })
            .collect();
        let mut sources: HashMap<String, usize> = pending
            .iter()
            .map(|work| (wdired_path_key(&work.current), work.plan))
            .collect();
        let mut successful: Vec<(usize, PathBuf, PathBuf)> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        while !pending.is_empty() {
            let mut residue = Vec::new();
            let mut progressed = false;
            for mut work in pending.drain(..) {
                let current_key = wdired_path_key(&work.current);
                let target_key = wdired_path_key(&work.target);
                let target_exists = wdired_target_exists(&work.target);
                let same_file = target_exists && wdired_same_file(&work.current, &work.target);
                let occupied_by_source = sources
                    .get(&target_key)
                    .is_some_and(|plan| *plan != work.plan);

                if target_exists && !same_file && occupied_by_source {
                    residue.push(work);
                    continue;
                }
                if target_exists && !same_file && !overwrite_keys.contains(&target_key) {
                    errors.push(format!("{} already exists", work.target.display()));
                    sources.remove(&current_key);
                    continue;
                }
                if same_file {
                    match wdired_temp_path(&work.current) {
                        Ok(temp) => match fs::rename(&work.current, &temp) {
                            Ok(()) => {
                                sources.remove(&current_key);
                                work.current = temp;
                                sources.insert(wdired_path_key(&work.current), work.plan);
                                residue.push(work);
                                progressed = true;
                            }
                            Err(err) => {
                                errors.push(format!(
                                    "Rename {} failed: {err}",
                                    work.current.display()
                                ));
                                sources.remove(&current_key);
                            }
                        },
                        Err(err) => {
                            errors.push(format!("Cannot create temporary name: {err}"));
                            sources.remove(&current_key);
                        }
                    }
                    continue;
                }
                let parent_ready = match work.target.parent() {
                    Some(parent) => fs::create_dir_all(parent).is_ok(),
                    None => true,
                };
                if !parent_ready {
                    errors.push(format!(
                        "Cannot create directory for {}",
                        work.target.display()
                    ));
                    sources.remove(&current_key);
                    continue;
                }
                let backup = if target_exists {
                    match wdired_temp_path(&work.target)
                        .and_then(|temp| fs::rename(&work.target, &temp).map(|()| temp))
                    {
                        Ok(temp) => Some(temp),
                        Err(err) => {
                            errors
                                .push(format!("Cannot overwrite {}: {err}", work.target.display()));
                            sources.remove(&current_key);
                            continue;
                        }
                    }
                } else {
                    None
                };
                match fs::rename(&work.current, &work.target) {
                    Ok(()) => {
                        if let Some(backup) = backup {
                            remove_wdired_path(&backup);
                        }
                        sources.remove(&current_key);
                        successful.push((work.plan, work.original, work.target));
                        progressed = true;
                    }
                    Err(err) => {
                        if let Some(backup) = backup {
                            let _ = fs::rename(&backup, &work.target);
                        }
                        if work.current != work.original
                            && !wdired_target_exists(&work.original)
                            && fs::rename(&work.current, &work.original).is_ok()
                        {
                            work.current = work.original.clone();
                        }
                        if work.current == work.original {
                            errors.push(format!(
                                "Rename {} to {} failed: {err}",
                                work.original.display(),
                                work.target.display()
                            ));
                        } else {
                            errors.push(format!(
                                "Rename {} to {} failed: {err}; left at {}",
                                work.original.display(),
                                work.target.display(),
                                work.current.display()
                            ));
                        }
                        sources.remove(&current_key);
                    }
                }
            }

            if residue.is_empty() {
                break;
            }
            if !progressed {
                let mut work = residue.remove(0);
                let current_key = wdired_path_key(&work.current);
                match wdired_temp_path(&work.current)
                    .and_then(|temp| fs::rename(&work.current, &temp).map(|()| temp))
                {
                    Ok(temp) => {
                        sources.remove(&current_key);
                        work.current = temp;
                        sources.insert(wdired_path_key(&work.current), work.plan);
                        residue.push(work);
                    }
                    Err(err) => {
                        errors.push(format!(
                            "Cannot break rename cycle at {}: {err}",
                            work.current.display()
                        ));
                        sources.remove(&current_key);
                    }
                }
            }
            pending = residue;
        }

        for (_, old, new) in &successful {
            self.rename_open_buffers(old, new);
        }

        let mut successful_by_entry: HashMap<usize, (&PathBuf, &PathBuf)> = HashMap::new();
        for (plan, old, target) in &successful {
            successful_by_entry.insert(*plan, (old, target));
        }
        let overwritten: HashSet<String> = commit
            .conflicts
            .iter()
            .map(|position| wdired_path_key(&commit.renames[*position].2))
            .collect();
        let mut marks: Vec<(String, Mark)> = Vec::new();
        for (i, (old_name, old_path, mark)) in old_marks.iter().enumerate() {
            if successful_by_entry.contains_key(&i) {
                continue;
            }
            if overwritten.contains(&wdired_path_key(old_path)) {
                continue;
            }
            marks.push((old_name.clone(), *mark));
        }
        for (plan, _, target) in &successful {
            if target.parent() == Some(dir.as_path()) {
                if let Some(name) = target.file_name() {
                    marks.push((name.to_string_lossy().into_owned(), old_marks[*plan].2));
                }
            }
        }

        let preferred = point_index.and_then(|index| {
            successful_by_entry
                .get(&index)
                .and_then(|(_, target)| {
                    (target.parent() == Some(dir.as_path()))
                        .then(|| target.file_name().map(|n| n.to_string_lossy().into_owned()))
                        .flatten()
                })
                .or_else(|| {
                    (!successful_by_entry.contains_key(&index)).then(|| old_names[index].clone())
                })
        });
        let fallback = point_index
            .map(|index| {
                old_names[index + 1..]
                    .iter()
                    .enumerate()
                    .map(|(offset, name)| {
                        let entry = index + 1 + offset;
                        wdired_final_name(name, entry, &successful_by_entry, &dir)
                    })
                    .chain(
                        old_names[..index]
                            .iter()
                            .enumerate()
                            .rev()
                            .map(|(entry, name)| {
                                wdired_final_name(name, entry, &successful_by_entry, &dir)
                            }),
                    )
                    .collect()
            })
            .unwrap_or_default();

        self.buffers[idx].dired_edit = None;
        let renamed = successful.len();
        let reloaded = self.reload_dired(
            idx,
            DiredReload {
                dir,
                sort,
                marks,
                preferred,
                fallback,
                default_point,
            },
        );
        if !reloaded {
            return;
        }
        self.message = Some(if errors.is_empty() {
            format!("Renamed {renamed} file(s)")
        } else if renamed == 0 {
            format!("Rename failed: {}", errors[0])
        } else {
            format!(
                "Renamed {renamed} file(s); {} failed: {}",
                errors.len(),
                errors[0]
            )
        });
    }

    fn wdired_name_range(&self) -> Option<(usize, usize, usize)> {
        let buf = self.current();
        let edit = buf.dired_edit.as_ref()?;
        let dired = buf.dired.as_ref()?;
        let row = buf.buffer.point().row;
        let i = dired.entry_index_at(row)?;
        let start = dired.filename_col(row)?;
        let end = start + edit.names.get(i)?.chars().count();
        Some((i, start, end))
    }

    fn wdired_clamp_point(&mut self) {
        let Some((_, start, end)) = self.wdired_name_range() else {
            return;
        };
        let point = self.current().buffer.point();
        let col = point.col.clamp(start, end);
        if col != point.col {
            self.current_mut()
                .buffer
                .set_point(Pos::new(point.row, col));
        }
    }

    fn wdired_redraw_name(&mut self, i: usize, cursor_col: usize) {
        let idx = self.current;
        let theme = self.theme;
        let Some((row, line)) = ({
            let buf = &self.buffers[idx];
            let dired = buf.dired.as_ref();
            let name = buf
                .dired_edit
                .as_ref()
                .and_then(|edit| edit.names.get(i))
                .cloned();
            dired.zip(name).map(|(dired, name)| {
                (
                    dired.row_of(i),
                    dired.render_line_with_name(i, &name, &theme),
                )
            })
        }) else {
            return;
        };
        self.buffers[idx].buffer.set_line(row, line);
        self.buffers[idx]
            .buffer
            .set_point(Pos::new(row, cursor_col));
    }

    fn wdired_store_name(&mut self, i: usize, start: usize, chars: &[char], offset: usize) {
        let idx = self.current;
        if let Some(edit) = self.buffers[idx].dired_edit.as_mut() {
            edit.names[i] = chars.iter().collect();
        }
        self.wdired_redraw_name(i, start + offset);
    }

    fn wdired_insert_text(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return true;
        }
        if text.chars().any(|c| matches!(c, '\n' | '\r' | '\0')) {
            self.message = Some("File names cannot contain newlines".to_string());
            return true;
        }
        let Some((i, start, _)) = self.wdired_name_range() else {
            self.message = Some("Text is read-only".to_string());
            return true;
        };
        let mut chars: Vec<char> = self.current().dired_edit.as_ref().unwrap().names[i]
            .chars()
            .collect();
        let offset = self
            .current()
            .buffer
            .point()
            .col
            .saturating_sub(start)
            .min(chars.len());
        let inserted = text.chars().count();
        chars.splice(offset..offset, text.chars());
        self.wdired_store_name(i, start, &chars, offset + inserted);
        true
    }

    fn wdired_delete_backward(&mut self) -> bool {
        let Some((i, start, _)) = self.wdired_name_range() else {
            self.message = Some("Text is read-only".to_string());
            return true;
        };
        let mut chars: Vec<char> = self.current().dired_edit.as_ref().unwrap().names[i]
            .chars()
            .collect();
        let offset = self
            .current()
            .buffer
            .point()
            .col
            .saturating_sub(start)
            .min(chars.len());
        if offset == 0 {
            self.message = Some("Beginning of file name".to_string());
            return true;
        }
        chars.remove(offset - 1);
        self.wdired_store_name(i, start, &chars, offset - 1);
        true
    }

    fn wdired_delete_forward(&mut self) -> bool {
        let Some((i, start, _)) = self.wdired_name_range() else {
            self.message = Some("Text is read-only".to_string());
            return true;
        };
        let mut chars: Vec<char> = self.current().dired_edit.as_ref().unwrap().names[i]
            .chars()
            .collect();
        let offset = self
            .current()
            .buffer
            .point()
            .col
            .saturating_sub(start)
            .min(chars.len());
        if offset >= chars.len() {
            self.message = Some("End of file name".to_string());
            return true;
        }
        chars.remove(offset);
        self.wdired_store_name(i, start, &chars, offset);
        true
    }

    fn wdired_kill_line(&mut self, prev: LastCommand) -> bool {
        let Some((i, start, _)) = self.wdired_name_range() else {
            self.message = Some("Text is read-only".to_string());
            return true;
        };
        let mut chars: Vec<char> = self.current().dired_edit.as_ref().unwrap().names[i]
            .chars()
            .collect();
        let offset = self
            .current()
            .buffer
            .point()
            .col
            .saturating_sub(start)
            .min(chars.len());
        let killed: String = chars[offset..].iter().collect();
        chars.truncate(offset);
        self.wdired_store_name(i, start, &chars, offset);
        if !killed.is_empty() {
            self.kill_new(&killed, prev == LastCommand::Kill);
            self.last_command = LastCommand::Kill;
        }
        true
    }

    fn wdired_kill_word(&mut self, backward: bool, prev: LastCommand) -> bool {
        let Some((i, start, _)) = self.wdired_name_range() else {
            self.message = Some("Text is read-only".to_string());
            return true;
        };
        let mut chars: Vec<char> = self.current().dired_edit.as_ref().unwrap().names[i]
            .chars()
            .collect();
        let point = self
            .current()
            .buffer
            .point()
            .col
            .saturating_sub(start)
            .min(chars.len());
        let (from, to) = if backward {
            let mut from = point;
            while from > 0 && !wdired_word(chars[from - 1]) {
                from -= 1;
            }
            while from > 0 && wdired_word(chars[from - 1]) {
                from -= 1;
            }
            (from, point)
        } else {
            let mut to = point;
            while to < chars.len() && !wdired_word(chars[to]) {
                to += 1;
            }
            while to < chars.len() && wdired_word(chars[to]) {
                to += 1;
            }
            (point, to)
        };
        if from == to {
            return true;
        }
        let killed: String = chars[from..to].iter().collect();
        chars.drain(from..to);
        self.wdired_store_name(i, start, &chars, from);
        self.kill_new(&killed, prev == LastCommand::Kill);
        self.last_command = LastCommand::Kill;
        true
    }

    fn wdired_yank(&mut self) -> bool {
        let Some(text) = self.kill_ring.current() else {
            self.message = Some("Kill ring is empty".to_string());
            return true;
        };
        self.wdired_insert_text(&text)
    }

    fn handle_wdired_key(&mut self, key: KeyEvent) -> bool {
        let prev = self.last_command;
        self.last_command = LastCommand::Other;
        match keymap::translate(key) {
            Action::SelfInsert(c) => self.wdired_insert_text(&c.to_string()),
            Action::DeleteBackward => self.wdired_delete_backward(),
            Action::DeleteForward => self.wdired_delete_forward(),
            Action::KillLine => self.wdired_kill_line(prev),
            Action::BackwardKillWord => self.wdired_kill_word(true, prev),
            Action::KillWord => self.wdired_kill_word(false, prev),
            Action::Yank => self.wdired_yank(),
            Action::Open | Action::Complete | Action::OpenLine => {
                self.message = Some(
                    keymap::describe(key)
                        .map(|key| format!("{key} is undefined"))
                        .unwrap_or_default(),
                );
                true
            }
            _ => false,
        }
    }

    /// Cycle the listing order name -> date -> size (`s`).
    fn dired_cycle_sort(&mut self) {
        let idx = self.current;
        if self.buffers[idx].dired.is_none() {
            return;
        }
        // Remember the file under point so it stays under point.
        let anchor = self.dired_entry_at_point().and_then(|i| {
            self.buffers[idx]
                .dired
                .as_ref()
                .map(|d| d.entries[i].name.clone())
        });
        let theme = self.theme;
        let (sort, lines, target) = {
            let dired = self.buffers[idx].dired.as_mut().unwrap();
            dired.set_sort(dired.sort.next());
            let lines = dired.render(&theme);
            let target = anchor.and_then(|name| {
                dired.entries.iter().position(|e| e.name == name).map(|i| {
                    let row = dired.row_of(i);
                    (row, dired.filename_col(row).unwrap_or(0))
                })
            });
            (dired.sort, lines, target)
        };
        self.buffers[idx].buffer.set_content(lines);
        match target {
            Some((row, col)) => self.buffers[idx].buffer.set_point(Pos::new(row, col)),
            None => self.goto_first_file(idx),
        }
        self.message = Some(format!("Sorting by {}", sort.label()));
    }

    /// Kill the current Dired buffer (`q`).
    fn dired_quit(&mut self) {
        let idx = self.current;
        self.kill_buffer(idx);
    }

    /// Set the mark of the entry at point and move `advance` file lines.
    fn dired_set_mark(&mut self, mark: Mark, advance: isize) {
        let Some(i) = self.dired_entry_at_point() else {
            self.message = Some("No file on this line".to_string());
            return;
        };
        let idx = self.current;
        if self.buffers[idx].dired.as_ref().unwrap().entries[i].mark != mark {
            self.buffers[idx].dired.as_mut().unwrap().entries[i].mark = mark;
            let theme = self.theme;
            let row = self.buffers[idx].dired.as_ref().unwrap().row_of(i);
            let line = self.buffers[idx]
                .dired
                .as_ref()
                .unwrap()
                .render_line(i, &theme);
            self.buffers[idx].buffer.set_line(row, line);
        }
        if advance != 0 {
            self.dired_move(advance, 1);
        }
    }

    /// Swap `*` marks with unmarked (`t`).
    fn dired_toggle_marks(&mut self) {
        let idx = self.current;
        if let Some(d) = self.buffers[idx].dired.as_mut() {
            for entry in &mut d.entries {
                entry.mark = match entry.mark {
                    Mark::None => Mark::Marked,
                    Mark::Marked => Mark::None,
                    Mark::Flagged => Mark::Flagged,
                };
            }
        }
        self.dired_rebuild(idx);
    }

    /// Remove every mark (`U`).
    fn dired_unmark_all(&mut self) {
        let idx = self.current;
        if let Some(d) = self.buffers[idx].dired.as_mut() {
            for entry in &mut d.entries {
                entry.mark = Mark::None;
            }
        }
        self.dired_rebuild(idx);
    }

    /// Prompt for an extension whose files are then marked (`* .`). The
    /// current file's extension is offered as the default.
    fn start_mark_extension(&mut self) {
        let input = self
            .dired_entry_at_point()
            .and_then(|i| {
                self.current()
                    .dired
                    .as_ref()
                    .and_then(|d| d.entries[i].path.extension())
                    .map(|e| e.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        self.prompt = Prompt::MarkExtension {
            cursor: input.chars().count(),
            input,
        };
    }

    /// Mark every file whose name ends in `.<ext>` (`* .`).
    fn mark_by_extension(&mut self, ext: &str) {
        let ext = ext.trim().trim_start_matches('.');
        if ext.is_empty() {
            self.message = Some("No extension given".to_string());
            return;
        }
        let idx = self.current;
        let suffix = format!(".{}", ext.to_lowercase());
        let mut count = 0usize;
        if let Some(d) = self.buffers[idx].dired.as_mut() {
            for entry in &mut d.entries {
                if entry.name.to_lowercase().ends_with(&suffix) {
                    entry.mark = Mark::Marked;
                    count += 1;
                }
            }
        }
        self.dired_rebuild(idx);
        self.message = Some(format!("Marked {count} file(s)"));
    }

    /// Flag for deletion the files matching `pred` (`#` / `~`); a numeric
    /// argument unmarks them instead, like `dired-flag-auto-save-files` and
    /// `dired-flag-backup-files`.
    fn dired_flag_files(&mut self, what: &str, pred: impl Fn(&str) -> bool) {
        let unflag = self.numeric_arg.take().is_some();
        let idx = self.current;
        let mark = if unflag { Mark::None } else { Mark::Flagged };
        let mut count = 0usize;
        if let Some(d) = self.buffers[idx].dired.as_mut() {
            for entry in &mut d.entries {
                if !entry.is_dir && pred(&entry.name) {
                    entry.mark = mark;
                    count += 1;
                }
            }
        }
        self.dired_rebuild(idx);
        self.message = Some(format!(
            "{} {count} {what}",
            if unflag { "Unmarked" } else { "Flagged" }
        ));
    }

    /// Re-render a listing in place, keeping point.
    fn dired_rebuild(&mut self, idx: usize) {
        let theme = self.theme;
        let lines = self.buffers[idx]
            .dired
            .as_ref()
            .map(|d| d.render(&theme))
            .unwrap_or_default();
        let pos = self.buffers[idx].buffer.point();
        self.buffers[idx].buffer.set_content(lines);
        self.buffers[idx].buffer.set_point(pos);
    }

    /// Paths of every entry carrying `mark`.
    fn dired_paths(&self, mark: Mark) -> Vec<PathBuf> {
        self.current()
            .dired
            .as_ref()
            .map(|d| {
                d.entries
                    .iter()
                    .filter(|e| e.mark == mark)
                    .map(|e| e.path.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Copy the marked (or current) file names to the kill ring (`w`). With a
    /// numeric argument, copy absolute paths.
    fn dired_copy_names(&mut self) {
        // On the `..` row, copy the parent directory itself (absolute).
        if let Some(parent) = self.dired_parent_at_point() {
            self.numeric_arg = None;
            let mut text = parent.display().to_string();
            if !text.ends_with(std::path::MAIN_SEPARATOR) {
                text.push(std::path::MAIN_SEPARATOR);
            }
            self.kill_new(&text, false);
            self.message = Some(text);
            return;
        }
        // On a directory header line, copy the directory itself (absolute),
        // ignoring marks, like `dired-copy-filename-as-kill`.
        if self.dired_entry_at_point().is_none() {
            if let Some(dir) = self.current().dired.as_ref().map(|d| d.dir.clone()) {
                let mut text = dir.display().to_string();
                if !text.ends_with(std::path::MAIN_SEPARATOR) {
                    text.push(std::path::MAIN_SEPARATOR);
                }
                self.kill_new(&text, false);
                self.message = Some(text);
                return;
            }
        }

        let absolute = self.numeric_arg.take().is_some();
        let marked = self.dired_paths(Mark::Marked);
        let chosen: Vec<PathBuf> = if !marked.is_empty() {
            marked
        } else if let Some(i) = self.dired_entry_at_point() {
            vec![self.current().dired.as_ref().unwrap().entries[i]
                .path
                .clone()]
        } else {
            self.message = Some("No file on this line".to_string());
            return;
        };
        let names: Vec<String> = chosen
            .iter()
            .map(|p| {
                if absolute {
                    p.display().to_string()
                } else {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.display().to_string())
                }
            })
            .collect();
        let text = names.join(" ");
        self.kill_new(&text, false);
        // Emacs echoes the copied names (`(message "%s" string)`).
        self.message = Some(text);
    }

    /// Prompt for a new name for the marked files, or the file at point (`R`).
    fn start_rename(&mut self) {
        self.start_transfer(false);
    }

    /// Prompt for a destination for the marked files, or the file at point
    /// (`C`, `dired-do-copy`).
    fn start_copy(&mut self) {
        self.start_transfer(true);
    }

    fn start_transfer(&mut self, copy: bool) {
        // A copy and a group rename ask for a target directory. Rename starts
        // from the current directory plus the file name when one file is
        // involved.
        let marked = self.dired_paths(Mark::Marked);
        let dir = self.default_directory();
        let at_point = self.dired_entry_at_point().and_then(|i| {
            self.current()
                .dired
                .as_ref()
                .map(|d| d.entries[i].path.clone())
        });
        let single = if marked.len() == 1 {
            marked.first().cloned()
        } else {
            at_point
        };
        let input = if copy || marked.len() > 1 {
            directory_input(&dir)
        } else {
            single
                .and_then(|path| {
                    path.file_name()
                        .map(|n| format!("{}{}", directory_input(&dir), n.to_string_lossy()))
                })
                .unwrap_or_default()
        };
        self.prompt = Prompt::Rename {
            cursor: input.chars().count(),
            input,
            selected: None,
            copy,
        };
    }

    /// Rename/move the marked files (or the file at point) to `target` (`R`).
    fn commit_rename(&mut self, target: &str) {
        let target = target.trim();
        let marked = self.dired_paths(Mark::Marked);
        let sources: Vec<PathBuf> = if !marked.is_empty() {
            marked
        } else if let Some(i) = self.dired_entry_at_point() {
            vec![self.current().dired.as_ref().unwrap().entries[i]
                .path
                .clone()]
        } else {
            self.message = Some("No file on this line".to_string());
            return;
        };
        if target.is_empty() {
            self.message = Some("No target name given".to_string());
            return;
        }
        let dest = self.expand_path(target);
        // A single file is renamed to `dest`; several files (or an existing
        // directory) are moved into `dest` as a directory.
        let into_dir = sources.len() > 1 || dest.is_dir();
        if sources.len() > 1 && !dest.is_dir() {
            self.message = Some(format!("Not a directory: {}", dest.display()));
            return;
        }

        let mut renamed: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut errors = 0usize;
        for src in &sources {
            let to = if into_dir {
                match src.file_name() {
                    Some(name) => dest.join(name),
                    None => {
                        errors += 1;
                        continue;
                    }
                }
            } else {
                dest.clone()
            };
            match fs::rename(src, &to) {
                Ok(()) => renamed.push((src.clone(), to)),
                Err(_) => errors += 1,
            }
        }
        for (old, new) in &renamed {
            self.rename_open_buffers(old, new);
        }
        self.dired_revert();
        // Keep point on the renamed file when it is still in this listing
        // (i.e. it was renamed in place rather than moved elsewhere).
        let idx = self.current;
        if let Some(dir) = self.buffers[idx].dired.as_ref().map(|d| d.dir.clone()) {
            if let Some(name) = renamed.iter().find_map(|(_, new)| {
                (new.parent() == Some(dir.as_path()))
                    .then(|| new.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .flatten()
            }) {
                self.dired_goto_name(idx, &name);
            }
        }
        self.message = Some(if errors == 0 {
            match renamed.as_slice() {
                [(_, new)] => format!("Renamed to {}", new.display()),
                _ => format!("Renamed {} file(s)", renamed.len()),
            }
        } else {
            format!("Renamed {} file(s); {errors} failed", renamed.len())
        });
    }

    /// Copy the marked files (or the file at point) to `target` (`C`,
    /// `dired-do-copy`).
    fn commit_copy(&mut self, target: &str) {
        let target = target.trim();
        let marked = self.dired_paths(Mark::Marked);
        let sources: Vec<PathBuf> = if !marked.is_empty() {
            marked
        } else if let Some(i) = self.dired_entry_at_point() {
            vec![self.current().dired.as_ref().unwrap().entries[i]
                .path
                .clone()]
        } else {
            self.message = Some("No file on this line".to_string());
            return;
        };
        if target.is_empty() {
            self.message = Some("No target name given".to_string());
            return;
        }
        let dest = self.expand_path(target);
        // `C` always copies into a target directory, keeping each file name.
        if !dest.is_dir() {
            self.message = Some(format!("Not a directory: {}", dest.display()));
            return;
        }

        let mut copied: Vec<PathBuf> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        for src in &sources {
            let to = match src.file_name() {
                Some(name) => dest.join(name),
                None => {
                    errors.push(format!("{} has no file name", src.display()));
                    continue;
                }
            };
            if to.exists() {
                errors.push(format!("{} already exists", to.display()));
                continue;
            }
            match copy_path(src, &to) {
                Ok(()) => copied.push(to),
                Err(err) => errors.push(format!("{}: {err}", src.display())),
            }
        }
        self.dired_revert();
        // Keep point on a copy that landed in this directory.
        let idx = self.current;
        if let Some(dir) = self.buffers[idx].dired.as_ref().map(|d| d.dir.clone()) {
            if let Some(name) = copied.iter().find_map(|new| {
                (new.parent() == Some(dir.as_path()))
                    .then(|| new.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .flatten()
            }) {
                self.dired_goto_name(idx, &name);
            }
        }
        self.message = Some(match (copied.as_slice(), errors.is_empty()) {
            ([new], true) => format!("Copied to {}", new.display()),
            (_, true) => format!("Copied {} file(s)", copied.len()),
            ([], false) => format!("Copy failed: {}", errors.join("; ")),
            (_, false) => format!("Copied {} file(s); {} failed", copied.len(), errors.len()),
        });
    }

    /// Prompt for a directory to create (`+`), defaulting to the current
    /// directory.
    fn start_create_directory(&mut self) {
        let mut input = self.default_directory().display().to_string();
        if !input.ends_with(std::path::MAIN_SEPARATOR) {
            input.push(std::path::MAIN_SEPARATOR);
        }
        self.prompt = Prompt::CreateDirectory {
            cursor: input.chars().count(),
            input,
            selected: None,
        };
    }

    /// Create the directory `target` (`+`), like `dired-create-directory`.
    fn commit_create_directory(&mut self, target: &str) {
        let target = target.trim();
        if target.is_empty() {
            self.message = Some("No directory name given".to_string());
            return;
        }
        let path = self.expand_path(target);
        if path.exists() {
            self.message = Some(format!("{} already exists", path.display()));
            return;
        }
        match fs::create_dir_all(&path) {
            Ok(()) => {
                self.dired_revert();
                if let Some(name) = path.file_name() {
                    let name = name.to_string_lossy().into_owned();
                    let idx = self.current;
                    self.dired_goto_name(idx, &name);
                }
                self.message = Some(format!("Created {}", path.display()));
            }
            Err(err) => self.message = Some(format!("Cannot create {}: {err}", path.display())),
        }
    }

    /// Point open file/hexl buffers at `new` after their path was renamed.
    fn rename_open_buffers(&mut self, old: &Path, new: &Path) {
        for buf in &mut self.buffers {
            let path = Path::new(&buf.name);
            if path == old {
                buf.name = new.display().to_string();
            } else if let Ok(suffix) = path.strip_prefix(old) {
                if !suffix.as_os_str().is_empty() {
                    buf.name = new.join(suffix).display().to_string();
                }
            }
        }
    }

    /// Ask before deleting the files carrying `mark`.
    fn dired_start_delete(&mut self, mark: Mark) {
        let count = self.dired_paths(mark).len();
        if count == 0 {
            // Like `dired-do-delete` with no marks: offer the file at point.
            if mark == Mark::Marked {
                if let Some((name, path)) = self.dired_entry_at_point().and_then(|i| {
                    self.current()
                        .dired
                        .as_ref()
                        .map(|d| (d.entries[i].name.clone(), d.entries[i].path.clone()))
                }) {
                    self.prompt = Prompt::Confirm {
                        message: format!("Delete {name} (yes or no) "),
                        input: String::new(),
                        cursor: 0,
                        action: PendingAction::Current(path),
                    };
                    return;
                }
            }
            self.message = Some(
                match mark {
                    Mark::Flagged => "No files flagged for deletion",
                    _ => "No files marked",
                }
                .to_string(),
            );
            return;
        }
        let (action, what) = match mark {
            Mark::Flagged => (PendingAction::Flagged, "flagged"),
            _ => (PendingAction::Marked, "marked"),
        };
        self.prompt = Prompt::Confirm {
            message: format!("Delete {count} {what} file(s)? (yes or no) "),
            input: String::new(),
            cursor: 0,
            action,
        };
    }

    /// Run a confirmed action.
    fn run_pending(&mut self, action: PendingAction) {
        let mut deleted = 0;
        let mut errors = Vec::new();
        let mut all = false;
        let (paths, index) = match action {
            PendingAction::Flagged => (self.dired_paths(Mark::Flagged), 0),
            PendingAction::Marked => (self.dired_paths(Mark::Marked), 0),
            PendingAction::Current(path) => (vec![path], 0),
            PendingAction::Delete {
                paths,
                index,
                deleted: already_deleted,
                errors: previous_errors,
                all: delete_all,
            } => {
                deleted = already_deleted;
                errors = previous_errors;
                all = delete_all;
                delete_one(&paths[index], &mut deleted, &mut errors);
                (paths, index + 1)
            }
            PendingAction::WdiredCommit(mut commit) => {
                commit.next_conflict += 1;
                self.ask_wdired_conflict(commit);
                return;
            }
        };
        self.continue_delete(paths, index, deleted, errors, all);
    }

    /// Delete paths one at a time, asking before entering a non-empty
    /// directory, like `dired-delete-file`.
    fn continue_delete(
        &mut self,
        paths: Vec<PathBuf>,
        mut index: usize,
        mut deleted: usize,
        mut errors: Vec<String>,
        all: bool,
    ) {
        while index < paths.len() {
            let path = &paths[index];
            if !all && directory_has_entries(path) {
                let name = self.dired_relative(path);
                self.prompt = Prompt::Confirm {
                    message: format!("Recursively delete {name}? (yes, no, all, quit) "),
                    input: String::new(),
                    cursor: 0,
                    action: PendingAction::Delete {
                        paths,
                        index,
                        deleted,
                        errors,
                        all,
                    },
                };
                return;
            }
            delete_one(path, &mut deleted, &mut errors);
            index += 1;
        }
        self.dired_revert();
        self.message = Some(delete_summary(deleted, &errors));
    }

    /// The path shown in prompts, relative to the current Dired directory.
    fn dired_relative(&self, path: &Path) -> String {
        self.current()
            .dired
            .as_ref()
            .and_then(|d| path.strip_prefix(&d.dir).ok())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| path.display().to_string())
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
                self.dired_dir_line(-1);
                true
            }
            KeyCode::Char('>') => {
                self.dired_dir_line(1);
                true
            }
            KeyCode::Char('m') => {
                self.dired_set_mark(Mark::Marked, 1);
                true
            }
            KeyCode::Char('u') => {
                self.dired_set_mark(Mark::None, 1);
                true
            }
            KeyCode::Backspace => {
                self.dired_set_mark(Mark::None, -1);
                true
            }
            KeyCode::Char('d') => {
                self.dired_set_mark(Mark::Flagged, 1);
                true
            }
            KeyCode::Char('t') => {
                self.dired_toggle_marks();
                true
            }
            KeyCode::Char('U') => {
                self.dired_unmark_all();
                true
            }
            KeyCode::Char('x') => {
                self.dired_start_delete(Mark::Flagged);
                true
            }
            KeyCode::Char('D') => {
                self.dired_start_delete(Mark::Marked);
                true
            }
            KeyCode::Char('#') => {
                self.dired_flag_files("auto-save file(s)", is_auto_save_file);
                true
            }
            KeyCode::Char('~') => {
                self.dired_flag_files("backup file(s)", is_backup_file);
                true
            }
            KeyCode::Char('w') => {
                self.dired_copy_names();
                true
            }
            KeyCode::Char('s') => {
                self.dired_cycle_sort();
                true
            }
            KeyCode::Char('R') => {
                self.start_rename();
                true
            }
            KeyCode::Char('C') => {
                self.start_copy();
                true
            }
            KeyCode::Char('*') => {
                self.numeric_arg = None;
                self.start_prefix(Prefix::Star, "*");
                true
            }
            KeyCode::Char('+') => {
                self.start_create_directory();
                true
            }
            _ => false,
        }
    }

    // ---- the loop ----------------------------------------------------------

    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        loop {
            terminal.draw(|frame| ui::draw(frame, self))?;
            if self.should_quit {
                break;
            }
            if let Some(event) = self.next_event()? {
                self.handle_term_event(event);
            }
        }
        Ok(())
    }

    /// Wait for the next terminal event, or handle a due prefix hint, invalid
    /// confirmation message, or `M-w` indication once its delay has elapsed.
    /// Returns `None` when a timer fired instead of an event arriving.
    fn next_event(&mut self) -> Result<Option<TermEvent>> {
        let prefix_deadline = self.prefix_echo.as_ref().map(|(_, deadline)| *deadline);
        let confirm_deadline = self.confirm_flash.as_ref().map(|(_, deadline)| *deadline);
        let flash_deadline = self.copy_flash.as_ref().map(|(_, deadline)| *deadline);
        let deadline = [prefix_deadline, confirm_deadline, flash_deadline]
            .into_iter()
            .flatten()
            .min();
        if let Some(deadline) = deadline {
            let now = Instant::now();
            if deadline > now && event::poll(deadline - now)? {
                return Ok(Some(event::read()?));
            }
            let now = Instant::now();
            if self
                .copy_flash
                .as_ref()
                .is_some_and(|(_, deadline)| *deadline <= now)
            {
                if let Some((restore, _)) = self.copy_flash.take() {
                    self.current_mut().buffer.set_point(restore);
                }
            }
            self.expire_confirm_flash(now);
            self.echo_pending_prefix(now);
            return Ok(None);
        }
        Ok(Some(event::read()?))
    }

    /// Handle one terminal event. On Windows a paste arrives as a burst of key
    /// events rather than an `Event::Paste`, so a detected burst is collected
    /// and routed to `handle_paste` instead of being dispatched as commands.
    fn handle_term_event(&mut self, event: TermEvent) {
        use crossterm::event::KeyEventKind;
        match event {
            TermEvent::Paste(text) => self.handle_paste(text),
            TermEvent::Mouse(mouse) => self.handle_mouse(mouse),
            TermEvent::Key(mut key) => {
                if key.kind == KeyEventKind::Release {
                    return;
                }
                let latched = self.pasting_until.is_some_and(|t| Instant::now() < t);
                let ch = paste_char(&key);
                if paste::is_paste(paste::pending_events(), latched, ch.is_some()) {
                    let mut text = String::new();
                    if let Some(c) = ch {
                        text.push(c);
                    }
                    self.drain_paste(&mut text);
                    self.pasting_until = Some(Instant::now() + PASTE_LATCH);
                    self.handle_paste(text);
                } else {
                    self.pasting_until = None;
                    // Alacritty on Windows reports Ctrl+Space as a plain
                    // Space. Only promote a lone key; paste bursts have
                    // already been routed above and keep their spaces.
                    #[cfg(windows)]
                    keymap::promote_ctrl_space(&mut key, keymap::ctrl_is_down());
                    self.handle_key(key);
                }
            }
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.divider_drag = self
                    .dividers
                    .iter()
                    .find(|d| divider_contains(d.rect, mouse.column, mouse.row))
                    .map(|d| DividerDrag {
                        axis: d.axis,
                        container: d.container,
                        path: d.path.clone(),
                    });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(drag) = self.divider_drag.clone() else {
                    return;
                };
                let (first_weight, second_weight) = match drag.axis {
                    SplitAxis::Right => {
                        if drag.container.width < 3 {
                            return;
                        }
                        let content = u32::from(drag.container.width) - 1;
                        let x = u32::from(mouse.column).clamp(
                            u32::from(drag.container.x) + 1,
                            u32::from(drag.container.x) + content - 1,
                        );
                        let first = x - u32::from(drag.container.x);
                        (first as u16, (content - first) as u16)
                    }
                    SplitAxis::Below => {
                        if drag.container.height < 4 {
                            return;
                        }
                        let total = u32::from(drag.container.height);
                        let y = u32::from(mouse.row).clamp(
                            u32::from(drag.container.y) + 1,
                            u32::from(drag.container.y) + total - 3,
                        );
                        let first = y - u32::from(drag.container.y) + 1;
                        (first as u16, (total - first) as u16)
                    }
                };
                resize_split(
                    &mut self.window_tree,
                    &drag.path,
                    first_weight,
                    second_weight,
                );
            }
            MouseEventKind::Up(MouseButton::Left) => self.divider_drag = None,
            _ => {}
        }
    }

    /// Read any further already-queued key events (the rest of a paste) and
    /// append their text.
    fn drain_paste(&mut self, text: &mut String) {
        use crossterm::event::KeyEventKind;
        loop {
            if !event::poll(Duration::ZERO).unwrap_or(false) {
                break;
            }
            // On Windows, stop once the console queue is empty so a key that
            // arrived after the paste is not swallowed.
            if cfg!(windows) && paste::pending_events() == 0 {
                break;
            }
            match event::read() {
                Ok(TermEvent::Key(key)) if key.kind != KeyEventKind::Release => {
                    if let Some(c) = paste_char(&key) {
                        text.push(c);
                    }
                }
                Ok(TermEvent::Paste(t)) => text.push_str(&t),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }

    fn ensure_scroll_visible(&mut self) {
        let height = self.viewport_rows.max(1);
        let margin = self.scroll_margin;
        self.current_mut()
            .buffer
            .ensure_point_visible(height, margin);
    }

    pub(crate) fn prepare_active_view(&mut self, width: usize, height: usize) {
        let width = width.max(1);
        self.viewport_cols = width;
        self.viewport_rows = height.max(1);
        self.current_mut().buffer.set_width(width);
        self.ensure_scroll_visible();
    }

    // ---- key handling ------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.kind == KeyEventKind::Release {
            return;
        }
        // Typing during an invalid-answer message resumes the question and
        // feeds this key to it, so the first keystroke is not lost.
        if self.confirm_flash.is_some() {
            self.resume_confirm();
        }
        // Any keystroke cancels a pending `M-w` indication.
        self.copy_flash = None;
        // ...and an overdue-but-unshown prefix hint or number overlay.
        self.prefix_echo = None;
        self.show_window_numbers = false;
        if !matches!(self.prompt, Prompt::None) {
            self.handle_prompt_key(key);
            return;
        }
        if self.search.active {
            // Motion commands exit isearch and then perform their normal
            // action from the current match, like Emacs: forward searches
            // continue from the end of the match, backward ones from its start.
            if isearch_motion(key) {
                let direction = self.search.direction;
                let match_end = self.search.match_end;
                self.search.accept();
                if direction == SearchDirection::Forward {
                    if let Some(end) = match_end {
                        self.current_mut().buffer.set_point(end);
                    }
                }
            } else {
                self.handle_search_key(key);
                return;
            }
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
            self.start_prefix(Prefix::CtrlX, "C-x-");
            return;
        }
        if ctrl && key.code == KeyCode::Char('c') {
            self.numeric_arg = None;
            self.start_prefix(Prefix::CtrlC, "C-c-");
            return;
        }
        // C-h (help) prefix. Some terminals report Ctrl+H as Backspace.
        if ctrl && matches!(key.code, KeyCode::Char('h') | KeyCode::Backspace) {
            self.numeric_arg = None;
            self.start_prefix(Prefix::Help, "C-h-");
            return;
        }
        if !ctrl && key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('g') {
            self.numeric_arg = None;
            self.start_prefix(Prefix::MetaG, "M-g-");
            return;
        }

        if self.wdired_active() {
            self.message = None;
            if self.handle_wdired_key(key) {
                return;
            }
        }

        // Plain keys in a Dired buffer drive Dired commands (n, p, f, ^, g,
        // q, ...) rather than inserting text. Clear the echo area first (as
        // Emacs does before each command) so a command that reports something
        // leaves its message visible rather than having it wiped afterwards.
        if self.current().kind == Kind::Dired
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.message = None;
            if self.handle_dired_key(key.code) {
                return;
            }
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
            if self.wdired_active() {
                self.wdired_clamp_point();
            }
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

    /// Begin an incomplete key sequence and arrange for its hint to be
    /// echoed only if the user pauses.
    fn start_prefix(&mut self, prefix: Prefix, hint: &str) {
        self.prefix = prefix;
        self.show_window_numbers = false;
        self.message = None;
        let delay = if prefix == Prefix::MetaG {
            Duration::from_millis(self.config.ui.window_number_delay_ms)
        } else {
            PREFIX_ECHO_DELAY
        };
        self.start_echo_after(hint, delay);
    }

    fn start_echo(&mut self, hint: &str) {
        self.start_echo_after(hint, PREFIX_ECHO_DELAY);
    }

    fn start_echo_after(&mut self, hint: &str, delay: Duration) {
        self.prefix_echo = Some((hint.to_string(), Instant::now() + delay));
    }

    /// Show the pending prefix hint once its delay has elapsed.
    fn echo_pending_prefix(&mut self, now: Instant) {
        if let Some((hint, deadline)) = &self.prefix_echo {
            if *deadline <= now {
                let hint = hint.clone();
                self.prefix_echo = None;
                self.message = Some(hint);
                if self.prefix == Prefix::MetaG {
                    self.show_window_numbers = true;
                }
            }
        }
    }

    fn handle_prefix(&mut self, key: KeyEvent, ctrl: bool) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let prefix = std::mem::take(&mut self.prefix);
        self.message = None;
        match prefix {
            Prefix::None => {}
            Prefix::CtrlX => match key.code {
                KeyCode::Char('c') if ctrl => self.should_quit = true,
                KeyCode::Char('2') => self.split_active_window(SplitAxis::Below),
                KeyCode::Char('3') => self.split_active_window(SplitAxis::Right),
                KeyCode::Char('0') => self.delete_active_window(),
                KeyCode::Char('1') => self.delete_other_windows(),
                KeyCode::Char('o') => self.other_window(),
                KeyCode::Char('4') => self.start_prefix(Prefix::CtrlX4, "C-x 4-"),
                KeyCode::Char('b') => self.start_switch_buffer(false),
                KeyCode::Char('k') => self.start_kill_buffer(),
                KeyCode::Char('x') if ctrl => self.exchange_point_and_mark(),
                KeyCode::Char('f') if ctrl => self.start_find_file(false),
                KeyCode::Char('d') => self.start_find_file(true),
                KeyCode::Char('q') if self.current().kind == Kind::Dired => self.wdired_toggle(),
                KeyCode::Char('g') if ctrl => {}
                _ => self.undefined("C-x", key),
            },
            Prefix::CtrlX4 => match key.code {
                KeyCode::Char('b') => self.start_switch_buffer(true),
                KeyCode::Char('g') if ctrl => {}
                _ => self.undefined("C-x 4", key),
            },
            Prefix::MetaG => {
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                match key.code {
                    KeyCode::Char('f') if alt => self.focus_direction(WindowDirection::Right),
                    KeyCode::Char('b') if alt => self.focus_direction(WindowDirection::Left),
                    KeyCode::Char('p') if alt => self.focus_direction(WindowDirection::Up),
                    KeyCode::Char('n') if alt => self.focus_direction(WindowDirection::Down),
                    KeyCode::Char(d) if !ctrl && !alt && d.is_ascii_digit() => {
                        if let Some(number) = d.to_digit(10) {
                            self.focus_window_number(number as u8);
                        }
                    }
                    KeyCode::Esc => {}
                    KeyCode::Char('g') if ctrl => {}
                    _ => self.undefined("M-g", key),
                }
            }
            Prefix::CtrlC => match key.code {
                KeyCode::Char('c') if ctrl && self.wdired_active() => self.wdired_commit(),
                KeyCode::Char('k') if ctrl && self.wdired_active() => self.wdired_abort(),
                KeyCode::Esc if self.wdired_active() => self.wdired_abort(),
                KeyCode::Char('g') if ctrl => {}
                _ => self.undefined("C-c", key),
            },
            Prefix::Help => match key.code {
                KeyCode::Char('b') => self.show_help(),
                KeyCode::Char('g') if ctrl => {}
                _ => self.undefined("C-h", key),
            },
            Prefix::Star => match key.code {
                KeyCode::Char('!') => self.dired_unmark_all(),
                KeyCode::Char('.') => self.start_mark_extension(),
                _ => self.undefined("*", key),
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

    fn start_switch_buffer(&mut self, other_window: bool) {
        self.prompt = Prompt::SwitchBuffer {
            input: String::new(),
            cursor: 0,
            selected: 0,
            other_window,
        };
    }

    fn start_kill_buffer(&mut self) {
        self.prompt = Prompt::KillBuffer {
            input: String::new(),
            cursor: 0,
            selected: 0,
        };
    }

    fn start_find_file(&mut self, dired: bool) {
        let mut input = self.default_directory().display().to_string();
        if !input.ends_with(std::path::MAIN_SEPARATOR) {
            input.push(std::path::MAIN_SEPARATOR);
        }
        self.prompt = Prompt::FindFile {
            cursor: input.chars().count(),
            input,
            dired,
            selected: None,
        };
    }

    /// The directory new prompts start in: the current Dired directory, or
    /// the home directory.
    fn default_directory(&self) -> PathBuf {
        if let Some(d) = &self.current().dired {
            return d.dir.clone();
        }
        home_dir().unwrap_or_else(|| PathBuf::from("."))
    }

    /// Resolve a user-entered path against the default directory, expanding a
    /// leading `~`.
    fn expand_path(&self, input: &str) -> PathBuf {
        let trimmed = input.trim();
        if let Some(rest) = trimmed.strip_prefix('~') {
            if let Some(home) = home_dir() {
                let rest = rest.trim_start_matches(['/', '\\']);
                return if rest.is_empty() {
                    home
                } else {
                    home.join(rest)
                };
            }
        }
        let path = PathBuf::from(trimmed);
        if path.is_absolute() || trimmed.is_empty() {
            path
        } else {
            self.default_directory().join(path)
        }
    }

    /// Completion candidates for the last path component of `input`. With
    /// `dirs_only` (the `C-x d` prompt) only directories are listed.
    fn file_candidates(&self, input: &str, dirs_only: bool) -> Vec<String> {
        let (dir_part, prefix) = match input.rfind(['/', '\\']) {
            Some(i) => (&input[..=i], &input[i + 1..]),
            None => ("", input),
        };
        let base = if dir_part.is_empty() {
            self.default_directory()
        } else {
            self.expand_path(dir_part)
        };
        let prefix_lower = prefix.to_lowercase();
        let mut out = Vec::new();
        if let Ok(read) = fs::read_dir(&base) {
            for entry in read.flatten() {
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if dirs_only && !is_dir {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                // Match the typed component case-insensitively.
                if name.to_lowercase().starts_with(&prefix_lower) {
                    let sep = if is_dir {
                        std::path::MAIN_SEPARATOR.to_string()
                    } else {
                        String::new()
                    };
                    out.push(format!("{dir_part}{name}{sep}"));
                }
            }
        }
        out.sort();
        out.truncate(50);
        out
    }

    fn buffer_candidates(&self, input: &str, include_current: bool) -> Vec<usize> {
        let needle = input.to_lowercase();
        let mut candidates: Vec<usize> = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(i, b)| {
                (include_current || *i != self.current)
                    && (needle.is_empty() || b.name.to_lowercase().contains(&needle))
            })
            .map(|(i, _)| i)
            .collect();
        // Most recently visited first, so the current buffer leads the list.
        candidates.sort_by_key(|&i| std::cmp::Reverse(self.buffers[i].last_visited));
        candidates.truncate(5);
        candidates
    }

    fn switch_candidates(&self, input: &str) -> Vec<usize> {
        self.buffer_candidates(input, false)
    }

    fn kill_candidates(&self, input: &str) -> Vec<usize> {
        self.buffer_candidates(input, true)
    }

    fn prompt_move(&mut self, count: usize, delta: isize) {
        if let Prompt::FindFile { selected, .. }
        | Prompt::Rename { selected, .. }
        | Prompt::CreateDirectory { selected, .. } = &mut self.prompt
        {
            *selected = select_move(*selected, count, delta);
            return;
        }
        let selected = match &mut self.prompt {
            Prompt::SwitchBuffer { selected, .. } | Prompt::KillBuffer { selected, .. } => selected,
            Prompt::None
            | Prompt::Confirm { .. }
            | Prompt::FindFile { .. }
            | Prompt::Rename { .. }
            | Prompt::CreateDirectory { .. }
            | Prompt::MarkExtension { .. } => return,
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
            | Prompt::KillBuffer { input, cursor, .. }
            | Prompt::FindFile { input, cursor, .. }
            | Prompt::Rename { input, cursor, .. }
            | Prompt::MarkExtension { input, cursor }
            | Prompt::CreateDirectory { input, cursor, .. }
            | Prompt::Confirm { input, cursor, .. } => (input, cursor),
            Prompt::None => return None,
        };
        Some(f(input, cursor))
    }

    fn prompt_cursor_pos(&self) -> Option<usize> {
        match &self.prompt {
            Prompt::SwitchBuffer { cursor, .. }
            | Prompt::KillBuffer { cursor, .. }
            | Prompt::FindFile { cursor, .. }
            | Prompt::Rename { cursor, .. }
            | Prompt::MarkExtension { cursor, .. }
            | Prompt::CreateDirectory { cursor, .. }
            | Prompt::Confirm { cursor, .. } => Some(*cursor),
            Prompt::None => None,
        }
    }

    fn prompt_region(&self) -> Option<(usize, usize)> {
        let mark = self.prompt_mark?;
        let cursor = self.prompt_cursor_pos()?;
        Some(if mark <= cursor {
            (mark, cursor)
        } else {
            (cursor, mark)
        })
    }

    fn prompt_set_mark(&mut self) {
        if let Some(cursor) = self.prompt_cursor_pos() {
            self.prompt_mark = Some(cursor);
            self.message = Some("Mark set".to_string());
        }
    }

    fn prompt_adjust_mark_insert(&mut self, at: usize, len: usize) {
        if let Some(mark) = &mut self.prompt_mark {
            if *mark >= at {
                *mark += len;
            }
        }
    }

    fn prompt_adjust_mark_delete(&mut self, start: usize, end: usize) {
        if let Some(mark) = self.prompt_mark {
            self.prompt_mark = Some(if mark <= start {
                mark
            } else if mark <= end {
                start
            } else {
                mark - (end - start)
            });
        }
    }

    fn prompt_adjust_mark_replace(&mut self, start: usize, end: usize, new_len: usize) {
        if let Some(mark) = self.prompt_mark {
            self.prompt_mark = Some(if mark <= start {
                mark
            } else if mark >= end {
                mark - (end - start) + new_len
            } else {
                start + new_len.min(mark - start)
            });
        }
    }

    fn prompt_exchange_point_and_mark(&mut self) {
        let Some(mark) = self.prompt_mark else {
            return;
        };
        let Some(cursor) = self.prompt_cursor_pos() else {
            return;
        };
        self.prompt_mark = Some(cursor);
        self.prompt_edit(|_, cursor| *cursor = mark);
    }

    fn prompt_kill_region(&mut self, prev: LastCommand) {
        let Some((start, end)) = self.prompt_region() else {
            return;
        };
        if start == end {
            return;
        }
        let killed = self.prompt_edit(|input, cursor| {
            let from = char_to_byte(input, start);
            let to = char_to_byte(input, end);
            let text = input[from..to].to_string();
            input.replace_range(from..to, "");
            *cursor = start;
            text
        });
        if let Some(killed) = killed {
            self.prompt_mark = None;
            self.kill_new(&killed, prev == LastCommand::Kill);
            self.prompt_reset_selection();
            self.last_command = LastCommand::Kill;
        }
    }

    fn prompt_copy_region(&mut self, prev: LastCommand) {
        let Some((start, end)) = self.prompt_region() else {
            return;
        };
        if start == end {
            return;
        }
        let text = self.prompt_edit(|input, _| {
            let from = char_to_byte(input, start);
            let to = char_to_byte(input, end);
            input[from..to].to_string()
        });
        if let Some(text) = text {
            self.kill_new(&text, prev == LastCommand::Kill);
            self.last_command = LastCommand::Kill;
        }
    }

    fn prompt_reset_selection(&mut self) {
        match &mut self.prompt {
            Prompt::SwitchBuffer { selected, .. } | Prompt::KillBuffer { selected, .. } => {
                *selected = 0
            }
            Prompt::FindFile { selected, .. }
            | Prompt::Rename { selected, .. }
            | Prompt::CreateDirectory { selected, .. } => *selected = None,
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
        let inserted = self.prompt_edit(|input, cursor| {
            let idx = (*cursor).min(input.chars().count());
            input.insert_str(char_to_byte(input, idx), text);
            let len = text.chars().count();
            *cursor = idx + len;
            (idx, len)
        });
        if let Some((idx, len)) = inserted {
            self.prompt_adjust_mark_insert(idx, len);
            self.prompt_reset_selection();
            self.last_command = LastCommand::Other;
        }
    }

    fn prompt_delete_backward(&mut self) {
        let removed = self.prompt_edit(|input, cursor| {
            if *cursor > 0 {
                let start_char = *cursor - 1;
                let end_char = *cursor;
                let start = char_to_byte(input, start_char);
                let end = char_to_byte(input, end_char);
                input.replace_range(start..end, "");
                *cursor = start_char;
                Some((start_char, end_char))
            } else {
                None
            }
        });
        if let Some(Some((start, end))) = removed {
            self.prompt_adjust_mark_delete(start, end);
            self.prompt_reset_selection();
        }
    }

    fn prompt_delete_forward(&mut self) {
        let removed = self.prompt_edit(|input, cursor| {
            if *cursor < input.chars().count() {
                let start_char = *cursor;
                let end_char = *cursor + 1;
                let start = char_to_byte(input, start_char);
                let end = char_to_byte(input, end_char);
                input.replace_range(start..end, "");
                Some((start_char, end_char))
            } else {
                None
            }
        });
        if let Some(Some((start, end))) = removed {
            self.prompt_adjust_mark_delete(start, end);
            self.prompt_reset_selection();
        }
    }

    fn prompt_transpose_chars(&mut self) {
        if self
            .prompt_edit(|input, cursor| {
                let mut chars: Vec<char> = input.chars().collect();
                if let Some(new_cursor) = transpose_chars_at(&mut chars, *cursor) {
                    *input = chars.into_iter().collect();
                    *cursor = new_cursor;
                }
            })
            .is_some()
        {
            self.prompt_reset_selection();
            self.last_command = LastCommand::Other;
        }
    }

    fn prompt_transpose_words(&mut self) {
        if self
            .prompt_edit(|input, cursor| {
                let mut chars: Vec<char> = input.chars().collect();
                if let Some(new_cursor) = transpose_words_at(&mut chars, *cursor) {
                    *input = chars.into_iter().collect();
                    *cursor = new_cursor;
                }
            })
            .is_some()
        {
            self.prompt_reset_selection();
            self.last_command = LastCommand::Other;
        }
    }

    fn prompt_forward_word(&mut self) {
        self.prompt_edit(|input, cursor| *cursor = word_forward(input, *cursor));
    }

    fn prompt_backward_word(&mut self) {
        self.prompt_edit(|input, cursor| *cursor = word_backward(input, *cursor));
    }

    fn prompt_backward_kill_word(&mut self, prev: LastCommand) {
        let killed = self.prompt_edit(|input, cursor| {
            let end = (*cursor).min(input.chars().count());
            let start = word_backward(input, end);
            if start == end {
                return None;
            }
            let from = char_to_byte(input, start);
            let to = char_to_byte(input, end);
            let text = input[from..to].to_string();
            input.replace_range(from..to, "");
            *cursor = start;
            Some((start, end, text))
        });
        if let Some(Some((start, end, text))) = killed {
            self.prompt_adjust_mark_delete(start, end);
            self.kill_new(&text, prev == LastCommand::Kill);
            self.prompt_reset_selection();
            self.last_command = LastCommand::Kill;
        }
    }

    fn prompt_kill_word(&mut self, prev: LastCommand) {
        let killed = self.prompt_edit(|input, cursor| {
            let start = (*cursor).min(input.chars().count());
            let end = word_forward(input, start);
            if start == end {
                return None;
            }
            let from = char_to_byte(input, start);
            let to = char_to_byte(input, end);
            let text = input[from..to].to_string();
            input.replace_range(from..to, "");
            Some((start, end, text))
        });
        if let Some(Some((start, end, text))) = killed {
            self.prompt_adjust_mark_delete(start, end);
            self.kill_new(&text, prev == LastCommand::Kill);
            self.prompt_reset_selection();
            self.last_command = LastCommand::Kill;
        }
    }

    fn prompt_case_word(&mut self, up: bool) {
        let changed = self.prompt_edit(|input, cursor| {
            let chars: Vec<char> = input.chars().collect();
            let mut start = (*cursor).min(chars.len());
            while start < chars.len() && !is_word_char(chars[start]) {
                start += 1;
            }
            if start == chars.len() {
                *cursor = start;
                return None;
            }
            let mut end = start;
            while end < chars.len() && is_word_char(chars[end]) {
                end += 1;
            }
            let transform = |c: char| {
                if up {
                    c.to_uppercase().collect::<String>()
                } else {
                    c.to_lowercase().collect::<String>()
                }
            };
            let changed: String = chars[start..end].iter().map(|c| transform(*c)).collect();
            let new_len = changed.chars().count();
            let mut out: Vec<char> = chars[..start].to_vec();
            out.extend(changed.chars());
            out.extend_from_slice(&chars[end..]);
            *input = out.into_iter().collect();
            *cursor = start + new_len;
            Some((start, end, new_len))
        });
        if let Some(Some((start, end, new_len))) = changed {
            self.prompt_adjust_mark_replace(start, end, new_len);
            self.prompt_reset_selection();
            self.last_command = LastCommand::Other;
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

    fn prompt_kill_line(&mut self, prev: LastCommand) {
        let killed = self.prompt_edit(|input, cursor| {
            let start = (*cursor).min(input.chars().count());
            let at = char_to_byte(input, start);
            let killed = input[at..].to_string();
            if killed.is_empty() {
                return None;
            }
            input.truncate(at);
            Some((start, start + killed.chars().count(), killed))
        });
        if let Some(Some((start, end, killed))) = killed {
            self.prompt_adjust_mark_delete(start, end);
            self.kill_new(&killed, prev == LastCommand::Kill);
            self.prompt_reset_selection();
            self.last_command = LastCommand::Kill;
        }
    }

    fn prompt_yank(&mut self) {
        let text = match self.kill_ring.yank() {
            Some(text) => text,
            None => return,
        };
        let inserted = self.prompt_edit(|input, cursor| {
            let idx = (*cursor).min(input.chars().count());
            input.insert_str(char_to_byte(input, idx), &text);
            let len = text.chars().count();
            *cursor = idx + len;
            (idx, idx + len)
        });
        if let Some((start, end)) = inserted {
            self.prompt_adjust_mark_insert(start, end - start);
            self.prompt_last_yank = Some((start, end));
            self.prompt_reset_selection();
            self.last_command = LastCommand::Yank;
        }
    }

    fn prompt_yank_pop(&mut self, prev: LastCommand) {
        if prev != LastCommand::Yank {
            return;
        }
        let Some((start, end)) = self.prompt_last_yank else {
            return;
        };
        let Some(text) = self.kill_ring.yank_pop() else {
            return;
        };
        let replaced = self.prompt_edit(|input, cursor| {
            let from = char_to_byte(input, start);
            let to = char_to_byte(input, end);
            input.replace_range(from..to, &text);
            let len = text.chars().count();
            *cursor = start + len;
            (start, start + len)
        });
        if let Some((start, end)) = replaced {
            self.prompt_mark = None;
            self.prompt_last_yank = Some((start, end));
            self.prompt_reset_selection();
            self.last_command = LastCommand::Yank;
        }
    }

    fn prompt_complete(&mut self) {
        self.prompt_mark = None;
        self.prompt_last_yank = None;
        self.last_command = LastCommand::Other;
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
            Prompt::FindFile { input, dired, .. } => self.file_candidates(input, *dired),
            Prompt::Rename { input, copy, .. } => {
                self.file_candidates(input, *copy || self.dired_paths(Mark::Marked).len() > 1)
            }
            Prompt::CreateDirectory { input, .. } => self.file_candidates(input, false),
            Prompt::None | Prompt::Confirm { .. } | Prompt::MarkExtension { .. } => return,
        };
        if candidates.is_empty() {
            return;
        }
        // A highlighted completion wins over the common prefix, so Tab can
        // accept the directory picked from the candidate list.
        let selected = match &self.prompt {
            Prompt::FindFile { selected, .. }
            | Prompt::Rename { selected, .. }
            | Prompt::CreateDirectory { selected, .. } => *selected,
            _ => None,
        };
        let completion = selected
            .and_then(|i| candidates.get(i).cloned())
            .unwrap_or_else(|| {
                if candidates.len() == 1 {
                    candidates[0].clone()
                } else {
                    common_prefix(&candidates)
                }
            });
        match &mut self.prompt {
            Prompt::SwitchBuffer {
                input,
                cursor,
                selected,
                ..
            }
            | Prompt::KillBuffer {
                input,
                cursor,
                selected,
            } => {
                *cursor = completion.chars().count();
                *input = completion;
                *selected = 0;
            }
            Prompt::FindFile {
                input,
                cursor,
                selected,
                ..
            }
            | Prompt::Rename {
                input,
                cursor,
                selected,
                ..
            }
            | Prompt::CreateDirectory {
                input,
                cursor,
                selected,
            } => {
                *cursor = completion.chars().count();
                *input = completion;
                *selected = None;
            }
            _ => {}
        }
    }

    fn cancel_prompt(&mut self) {
        self.prompt = Prompt::None;
        self.prompt_mark = None;
        self.prompt_last_yank = None;
        self.prompt_cx = false;
        self.message = Option::None;
    }

    /// Show `read-answer`'s invalid-answer message, keeping the prompt hidden
    /// until a key arrives or the timeout elapses.
    fn flash_invalid_answer(&mut self, prompt: Prompt, options: &[&str]) {
        let choices = options
            .iter()
            .map(|option| format!("'{option}'"))
            .collect::<Vec<_>>()
            .join(" or ");
        self.prompt = Prompt::None;
        self.confirm_flash = Some((prompt, Instant::now() + CONFIRM_FLASH));
        self.message = Some(format!("Please answer {choices}."));
    }

    /// Bring back a confirmation that was waiting behind an invalid answer.
    fn resume_confirm(&mut self) {
        if let Some((prompt, _)) = self.confirm_flash.take() {
            self.prompt = prompt;
            self.message = None;
        }
    }

    /// Restore a confirmation once its invalid-answer message has timed out.
    fn expire_confirm_flash(&mut self, now: Instant) {
        if self
            .confirm_flash
            .as_ref()
            .is_some_and(|(_, deadline)| *deadline <= now)
        {
            self.resume_confirm();
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        if self.quoted {
            self.quoted = false;
            self.message = Option::None;
            if let KeyCode::Char(c) = key.code {
                self.prompt_insert(c);
            } else if key.code == KeyCode::Tab {
                self.prompt_insert('\t');
            }
            return;
        }
        if self.prompt_cx {
            self.prompt_cx = false;
            if key.code == KeyCode::Esc {
                self.cancel_prompt();
            } else if key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char('x')
            {
                self.prompt_exchange_point_and_mark();
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('x') {
            self.prompt_cx = true;
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.cancel_prompt();
                return;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cancel_prompt();
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
            Prompt::FindFile { input, dired, .. } => self.file_candidates(input, *dired).len(),
            Prompt::Rename { input, copy, .. } => self
                .file_candidates(input, *copy || self.dired_paths(Mark::Marked).len() > 1)
                .len(),
            Prompt::CreateDirectory { input, .. } => self.file_candidates(input, false).len(),
            Prompt::None | Prompt::Confirm { .. } | Prompt::MarkExtension { .. } => 0,
        };
        use Action::*;
        let action = keymap::translate(key);
        if action != DabbrevExpand {
            self.dabbrev = Option::None;
        }
        let prev = self.last_command;
        self.last_command = LastCommand::Other;
        match action {
            Abort => self.cancel_prompt(),
            SelfInsert(c) => self.prompt_insert(c),
            DeleteBackward => self.prompt_delete_backward(),
            DeleteForward => self.prompt_delete_forward(),
            BackwardChar => self.prompt_move_cursor(-1),
            ForwardChar => self.prompt_move_cursor(1),
            ForwardWord => self.prompt_forward_word(),
            BackwardWord => self.prompt_backward_word(),
            BeginningOfLine | BeginningOfBuffer => self.prompt_cursor_start(),
            EndOfLine | EndOfBuffer => self.prompt_cursor_end(),
            UpcaseWord => self.prompt_case_word(true),
            DowncaseWord => self.prompt_case_word(false),
            TransposeChars => self.prompt_transpose_chars(),
            TransposeWords => self.prompt_transpose_words(),
            BackwardKillWord => self.prompt_backward_kill_word(prev),
            KillWord => self.prompt_kill_word(prev),
            KillLine => self.prompt_kill_line(prev),
            SetMark => self.prompt_set_mark(),
            KillRegion => self.prompt_kill_region(prev),
            CopyRegion => self.prompt_copy_region(prev),
            Yank => self.prompt_yank(),
            YankPop => self.prompt_yank_pop(prev),
            QuotedInsert => {
                self.quoted = true;
                self.start_echo("C-q-");
            }
            PrevLine => self.prompt_move(count, -1),
            NextLine => self.prompt_move(count, 1),
            _ => {}
        }
    }

    fn commit_prompt(&mut self) {
        self.prompt_mark = None;
        self.prompt_last_yank = None;
        self.prompt_cx = false;
        self.message = Option::None;
        match std::mem::replace(&mut self.prompt, Prompt::None) {
            Prompt::None => {}
            Prompt::SwitchBuffer {
                input,
                selected,
                other_window,
                ..
            } => {
                let candidates = self.switch_candidates(&input);
                let target = if let Some(&i) = candidates.get(selected) {
                    Some(i)
                } else if input.trim().is_empty() {
                    Some(self.previous)
                } else {
                    self.find_buffer(input.trim())
                };
                if let Some(i) = target {
                    if other_window {
                        self.switch_buffer_in_other_window(i);
                    } else {
                        self.switch_to(i);
                    }
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
            Prompt::FindFile {
                input,
                dired,
                selected,
                ..
            } => {
                // Use the highlighted completion if one was chosen with
                // Up/Down; otherwise the typed input.
                let candidates = self.file_candidates(&input, dired);
                let chosen = match selected {
                    Some(i) => candidates.get(i).cloned().unwrap_or(input),
                    None => input,
                };
                let path = self.expand_path(&chosen);
                if path.as_os_str().is_empty() {
                    return;
                }
                if dired || path.is_dir() {
                    if path.is_dir() {
                        self.open_dired(path);
                    } else {
                        self.message = Some(format!("Not a directory: {}", path.display()));
                    }
                } else if path.exists() {
                    self.open_file(path);
                } else {
                    self.message = Some(format!("No such file: {}", path.display()));
                }
                self.history.push(&chosen);
            }
            Prompt::Rename {
                input,
                selected,
                copy,
                ..
            } => {
                let candidates = self.file_candidates(&input, copy);
                let chosen = match selected {
                    Some(i) => candidates.get(i).cloned().unwrap_or(input),
                    None => input,
                };
                if copy {
                    self.commit_copy(&chosen);
                } else {
                    self.commit_rename(&chosen);
                }
            }
            Prompt::CreateDirectory {
                input, selected, ..
            } => {
                let candidates = self.file_candidates(&input, false);
                let chosen = match selected {
                    Some(i) => candidates.get(i).cloned().unwrap_or(input),
                    None => input,
                };
                self.commit_create_directory(&chosen);
            }
            Prompt::MarkExtension { input, .. } => self.mark_by_extension(&input),
            Prompt::Confirm {
                message,
                input,
                action,
                ..
            } => match action {
                PendingAction::Delete {
                    paths,
                    index,
                    deleted,
                    errors,
                    all,
                } => match recursive_answer(&input) {
                    RecursiveAnswer::Yes => {
                        self.run_pending(PendingAction::Delete {
                            paths,
                            index,
                            deleted,
                            errors,
                            all,
                        });
                    }
                    RecursiveAnswer::No => {
                        self.continue_delete(paths, index + 1, deleted, errors, all);
                    }
                    RecursiveAnswer::All => {
                        self.continue_delete(paths, index, deleted, errors, true);
                    }
                    RecursiveAnswer::Quit => {
                        self.dired_revert();
                        self.message = Some("OK, canceled".to_string());
                    }
                    RecursiveAnswer::Invalid => {
                        self.flash_invalid_answer(
                            Prompt::Confirm {
                                message,
                                input: String::new(),
                                cursor: 0,
                                action: PendingAction::Delete {
                                    paths,
                                    index,
                                    deleted,
                                    errors,
                                    all,
                                },
                            },
                            &["yes", "no", "all", "quit"],
                        );
                    }
                },
                action => {
                    if answer_is_yes(&input) {
                        self.run_pending(action);
                    } else if answer_is_no(&input) {
                        self.message = Some("Canceled".to_string());
                    } else {
                        // Re-ask, like `yes-or-no-p`.
                        self.flash_invalid_answer(
                            Prompt::Confirm {
                                message,
                                input: String::new(),
                                cursor: 0,
                                action,
                            },
                            &["yes", "no"],
                        );
                    }
                }
            },
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
                other_window,
            } => {
                let names = self
                    .switch_candidates(input)
                    .into_iter()
                    .map(|i| self.buffers[i].name.clone())
                    .collect();
                let prefix = if *other_window {
                    format!(
                        "Switch to buffer in other window (default {}): ",
                        self.previous_buffer_name()
                    )
                } else {
                    format!(
                        "Switch to buffer (default {}): ",
                        self.previous_buffer_name()
                    )
                };
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
            Prompt::FindFile {
                input,
                cursor,
                dired,
                selected,
            } => {
                let prefix = if *dired {
                    "Dired (directory): "
                } else {
                    "Find file: "
                };
                Some((
                    format!("{prefix}{input}"),
                    prefix.chars().count() + *cursor,
                    self.file_candidates(input, *dired),
                    selected.unwrap_or(usize::MAX),
                ))
            }
            Prompt::Rename {
                input,
                cursor,
                selected,
                copy,
            } => {
                let prefix = if *copy { "Copy to: " } else { "Rename to: " };
                Some((
                    format!("{prefix}{input}"),
                    prefix.chars().count() + *cursor,
                    self.file_candidates(input, *copy || self.dired_paths(Mark::Marked).len() > 1),
                    selected.unwrap_or(usize::MAX),
                ))
            }
            Prompt::CreateDirectory {
                input,
                cursor,
                selected,
            } => {
                let prefix = "Create directory: ";
                Some((
                    format!("{prefix}{input}"),
                    prefix.chars().count() + *cursor,
                    self.file_candidates(input, false),
                    selected.unwrap_or(usize::MAX),
                ))
            }
            Prompt::MarkExtension { input, cursor } => {
                let prefix = "Mark extension: ";
                Some((
                    format!("{prefix}{input}"),
                    prefix.chars().count() + *cursor,
                    Vec::new(),
                    0,
                ))
            }
            Prompt::Confirm {
                message,
                input,
                cursor,
                ..
            } => Some((
                format!("{message}{input}"),
                message.chars().count() + *cursor,
                Vec::new(),
                0,
            )),
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
            OpenLine => {
                if self.check_editable() {
                    for _ in 0..n.max(0) {
                        self.current_mut().buffer.open_line();
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
            TransposeChars => self.current_mut().buffer.transpose_chars(),
            TransposeWords => self.current_mut().buffer.transpose_words(),
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
            SetMark => {
                self.current_mut().buffer.set_mark();
                self.message = Some("Mark set".to_string());
            }
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
                self.start_echo("C-q-");
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
                let at_last = self.current().buffer.point().row == self.current().buffer.last_row();
                self.current_mut().buffer.next_line();
                if at_last {
                    self.message = Some("End of buffer".to_string());
                }
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
        let rows = self.page_lines();
        self.current_mut().buffer.page_backward(rows);
    }

    fn page_down(&mut self) {
        let rows = self.page_lines();
        self.current_mut().buffer.page_forward(rows);
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

    /// `C-x C-x`: swap point and mark.
    fn exchange_point_and_mark(&mut self) {
        if !self.current_mut().buffer.exchange_point_and_mark() {
            self.message = Some("No mark set in this buffer".to_string());
        }
    }

    /// `M-w`: save the region to the kill ring (no deletion).
    fn copy_region(&mut self, prev: LastCommand) {
        let i = self.current;
        let Some((start, end)) = self.buffers[i].buffer.region() else {
            self.message = Some("The mark is not set now, so there is no region".to_string());
            return;
        };
        let Some(text) = self.buffers[i].buffer.region_text() else {
            return;
        };
        if start == end {
            return;
        }
        self.kill_new(&text, prev == LastCommand::Kill);
        self.last_command = LastCommand::Kill;

        // Emacs `indicate-copied-region`: flash the far end of the region, or
        // echo `Saved text until "..."` if it is off screen.
        let point = self.buffers[i].buffer.point();
        let other = if point == end { start } else { end };
        if self.visible_row(i, other.row) {
            self.copy_flash = Some((point, Instant::now() + COPY_FLASH));
            self.buffers[i].buffer.set_point(other);
        } else {
            let snippet = snippet(&text);
            self.message = Some(format!("Saved text until \"{snippet}\""));
        }
    }

    /// Whether `row` is currently within the buffer viewport.
    fn visible_row(&self, buf: usize, row: usize) -> bool {
        let height = self.viewport_rows.max(1);
        let point = self.buffers[buf].buffer.point();
        let mut top = self.buffers[buf].buffer.scroll();
        if point.row < top {
            top = point.row;
        }
        if point.row >= top + height {
            top = point.row + 1 - height;
        }
        row >= top && row < top + height
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
            Some((pos, len)) => {
                self.current_mut().buffer.set_point(pos);
                self.search.match_end = Some(Pos::new(pos.row, pos.col + len));
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
            KeyCode::Enter => {
                let end = if self.search.direction == SearchDirection::Forward {
                    self.search.match_end
                } else {
                    None
                };
                self.search.accept();
                if let Some(end) = end {
                    self.current_mut().buffer.set_point(end);
                }
            }
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
        if self.confirm_flash.is_some() {
            self.resume_confirm();
        }
        if !matches!(self.prompt, Prompt::None) {
            let filtered: String = text.chars().filter(|c| *c != '\r').collect();
            self.prompt_insert_str(&filtered);
            self.kill_ring.push(&filtered, false);
            return;
        }
        if self.wdired_active() {
            let filtered: String = text.chars().filter(|c| *c != '\r').collect();
            self.wdired_insert_text(&filtered);
            self.kill_ring.push(&text, false);
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

/// Copy a file or directory tree, preserving symlinks as links. The caller
/// checks that the destination does not already exist.
fn copy_path(src: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        return copy_symlink(src, to);
    }
    if meta.is_dir() {
        if destination_inside_source(src, to) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cannot copy a directory into itself",
            ));
        }
        fs::create_dir(to)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_path(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        fs::copy(src, to)?;
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(src: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(src)?, to)
}

#[cfg(windows)]
fn copy_symlink(src: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    let target = fs::read_link(src)?;
    if fs::metadata(src)?.is_dir() {
        std::os::windows::fs::symlink_dir(target, to)
    } else {
        std::os::windows::fs::symlink_file(target, to)
    }
}

#[cfg(not(any(unix, windows)))]
fn copy_symlink(src: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    fs::copy(src, to).map(|_| ())
}

/// Whether `to` would land inside the directory `src`.
fn destination_inside_source(src: &std::path::Path, to: &std::path::Path) -> bool {
    fn normalize(path: &std::path::Path) -> PathBuf {
        if let Ok(canonical) = fs::canonicalize(path) {
            return canonical;
        }
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
            return parent.join(name);
        }
        path.to_path_buf()
    }
    normalize(to).starts_with(normalize(src))
}

/// Delete each path (recursively for directories), returning a summary. A
/// symlink is removed as a link, never followed.
fn interval_gap(start: u32, end: u32, other_start: u32, other_end: u32) -> u32 {
    if end <= other_start {
        other_start - end
    } else {
        start.saturating_sub(other_end)
    }
}

fn capture_view(buffer: &Buffer) -> WindowView {
    WindowView {
        point: buffer.point(),
        scroll: buffer.scroll(),
        scroll_sub: buffer.scroll_sub(),
        width: buffer.width(),
    }
}

fn restore_view(buffer: &mut Buffer, view: WindowView) {
    buffer.set_width(view.width);
    buffer.set_point(view.point);
    buffer.set_view_top(view.scroll, view.scroll_sub);
}

fn divider_contains(rect: Rect, column: u16, row: u16) -> bool {
    let column = u32::from(column);
    let row = u32::from(row);
    let left = u32::from(rect.x);
    let top = u32::from(rect.y);
    column >= left
        && column < left + u32::from(rect.width)
        && row >= top
        && row < top + u32::from(rect.height)
}

fn wdired_validate_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        return Err("File name cannot be empty".to_string());
    }
    if name.chars().any(|c| matches!(c, '\0' | '\n' | '\r')) {
        return Err("File name cannot contain newlines".to_string());
    }
    let path = Path::new(name);
    if path.is_absolute() || path.has_root() {
        return Err("File name must be relative to the Dired directory".to_string());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
    {
        return Err("File name must be relative to the Dired directory".to_string());
    }
    match path.components().next_back() {
        Some(Component::Normal(_)) => Ok(()),
        _ => Err("Invalid file name".to_string()),
    }
}

fn wdired_normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            component => out.push(component.as_os_str()),
        }
    }
    out
}

fn wdired_path_key(path: &Path) -> String {
    let normalized = wdired_normalize_path(path);
    #[cfg(windows)]
    {
        normalized.to_string_lossy().to_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized.to_string_lossy().into_owned()
    }
}

fn wdired_target_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn wdired_same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn wdired_temp_path(path: &Path) -> std::io::Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rename".to_string());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    for counter in 0..1000 {
        let candidate = parent.join(format!(
            ".diredrs-{stem}-{}-{stamp}-{counter}.tmp",
            std::process::id()
        ));
        if !wdired_target_exists(&candidate) {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "cannot find a temporary name",
    ))
}

fn remove_wdired_path(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_dir() {
            let _ = fs::remove_dir_all(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
}

fn wdired_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn wdired_final_name(
    old_name: &str,
    entry: usize,
    successful: &HashMap<usize, (&PathBuf, &PathBuf)>,
    dir: &Path,
) -> String {
    successful
        .get(&entry)
        .and_then(|(_, target)| {
            (target.parent() == Some(dir))
                .then(|| {
                    target
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .flatten()
        })
        .unwrap_or_else(|| old_name.to_string())
}

fn window_ids(tree: &WindowTree, out: &mut Vec<WindowId>) {
    match tree {
        WindowTree::Leaf(id) => out.push(*id),
        WindowTree::Split { first, second, .. } => {
            window_ids(first, out);
            window_ids(second, out);
        }
    }
}

fn split_leaf(tree: &mut WindowTree, target: WindowId, axis: SplitAxis, new_id: WindowId) -> bool {
    match tree {
        WindowTree::Leaf(id) if *id == target => {
            *tree = WindowTree::Split {
                axis,
                first_weight: 1,
                second_weight: 1,
                first: Box::new(WindowTree::Leaf(*id)),
                second: Box::new(WindowTree::Leaf(new_id)),
            };
            true
        }
        WindowTree::Leaf(_) => false,
        WindowTree::Split { first, second, .. } => {
            split_leaf(first, target, axis, new_id) || split_leaf(second, target, axis, new_id)
        }
    }
}

fn resize_split(
    tree: &mut WindowTree,
    path: &[usize],
    first_weight: u16,
    second_weight: u16,
) -> bool {
    let WindowTree::Split {
        first_weight: current_first,
        second_weight: current_second,
        first,
        second,
        ..
    } = tree
    else {
        return false;
    };
    match path.first() {
        None => {
            *current_first = first_weight;
            *current_second = second_weight;
            true
        }
        Some(0) => resize_split(first, &path[1..], first_weight, second_weight),
        Some(1) => resize_split(second, &path[1..], first_weight, second_weight),
        Some(_) => false,
    }
}

/// Remove `target`, collapsing its parent. `None` means the whole tree was the
/// removed leaf.
fn remove_leaf(tree: WindowTree, target: WindowId) -> Option<WindowTree> {
    match tree {
        WindowTree::Leaf(id) if id == target => None,
        WindowTree::Leaf(_) => Some(tree),
        WindowTree::Split {
            axis,
            first_weight,
            second_weight,
            first,
            second,
        } => {
            let first = remove_leaf(*first, target);
            let second = remove_leaf(*second, target);
            match (first, second) {
                (None, None) => None,
                (Some(first), None) => Some(first),
                (None, Some(second)) => Some(second),
                (Some(first), Some(second)) => Some(WindowTree::Split {
                    axis,
                    first_weight,
                    second_weight,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
            }
        }
    }
}

fn directory_has_entries(path: &Path) -> bool {
    let is_dir = fs::symlink_metadata(path)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false);
    is_dir
        && fs::read_dir(path)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(true)
}

fn delete_one(path: &Path, deleted: &mut usize, errors: &mut Vec<String>) {
    let is_dir = fs::symlink_metadata(path)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false);
    let result = if is_dir {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => *deleted += 1,
        Err(err) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            errors.push(format!("{name}: {err}"));
        }
    }
}

fn delete_summary(deleted: usize, errors: &[String]) -> String {
    if errors.is_empty() {
        format!("Deleted {deleted} file(s)")
    } else {
        format!("Deleted {deleted}; errors: {}", errors.join("; "))
    }
}

/// The user's home directory, falling back to environment variables.
/// Whether `name` looks like an Emacs auto-save file (`#...#`), matching
/// `auto-save-file-name-p`.
fn is_auto_save_file(name: &str) -> bool {
    name.len() >= 2 && name.starts_with('#') && name.ends_with('#')
}

/// Whether `name` is an Emacs backup file (ends with `~`), matching
/// `backup-file-name-p`.
fn is_backup_file(name: &str) -> bool {
    name.ends_with('~')
}

/// Whether a `yes-or-no-p` answer means "yes".
fn answer_is_yes(answer: &str) -> bool {
    answer.trim().eq_ignore_ascii_case("yes")
}

/// Whether a `yes-or-no-p` answer means "no".
fn answer_is_no(answer: &str) -> bool {
    answer.trim().eq_ignore_ascii_case("no")
}

/// An answer to the recursive-delete `read-answer` question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecursiveAnswer {
    Yes,
    No,
    All,
    Quit,
    Invalid,
}

/// Parse `dired-delete-file`'s answers: yes, no, all, and quit.
fn recursive_answer(answer: &str) -> RecursiveAnswer {
    match answer.trim().to_ascii_lowercase().as_str() {
        "yes" => RecursiveAnswer::Yes,
        "no" => RecursiveAnswer::No,
        "all" => RecursiveAnswer::All,
        "quit" => RecursiveAnswer::Quit,
        _ => RecursiveAnswer::Invalid,
    }
}

/// A directory name as minibuffer input, with a trailing separator.
fn directory_input(dir: &std::path::Path) -> String {
    let mut input = dir.display().to_string();
    if !input.ends_with(std::path::MAIN_SEPARATOR) {
        input.push(std::path::MAIN_SEPARATOR);
    }
    input
}

fn home_dir() -> Option<PathBuf> {
    if let Some(dirs) = directories::UserDirs::new() {
        return Some(dirs.home_dir().to_path_buf());
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The text a key contributes to a paste, if any (printable characters,
/// Enter, and Tab). Modifier and navigation keys contribute nothing.
fn paste_char(key: &KeyEvent) -> Option<char> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) if !ctrl && !alt => Some(c),
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab => Some('\t'),
        _ => None,
    }
}

/// Move a `FindFile` candidate selection: `None` means "use the typed input".
/// The first Up from no selection lands on the last candidate, the first Down
/// on the first.
fn select_move(selected: Option<usize>, count: usize, delta: isize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let last = count - 1;
    let next = match selected {
        None => {
            if delta < 0 {
                last
            } else {
                0
            }
        }
        Some(s) => (s as isize + delta).clamp(0, last as isize) as usize,
    };
    Some(next)
}

/// Whether `key` is a motion command that should leave isearch and run.
fn isearch_motion(key: KeyEvent) -> bool {
    use Action::*;
    matches!(
        keymap::translate(key),
        ForwardChar
            | BackwardChar
            | NextLine
            | PrevLine
            | BeginningOfLine
            | EndOfLine
            | ForwardWord
            | BackwardWord
            | BeginningOfBuffer
            | EndOfBuffer
            | PageUp
            | PageDown
    )
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The char index just past the next word from `start`.
fn word_forward(text: &str, start: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut idx = start.min(chars.len());
    while idx < chars.len() && !is_word_char(chars[idx]) {
        idx += 1;
    }
    while idx < chars.len() && is_word_char(chars[idx]) {
        idx += 1;
    }
    idx
}

/// The char index at the start of the word before `start`.
fn word_backward(text: &str, start: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut idx = start.min(chars.len());
    while idx > 0 && !is_word_char(chars[idx - 1]) {
        idx -= 1;
    }
    while idx > 0 && is_word_char(chars[idx - 1]) {
        idx -= 1;
    }
    idx
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

/// A one-line, length-limited rendering of region text for the `M-w` echo.
fn snippet(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > 40 {
        joined.chars().take(40).collect::<String>() + "…"
    } else {
        joined
    }
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
        "diredrs - a directory editor with Emacs-style editing".to_string(),
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
        "  C-x 2 / C-x 3  split the window below / to the right".to_string(),
        "  C-x 0 / C-x 1  delete this / other windows".to_string(),
        "  C-x o          move to the other window".to_string(),
        "  C-x b          switch buffers".to_string(),
        "  C-x 4 b        switch buffers in the other window".to_string(),
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
        "  C-o            insert a newline, leaving point before it",
        "  M-u / M-l      upcase / downcase word",
        "  C-t / M-t      transpose characters / words",
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
        "Dired",
        "  n / p / C-n / C-p   next / previous line",
        "  SPC                 next line",
        "  DEL                 unmark backward (and move up)",
        "  RET / f / e         visit the file or directory at point",
        "  ^                   go up to the parent directory",
        "  g                   revert the listing",
        "  q                   bury the Dired buffer",
        "  m / u               mark / unmark (and move down)",
        "  d                   flag for deletion",
        "  x                   delete flagged files",
        "  D                   delete marked files",
        "  R                   rename / move file(s)",
        "  C                   copy file(s)",
        "  t / U               toggle / remove all marks",
        "  * !                 unmark all files",
        "  * .                 mark files by extension",
        "  #                   flag auto-save files for deletion",
        "  ~                   flag backup files for deletion",
        "  +                   create a directory",
        "  w                   copy marked file name(s) to the kill ring",
        "  s                   cycle sorting: name / date / size",
        "  < / >               previous / next directory line",
        "  C-x C-q             edit file names in the listing (Wdired)",
        "",
        "Wdired",
        "  C-c C-c / C-x C-q   commit renames and return to Dired",
        "  C-c ESC / C-c C-k   abort changes and return to Dired",
        "  typing              edit the filename at point",
        "  C-k / M-d / C-y     kill and yank within the filename",
        "",
        "Files and buffers",
        "  C-x C-f        find file (visit a file by path; TAB completes)",
        "  C-x d          open a directory in Dired",
        "  C-x b          switch buffer",
        "  C-x k          kill buffer",
        "  C-x C-x        exchange point and mark",
        "  C-x C-c        quit",
        "",
        "Windows",
        "  C-x 2          split the window below",
        "  C-x 3          split the window to the right",
        "  C-x 0          delete the active window",
        "  C-x 1          delete all other windows",
        "  C-x o          move to the next window",
        "  C-x 4 b        switch buffer in the other window",
        "  M-g M-f/M-b    move focus right / left",
        "  M-g M-p/M-n    move focus up / down",
        "  M-g 1-9 / 0    jump to window 1-9 / 10",
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
        "  On Windows Terminal Ctrl-V pastes (the terminal owns the chord); use",
        "  the PageDown key, or unbind Ctrl-V in the terminal to get C-v back.",
        "  Pasting into a read-only buffer is refused (\"Text is read-only\").",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dired::SortKey;
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    fn mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        app.handle_term_event(TermEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }

    fn press(app: &mut App, c: char) {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, c);
        }
    }

    fn replace_wdired_name(app: &mut App, name: &str) {
        ctrl(app, 'a');
        ctrl(app, 'k');
        type_text(app, name);
    }

    fn alt(app: &mut App, c: char) {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT));
    }

    fn ctrl(app: &mut App, c: char) {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn ctrl_x(app: &mut App, c: char) {
        ctrl(app, 'x');
        press(app, c);
    }

    fn switch_to_scratch_in_other_window(app: &mut App) {
        ctrl_x(app, '4');
        press(app, 'b');
        if let Prompt::SwitchBuffer {
            input,
            cursor,
            selected,
            ..
        } = &mut app.prompt
        {
            *input = "*scratch*".to_string();
            *cursor = input.chars().count();
            *selected = 0;
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    fn only_split_axis(app: &App) -> SplitAxis {
        match app.window_tree() {
            WindowTree::Split { axis, .. } => *axis,
            WindowTree::Leaf(_) => panic!("expected a split window tree"),
        }
    }

    fn start_find_prompt(app: &mut App, input: &str) {
        app.prompt = Prompt::FindFile {
            input: input.to_string(),
            cursor: 0,
            dired: false,
            selected: None,
        };
    }

    fn prompt_text(app: &App) -> (String, usize) {
        match &app.prompt {
            Prompt::FindFile { input, cursor, .. } => (input.clone(), *cursor),
            _ => panic!("expected the find-file prompt"),
        }
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
    fn c_x_2_and_3_split_the_active_window() {
        let mut app = App::new(Config::default());
        let active = app.active_window;
        let current = app.current;
        assert_eq!(app.windows.len(), 1);

        ctrl_x(&mut app, '2');
        assert_eq!(app.windows.len(), 2);
        assert_eq!(app.active_window, active);
        assert_eq!(app.current, current);
        assert!(app.windows.iter().all(|w| w.buffer == current));

        ctrl_x(&mut app, '3');
        assert_eq!(app.windows.len(), 3);
        assert_eq!(app.active_window, active);
        let ids = app.window_ids();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], active);
        assert!(app.windows.iter().all(|w| w.buffer == current));
    }

    #[test]
    fn dragging_the_vertical_divider_resizes_windows() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = App::new(Config::default());
        ctrl_x(&mut app, '3');
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();

        let first = app.window_ids()[0];
        let width = |app: &App, id| {
            app.window_areas
                .iter()
                .find(|(window, _)| *window == id)
                .unwrap()
                .1
                .width
        };
        let before = width(&app, first);
        let divider = app
            .dividers
            .iter()
            .find(|d| d.axis == SplitAxis::Right)
            .cloned()
            .unwrap();
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            divider.rect.x,
            divider.rect.y,
        );
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            divider.rect.x + 5,
            divider.rect.y,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            divider.rect.x + 5,
            divider.rect.y,
        );
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        assert_eq!(width(&app, first), before + 5);

        // Dragging past the left edge clamps the first pane to one column.
        let divider = app
            .dividers
            .iter()
            .find(|d| d.axis == SplitAxis::Right)
            .cloned()
            .unwrap();
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            divider.rect.x,
            divider.rect.y,
        );
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            0,
            divider.rect.y,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            0,
            divider.rect.y,
        );
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        assert_eq!(width(&app, first), 1);
    }

    #[test]
    fn dragging_the_upper_mode_line_resizes_stacked_windows() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = App::new(Config::default());
        ctrl_x(&mut app, '2');
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();

        let first = app.window_ids()[0];
        let height = |app: &App, id| {
            app.window_areas
                .iter()
                .find(|(window, _)| *window == id)
                .unwrap()
                .1
                .height
        };
        let before = height(&app, first);
        let divider = app
            .dividers
            .iter()
            .find(|d| d.axis == SplitAxis::Below)
            .cloned()
            .unwrap();
        assert_eq!(divider.rect.height, 1);
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            divider.rect.x,
            divider.rect.y,
        );
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            divider.rect.x,
            divider.rect.y + 3,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            divider.rect.x,
            divider.rect.y + 3,
        );
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        assert_eq!(height(&app, first), before + 3);

        // Dragging to the top clamps the upper pane to one content row.
        let divider = app
            .dividers
            .iter()
            .find(|d| d.axis == SplitAxis::Below)
            .cloned()
            .unwrap();
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            divider.rect.x,
            divider.rect.y,
        );
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            divider.rect.x,
            0,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            divider.rect.x,
            0,
        );
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        assert_eq!(height(&app, first), 1);
    }

    #[test]
    fn dragging_outside_a_divider_does_not_resize_windows() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = App::new(Config::default());
        ctrl_x(&mut app, '3');
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let before: Vec<(WindowId, Rect)> = app.window_areas.clone();

        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 0);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 20, 10);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 20, 10);
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        assert_eq!(app.window_areas, before);
    }

    #[test]
    fn c_x_0_deletes_the_active_window_and_c_x_1_keeps_it() {
        let mut app = App::new(Config::default());
        ctrl_x(&mut app, '2');
        ctrl_x(&mut app, '3');
        let before = app.window_ids();
        assert_eq!(before.len(), 3);

        ctrl_x(&mut app, '0');
        assert_eq!(app.windows.len(), 2);
        assert_eq!(app.active_window, before[1]);
        assert_eq!(app.window_ids(), vec![before[1], before[2]]);
        assert_eq!(app.current, app.window(app.active_window).unwrap().buffer);

        let active = app.active_window;
        let current = app.current;
        ctrl_x(&mut app, '1');
        assert_eq!(app.windows.len(), 1);
        assert_eq!(app.active_window, active);
        assert_eq!(app.current, current);
        assert_eq!(app.window_ids(), vec![active]);

        app.message = None;
        ctrl_x(&mut app, '0');
        assert_eq!(app.windows.len(), 1);
        assert_eq!(app.message.as_deref(), Some("Only one window"));
    }

    #[test]
    fn c_x_o_preserves_independent_views() {
        let mut app = App::new(Config::default());
        let scratch = app
            .buffers
            .iter()
            .position(|b| b.kind == Kind::Scratch)
            .unwrap();
        app.switch_to(scratch);
        let first = app.active_window;

        ctrl_x(&mut app, '2');
        ctrl(&mut app, 'n');
        let first_point = app.current().buffer.point();
        assert_eq!(first_point.row, 1);

        ctrl_x(&mut app, 'o');
        assert_ne!(app.active_window, first);
        assert_eq!(app.current().buffer.point(), Pos::new(0, 0));

        ctrl(&mut app, 'n');
        ctrl(&mut app, 'n');
        let second_point = app.current().buffer.point();
        assert_eq!(second_point.row, 2);

        ctrl_x(&mut app, 'o');
        assert_eq!(app.active_window, first);
        assert_eq!(app.current().buffer.point(), first_point);
        ctrl_x(&mut app, 'o');
        assert_eq!(app.current().buffer.point(), second_point);
    }

    #[test]
    fn m_g_moves_focus_directionally() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = App::new(Config::default());
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        ctrl_x(&mut app, '2');
        ctrl_x(&mut app, '3');
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();

        let ids = app.window_ids();
        let left = ids[0];
        let right = ids[1];
        let bottom = ids[2];
        assert_eq!(app.active_window, left);

        alt(&mut app, 'g');
        alt(&mut app, 'f');
        assert_eq!(app.active_window, right);

        alt(&mut app, 'g');
        alt(&mut app, 'b');
        assert_eq!(app.active_window, left);

        alt(&mut app, 'g');
        alt(&mut app, 'n');
        assert_eq!(app.active_window, bottom);

        alt(&mut app, 'g');
        alt(&mut app, 'p');
        assert_eq!(app.active_window, left);
    }

    #[test]
    fn m_g_numbers_jump_and_follow_tree_order() {
        let mut app = App::new(Config::default());
        ctrl_x(&mut app, '2');
        ctrl_x(&mut app, '3');
        let ids = app.window_ids();
        assert_eq!(app.window_number(ids[0]), Some(1));
        assert_eq!(app.window_number(ids[1]), Some(2));
        assert_eq!(app.window_number(ids[2]), Some(3));

        alt(&mut app, 'g');
        press(&mut app, '2');
        assert_eq!(app.active_window, ids[1]);

        alt(&mut app, 'g');
        press(&mut app, '0');
        assert_eq!(app.active_window, ids[1]);
        assert_eq!(app.message.as_deref(), Some("No window 0"));

        ctrl_x(&mut app, '0');
        let after = app.window_ids();
        assert_eq!(after, vec![ids[0], ids[2]]);
        assert_eq!(app.window_number(after[0]), Some(1));
        assert_eq!(app.window_number(after[1]), Some(2));

        alt(&mut app, 'g');
        press(&mut app, '1');
        assert_eq!(app.active_window, after[0]);
    }

    #[test]
    fn waiting_after_m_g_shows_numbers_until_the_next_key() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = App::new(Config::default());
        ctrl_x(&mut app, '2');
        alt(&mut app, 'g');
        assert!(!app.show_window_numbers());

        let delay = Duration::from_millis(app.config.ui.window_number_delay_ms);
        assert_eq!(delay, Duration::from_millis(250));
        app.echo_pending_prefix(Instant::now() + Duration::from_millis(100));
        assert!(!app.show_window_numbers());
        app.echo_pending_prefix(Instant::now() + delay);
        assert!(app.show_window_numbers());

        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let blocks = (0..12u16)
            .flat_map(|y| (0..40u16).map(move |x| (x, y)))
            .filter(|(x, y)| buffer[(*x, *y)].symbol() == "█")
            .count();
        assert!(blocks > 0);

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert!(!app.show_window_numbers());
    }

    #[test]
    fn m_g_overlay_delay_is_configurable() {
        let mut config = Config::default();
        config.ui.window_number_delay_ms = 600;
        let mut app = App::new(config);
        alt(&mut app, 'g');
        app.echo_pending_prefix(Instant::now() + Duration::from_millis(500));
        assert!(!app.show_window_numbers());
        app.echo_pending_prefix(Instant::now() + Duration::from_millis(600));
        assert!(app.show_window_numbers());
    }

    #[test]
    fn c_x_4_b_splits_when_needed_and_switches_the_other_window() {
        let dir = temp_dir("other-buffer");
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        let dired = app.active_window;
        let dired_buffer = app.current;

        ctrl_x(&mut app, '4');
        press(&mut app, 'b');
        assert!(matches!(
            app.prompt,
            Prompt::SwitchBuffer {
                other_window: true,
                ..
            }
        ));
        assert!(app
            .prompt_view()
            .unwrap()
            .0
            .starts_with("Switch to buffer in other window"));
        if let Prompt::SwitchBuffer {
            input,
            cursor,
            selected,
            ..
        } = &mut app.prompt
        {
            *input = "*scratch*".to_string();
            *cursor = input.chars().count();
            *selected = 0;
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.windows.len(), 2);
        assert_ne!(app.active_window, dired);
        assert_eq!(app.current().kind, Kind::Scratch);
        assert_eq!(app.window(dired).unwrap().buffer, dired_buffer);
        let other = app.window(app.active_window).unwrap();
        assert_eq!(other.buffer, app.current);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_x_4_b_splits_wide_windows_side_by_side() {
        let dir = temp_dir("other-buffer-wide");
        let mut app = App::new(Config::default());
        app.prepare_active_view(100, 20);
        app.open_dired(dir.clone());

        switch_to_scratch_in_other_window(&mut app);

        assert_eq!(app.windows.len(), 2);
        assert_eq!(only_split_axis(&app), SplitAxis::Right);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_x_4_b_stacks_tall_windows() {
        let dir = temp_dir("other-buffer-tall");
        let mut app = App::new(Config::default());
        app.prepare_active_view(30, 40);
        app.open_dired(dir.clone());

        switch_to_scratch_in_other_window(&mut app);

        assert_eq!(app.windows.len(), 2);
        assert_eq!(only_split_axis(&app), SplitAxis::Below);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_x_4_b_uses_the_next_window_when_already_split() {
        let dir = temp_dir("other-existing");
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        ctrl_x(&mut app, '2');
        let first = app.active_window;
        let other = app.next_window_id().unwrap();
        let dired = app.current;

        ctrl_x(&mut app, '4');
        press(&mut app, 'b');
        if let Prompt::SwitchBuffer {
            input,
            cursor,
            selected,
            ..
        } = &mut app.prompt
        {
            *input = "*scratch*".to_string();
            *cursor = input.chars().count();
            *selected = 0;
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.windows.len(), 2);
        assert_eq!(app.active_window, other);
        assert_eq!(app.current().kind, Kind::Scratch);
        assert_eq!(app.window(first).unwrap().buffer, dired);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn split_windows_render_both_mode_lines() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let dir = temp_dir("split-render");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let mut app = App::new(Config::default());
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        app.open_dired(dir.clone());

        ctrl_x(&mut app, '3');
        ctrl_x(&mut app, '4');
        press(&mut app, 'b');
        if let Prompt::SwitchBuffer {
            input,
            cursor,
            selected,
            ..
        } = &mut app.prompt
        {
            *input = "*scratch*".to_string();
            *cursor = input.chars().count();
            *selected = 0;
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let row = |y: u16| {
            (0..40u16)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        let border = (0..40u16)
            .find(|x| buffer[(*x, 0)].symbol() == "│")
            .expect("missing vertical border");
        for y in 0..11u16 {
            assert_eq!(buffer[(border, y)].symbol(), "│", "divider gap at row {y}");
        }

        let mode_line = row(10);
        assert!(mode_line.contains("*scratch*"), "{mode_line}");
        assert!(mode_line.contains(" 1 "), "{mode_line}");
        assert!(mode_line.contains(" 2 "), "{mode_line}");
        let reversed = (0..40u16)
            .filter(|x| {
                buffer[(*x, 10)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            })
            .count();
        assert_eq!(reversed, 39, "{mode_line}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn killing_a_buffer_repairs_every_window() {
        let dir = temp_dir("kill-window");
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        let dired = app.current;

        ctrl_x(&mut app, '4');
        press(&mut app, 'b');
        if let Prompt::SwitchBuffer {
            input,
            cursor,
            selected,
            ..
        } = &mut app.prompt
        {
            *input = "*scratch*".to_string();
            *cursor = input.chars().count();
            *selected = 0;
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.current().kind, Kind::Scratch);

        app.kill_buffer(dired);
        assert_eq!(app.windows.len(), 2);
        assert!(app.windows.iter().all(|w| w.buffer < app.buffers.len()));
        assert!(!app
            .windows
            .iter()
            .any(|w| app.buffers[w.buffer].name == dir.display().to_string()));
        assert_eq!(app.current, app.window(app.active_window).unwrap().buffer);

        std::fs::remove_dir_all(&dir).ok();
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
    fn dired_ret_on_dotdot_opens_the_parent() {
        let dir = temp_dir("parent-entry");
        let child = dir.join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("note.txt"), b"x").unwrap();

        let mut app = App::new(Config::default());
        app.open_dired(child.clone());
        let d = app.current().dired.as_ref().unwrap();
        assert_eq!(d.parent_path(), Some(dir.as_path()));
        assert!(app.current().buffer.line(1).text().contains(".."));

        // Point starts on note.txt; `p` moves onto `..`, RET visits the parent.
        press(&mut app, 'p');
        assert_eq!(app.current().buffer.point().row, 1);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.current().name, dir.display().to_string());

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
    fn hex_view_links_hex_and_ascii() {
        let dir = temp_dir("hexlink");
        let path = dir.join("data.bin");
        std::fs::write(&path, [0u8, 1, 2, 3, 0xff]).unwrap();

        let mut app = App::new(Config::default());
        app.open_file(path.clone());
        assert_eq!(app.current().kind, Kind::Hexl);

        let w = app.current().hexl_width.unwrap();
        let hex_start = w + 2;
        let ascii_start = w + 43;

        // Point on byte 1's hex digit -> that hex pair and its ASCII char.
        app.current_mut()
            .buffer
            .set_point(Pos::new(0, hex_start + 2));
        let hl = app.hexl_highlights();
        assert!(hl.contains(&(Pos::new(0, hex_start + 2), 2)));
        assert!(hl.contains(&(Pos::new(0, ascii_start + 1), 1)));

        // Point on ASCII char 3 -> its hex pair and the ASCII char.
        app.current_mut()
            .buffer
            .set_point(Pos::new(0, ascii_start + 3));
        let hl = app.hexl_highlights();
        assert!(hl.contains(&(Pos::new(0, hex_start + 7), 2)));
        assert!(hl.contains(&(Pos::new(0, ascii_start + 3), 1)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn marks_flags_and_deletes() {
        let dir = temp_dir("marks");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();

        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        // Mark the first entry; the row shows `*` and point advances.
        press(&mut app, 'm');
        let row0 = app.current().dired.as_ref().unwrap().row_of(0);
        assert_eq!(
            app.current().dired.as_ref().unwrap().entries[0].mark,
            Mark::Marked
        );
        assert!(app.current().buffer.line(row0).text().starts_with("* "));

        // Flag the second entry, then delete flagged files. The confirmation
        // is a `yes-or-no-p`: type the answer, then RET.
        press(&mut app, 'd');
        assert_eq!(
            app.current().dired.as_ref().unwrap().entries[1].mark,
            Mark::Flagged
        );
        press(&mut app, 'x');
        assert!(matches!(app.prompt, Prompt::Confirm { .. }));
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!dir.join("b.txt").exists());
        let d = app.current().dired.as_ref().unwrap();
        assert_eq!(d.entries.len(), 1);
        assert_eq!(d.entries[0].name, "a.txt");
        // The mark on a.txt survives the revert.
        assert_eq!(d.entries[0].mark, Mark::Marked);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn d_and_x_delete_only_their_own_marks() {
        let dir = temp_dir("mixed-marks");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        std::fs::write(dir.join("c.txt"), b"c").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'm'); // * a.txt
        press(&mut app, 'd'); // D b.txt
        press(&mut app, 'D');
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        // `D` removed only the `*` mark; the flagged file remains flagged.
        assert!(!dir.join("a.txt").exists());
        assert!(dir.join("b.txt").exists());
        assert!(dir.join("c.txt").exists());
        let marks: Vec<(String, Mark)> = app
            .current()
            .dired
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .map(|e| (e.name.clone(), e.mark))
            .collect();
        assert_eq!(
            marks,
            [
                ("b.txt".to_string(), Mark::Flagged),
                ("c.txt".to_string(), Mark::None)
            ]
        );

        // `x` then removes only the flagged file.
        press(&mut app, 'x');
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!dir.join("b.txt").exists());
        assert!(dir.join("c.txt").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn d_without_marks_deletes_the_file_at_point() {
        let dir = temp_dir("delete-current");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        // No marks: `D` asks about the current file by name.
        press(&mut app, 'D');
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. } if message == "Delete a.txt (yes or no) "
        ));
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!dir.join("a.txt").exists());
        assert!(dir.join("b.txt").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deleting_a_file_keeps_point_on_the_nearest_entry() {
        let dir = temp_dir("delete-point");
        for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        let idx = app.current;
        app.dired_goto_name(idx, "c.txt");

        press(&mut app, 'D');
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let entry = app.dired_entry_at_point().unwrap();
        assert_eq!(
            app.current().dired.as_ref().unwrap().entries[entry].name,
            "d.txt"
        );

        press(&mut app, 'D');
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let entry = app.dired_entry_at_point().unwrap();
        assert_eq!(
            app.current().dired.as_ref().unwrap().entries[entry].name,
            "b.txt"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deleting_a_nonempty_directory_asks_to_recurse() {
        let dir = temp_dir("delete-recursive");
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("nested.txt"), b"x").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'D');
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. } if message == "Recursively delete sub? (yes, no, all, quit) "
        ));
        assert!(sub.join("nested.txt").exists());

        // Short answers are rejected with a timed message.
        type_into_prompt(&mut app, "y");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.prompt, Prompt::None));
        assert_eq!(
            app.message.as_deref(),
            Some("Please answer 'yes' or 'no' or 'all' or 'quit'.")
        );
        assert!(sub.join("nested.txt").exists());

        // Once the message times out, the question returns.
        app.expire_confirm_flash(Instant::now() + CONFIRM_FLASH);
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. }
                if message == "Recursively delete sub? (yes, no, all, quit) "
        ));
        assert!(app.message.is_none());

        // `no` skips this directory.
        type_into_prompt(&mut app, "no");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(sub.join("nested.txt").exists());
        assert!(matches!(app.prompt, Prompt::None));

        press(&mut app, 'D');
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!sub.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn all_skips_the_remaining_recursive_questions() {
        let dir = temp_dir("delete-recursive-all");
        for name in ["aaa", "bbb"] {
            let sub = dir.join(name);
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("nested.txt"), b"x").unwrap();
        }
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'd');
        press(&mut app, 'd');
        press(&mut app, 'x');
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. }
                if message == "Recursively delete aaa? (yes, no, all, quit) "
        ));

        type_into_prompt(&mut app, "all");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!dir.join("aaa").exists());
        assert!(!dir.join("bbb").exists());
        assert!(matches!(app.prompt, Prompt::None));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn quit_stops_the_remaining_recursive_deletions() {
        let dir = temp_dir("delete-recursive-quit");
        for name in ["aaa", "bbb"] {
            let sub = dir.join(name);
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("nested.txt"), b"x").unwrap();
        }
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'd');
        press(&mut app, 'd');
        press(&mut app, 'x');
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        type_into_prompt(&mut app, "quit");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(dir.join("aaa").exists());
        assert!(dir.join("bbb").exists());
        assert!(matches!(app.prompt, Prompt::None));
        assert_eq!(app.message.as_deref(), Some("OK, canceled"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deleting_an_empty_directory_does_not_ask_to_recurse() {
        let dir = temp_dir("delete-empty-recursive");
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'D');
        type_into_prompt(&mut app, "yes");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!sub.exists());
        assert!(matches!(app.prompt, Prompt::None));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_confirmation_needs_the_answer_then_ret() {
        let dir = temp_dir("confirm");
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        press(&mut app, 'd'); // flag a.txt
        press(&mut app, 'x'); // ask

        // A bare `y` is not an accepted answer: show the choices instead.
        press(&mut app, 'y');
        assert!(dir.join("a.txt").exists());
        assert!(matches!(app.prompt, Prompt::Confirm { .. }));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(dir.join("a.txt").exists());
        assert!(matches!(app.prompt, Prompt::None));
        assert_eq!(app.message.as_deref(), Some("Please answer 'yes' or 'no'."));

        // Typing while the message is visible returns to the prompt with the
        // first keystroke retained.
        press(&mut app, 'y');
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { input, .. } if input == "y"
        ));
        assert!(app.message.is_none());
        type_into_prompt(&mut app, "es");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!dir.join("a.txt").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn canceling_delete_before_confirmation_leaves_files_untouched() {
        let dir = temp_dir("delete-cancel");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'd'); // flag a.txt
        press(&mut app, 'x'); // ask
        assert!(matches!(app.prompt, Prompt::Confirm { .. }));
        ctrl(&mut app, 'g');
        assert!(matches!(app.prompt, Prompt::None));
        assert!(dir.join("a.txt").exists());
        assert!(dir.join("b.txt").exists());

        // With no marks, `D` offers the file at point; `no` cancels it.
        press(&mut app, 'D');
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. } if message == "Delete b.txt (yes or no) "
        ));
        answer(&mut app, "no");
        assert!(dir.join("a.txt").exists());
        assert!(dir.join("b.txt").exists());
        assert!(matches!(app.prompt, Prompt::None));
        assert_eq!(app.message.as_deref(), Some("Canceled"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn header_and_parent_rows_never_offer_delete() {
        let dir = temp_dir("delete-nonfile-rows");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        // Point starts on a.txt; move up onto the synthetic `..` row.
        press(&mut app, 'p');
        assert_eq!(app.current().buffer.point().row, 1);
        press(&mut app, 'd');
        assert_eq!(app.message.as_deref(), Some("No file on this line"));
        press(&mut app, 'D');
        assert!(matches!(app.prompt, Prompt::None));
        assert_eq!(app.message.as_deref(), Some("No files marked"));
        press(&mut app, 'x');
        assert!(matches!(app.prompt, Prompt::None));
        assert_eq!(
            app.message.as_deref(),
            Some("No files flagged for deletion")
        );

        // The header line is equally untouchable.
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.current().buffer.point().row, 0);
        press(&mut app, 'D');
        assert!(matches!(app.prompt, Prompt::None));
        assert_eq!(app.message.as_deref(), Some("No files marked"));
        press(&mut app, 'x');
        assert!(matches!(app.prompt, Prompt::None));
        assert_eq!(
            app.message.as_deref(),
            Some("No files flagged for deletion")
        );

        assert!(dir.join("a.txt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recursive_delete_handles_hidden_contents_and_preserves_siblings() {
        let dir = temp_dir("delete-recursive-tree");
        let victim = dir.join("victim");
        std::fs::create_dir_all(victim.join("nested")).unwrap();
        std::fs::create_dir_all(victim.join(".hidden-dir")).unwrap();
        std::fs::write(victim.join("nested").join("deep.txt"), b"x").unwrap();
        std::fs::write(victim.join(".hidden"), b"x").unwrap();
        std::fs::write(victim.join(".hidden-dir").join("secret.txt"), b"x").unwrap();
        let sibling_dir = dir.join("sibling-dir");
        std::fs::create_dir(&sibling_dir).unwrap();
        std::fs::write(sibling_dir.join("keep.txt"), b"x").unwrap();
        std::fs::write(dir.join("sibling.txt"), b"x").unwrap();
        let hidden_only = dir.join("hidden-only");
        std::fs::create_dir(&hidden_only).unwrap();
        std::fs::write(hidden_only.join(".secret"), b"x").unwrap();

        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        let idx = app.current;
        app.dired_goto_name(idx, "victim");
        press(&mut app, 'D');
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. } if message == "Delete victim (yes or no) "
        ));
        answer(&mut app, "yes");
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. }
                if message == "Recursively delete victim? (yes, no, all, quit) "
        ));
        answer(&mut app, "yes");

        assert!(!victim.exists());
        assert!(dir.join("sibling.txt").exists());
        assert!(sibling_dir.join("keep.txt").exists());

        // A directory whose only contents are hidden still needs the warning.
        let idx = app.current;
        app.dired_goto_name(idx, "hidden-only");
        press(&mut app, 'D');
        answer(&mut app, "yes");
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. }
                if message == "Recursively delete hidden-only? (yes, no, all, quit) "
        ));
        assert!(hidden_only.join(".secret").exists());
        answer(&mut app, "yes");
        assert!(!hidden_only.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recursive_no_skips_only_that_directory_and_continues() {
        let dir = temp_dir("delete-recursive-no");
        for name in ["aaa", "bbb"] {
            let sub = dir.join(name);
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("nested.txt"), b"x").unwrap();
        }
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'd');
        press(&mut app, 'd');
        press(&mut app, 'x');
        answer(&mut app, "yes");
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. }
                if message == "Recursively delete aaa? (yes, no, all, quit) "
        ));

        answer(&mut app, "no");
        assert!(dir.join("aaa").join("nested.txt").exists());
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. }
                if message == "Recursively delete bbb? (yes, no, all, quit) "
        ));

        answer(&mut app, "yes");
        assert!(dir.join("aaa").join("nested.txt").exists());
        assert!(!dir.join("bbb").exists());
        assert!(matches!(app.prompt, Prompt::None));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recursive_quit_keeps_prior_deletions_and_remaining_targets() {
        let dir = temp_dir("delete-recursive-partial-quit");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        for name in ["bbb", "ccc"] {
            let sub = dir.join(name);
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("nested.txt"), b"x").unwrap();
        }
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 't'); // mark every entry
        press(&mut app, 'D');
        answer(&mut app, "yes");
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. }
                if message == "Recursively delete bbb? (yes, no, all, quit) "
        ));
        answer(&mut app, "yes");
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. }
                if message == "Recursively delete ccc? (yes, no, all, quit) "
        ));
        answer(&mut app, "quit");

        assert!(!dir.join("a.txt").exists());
        assert!(!dir.join("bbb").exists());
        assert!(dir.join("ccc").join("nested.txt").exists());
        assert!(matches!(app.prompt, Prompt::None));
        assert_eq!(app.message.as_deref(), Some("OK, canceled"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deleting_symlinks_removes_links_not_targets() {
        let root = temp_dir("delete-symlinks");
        let dired_dir = root.join("dired");
        let target_dir = root.join("target-dir");
        let target_file = root.join("target-file.txt");
        std::fs::create_dir(&dired_dir).unwrap();
        std::fs::create_dir(&target_dir).unwrap();
        std::fs::write(target_dir.join("keep.txt"), b"x").unwrap();
        std::fs::write(&target_file, b"x").unwrap();

        let file_link = dired_dir.join("file-link");
        let dir_link = dired_dir.join("dir-link");
        if !try_symlink(&target_file, &file_link, false)
            || !try_symlink(&target_dir, &dir_link, true)
        {
            eprintln!("skipping symlink deletion test: cannot create symlinks");
            std::fs::remove_dir_all(&root).ok();
            return;
        }

        let mut app = App::new(Config::default());
        app.open_dired(dired_dir.clone());
        press(&mut app, 't');
        press(&mut app, 'x');
        answer(&mut app, "yes");

        assert!(matches!(app.prompt, Prompt::None));
        assert!(!file_link.exists());
        assert!(!dir_link.exists());
        assert!(target_file.exists());
        assert!(target_dir.join("keep.txt").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn recursive_delete_does_not_follow_nested_symlinks() {
        let root = temp_dir("delete-nested-symlink");
        let victim = root.join("victim");
        let outside = root.join("outside");
        std::fs::create_dir(&victim).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(victim.join("ordinary.txt"), b"x").unwrap();
        std::fs::write(outside.join("keep.txt"), b"x").unwrap();
        if !try_symlink(&outside, &victim.join("outside-link"), true) {
            eprintln!("skipping nested symlink test: cannot create symlinks");
            std::fs::remove_dir_all(&root).ok();
            return;
        }

        let mut app = App::new(Config::default());
        app.open_dired(root.clone());
        let idx = app.current;
        app.dired_goto_name(idx, "victim");
        press(&mut app, 'D');
        answer(&mut app, "yes");
        assert!(matches!(
            &app.prompt,
            Prompt::Confirm { message, .. }
                if message == "Recursively delete victim? (yes, no, all, quit) "
        ));
        answer(&mut app, "yes");

        assert!(!victim.exists());
        assert!(outside.join("keep.txt").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn delete_failure_is_reported_and_does_not_abort_the_batch() {
        let dir = temp_dir("delete-failure");
        let missing = dir.join("a-missing.txt");
        let present = dir.join("b-present.txt");
        std::fs::write(&missing, b"a").unwrap();
        std::fs::write(&present, b"b").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'd');
        press(&mut app, 'd');
        std::fs::remove_file(&missing).unwrap();
        press(&mut app, 'x');
        answer(&mut app, "yes");

        assert!(!present.exists());
        let message = app.message.as_deref().unwrap();
        assert!(
            message.starts_with("Deleted 1; errors: a-missing.txt:"),
            "unexpected message: {message}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn w_copies_marked_names() {
        let dir = temp_dir("copy");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();

        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        press(&mut app, 'm'); // mark a.txt
        press(&mut app, 'm'); // mark b.txt
        press(&mut app, 'w');
        assert_eq!(app.kill_ring.current().as_deref(), Some("a.txt b.txt"));
        // The copied names are echoed, like Emacs.
        assert_eq!(app.message.as_deref(), Some("a.txt b.txt"));

        // With nothing marked, the file at point is copied.
        press(&mut app, 'U'); // unmark all; point is still on b.txt
        press(&mut app, 'w');
        assert_eq!(app.kill_ring.current().as_deref(), Some("b.txt"));
        assert_eq!(app.message.as_deref(), Some("b.txt"));

        std::fs::remove_dir_all(&dir).ok();
    }

    fn type_into_prompt(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn answer(app: &mut App, text: &str) {
        type_into_prompt(app, text);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[cfg(unix)]
    fn try_symlink(target: &Path, link: &Path, _is_dir: bool) -> bool {
        std::os::unix::fs::symlink(target, link).expect("create symlink");
        true
    }

    #[cfg(windows)]
    fn try_symlink(target: &Path, link: &Path, is_dir: bool) -> bool {
        let result = if is_dir {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        };
        match result {
            Ok(()) => true,
            Err(err)
                if err.kind() == std::io::ErrorKind::PermissionDenied
                    || err.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(err) => panic!("cannot create symlink {}: {err}", link.display()),
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn try_symlink(_target: &Path, _link: &Path, _is_dir: bool) -> bool {
        false
    }

    #[test]
    fn find_file_visits_a_path() {
        let dir = temp_dir("ff");
        let path = dir.join("hello.txt");
        std::fs::write(&path, b"hi\n").unwrap();

        let mut app = App::new(Config::default());
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(matches!(app.prompt, Prompt::FindFile { dired: false, .. }));

        // Replace the default input with the absolute path.
        if let Prompt::FindFile { input, cursor, .. } = &mut app.prompt {
            *input = String::new();
            *cursor = 0;
        }
        type_into_prompt(&mut app, &path.display().to_string());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.current().kind, Kind::File);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cx_d_opens_a_directory() {
        let dir = temp_dir("cxd");
        std::fs::write(dir.join("x"), b"x").unwrap();

        let mut app = App::new(Config::default());
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(matches!(app.prompt, Prompt::FindFile { dired: true, .. }));

        if let Prompt::FindFile { input, cursor, .. } = &mut app.prompt {
            *input = String::new();
            *cursor = 0;
        }
        type_into_prompt(&mut app, &dir.display().to_string());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.current().kind, Kind::Dired);
        assert_eq!(app.current().name, dir.display().to_string());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn select_move_clamps_at_edges() {
        assert_eq!(select_move(None, 3, 1), Some(0));
        assert_eq!(select_move(None, 3, -1), Some(2));
        assert_eq!(select_move(Some(0), 3, -1), Some(0));
        assert_eq!(select_move(Some(0), 3, 1), Some(1));
        assert_eq!(select_move(Some(2), 3, 1), Some(2));
        assert_eq!(select_move(Some(1), 0, 1), None);
    }

    #[test]
    fn cx_d_arrows_select_a_directory() {
        let dir = temp_dir("cxd-nav");
        std::fs::create_dir(dir.join("aaa")).unwrap();
        std::fs::create_dir(dir.join("bbb")).unwrap();
        std::fs::write(dir.join("file.txt"), b"x").unwrap();

        let mut app = App::new(Config::default());
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        // Point the prompt at our directory.
        if let Prompt::FindFile { input, cursor, .. } = &mut app.prompt {
            *input = format!("{}{}", dir.display(), std::path::MAIN_SEPARATOR);
            *cursor = input.chars().count();
        }
        // Only directories are listed (aaa, bbb); files are excluded.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.current().kind, Kind::Dired);
        let opened = app.current().dired.as_ref().unwrap();
        assert_eq!(opened.dir.file_name().and_then(|n| n.to_str()), Some("bbb"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kill_list_puts_current_first() {
        let dir = temp_dir("killorder");
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        // Open the help buffer; it becomes the current buffer.
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(app.current().name, "*Help*");
        let cands = app.kill_candidates("");
        assert_eq!(app.buffers[cands[0]].name, "*Help*");

        // `C-x k RET` with the default selection kills the current buffer.
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.find_buffer("*Help*").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn switch_list_is_recency_ordered() {
        let a = temp_dir("orderA");
        let b = temp_dir("orderB");
        let mut app = App::new(Config::default());
        app.open_dired(a.clone());
        app.open_dired(b.clone());
        app.open_dired(a.clone()); // current is a

        // Excluding the current buffer, b was visited most recently.
        let cands = app.switch_candidates("");
        assert_eq!(app.buffers[cands[0]].name, b.display().to_string());

        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn dired_can_reach_the_top() {
        let dir = temp_dir("diredtop");
        for i in 0..40 {
            std::fs::write(dir.join(format!("f{i:02}.txt")), b"x").unwrap();
        }
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        // Scroll well down, then walk back up to the first file line.
        app.current_mut().buffer.set_view_top(30, 0);
        app.current_mut().buffer.set_point(Pos::new(35, 0));
        app.ensure_scroll_visible();
        for _ in 0..50 {
            press(&mut app, 'p');
        }
        app.ensure_scroll_visible();

        assert_eq!(
            app.current().buffer.scroll(),
            0,
            "view should reach the top"
        );
        assert_eq!(app.current().buffer.scroll_sub(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dired_c_p_reaches_the_header_line() {
        let dir = temp_dir("diredcp");
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        // Point starts on the first real file line, below `..` (row 2).
        assert_eq!(app.current().buffer.point().row, 2);

        // C-p moves up through `..` to the header line (row 0), at line start.
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.current().buffer.point().row, 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.current().buffer.point(), Pos::new(0, 0));

        // C-n comes back down onto the first file's line.
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.current().buffer.point().row, 2);
        assert_eq!(app.dired_entry_at_point(), Some(0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ctrl_o_opens_a_line_in_scratch() {
        let mut app = App::new(Config::default());
        let scratch = app
            .buffers
            .iter()
            .position(|b| b.kind == Kind::Scratch)
            .unwrap();
        app.switch_to(scratch);
        app.current_mut().buffer = Buffer::new();
        for c in "hello".chars() {
            press(&mut app, c);
        }
        // Point is at column 5; move back to column 2, then open a line.
        for _ in 0..3 {
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));

        assert_eq!(app.current().buffer.line_text(0), "he");
        assert_eq!(app.current().buffer.line_text(1), "llo");
    }

    #[test]
    fn mode_line_shows_the_sort_key() {
        let dir = temp_dir("sortmodeline");
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        assert_eq!(app.mode_line_mode(), "Dired by name");
        press(&mut app, 's');
        assert_eq!(app.mode_line_mode(), "Dired by date");
        press(&mut app, 's');
        assert_eq!(app.mode_line_mode(), "Dired by size");
        press(&mut app, 's');
        assert_eq!(app.mode_line_mode(), "Dired by name");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn angle_brackets_move_between_directory_lines() {
        let dir = temp_dir("dirline");
        std::fs::create_dir(dir.join("adir")).unwrap();
        std::fs::create_dir(dir.join("bdir")).unwrap();
        std::fs::write(dir.join("file.txt"), b"x").unwrap();

        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        let name_at_point = |app: &App| -> String {
            let i = app.dired_entry_at_point().unwrap();
            app.current().dired.as_ref().unwrap().entries[i]
                .name
                .clone()
        };
        // Sorted by name: adir, bdir, file.txt. Point starts on adir.
        assert_eq!(name_at_point(&app), "adir");
        press(&mut app, '>'); // skips file.txt, lands on bdir
        assert_eq!(name_at_point(&app), "bdir");
        press(&mut app, '>'); // no further directory line: point stays
        assert_eq!(name_at_point(&app), "bdir");
        press(&mut app, '<'); // back to adir
        assert_eq!(name_at_point(&app), "adir");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn long_lines_wrap_onto_the_next_row() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let dir = temp_dir("wrap");
        // A 30-char line in a 10-wide buffer area wraps to three rows.
        std::fs::write(dir.join("long.txt"), b"012345678901234567890123456789\n").unwrap();

        let mut app = App::new(Config::default());
        let mut terminal = Terminal::new(TestBackend::new(10, 10)).unwrap();
        app.open_file(dir.join("long.txt"));
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let row = |y: u16| {
            (0..10u16)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        assert_eq!(row(0), "0123456789");
        assert_eq!(row(1), "0123456789");
        assert_eq!(row(2), "0123456789");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn listing_uses_human_readable_sizes() {
        let dir = temp_dir("humantoggle");
        std::fs::write(dir.join("big.bin"), vec![0u8; 2048]).unwrap();
        let mut app = App::new(Config::default());
        assert!(app.human_sizes);
        app.open_dired(dir.clone());
        // Row 2 is the first real file line (header is row 0, `..` is row 1).
        assert!(app.current().buffer.line(2).text().contains("2.0K"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn star_bang_unmarks_all() {
        let dir = temp_dir("unmarkall");
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        press(&mut app, 'm');
        press(&mut app, 'm');
        assert!(app
            .current()
            .dired
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .any(|e| e.mark == Mark::Marked));

        press(&mut app, '*');
        press(&mut app, '!');
        assert!(app
            .current()
            .dired
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .all(|e| e.mark == Mark::None));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn w_on_the_header_copies_the_directory() {
        let dir = temp_dir("copydir");
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        // Move to the header line, then copy: the directory goes on the ring.
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        press(&mut app, 'w');
        let expected = format!("{}{}", dir.display(), std::path::MAIN_SEPARATOR);
        assert_eq!(app.kill_ring.current().as_deref(), Some(expected.as_str()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plus_creates_a_directory() {
        let dir = temp_dir("mkdir");
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, '+');
        assert!(matches!(app.prompt, Prompt::CreateDirectory { .. }));
        if let Prompt::CreateDirectory { input, cursor, .. } = &mut app.prompt {
            *input = format!("{}{}newdir", dir.display(), std::path::MAIN_SEPARATOR);
            *cursor = input.chars().count();
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(dir.join("newdir").is_dir());
        let d = app.current().dired.as_ref().unwrap();
        assert!(d.entries.iter().any(|e| e.name == "newdir"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_flags_auto_save_files() {
        let dir = temp_dir("autosave");
        std::fs::write(dir.join("real.txt"), b"x").unwrap();
        std::fs::write(dir.join("#other#"), b"x").unwrap();
        std::fs::write(dir.join("#real.txt#"), b"x").unwrap();
        std::fs::write(dir.join("#open"), b"x").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        press(&mut app, '#');

        let d = app.current().dired.as_ref().unwrap();
        let flagged: Vec<&str> = d
            .entries
            .iter()
            .filter(|e| e.mark == Mark::Flagged)
            .map(|e| e.name.as_str())
            .collect();
        // Only names that both begin and end with `#`.
        assert_eq!(flagged, ["#other#", "#real.txt#"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tilde_flags_backup_files() {
        let dir = temp_dir("backup");
        std::fs::write(dir.join("real.txt"), b"x").unwrap();
        std::fs::write(dir.join("real.txt~"), b"x").unwrap();
        std::fs::write(dir.join("notes~"), b"x").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        press(&mut app, '~');

        let d = app.current().dired.as_ref().unwrap();
        let flagged: Vec<&str> = d
            .entries
            .iter()
            .filter(|e| e.mark == Mark::Flagged)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(flagged, ["notes~", "real.txt~"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn star_dot_marks_by_extension() {
        let dir = temp_dir("markext");
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        std::fs::write(dir.join("c.rs"), b"x").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        // `* .` opens the extension prompt, defaulted to the current file's.
        press(&mut app, '*');
        press(&mut app, '.');
        if let Prompt::MarkExtension { input, cursor } = &mut app.prompt {
            *input = "txt".to_string();
            *cursor = input.chars().count();
        } else {
            panic!("expected the extension prompt");
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let d = app.current().dired.as_ref().unwrap();
        let marked: Vec<&str> = d
            .entries
            .iter()
            .filter(|e| e.mark == Mark::Marked)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(marked, ["a.txt", "b.txt"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn t_inverts_the_selection() {
        let dir = temp_dir("invert");
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        press(&mut app, 'm'); // mark a.txt

        press(&mut app, 't');
        let d = app.current().dired.as_ref().unwrap();
        let a = d.entries.iter().find(|e| e.name == "a.txt").unwrap();
        let b = d.entries.iter().find(|e| e.name == "b.txt").unwrap();
        assert_eq!(a.mark, Mark::None);
        assert_eq!(b.mark, Mark::Marked);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn r_renames_the_file_at_point() {
        let dir = temp_dir("rename");
        // `aaa.txt` sorts first so the rename target is not the first file.
        std::fs::write(dir.join("aaa.txt"), b"x").unwrap();
        std::fs::write(dir.join("old.txt"), b"hello").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'n'); // move to old.txt
        press(&mut app, 'R');
        if let Prompt::Rename { input, cursor, .. } = &mut app.prompt {
            // Start from the current directory plus the current file name.
            assert_eq!(*input, dir.join("old.txt").display().to_string());
            *input = "new.txt".to_string();
            *cursor = input.chars().count();
        } else {
            panic!("expected the rename prompt");
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!dir.join("old.txt").exists());
        assert!(dir.join("new.txt").exists());
        let d = app.current().dired.as_ref().unwrap();
        assert!(d.entries.iter().any(|e| e.name == "new.txt"));
        // Point stays on the renamed file.
        let i = app.dired_entry_at_point().unwrap();
        assert_eq!(
            app.current().dired.as_ref().unwrap().entries[i].name,
            "new.txt"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn r_renames_marked_files_into_a_target_directory() {
        let dir = temp_dir("rename-marked");
        let target = dir.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'm'); // a.txt
        press(&mut app, 'm'); // b.txt
        press(&mut app, 'R');
        if let Prompt::Rename {
            input,
            cursor,
            copy,
            ..
        } = &mut app.prompt
        {
            // A group rename prompts for a directory, defaulting to the
            // current one.
            assert!(!*copy);
            assert_eq!(*input, directory_input(&dir));
            *input = target.display().to_string();
            *cursor = input.chars().count();
        } else {
            panic!("expected the rename prompt");
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!dir.join("a.txt").exists());
        assert!(!dir.join("b.txt").exists());
        assert_eq!(std::fs::read(target.join("a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(target.join("b.txt")).unwrap(), b"b");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_copies_the_file_at_point() {
        let dir = temp_dir("copy-file");
        let target = dir.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(dir.join("aaa.txt"), b"x").unwrap();
        std::fs::write(dir.join("old.txt"), b"hello").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'n'); // move to old.txt
        press(&mut app, 'C');
        if let Prompt::Rename {
            input,
            cursor,
            copy,
            ..
        } = &mut app.prompt
        {
            assert!(*copy);
            // Copy prompts for a directory, never a file name.
            assert_eq!(*input, directory_input(&dir));
            *input = target.display().to_string();
            *cursor = input.chars().count();
        } else {
            panic!("expected the copy prompt");
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        // The original remains and the copy has the same contents.
        assert_eq!(std::fs::read(dir.join("old.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(target.join("old.txt")).unwrap(), b"hello");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_copies_marked_files_into_a_directory() {
        let dir = temp_dir("copy-marked");
        let target = dir.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'm'); // a.txt
        press(&mut app, 'm'); // b.txt
        press(&mut app, 'C');
        if let Prompt::Rename { input, cursor, .. } = &mut app.prompt {
            assert_eq!(*input, directory_input(&dir));
            *input = target.display().to_string();
            *cursor = input.chars().count();
        } else {
            panic!("expected the copy prompt");
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(std::fs::read(target.join("a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(target.join("b.txt")).unwrap(), b"b");
        // The originals stay in place.
        assert!(dir.join("a.txt").exists());
        assert!(dir.join("b.txt").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_requires_a_target_directory() {
        let dir = temp_dir("copy-not-dir");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'C');
        if let Prompt::Rename { input, cursor, .. } = &mut app.prompt {
            *input = "missing".to_string();
            *cursor = input.chars().count();
        } else {
            panic!("expected the copy prompt");
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!dir.join("missing").exists());
        assert!(app
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("Not a directory"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_refuses_to_overwrite_an_existing_file() {
        let dir = temp_dir("copy-exists");
        let target = dir.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(target.join("a.txt"), b"keep").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'C');
        if let Prompt::Rename { input, cursor, .. } = &mut app.prompt {
            *input = target.display().to_string();
            *cursor = input.chars().count();
        } else {
            panic!("expected the copy prompt");
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(std::fs::read(target.join("a.txt")).unwrap(), b"keep");
        assert!(app
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("already exists"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_copies_a_directory_recursively() {
        let dir = temp_dir("copy-dir");
        let src = dir.join("src");
        let target = dir.join("target");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(src.join("inner.txt"), b"inner").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'C');
        if let Prompt::Rename { input, cursor, .. } = &mut app.prompt {
            *input = target.display().to_string();
            *cursor = input.chars().count();
        } else {
            panic!("expected the copy prompt");
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            std::fs::read(target.join("src").join("inner.txt")).unwrap(),
            b"inner"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copying_a_directory_into_itself_is_refused() {
        let dir = temp_dir("copy-into-self");
        let src = dir.join("src");
        std::fs::create_dir(&src).unwrap();
        assert!(copy_path(&src, &src.join("nested")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn s_toggles_dired_sort_keeping_point() {
        let dir = temp_dir("sorttoggle");
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"x").unwrap();

        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());
        // Move to the second file (b.txt) so point is not the first entry.
        press(&mut app, 'n');

        press(&mut app, 's');
        assert_eq!(app.current().dired.as_ref().unwrap().sort, SortKey::Date);
        // Point stays on the same file across the re-sort.
        let i = app.dired_entry_at_point().unwrap();
        assert_eq!(
            app.current().dired.as_ref().unwrap().entries[i].name,
            "b.txt"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn buffer_match_is_case_insensitive() {
        let a = temp_dir("casebufA");
        let b = temp_dir("casebufB");
        let mut app = App::new(Config::default());
        app.open_dired(a.clone());
        app.open_dired(b.clone()); // make `a` non-current

        // Typing the name in a different case still finds the buffer.
        let cands = app.switch_candidates(&a.display().to_string().to_uppercase());
        assert!(cands
            .iter()
            .any(|&i| app.buffers[i].name == a.display().to_string()));

        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn path_completion_is_case_insensitive() {
        let dir = temp_dir("casefile");
        std::fs::write(dir.join("Foo.txt"), b"x").unwrap();
        let app = App::new(Config::default());

        let input = format!("{}{}foo", dir.display(), std::path::MAIN_SEPARATOR);
        let cands = app.file_candidates(&input, false);
        assert!(cands.iter().any(|c| c.ends_with("Foo.txt")));

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

    #[test]
    fn switching_to_shorter_buffer_clears_leftovers() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let dir = temp_dir("switch");
        std::fs::write(dir.join("short.txt"), b"one\ntwo\n").unwrap();

        let mut app = App::new(Config::default());
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        // Render a long buffer (scratch has many lines).
        let scratch = app
            .buffers
            .iter()
            .position(|b| b.kind == Kind::Scratch)
            .unwrap();
        app.switch_to(scratch);
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        // Switch to a two-line file.
        app.open_file(dir.join("short.txt"));
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        // Rows below the two lines (buffer area is 0..8) must be blank.
        for y in 2..8u16 {
            for x in 0..40u16 {
                assert_eq!(buf[(x, y)].symbol(), " ", "leftover at ({x},{y})");
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paste_char_maps_text_keys_only() {
        assert_eq!(
            paste_char(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some('a')
        );
        assert_eq!(
            paste_char(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some('\n')
        );
        assert_eq!(
            paste_char(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Some('\t')
        );
        assert_eq!(
            paste_char(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            paste_char(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn paste_into_read_only_buffer_is_refused() {
        let dir = temp_dir("paste-refuse");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        let before = app.current().dired.as_ref().unwrap().entries.len();
        // A pasted "d x" must not flag or delete anything.
        app.handle_paste("d x".to_string());
        assert_eq!(app.message.as_deref(), Some("Text is read-only"));
        assert_eq!(app.current().dired.as_ref().unwrap().entries.len(), before);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paste_into_scratch_inserts_text() {
        let mut app = App::new(Config::default());
        if let Some(i) = app.buffers.iter().position(|b| b.kind == Kind::Scratch) {
            app.switch_to(i);
        }
        app.current_mut().buffer.beginning_of_buffer();
        app.handle_paste("hello".to_string());
        assert!(app.current().buffer.line(0).text().contains("hello"));
    }

    #[test]
    fn transpose_bindings_edit_the_buffer() {
        let mut app = App::new(Config::default());
        if let Some(i) = app.buffers.iter().position(|b| b.kind == Kind::Scratch) {
            app.switch_to(i);
        }
        app.current_mut()
            .buffer
            .set_content(vec![Line::plain("teh")]);
        app.current_mut().buffer.end_of_line();
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.current().buffer.line(0).text(), "the");

        app.current_mut()
            .buffer
            .set_content(vec![Line::plain("hello world")]);
        app.current_mut().buffer.end_of_line();
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT));
        assert_eq!(app.current().buffer.line(0).text(), "world hello");
    }

    #[test]
    fn prefix_echo_waits_for_idle_delay() {
        let mut app = App::new(Config::default());
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(app.prefix, Prefix::CtrlC));
        // Nothing is echoed immediately.
        assert_eq!(app.message, None);
        assert!(app.prefix_echo.is_some());

        app.echo_pending_prefix(Instant::now() + PREFIX_ECHO_DELAY);
        assert_eq!(app.message.as_deref(), Some("C-c-"));
        assert!(app.prefix_echo.is_none());
    }

    #[test]
    fn completing_a_prefix_cancels_its_echo() {
        let mut app = App::new(Config::default());
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(matches!(app.prompt, Prompt::FindFile { .. }));
        assert!(app.prefix_echo.is_none());
        assert_ne!(app.message.as_deref(), Some("C-x-"));
    }

    #[test]
    fn prompt_word_motion_and_kills() {
        let mut app = App::new(Config::default());
        start_find_prompt(&mut app, "hello world foo");
        alt(&mut app, 'f');
        assert_eq!(prompt_text(&app).1, 5);
        alt(&mut app, 'f');
        assert_eq!(prompt_text(&app).1, 11);
        alt(&mut app, 'b');
        assert_eq!(prompt_text(&app).1, 6);
        alt(&mut app, 'b');
        assert_eq!(prompt_text(&app).1, 0);

        start_find_prompt(&mut app, "hello world");
        app.prompt = Prompt::FindFile {
            input: "hello world".to_string(),
            cursor: 11,
            dired: false,
            selected: None,
        };
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(prompt_text(&app), ("hello ".to_string(), 6));
        assert_eq!(app.kill_ring.current().as_deref(), Some("world"));

        start_find_prompt(&mut app, "hello world");
        alt(&mut app, 'd');
        assert_eq!(prompt_text(&app), (" world".to_string(), 0));

        start_find_prompt(&mut app, "hello");
        alt(&mut app, '>');
        assert_eq!(prompt_text(&app).1, 5);
        alt(&mut app, '<');
        assert_eq!(prompt_text(&app).1, 0);
    }

    #[test]
    fn prompt_case_words() {
        let mut app = App::new(Config::default());
        start_find_prompt(&mut app, "hello world");
        alt(&mut app, 'u');
        assert_eq!(prompt_text(&app), ("HELLO world".to_string(), 5));

        start_find_prompt(&mut app, "HELLO world");
        alt(&mut app, 'l');
        assert_eq!(prompt_text(&app), ("hello world".to_string(), 5));
    }

    #[test]
    fn prompt_tab_accepts_the_selected_directory() {
        let dir = temp_dir("prompt-tab");
        std::fs::create_dir(dir.join("alpha")).unwrap();
        std::fs::create_dir(dir.join("beta")).unwrap();
        let input = directory_input(&dir);
        let expected = directory_input(&dir.join("beta"));
        let mut app = App::new(Config::default());

        app.prompt = Prompt::FindFile {
            input: input.clone(),
            cursor: input.chars().count(),
            dired: true,
            selected: Some(1),
        };
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            prompt_text(&app),
            (expected.clone(), expected.chars().count())
        );
        assert!(matches!(
            app.prompt,
            Prompt::FindFile { selected: None, .. }
        ));

        app.prompt = Prompt::Rename {
            input: input.clone(),
            cursor: input.chars().count(),
            selected: Some(1),
            copy: true,
        };
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(
            &app.prompt,
            Prompt::Rename {
                input,
                cursor,
                selected: None,
                ..
            } if *input == expected && *cursor == expected.chars().count()
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_tab_completion_keeps_the_prompt_open_for_refinement() {
        let dir = temp_dir("prompt-tab-refine");
        let alpha = dir.join("alpha");
        std::fs::create_dir(&alpha).unwrap();
        std::fs::create_dir(alpha.join("inner")).unwrap();
        std::fs::create_dir(dir.join("beta")).unwrap();
        let input = directory_input(&dir);
        let mut app = App::new(Config::default());

        app.prompt = Prompt::FindFile {
            input,
            cursor: directory_input(&dir).chars().count(),
            dired: true,
            selected: Some(0),
        };
        // Tab completes the highlighted directory but does not submit it.
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(prompt_text(&app).0, directory_input(&alpha));
        assert!(matches!(app.prompt, Prompt::FindFile { .. }));

        // The candidate list now shows alpha's children; pick and complete one.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(prompt_text(&app).0, directory_input(&alpha.join("inner")));
        assert!(matches!(app.prompt, Prompt::FindFile { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_mark_regions_and_exchange() {
        let mut app = App::new(Config::default());
        app.prompt = Prompt::FindFile {
            input: "hello world".to_string(),
            cursor: 11,
            dired: false,
            selected: None,
        };
        ctrl(&mut app, ' ');
        assert_eq!(app.prompt_mark, Some(11));
        for _ in 0..5 {
            ctrl(&mut app, 'b');
        }
        alt(&mut app, 'w');
        assert_eq!(app.kill_ring.current().as_deref(), Some("world"));
        assert_eq!(prompt_text(&app), ("hello world".to_string(), 6));

        ctrl(&mut app, 'w');
        assert_eq!(prompt_text(&app), ("hello ".to_string(), 6));
        assert_eq!(app.prompt_mark, None);

        start_find_prompt(&mut app, "hello");
        app.prompt = Prompt::FindFile {
            input: "hello".to_string(),
            cursor: 5,
            dired: false,
            selected: None,
        };
        ctrl(&mut app, ' ');
        for _ in 0..2 {
            ctrl(&mut app, 'b');
        }
        ctrl(&mut app, 'x');
        ctrl(&mut app, 'x');
        assert_eq!(prompt_text(&app), ("hello".to_string(), 5));
        assert_eq!(app.prompt_mark, Some(3));
    }

    #[test]
    fn prompt_yank_pop_and_quoted_insert() {
        let mut app = App::new(Config::default());
        app.kill_ring.push("one", false);
        app.kill_ring.push("two", false);
        start_find_prompt(&mut app, "x");
        ctrl(&mut app, 'y');
        assert_eq!(prompt_text(&app).0, "twox");
        alt(&mut app, 'y');
        assert_eq!(prompt_text(&app).0, "onex");
        alt(&mut app, 'y');
        assert_eq!(prompt_text(&app).0, "twox");

        start_find_prompt(&mut app, "");
        ctrl(&mut app, 'q');
        press(&mut app, 'a');
        assert_eq!(prompt_text(&app).0, "a");
    }

    #[test]
    fn ctrl_space_sets_mark_for_copy_region() {
        let mut app = App::new(Config::default());
        if let Some(i) = app.buffers.iter().position(|b| b.kind == Kind::Scratch) {
            app.switch_to(i);
        }
        app.current_mut()
            .buffer
            .set_content(vec![Line::plain("hello world")]);
        app.current_mut().buffer.end_of_line();
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        assert_eq!(app.message.as_deref(), Some("Mark set"));
        for _ in 0..5 {
            app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT));
        assert_eq!(app.kill_ring.current().as_deref(), Some("world"));
        assert_eq!(app.current().buffer.line(0).text(), "hello world");
        // Point flashes at the far end of the region (the mark).
        assert_eq!(app.current().buffer.point(), Pos::new(0, 11));
        assert!(app.copy_flash.is_some());
    }

    #[test]
    fn copy_flash_restores_point_after_its_deadline() {
        let mut app = App::new(Config::default());
        if let Some(i) = app.buffers.iter().position(|b| b.kind == Kind::Scratch) {
            app.switch_to(i);
        }
        app.current_mut()
            .buffer
            .set_content(vec![Line::plain("hello world")]);
        app.current_mut().buffer.set_point(Pos::new(0, 6));
        app.current_mut().buffer.set_mark();
        app.current_mut().buffer.set_point(Pos::new(0, 11));
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT));
        assert_eq!(app.current().buffer.point(), Pos::new(0, 6));

        app.copy_flash = Some((Pos::new(0, 11), Instant::now()));
        assert!(app.next_event().unwrap().is_none());
        assert_eq!(app.current().buffer.point(), Pos::new(0, 11));
        assert!(app.copy_flash.is_none());
    }

    #[test]
    fn snippet_is_single_line_and_limited() {
        assert_eq!(snippet("a\nb   c"), "a b c");
        let long = "x".repeat(100);
        assert!(snippet(&long).chars().count() <= 41);
    }

    #[test]
    fn ctrl_space_then_m_w_copies_a_dired_region() {
        let dir = temp_dir("copy-region");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        app.current_mut().buffer.beginning_of_buffer();
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(
            KeyCode::Char('>'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT));

        let copied = app.kill_ring.current().unwrap();
        assert!(copied.contains("a.txt"));
        assert!(copied.contains("b.txt"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_x_c_x_exchanges_point_and_mark() {
        let mut app = App::new(Config::default());
        if let Some(i) = app.buffers.iter().position(|b| b.kind == Kind::Scratch) {
            app.switch_to(i);
        }
        app.current_mut()
            .buffer
            .set_content(vec![Line::plain("hello world")]);
        app.current_mut().buffer.set_point(Pos::new(0, 6));
        app.current_mut().buffer.set_mark();
        app.current_mut().buffer.set_point(Pos::new(0, 11));
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert_eq!(app.current().buffer.point(), Pos::new(0, 6));
        assert_eq!(app.current().buffer.region_text().as_deref(), Some("world"));
    }

    #[test]
    fn c_x_c_x_without_mark_reports_it() {
        let mut app = App::new(Config::default());
        if let Some(i) = app.buffers.iter().position(|b| b.kind == Kind::Scratch) {
            app.switch_to(i);
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert_eq!(app.message.as_deref(), Some("No mark set in this buffer"));
    }

    #[test]
    fn motion_key_exits_isearch_and_moves() {
        let mut app = App::new(Config::default());
        if let Some(i) = app.buffers.iter().position(|b| b.kind == Kind::Scratch) {
            app.switch_to(i);
        }
        app.current_mut().buffer.set_content(vec![
            Line::plain("alpha"),
            Line::plain("beta"),
            Line::plain("gamma"),
        ]);
        app.current_mut().buffer.beginning_of_buffer();
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        for c in "beta".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(app.search.active);
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(!app.search.active);
        assert_eq!(app.current().buffer.point(), Pos::new(2, 4));
    }

    #[test]
    fn enter_accepts_forward_search_at_match_end() {
        let mut app = App::new(Config::default());
        if let Some(i) = app.buffers.iter().position(|b| b.kind == Kind::Scratch) {
            app.switch_to(i);
        }
        app.current_mut().buffer.set_content(vec![
            Line::plain("alpha"),
            Line::plain("beta"),
            Line::plain("gamma"),
        ]);
        app.current_mut().buffer.beginning_of_buffer();
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        for c in "beta".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(app.current().buffer.point(), Pos::new(1, 0));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.search.active);
        assert_eq!(app.current().buffer.point(), Pos::new(1, 4));
    }

    #[test]
    fn c_n_on_last_line_reports_end_of_buffer() {
        let mut app = App::new(Config::default());
        if let Some(i) = app.buffers.iter().position(|b| b.kind == Kind::Scratch) {
            app.switch_to(i);
        }
        app.current_mut()
            .buffer
            .set_content(vec![Line::plain("hello")]);
        app.current_mut().buffer.beginning_of_line();
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.current().buffer.point(), Pos::new(0, 5));
        assert_eq!(app.message.as_deref(), Some("End of buffer"));
    }

    #[test]
    fn wdired_enters_and_edits_only_the_displayed_name() {
        let dir = temp_dir("wdired-edit");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        ctrl_x(&mut app, 'q');
        assert_eq!(app.mode_line_mode(), "Editable Dired");
        assert!(app.current().dired_edit.is_some());

        press(&mut app, 'X');
        assert_eq!(
            app.current().dired_edit.as_ref().unwrap().names[0],
            "Xa.txt"
        );
        let row = app.current().dired.as_ref().unwrap().row_of(0);
        assert!(app.current().buffer.line_text(row).contains("Xa.txt"));
        assert!(dir.join("a.txt").exists());
        assert!(!dir.join("Xa.txt").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_c_c_c_commits_renames_and_returns_to_dired() {
        let dir = temp_dir("wdired-commit");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        ctrl_x(&mut app, 'q');
        replace_wdired_name(&mut app, "c.txt");
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');

        assert_eq!(app.mode_line_mode(), "Dired by name");
        assert!(app.current().dired_edit.is_none());
        assert!(dir.join("c.txt").exists());
        assert!(!dir.join("a.txt").exists());
        let dired = app.current().dired.as_ref().unwrap();
        let index = dired
            .entry_index_at(app.current().buffer.point().row)
            .unwrap();
        assert_eq!(dired.entries[index].name, "c.txt");
        assert_eq!(app.message.as_deref(), Some("Renamed 1 file(s)"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_x_c_q_also_commits_and_updates_open_buffers() {
        let dir = temp_dir("wdired-toggle-commit");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let mut app = App::new(Config::default());
        app.open_file(dir.join("a.txt"));
        app.open_dired(dir.clone());

        ctrl_x(&mut app, 'q');
        replace_wdired_name(&mut app, "c.txt");
        ctrl_x(&mut app, 'q');

        assert!(dir.join("c.txt").exists());
        assert!(app
            .buffers
            .iter()
            .any(|buf| buf.name == dir.join("c.txt").display().to_string()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn c_c_esc_aborts_and_restores_the_listing() {
        let dir = temp_dir("wdired-abort");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        ctrl_x(&mut app, 'q');
        replace_wdired_name(&mut app, "changed.txt");
        ctrl(&mut app, 'c');
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.mode_line_mode(), "Dired by name");
        assert!(app.current().dired_edit.is_none());
        assert!(dir.join("a.txt").exists());
        assert!(!dir.join("changed.txt").exists());
        let row = app.current().dired.as_ref().unwrap().row_of(0);
        assert!(app.current().buffer.line_text(row).contains("a.txt"));
        assert_eq!(app.message.as_deref(), Some("Changes aborted"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wdired_rejects_duplicate_target_names_without_changes() {
        let dir = temp_dir("wdired-duplicate");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        ctrl_x(&mut app, 'q');
        replace_wdired_name(&mut app, "same.txt");
        ctrl(&mut app, 'n');
        replace_wdired_name(&mut app, "same.txt");
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');

        assert!(app.current().dired_edit.is_some());
        assert_eq!(
            app.message.as_deref(),
            Some("Trying to rename 2 files to the same name")
        );
        assert!(dir.join("a.txt").exists());
        assert!(dir.join("b.txt").exists());
        assert!(!dir.join("same.txt").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wdired_swaps_names_through_temporary_paths() {
        let dir = temp_dir("wdired-swap");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        ctrl_x(&mut app, 'q');
        replace_wdired_name(&mut app, "b.txt");
        ctrl(&mut app, 'n');
        replace_wdired_name(&mut app, "a.txt");
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');

        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"b");
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"a");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wdired_handles_rename_chains() {
        let dir = temp_dir("wdired-chain");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        ctrl_x(&mut app, 'q');
        replace_wdired_name(&mut app, "b.txt");
        ctrl(&mut app, 'n');
        replace_wdired_name(&mut app, "c.txt");
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');

        assert!(!dir.join("a.txt").exists());
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(dir.join("c.txt")).unwrap(), b"b");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wdired_creates_parent_directories_for_relative_paths() {
        let dir = temp_dir("wdired-subpath");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        ctrl_x(&mut app, 'q');
        replace_wdired_name(&mut app, "sub/c.txt");
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');

        assert_eq!(std::fs::read(dir.join("sub/c.txt")).unwrap(), b"a");
        assert!(!dir.join("a.txt").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wdired_prompts_before_overwriting_and_can_cancel() {
        let dir = temp_dir("wdired-overwrite");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        ctrl_x(&mut app, 'q');
        replace_wdired_name(&mut app, "b.txt");
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');
        assert!(matches!(app.prompt, Prompt::Confirm { .. }));

        answer(&mut app, "no");
        assert!(app.current().dired_edit.is_some());
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"b");

        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');
        answer(&mut app, "yes");
        assert!(app.current().dired_edit.is_none());
        assert!(!dir.join("a.txt").exists());
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"a");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wdired_rejects_empty_names_and_protected_text() {
        let dir = temp_dir("wdired-protected");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        ctrl_x(&mut app, 'q');
        ctrl(&mut app, 'a');
        ctrl(&mut app, 'k');
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');
        assert_eq!(app.message.as_deref(), Some("File name cannot be empty"));
        assert!(app.current().dired_edit.is_some());

        app.current_mut().buffer.set_point(Pos::new(0, 0));
        press(&mut app, 'X');
        assert_eq!(app.message.as_deref(), Some("Text is read-only"));
        assert!(app.current().buffer.line_text(0).starts_with("  "));
        assert!(dir.join("a.txt").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wdired_preserves_marks_when_renaming() {
        let dir = temp_dir("wdired-marks");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        press(&mut app, 'm');
        ctrl_x(&mut app, 'q');
        ctrl(&mut app, 'p');
        replace_wdired_name(&mut app, "c.txt");
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');

        let dired = app.current().dired.as_ref().unwrap();
        let entry = dired.entries.iter().find(|e| e.name == "c.txt").unwrap();
        assert_eq!(entry.mark, Mark::Marked);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn wdired_handles_case_only_renames() {
        let dir = temp_dir("wdired-case");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let mut app = App::new(Config::default());
        app.open_dired(dir.clone());

        ctrl_x(&mut app, 'q');
        replace_wdired_name(&mut app, "A.txt");
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');

        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"A.txt".to_string()));
        assert!(!names.contains(&"a.txt".to_string()));

        std::fs::remove_dir_all(&dir).ok();
    }
}
