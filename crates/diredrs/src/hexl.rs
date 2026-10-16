//! A hex viewer in the style of Emacs' `hexl-mode`.
//!
//! Each line shows a hex offset, sixteen bytes split into eight groups of two,
//! then the printable ASCII (`.` for non-printable bytes):
//!
//! ```text
//! 00000000: 7f45 4c46 0201 0100 0000 0000 0000 0000  .ELF............
//! ```

use crate::buffer::{Color, Line, Span, Style};

/// At most this many bytes are rendered, so opening a huge binary stays
/// responsive. The windowing of larger files is future work.
pub const MAX_BYTES: usize = 1 << 20;

/// Render `bytes` as hexl display lines.
pub fn render(bytes: &[u8]) -> Vec<Line> {
    let width = offset_width(bytes.len());
    let mut lines = Vec::with_capacity(bytes.len() / 16 + 1);
    for (row, chunk) in bytes.chunks(16).enumerate() {
        let offset = row * 16;
        let mut hex = String::with_capacity(40);
        for i in 0..16 {
            if i % 2 == 0 && i != 0 {
                hex.push(' ');
            }
            match chunk.get(i) {
                Some(byte) => hex.push_str(&format!("{byte:02x}")),
                None => hex.push_str("  "),
            }
        }
        let ascii: String = chunk
            .iter()
            .map(|b| {
                if (0x20..=0x7e).contains(b) {
                    *b as char
                } else {
                    '.'
                }
            })
            .collect();
        lines.push(Line::from_spans(vec![
            Span::new(
                format!("{offset:0width$x}", width = width),
                Style::fg(Color::DarkGray),
            ),
            Span::new(": ", Style::default()),
            Span::new(hex, Style::default()),
            Span::new("  ", Style::default()),
            Span::new(ascii, Style::default()),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::plain(""));
    }
    lines
}

/// The hex offset column width: at least 8 digits, more for larger files.
fn offset_width(len: usize) -> usize {
    let max = len.saturating_sub(1);
    format!("{max:x}").len().max(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_elf_header_line() {
        let bytes = [0x7f, 0x45, 0x4c, 0x46, 0x02, 0x01, 0x01, 0x00];
        let lines = render(&bytes);
        assert_eq!(lines.len(), 1);
        let text = lines[0].text();
        assert!(text.starts_with("00000000: 7f45 4c46 0201 0100 "));
        // Non-printable leading byte becomes a dot.
        assert!(text.ends_with(".ELF...."));
    }

    #[test]
    fn incomplete_last_line_is_padded() {
        let bytes: Vec<u8> = (0..18).collect();
        let lines = render(&bytes);
        assert_eq!(lines.len(), 2);
        // Second line has only two bytes.
        assert!(lines[1].text().starts_with("00000010: 1011  "));
        assert_eq!(offset_width(bytes.len()), 8);
    }

    #[test]
    fn empty_input_yields_one_line() {
        assert_eq!(render(&[]).len(), 1);
    }
}
