//! A text buffer with an Emacs-style point, mark, and incremental search.
//!
//! Buffers are free text: the whole buffer is editable unless it has been
//! marked read-only. Directory listings, file views, and the hex viewer are
//! read-only; `*scratch*` is editable.

/// A logical position: a row index and a character column within that row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

impl Pos {
    pub fn new(row: usize, col: usize) -> Self {
        Pos { row, col }
    }
}

/// A minimal, UI-independent color palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum Color {
    #[default]
    Default,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    /// A 24-bit truecolor value.
    Rgb(u8, u8, u8),
}

/// A minimal, UI-independent text style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub color: Color,
    /// Background color; `Color::Default` leaves the terminal default.
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub reverse: bool,
}

impl Style {
    pub const fn fg(color: Color) -> Self {
        Style {
            color,
            bg: Color::Default,
            bold: false,
            italic: false,
            underline: false,
            dim: false,
            reverse: false,
        }
    }

    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    pub const fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Style {
            color: Color::Rgb(r, g, b),
            bg: Color::Default,
            bold: false,
            italic: false,
            underline: false,
            dim: false,
            reverse: false,
        }
    }
}

/// A styled run of text within a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Span {
            text: text.into(),
            style,
        }
    }
}

/// A single display line, made of one or more styled spans.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn plain(text: impl Into<String>) -> Self {
        Line {
            spans: vec![Span::new(text, Style::default())],
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Line {
            spans: vec![Span::new(text, style)],
        }
    }

    pub fn from_spans(spans: Vec<Span>) -> Self {
        Line { spans }
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        for span in &self.spans {
            out.push_str(&span.text);
        }
        out
    }

    pub fn chars(&self) -> Vec<char> {
        self.text().chars().collect()
    }

    pub fn char_len(&self) -> usize {
        self.spans.iter().map(|s| s.text.chars().count()).sum()
    }
}

/// The buffer model: styled lines plus a point, mark, and scroll offset.
#[derive(Debug, Clone)]
pub struct Buffer {
    lines: Vec<Line>,
    point: Pos,
    mark: Option<Pos>,
    scroll: usize,
    read_only: bool,
}

impl Default for Buffer {
    fn default() -> Self {
        Buffer::new()
    }
}

impl Buffer {
    /// An empty, editable buffer holding a single blank line.
    pub fn new() -> Self {
        Buffer {
            lines: vec![Line::plain("")],
            point: Pos::new(0, 0),
            mark: None,
            scroll: 0,
            read_only: false,
        }
    }

