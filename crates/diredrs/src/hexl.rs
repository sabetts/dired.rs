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
pub fn offset_width(len: usize) -> usize {
    let max = len.saturating_sub(1);
    format!("{max:x}").len().max(8)
}

/// Width of the hex byte field: eight groups of two bytes (four hex digits)
/// separated by single spaces.
const HEX_WIDTH: usize = 8 * 4 + 7;

/// For the character column `col` on a rendered hexl line, the byte it refers
/// to: returns `(hex_col, ascii_col)`, the starting columns of that byte's hex
/// pair and its ASCII character, or `None` when the column is not over a byte
/// (the offset field, a group separator, or padding past the end of the line).
pub fn byte_at(line: &str, col: usize, offset_width: usize) -> Option<(usize, usize)> {
    let len = line.chars().count();
    let hex_start = offset_width + 2;
    let hex_end = hex_start + HEX_WIDTH;
    let ascii_start = hex_end + 2;
    let nbytes = len.saturating_sub(ascii_start).min(16);
    if nbytes == 0 {
        return None;
    }
    if (hex_start..hex_end).contains(&col) {
        let rel = col - hex_start;
        (0..nbytes).find_map(|j| {
            let h = j * 2 + j / 2;
            (rel == h || rel == h + 1).then_some((hex_start + h, ascii_start + j))
        })
    } else if col >= ascii_start {
        let j = col - ascii_start;
        (j < nbytes).then(|| {
            let h = j * 2 + j / 2;
            (hex_start + h, ascii_start + j)
        })
    } else {
        None
    }
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

    #[test]
    fn maps_columns_to_bytes() {
        let bytes: Vec<u8> = (0..20).collect();
        let line = render(&bytes)[0].text();
        let w = offset_width(bytes.len());
        let hex_start = w + 2;
        let ascii_start = w + 43;

        // Byte 0's first hex digit.
        assert_eq!(byte_at(&line, hex_start, w), Some((hex_start, ascii_start)));
        // Byte 1's second hex digit.
        assert_eq!(
            byte_at(&line, hex_start + 3, w),
            Some((hex_start + 2, ascii_start + 1))
        );
        // The ASCII char for byte 3 links back to its hex pair.
        assert_eq!(
            byte_at(&line, ascii_start + 3, w),
            Some((hex_start + 7, ascii_start + 3))
        );
        // A group separator, the offset field, and past the ASCII column are
        // not over a byte.
        assert_eq!(byte_at(&line, hex_start + 4, w), None);
        assert_eq!(byte_at(&line, 0, w), None);
        assert_eq!(byte_at(&line, ascii_start + 16, w), None);
    }

    #[test]
    fn last_line_maps_only_present_bytes() {
        let bytes: Vec<u8> = (0..18).collect();
        let line = render(&bytes)[1].text();
        let w = offset_width(bytes.len());
        let ascii_start = w + 43;
        assert!(byte_at(&line, ascii_start + 1, w).is_some());
        assert_eq!(byte_at(&line, ascii_start + 2, w), None);
    }
}
