//! Emacs-style Buffer Menu entries and rendering.
//!
//! The menu is a read-only buffer listing every other buffer with Emacs's
//! `C R M Buffer Size Mode File` columns.

use crate::buffer::{Line, Span};
use crate::theme::Theme;

const NAME_MIN_WIDTH: usize = 19;
const SIZE_WIDTH: usize = 7;
const MODE_WIDTH: usize = 16;
/// Characters used by the three flag columns, their separators, the size and
/// mode columns, and the spaces between them.
const FIXED_WIDTH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MenuMark {
    #[default]
    None,
    Display,
    Delete,
}

impl MenuMark {
    fn ch(self) -> char {
        match self {
            MenuMark::None => ' ',
            MenuMark::Display => '>',
            MenuMark::Delete => 'D',
        }
    }
}

#[derive(Debug, Clone)]
pub struct BufferMenuEntry {
    pub name: String,
    pub size: usize,
    pub mode: String,
    pub file: String,
    pub read_only: bool,
    pub mark: MenuMark,
}

#[derive(Debug, Clone, Default)]
pub struct BufferMenu {
    pub entries: Vec<BufferMenuEntry>,
    /// The buffer from which the menu was opened; marked with `.`.
    pub origin: Option<String>,
}

impl BufferMenu {
    pub fn row_of(&self, index: usize) -> usize {
        index + 1
    }

    pub fn entry_index_at(&self, row: usize) -> Option<usize> {
        row.checked_sub(1)
            .filter(|index| *index < self.entries.len())
    }

    pub fn render(&self, theme: &Theme, width: usize) -> Vec<Line> {
        let width = width.max(1);
        let name_width = name_width(&self.entries, width);
        let file_width = width.saturating_sub(FIXED_WIDTH + name_width);

        let header = format!(
            "C R M {:<name_width$} {:>SIZE_WIDTH$} {:<MODE_WIDTH$} {}",
            truncate("Buffer", name_width),
            "Size",
            truncate("Mode", MODE_WIDTH),
            truncate("File", file_width),
        );
        let mut lines = vec![Line::from_spans(vec![Span::new(header, theme.header)])];
        for entry in &self.entries {
            let c = if entry.mark != MenuMark::None {
                entry.mark.ch()
            } else if self.origin.as_deref() == Some(entry.name.as_str()) {
                '.'
            } else {
                ' '
            };
            let row = format!(
                "{} {} {} {:<name_width$} {:>SIZE_WIDTH$} {:<MODE_WIDTH$} {}",
                c,
                if entry.read_only { '%' } else { ' ' },
                ' ',
                truncate(&entry.name, name_width),
                entry.size,
                truncate(&entry.mode, MODE_WIDTH),
                truncate(&entry.file, file_width),
            );
            lines.push(Line::plain(row));
        }
        lines
    }
}

fn name_width(entries: &[BufferMenuEntry], width: usize) -> usize {
    let longest = entries
        .iter()
        .map(|entry| entry.name.chars().count())
        .max()
        .unwrap_or(0);
    let preferred = (width * 5 / 21).max(NAME_MIN_WIDTH);
    let available = width.saturating_sub(FIXED_WIDTH).max(1);
    preferred.min(longest.max(NAME_MIN_WIDTH)).min(available)
}

fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width == 1 {
        return "\u{2026}".to_string();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Background;

    fn entry(name: &str, mark: MenuMark) -> BufferMenuEntry {
        BufferMenuEntry {
            name: name.to_string(),
            size: 12,
            mode: "Fundamental".to_string(),
            file: String::new(),
            read_only: false,
            mark,
        }
    }

    #[test]
    fn renders_flags_and_columns() {
        let menu = BufferMenu {
            entries: vec![
                entry("*scratch*", MenuMark::None),
                entry("notes.txt", MenuMark::Display),
            ],
            origin: Some("*scratch*".to_string()),
        };
        let theme = Theme::for_background(Background::Dark);
        let lines = menu.render(&theme, 80);

        assert!(lines[0].text().starts_with("C R M Buffer"));
        assert!(lines[1].text().starts_with(". "));
        assert!(lines[1].text().contains("*scratch*"));
        assert!(lines[2].text().starts_with("> "));
        assert!(lines[2].text().contains("notes.txt"));
        assert!(lines[1].text().contains("Fundamental"));
        assert!(lines[1].text().contains("12"));
    }

    #[test]
    fn read_only_and_delete_flags() {
        let mut item = entry("a.txt", MenuMark::Delete);
        item.read_only = true;
        let menu = BufferMenu {
            entries: vec![item],
            origin: None,
        };
        let theme = Theme::for_background(Background::Dark);
        let line = menu.render(&theme, 80)[1].text();
        assert!(line.starts_with("D %"));
    }

    #[test]
    fn narrow_width_truncates_to_one_row() {
        let menu = BufferMenu {
            entries: vec![entry("a-very-long-buffer-name", MenuMark::None)],
            origin: None,
        };
        let theme = Theme::for_background(Background::Dark);
        let line = menu.render(&theme, 40)[1].text();
        assert!(line.chars().count() <= 40);
        assert!(line.contains('\u{2026}'));
    }
}
