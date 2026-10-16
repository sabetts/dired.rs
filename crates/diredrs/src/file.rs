//! Reading files into read-only text buffers.
//!
//! A file is treated as text unless it contains a NUL byte or is not valid
//! UTF-8; those are shown in the hex viewer instead (see `hexl`).

/// Whether `bytes` look binary (a NUL byte, or not valid UTF-8).
pub fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    std::str::from_utf8(bytes).is_err()
}

/// Split file `bytes` into display lines, dropping a single trailing newline
/// and any carriage returns (so CRLF files render cleanly).
pub fn to_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_suffix('\n').unwrap_or(&text);
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_text_and_binary() {
        assert!(!looks_binary(b"hello\nworld\n"));
        assert!(looks_binary(b"abc\0def"));
        // A lone high byte is not valid UTF-8.
        assert!(looks_binary(&[0xff, 0xfe, 0x00]));
    }

    #[test]
    fn splits_lines_and_trailing_newline() {
        let lines = to_lines(b"a\nb\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "a");
        assert_eq!(lines[1], "b");
    }

    #[test]
    fn crlf_is_stripped() {
        let lines = to_lines(b"a\r\nb\r\n");
        assert_eq!(lines[0], "a");
        assert_eq!(lines[1], "b");
    }

    #[test]
    fn empty_file_is_one_blank_line() {
        let lines = to_lines(b"");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "");
    }
}