    /// A read-only buffer from the given lines (listings, file views).
    pub fn read_only_from(lines: Vec<Line>) -> Self {
        let mut buffer = Buffer::new();
        buffer.set_content(lines);
        buffer.read_only = true;
        buffer
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    pub fn editable(&self) -> bool {
        !self.read_only
    }

    /// Replace all content.
    pub fn set_content(&mut self, lines: Vec<Line>) {
        self.lines = if lines.is_empty() {
            vec![Line::plain("")]
        } else {
            lines
        };
        self.point = Pos::new(0, 0);
        self.scroll = 0;
        self.mark = None;
        self.clamp_point();
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, row: usize) -> &Line {
        &self.lines[row]
    }

    #[cfg(test)]
    pub fn line_text(&self, row: usize) -> String {
        self.lines[row].text()
    }

    /// Replace the line at `row` (used to redraw a single listing entry).
    pub fn set_line(&mut self, row: usize, line: Line) {
        if row < self.lines.len() {
            self.lines[row] = line;
        }
    }

    pub fn last_row(&self) -> usize {
        self.lines.len() - 1
    }

    pub fn point(&self) -> Pos {
        self.point
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn set_point(&mut self, pos: Pos) {
        let row = pos.row.min(self.last_row());
        self.point = Pos::new(row, self.clamp_col(row, pos.col));
        self.scroll_into_view();
    }

    fn clamp_point(&mut self) {
        let row = self.point.row.min(self.last_row());
        self.point = Pos::new(row, self.clamp_col(row, self.point.col));
        self.scroll = self.scroll.min(self.last_row());
    }

    // ---- marks, region -----------------------------------------------------

    pub fn set_mark(&mut self) {
        self.mark = Some(self.point);
    }

    pub fn clear_mark(&mut self) {
        self.mark = None;
    }

    pub fn region(&self) -> Option<(Pos, Pos)> {
        self.mark.map(|m| {
            if m <= self.point {
                (m, self.point)
            } else {
                (self.point, m)
            }
        })
    }

    pub fn region_text(&self) -> Option<String> {
        let (start, end) = self.region()?;
        Some(self.text_between(start, end))
    }

    pub fn text_between(&self, start: Pos, end: Pos) -> String {
        if start == end {
            return String::new();
        }
        let mut out = String::new();
        for row in start.row..=end.row {
            let chars = self.lines[row].chars();
            let from = if row == start.row { start.col } else { 0 };
            let to = if row == end.row { end.col } else { chars.len() };
            let from = from.min(chars.len());
            let to = to.min(chars.len());
            out.extend(&chars[from..to]);
            if row != end.row {
                out.push('\n');
            }
        }
        out
    }

    // ---- motion ------------------------------------------------------------

    pub fn forward_char(&mut self) {
        let row = self.point.row;
        let len = self.lines[row].char_len();
        if self.point.col < len {
            self.point.col += 1;
        } else if row < self.last_row() {
            self.point.row += 1;
            self.point.col = 0;
        }
        self.scroll_into_view();
    }

    pub fn backward_char(&mut self) {
        if self.point.col > 0 {
            self.point.col -= 1;
        } else if self.point.row > 0 {
            self.point.row -= 1;
            self.point.col = self.lines[self.point.row].char_len();
        }
        self.scroll_into_view();
    }

    pub fn next_line(&mut self) {
        if self.point.row < self.last_row() {
            self.point.row += 1;
            self.point.col = self.clamp_col(self.point.row, self.point.col);
        }
        self.scroll_into_view();
    }

    pub fn prev_line(&mut self) {
        if self.point.row > 0 {
            self.point.row -= 1;
            self.point.col = self.clamp_col(self.point.row, self.point.col);
        }
        self.scroll_into_view();
    }

    pub fn beginning_of_line(&mut self) {
        self.point.col = 0;
    }

    pub fn end_of_line(&mut self) {
        self.point.col = self.lines[self.point.row].char_len();
        self.scroll_into_view();
    }

    pub fn beginning_of_buffer(&mut self) {
        self.point = Pos::new(0, 0);
        self.scroll = 0;
    }

    pub fn end_of_buffer(&mut self) {
        let row = self.last_row();
        self.point = Pos::new(row, self.lines[row].char_len());
        self.scroll_into_view();
    }

    pub fn forward_word(&mut self) {
        let chars = self.lines[self.point.row].chars();
        let mut col = self.point.col;
        if col >= chars.len() {
            self.next_line();
            return;
        }
        while col < chars.len() && !is_word(chars[col]) {
            col += 1;
        }
        while col < chars.len() && is_word(chars[col]) {
            col += 1;
        }
        self.point.col = col;
        self.scroll_into_view();
    }

    pub fn backward_word(&mut self) {
        let chars = self.lines[self.point.row].chars();
        let mut col = self.point.col;
        while col > 0 && !is_word(chars[col - 1]) {
            col -= 1;
        }
        while col > 0 && is_word(chars[col - 1]) {
            col -= 1;
        }
        self.point.col = col;
        self.scroll_into_view();
    }

    pub fn page_forward(&mut self, rows: usize) {
        let target = (self.point.row + rows).min(self.last_row());
        let delta = target - self.point.row;
        self.point.row = target;
        self.point.col = self.clamp_col(self.point.row, self.point.col);
        self.scroll = (self.scroll + delta).min(self.point.row);
    }

    pub fn page_backward(&mut self, rows: usize) {
        let target = self.point.row.saturating_sub(rows);
        let delta = self.point.row - target;
        self.point.row = target;
        self.point.col = self.clamp_col(self.point.row, self.point.col);
        self.scroll = self.scroll.saturating_sub(delta);
    }

    /// Keep point within a window `height` rows tall, scrolling only when point
    /// reaches the top or bottom edge (Emacs-style).
    pub fn ensure_point_visible(&mut self, height: usize) {
        let height = height.max(1);
        if self.point.row < self.scroll {
            self.scroll = self.point.row;
        } else if self.point.row >= self.scroll + height {
            self.scroll = self.point.row + 1 - height;
        }
    }

    // ---- case changing -----------------------------------------------------

    pub fn upcase_word(&mut self) {
        self.case_word(|word| word.iter().flat_map(|c| c.to_uppercase()).collect());
    }

    pub fn downcase_word(&mut self) {
        self.case_word(|word| word.iter().flat_map(|c| c.to_lowercase()).collect());
    }

    fn case_word(&mut self, transform: impl Fn(&[char]) -> String) {
        if !self.editable() {
            return;
        }
        let row = self.point.row;
        let chars = self.lines[row].chars();
        let mut col = self.point.col.min(chars.len());
        while col < chars.len() && !is_word(chars[col]) {
            col += 1;
        }
        let start = col;
        while col < chars.len() && is_word(chars[col]) {
            col += 1;
        }
        let end = col;
        if start == end {
            self.point.col = end;
            return;
        }
        let changed = transform(&chars[start..end]);
        let new_len = changed.chars().count();
        let mut out: Vec<char> = chars[..start].to_vec();
        out.extend(changed.chars());
        out.extend_from_slice(&chars[end..]);
        self.lines[row] = Line::plain(out.into_iter().collect::<String>());
        self.point.col = start + new_len;
        self.scroll_into_view();
    }

    pub fn backward_upcase_word(&mut self) {
        self.backward_case_word(|word| word.iter().flat_map(|c| c.to_uppercase()).collect());
    }

    pub fn backward_downcase_word(&mut self) {
        self.backward_case_word(|word| word.iter().flat_map(|c| c.to_lowercase()).collect());
    }

    fn backward_case_word(&mut self, transform: impl Fn(&[char]) -> String) {
        if !self.editable() {
            return;
        }
        let row = self.point.row;
        let chars = self.lines[row].chars();
        let end = self.point.col.min(chars.len());
        let mut i = end;
        while i > 0 && !is_word(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(chars[i - 1]) {
            i -= 1;
        }
        let start = i;
        if start == end {
            self.point.col = start;
            return;
        }
        let changed = transform(&chars[start..end]);
        let mut out: Vec<char> = chars[..start].to_vec();
        out.extend(changed.chars());
        out.extend_from_slice(&chars[end..]);
        self.lines[row] = Line::plain(out.into_iter().collect::<String>());
        self.point.col = start;
        self.scroll_into_view();
    }

    // ---- editing -----------------------------------------------------------

    pub fn insert_char(&mut self, c: char) {
        if !self.editable() {
            return;
        }
        let row = self.point.row;
        let mut chars = self.lines[row].chars();
        let idx = self.point.col.min(chars.len());
        chars.insert(idx, c);
        self.lines[row] = Line::plain(chars.into_iter().collect::<String>());
        self.point.col += 1;
        self.scroll_into_view();
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.insert_newline();
            } else if c != '\r' {
                self.insert_char(c);
            }
        }
    }

    /// Insert a line break at point.
    pub fn insert_newline(&mut self) {
        if !self.editable() {
            return;
        }
        let row = self.point.row;
        let chars = self.lines[row].chars();
        let idx = self.point.col.min(chars.len());
        let left: String = chars[..idx].iter().collect();
        let right: String = chars[idx..].iter().collect();
        self.lines[row] = Line::plain(left);
        self.lines.insert(row + 1, Line::plain(right));
        self.point = Pos::new(row + 1, 0);
        self.scroll_into_view();
    }

    pub fn delete_backward(&mut self) {
        if !self.editable() {
            return;
        }
        if self.point.col > 0 {
            let row = self.point.row;
            let mut chars = self.lines[row].chars();
            chars.remove(self.point.col - 1);
            self.lines[row] = Line::plain(chars.into_iter().collect::<String>());
            self.point.col -= 1;
        } else if self.point.row > 0 {
            let row = self.point.row;
            let prev_len = self.lines[row - 1].char_len();
            let current = self.lines[row].text();
            let mut prev = self.lines[row - 1].chars();
            prev.extend(current.chars());
            self.lines[row - 1] = Line::plain(prev.into_iter().collect::<String>());
            self.lines.remove(row);
            self.point = Pos::new(row - 1, prev_len);
        }
        self.scroll_into_view();
    }

    pub fn delete_forward(&mut self) {
        if !self.editable() {
            return;
        }
        let row = self.point.row;
        let len = self.lines[row].char_len();
        if self.point.col < len {
            let mut chars = self.lines[row].chars();
            chars.remove(self.point.col);
            self.lines[row] = Line::plain(chars.into_iter().collect::<String>());
        } else if row + 1 < self.lines.len() {
            let next = self.lines[row + 1].text();
            let mut chars = self.lines[row].chars();
            chars.extend(next.chars());
            self.lines[row] = Line::plain(chars.into_iter().collect::<String>());
            self.lines.remove(row + 1);
        }
        self.scroll_into_view();
    }

    /// Kill from point to end of line; returns the killed text.
    pub fn kill_line(&mut self) -> String {
        if !self.editable() {
            return String::new();
        }
        let row = self.point.row;
        let chars = self.lines[row].chars();
        let idx = self.point.col.min(chars.len());
        let killed: String = chars[idx..].iter().collect();
        let kept: String = chars[..idx].iter().collect();
        self.lines[row] = Line::plain(kept);
        self.scroll_into_view();
        killed
    }

    /// Delete the text between `start` and `end` (ordered, `start <= end`).
    pub fn delete_region(&mut self, start: Pos, end: Pos) -> bool {
        if start == end {
            return true;
        }
        if start > end || !self.editable() {
            return false;
        }
        if start.row == end.row {
            let mut chars = self.lines[start.row].chars();
            chars.drain(start.col..end.col.min(chars.len()));
            self.lines[start.row] = Line::plain(chars.into_iter().collect::<String>());
            self.point = Pos::new(start.row, start.col);
        } else {
            let left: String = self.lines[start.row].chars()[..start.col].iter().collect();
            let right_chars = self.lines[end.row].chars();
            let right: String = right_chars[end.col.min(right_chars.len())..]
                .iter()
                .collect();
            self.lines[start.row] = Line::plain(format!("{left}{right}"));
            for _ in 0..(end.row - start.row) {
                self.lines.remove(start.row + 1);
            }
            self.point = Pos::new(start.row, start.col);
        }
        self.mark = None;
        self.clamp_point();
        self.scroll_into_view();
        true
    }

    /// Kill the word before point (`M-DEL`), returning the killed text.
    pub fn backward_kill_word(&mut self) -> String {
        if !self.editable() {
            return String::new();
        }
        let row = self.point.row;
        let chars = self.lines[row].chars();
        let mut col = self.point.col.min(chars.len());
        while col > 0 && !is_word(chars[col - 1]) {
            col -= 1;
        }
        while col > 0 && is_word(chars[col - 1]) {
            col -= 1;
        }
        if col == self.point.col {
            if row == 0 {
                return String::new();
            }
            self.delete_backward();
            return "\n".to_string();
        }
        let start = Pos::new(row, col);
        let end = self.point;
        let text = self.text_between(start, end);
        self.delete_region(start, end);
        text
    }

    /// Kill from point to the end of the next word (`M-d`), returning the text.
    pub fn kill_word(&mut self) -> String {
        if !self.editable() {
            return String::new();
        }
        let row = self.point.row;
        let chars = self.lines[row].chars();
        let mut col = self.point.col.min(chars.len());
        while col < chars.len() && !is_word(chars[col]) {
            col += 1;
        }
        while col < chars.len() && is_word(chars[col]) {
            col += 1;
        }
        if col == self.point.col {
            if row == self.last_row() {
                return String::new();
            }
            self.delete_forward();
            return "\n".to_string();
        }
        let start = self.point;
        let end = Pos::new(row, col);
        let text = self.text_between(start, end);
        self.delete_region(start, end);
        text
    }

    // ---- dabbrev -----------------------------------------------------------

    pub fn word_before_point(&self) -> String {
        let row = self.point.row;
        let chars = self.lines[row].chars();
        let end = self.point.col.min(chars.len());
        let mut start = end;
        while start > 0 && is_word(chars[start - 1]) {
            start -= 1;
        }
        chars[start..end].iter().collect()
    }

    pub fn replace_word_before_point(&mut self, word: &str) {
        if !self.editable() {
            return;
        }
        let row = self.point.row;
        let chars = self.lines[row].chars();
        let end = self.point.col.min(chars.len());
        let mut start = end;
        while start > 0 && is_word(chars[start - 1]) {
            start -= 1;
        }
        let mut out: Vec<char> = chars[..start].to_vec();
        out.extend(word.chars());
        out.extend_from_slice(&chars[end..]);
        self.lines[row] = Line::plain(out.into_iter().collect::<String>());
        self.point.col = start + word.chars().count();
        self.scroll_into_view();
    }

    /// Words in the buffer that begin with `prefix` (case-insensitively) and
    /// are longer than it, most recent first and de-duplicated.
    pub fn dabbrev_candidates(&self, prefix: &str) -> Vec<String> {
        if prefix.is_empty() {
            return Vec::new();
        }
        let lower = prefix.to_lowercase();
        let min_len = prefix.chars().count();
        let mut out: Vec<String> = Vec::new();
        for row in (0..self.lines.len()).rev() {
            for word in words_in(&self.lines[row].text()).into_iter().rev() {
                if word.chars().count() > min_len
                    && word.to_lowercase().starts_with(&lower)
                    && !out.contains(&word)
                {
                    out.push(word);
                }
            }
        }
        out
    }

    // ---- search ------------------------------------------------------------

    /// Find the next occurrence of `query` from `from` in the given direction.
    /// Returns the match start and its character length.
    pub fn search(&self, query: &str, from: Pos, forward: bool) -> Option<(Pos, usize)> {
        if query.is_empty() {
            return None;
        }
        let needle: Vec<char> = query.to_lowercase().chars().collect();
        let rows: Vec<usize> = if forward {
            (from.row..self.lines.len()).collect()
        } else {
            (0..=from.row.min(self.last_row())).rev().collect()
        };
        for row in rows {
            let hay: Vec<char> = self.lines[row].text().to_lowercase().chars().collect();
            if forward {
                let start = if row == from.row { from.col } else { 0 };
                if let Some(col) = find_in(&hay, &needle, start.min(hay.len()), true) {
                    return Some((Pos::new(row, col), needle.len()));
                }
            } else {
                let end = if row == from.row { from.col } else { hay.len() };
                if let Some(col) = find_in(&hay, &needle, end.min(hay.len()), false) {
                    return Some((Pos::new(row, col), needle.len()));
                }
            }
        }
        None
    }

    /// All non-overlapping matches of `query`, for highlighting.
    pub fn matches(&self, query: &str) -> Vec<(Pos, usize)> {
        if query.is_empty() {
            return Vec::new();
        }
        let needle: Vec<char> = query.to_lowercase().chars().collect();
        let mut out = Vec::new();
        for row in 0..self.lines.len() {
            let hay: Vec<char> = self.lines[row].text().to_lowercase().chars().collect();
            let mut start = 0;
            while let Some(col) = find_in(&hay, &needle, start, true) {
                out.push((Pos::new(row, col), needle.len()));
                start = col + needle.len().max(1);
            }
        }
        out
    }

    // ---- helpers -----------------------------------------------------------

    fn clamp_col(&self, row: usize, col: usize) -> usize {
        col.min(self.lines[row].char_len())
    }

    fn scroll_into_view(&mut self) {
        if self.point.row < self.scroll {
            self.scroll = self.point.row;
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Split `text` into runs of word characters.
fn words_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if is_word(c) {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Find `needle` inside `hay`, starting at char index `start`. When `forward`
/// is false, returns the last match strictly before `start`.
fn find_in(hay: &[char], needle: &[char], start: usize, forward: bool) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    if forward {
        let last = hay.len() - needle.len();
        for i in start..=last {
            if hay[i..i + needle.len()] == *needle {
                return Some(i);
            }
        }
    } else {
        let start = start.min(hay.len());
        if start < needle.len() {
            return None;
        }
        for i in (0..=start - needle.len()).rev() {
            if hay[i..i + needle.len()] == *needle {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_buffer(content: &str) -> Buffer {
        let lines = content.split('\n').map(Line::plain).collect();
        Buffer::read_only_from(lines)
    }

    #[test]
    fn starts_empty_and_editable() {
        let b = Buffer::new();
        assert_eq!(b.len(), 1);
        assert!(b.editable());
        assert_eq!(b.point(), Pos::new(0, 0));
    }

    #[test]
    fn typing_and_deleting() {
        let mut b = Buffer::new();
        for c in "hello".chars() {
            b.insert_char(c);
        }
        assert_eq!(b.line_text(0), "hello");
        b.delete_backward();
        assert_eq!(b.line_text(0), "hell");
        b.backward_char();
        b.insert_char('X');
        assert_eq!(b.line_text(0), "helXl");
    }

    #[test]
    fn read_only_refuses_edits() {
        let mut b = text_buffer("abc");
        b.insert_char('x');
        assert_eq!(b.line_text(0), "abc");
        assert!(!b.delete_region(Pos::new(0, 0), Pos::new(0, 1)));
    }

    #[test]
    fn text_backspace_joins_lines() {
        let mut b = Buffer::new();
        b.insert_char('a');
        b.insert_newline();
        b.insert_char('b');
        b.delete_backward();
        assert_eq!(b.line_text(1), "");
        b.delete_backward();
        assert_eq!(b.len(), 1);
        assert_eq!(b.line_text(0), "a");
        assert_eq!(b.point(), Pos::new(0, 1));
    }

    #[test]
    fn text_forward_delete_joins_lines() {
        let mut b = Buffer::new();
        b.insert_char('a');
        b.insert_newline();
        b.insert_char('b');
        b.set_point(Pos::new(0, 1));
        b.delete_forward();
        assert_eq!(b.len(), 1);
        assert_eq!(b.line_text(0), "ab");
    }

    #[test]
    fn upcase_and_downcase_word() {
        let mut b = Buffer::new();
        b.insert_str("hello world");
        b.beginning_of_line();
        b.upcase_word();
        assert_eq!(b.line_text(0), "HELLO world");
        assert_eq!(b.point().col, 5);
        b.beginning_of_line();
        b.downcase_word();
        assert_eq!(b.line_text(0), "hello world");
    }

    #[test]
    fn backward_kill_word_deletes_word() {
        let mut b = Buffer::new();
        b.insert_str("hello world");
        assert_eq!(b.backward_kill_word(), "world");
        assert_eq!(b.line_text(0), "hello ");
        assert_eq!(b.backward_kill_word(), "hello ");
        assert_eq!(b.line_text(0), "");
    }

    #[test]
    fn kill_line_returns_remainder() {
        let mut b = Buffer::new();
        b.insert_str("hello world");
        b.set_point(Pos::new(0, 5));
        assert_eq!(b.kill_line(), " world");
        assert_eq!(b.line_text(0), "hello");
    }

    #[test]
    fn region_text() {
        let mut b = Buffer::new();
        b.insert_str("abcdef");
        b.set_point(Pos::new(0, 3));
        b.set_mark();
        b.set_point(Pos::new(0, 5));
        assert_eq!(b.region_text().unwrap(), "de");
    }

    #[test]
    fn delete_region_multiline_text() {
        let mut b = Buffer::new();
        b.insert_str("abc\ndef");
        b.set_point(Pos::new(0, 1));
        b.set_mark();
        b.set_point(Pos::new(1, 2));
        let (start, end) = b.region().unwrap();
        assert!(b.delete_region(start, end));
        assert_eq!(b.line_text(0), "af");
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn search_forward_backward() {
        let b = text_buffer("first line\nsecond line here");
        let hit = b.search("line", Pos::new(0, 0), true).unwrap();
        assert_eq!(hit.0, Pos::new(0, 6));
        let second = b
            .search("line", Pos::new(hit.0.row, hit.0.col + 1), true)
            .unwrap();
        assert_eq!(second.0.row, 1);
        let back = b.search("first", Pos::new(1, 0), false).unwrap();
        assert_eq!(back.0, Pos::new(0, 0));
    }

    #[test]
    fn matches_all() {
        let b = text_buffer("first line\nsecond line here");
        assert_eq!(b.matches("line").len(), 2);
    }
}
