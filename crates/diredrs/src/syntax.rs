//! Extension-based syntax highlighting, matching Emacs' defaults.
//!
//! Faces and colours come from Emacs (`font-lock.el`, `faces.el`,
//! markdown-mode.el); see [`SyntaxFaces`](crate::theme::SyntaxFaces). TOML
//! mirrors `conf-toml-mode`; Markdown mirrors the common `markdown-mode`
//! constructs (headers, fenced code, emphasis, code, links, lists,
//! blockquotes, rules, HTML comments). It is a close approximation, not a
//! complete port of font-lock.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::buffer::{Line, Span, Style};
use crate::theme::SyntaxFaces;

/// A highlightable language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Markdown,
    Toml,
    Ini,
}

/// Emacs `files.el` matches `*config`, `*.conf`, `*.cfg`, `*.cf` (and a dotted
/// suffix) by name to `conf-mode`.
fn conf_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\.?c(?:on)?f(?:i?g)?(?:\.[a-z0-9._-]+)?$").unwrap())
}

/// The language to highlight a file as, chosen by name/extension.
pub fn detect(path: &Path) -> Option<Lang> {
    let name = path.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext.to_ascii_lowercase().as_str() {
            "md" | "markdown" | "mdown" | "mkd" | "mkdn" => return Some(Lang::Markdown),
            "toml" => return Some(Lang::Toml),
            "ini" | "cf" | "cfg" | "conf" | "config" => return Some(Lang::Ini),
            _ => {}
        }
    }
    // `~/.gitconfig` (and `*config`/`*.conf`/`*.cfg`/`*.cf`) use conf-mode.
    // `conf-mode-maybe` defers `.xml` to xml-mode, which we do not highlight.
    if (lower == "gitconfig" || lower == ".gitconfig" || conf_name_re().is_match(&lower))
        && !lower.ends_with(".xml")
    {
        return Some(Lang::Ini);
    }
    None
}

/// Highlight every line of a file, one styled [`Line`] per input line.
pub fn highlight(lang: Lang, lines: &[String], faces: &SyntaxFaces) -> Vec<Line> {
    match lang {
        Lang::Markdown => markdown::highlight(lines, faces),
        Lang::Toml => toml::highlight(lines, faces),
        Lang::Ini => ini::highlight(lines, faces),
    }
}

/// Accumulates per-character styles for a single line and turns them into a
/// [`Line`] of runs.
struct Painter {
    chars: Vec<char>,
    styles: Vec<Style>,
}

impl Painter {
    fn new(line: &str, base: Style) -> Self {
        let chars: Vec<char> = line.chars().collect();
        let styles = vec![base; chars.len()];
        Painter { chars, styles }
    }

    fn len(&self) -> usize {
        self.chars.len()
    }

    fn chars(&self) -> &[char] {
        &self.chars
    }

    fn char_at(&self, i: usize) -> char {
        self.chars[i]
    }

    /// Replace the styles of `[start, end)`.
    fn fill(&mut self, start: usize, end: usize, style: Style) {
        let end = end.min(self.styles.len());
        for i in start.min(end)..end {
            self.styles[i] = style;
        }
    }

    /// Compose a change onto the styles of `[start, end)`.
    fn modify(&mut self, start: usize, end: usize, f: impl Fn(&mut Style)) {
        let end = end.min(self.styles.len());
        for i in start.min(end)..end {
            f(&mut self.styles[i]);
        }
    }

    fn into_line(self) -> Line {
        if self.chars.is_empty() {
            return Line::plain("");
        }
        let mut spans: Vec<Span> = Vec::new();
        let mut i = 0;
        while i < self.chars.len() {
            let style = self.styles[i];
            let mut j = i + 1;
            while j < self.chars.len() && self.styles[j] == style {
                j += 1;
            }
            let text: String = self.chars[i..j].iter().collect();
            spans.push(Span::new(text, style));
            i = j;
        }
        Line::from_spans(spans)
    }
}

/// Convert a byte range within `line` to a char-index range.
fn char_range(line: &str, start: usize, end: usize) -> (usize, usize) {
    (line[..start].chars().count(), line[..end].chars().count())
}

/// Find the next index at or after `from` where `ch` occurs.
fn find_char(chars: &[char], from: usize, ch: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == ch)
}

/// The next run of exactly `run` copies of `ch` at or after `from`.
fn find_run(chars: &[char], from: usize, ch: char, run: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == ch {
            let start = i;
            while i < chars.len() && chars[i] == ch {
                i += 1;
            }
            if i - start == run {
                return Some((start, i));
            }
        } else {
            i += 1;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// TOML (`conf-toml-mode`)
// ---------------------------------------------------------------------------

mod toml {
    use super::*;

    fn key_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^[ \t]*([^=\n]+?)(?:\[([^\]\n]*?)\])?[ \t]*=").unwrap())
    }

    fn section_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^[ \t]*\[+([^\]\n,]+?)\]+").unwrap())
    }

    fn bool_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"\b(true|false)\b").unwrap())
    }

    pub fn highlight(lines: &[String], faces: &SyntaxFaces) -> Vec<Line> {
        lines.iter().map(|line| one(line, faces)).collect()
    }

    fn one(line: &str, faces: &SyntaxFaces) -> Line {
        let mut p = Painter::new(line, Style::default());

        // key = value, or key[index] = value.
        if let Some(caps) = key_re().captures(line) {
            if let Some(m) = caps.get(1) {
                let (s, e) = char_range(line, m.start(), m.end());
                p.fill(s, e, faces.variable);
            }
            if let Some(m) = caps.get(2) {
                let (s, e) = char_range(line, m.start(), m.end());
                p.fill(s, e, faces.constant);
            }
        }
        // [section] / [[section]].
        if let Some(caps) = section_re().captures(line) {
            if let Some(m) = caps.get(1) {
                let (s, e) = char_range(line, m.start(), m.end());
                p.fill(s, e, faces.r#type);
            }
        }
        // true / false.
        for m in bool_re().find_iter(line) {
            let (s, e) = char_range(line, m.start(), m.end());
            p.fill(s, e, faces.keyword);
        }

        // Strings and comments override the above.
        let n = p.len();
        let mut i = 0;
        while i < n {
            let c = p.char_at(i);
            if c == '#' {
                p.fill(i, n, faces.comment);
                break;
            } else if c == '"' || c == '\'' {
                let quote = c;
                let start = i;
                i += 1;
                while i < n {
                    let d = p.char_at(i);
                    if quote == '"' && d == '\\' {
                        i += 2;
                        continue;
                    }
                    if d == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                p.fill(start, i.min(n), faces.string);
            } else {
                i += 1;
            }
        }
        p.into_line()
    }
}

// ---------------------------------------------------------------------------
// INI (`conf-mode`, Windows/Unix styles)
// ---------------------------------------------------------------------------

mod ini {
    use super::*;

    fn section_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^[ \t]*\[(.+)\]").unwrap())
    }

    fn key_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^[ \t]*([^=\n]+?)(?:\[([^\]\n]*?)\])?[ \t]*=").unwrap())
    }

    /// Emacs' `conf-mode` picks the comment character from the buffer
    /// contents: `#` (Unix) or `;` (Windows), with ties favouring `;`.
    fn comment_char(lines: &[String]) -> char {
        let (mut unix, mut win) = (0, 0);
        for line in lines {
            match line.trim_start().chars().next() {
                Some('#') => unix += 1,
                Some(';') => win += 1,
                _ => {}
            }
        }
        if unix > win {
            '#'
        } else {
            ';'
        }
    }

    pub fn highlight(lines: &[String], faces: &SyntaxFaces) -> Vec<Line> {
        let comment = comment_char(lines);
        lines.iter().map(|line| one(line, comment, faces)).collect()
    }

    fn one(line: &str, comment: char, faces: &SyntaxFaces) -> Line {
        let mut p = Painter::new(line, Style::default());

        // key = value, or key[index] = value.
        if let Some(caps) = key_re().captures(line) {
            if let Some(m) = caps.get(1) {
                let (s, e) = char_range(line, m.start(), m.end());
                p.fill(s, e, faces.variable);
            }
            if let Some(m) = caps.get(2) {
                let (s, e) = char_range(line, m.start(), m.end());
                p.fill(s, e, faces.constant);
            }
        }
        // [section].
        if let Some(caps) = section_re().captures(line) {
            if let Some(m) = caps.get(1) {
                let (s, e) = char_range(line, m.start(), m.end());
                p.fill(s, e, faces.r#type);
            }
        }

        // Strings and the chosen comment character override the above.
        let n = p.len();
        let mut i = 0;
        while i < n {
            let c = p.char_at(i);
            if c == comment {
                p.fill(i, n, faces.comment);
                break;
            } else if c == '"' || c == '\'' {
                let quote = c;
                let start = i;
                i += 1;
                while i < n {
                    let d = p.char_at(i);
                    if quote == '"' && d == '\\' {
                        i += 2;
                        continue;
                    }
                    if d == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                p.fill(start, i.min(n), faces.string);
            } else {
                i += 1;
            }
        }
        p.into_line()
    }
}

// ---------------------------------------------------------------------------
// Markdown (`markdown-mode`)
// ---------------------------------------------------------------------------

mod markdown {
    use super::*;

    pub fn highlight(lines: &[String], faces: &SyntaxFaces) -> Vec<Line> {
        let mut out: Vec<Line> = Vec::with_capacity(lines.len());
        let mut in_comment = false;
        let mut fence: Option<(char, usize)> = None;

        for (idx, line) in lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();

            // Inside an HTML comment.
            if in_comment {
                let mut p = Painter::new(line, Style::default());
                match find_substr(line, "-->") {
                    Some(byte) => {
                        let end = line[..byte + 3].chars().count();
                        p.fill(0, end, faces.comment);
                        in_comment = false;
                    }
                    None => p.fill(0, p.len(), faces.comment),
                }
                out.push(p.into_line());
                continue;
            }

            // Inside a fenced code block.
            if let Some((fc, flen)) = fence {
                if is_closing_fence(&chars, fc, flen) {
                    let mut p = Painter::new(line, Style::default());
                    p.fill(0, p.len(), faces.markup);
                    out.push(p.into_line());
                    fence = None;
                } else {
                    out.push(Painter::new(line, faces.constant).into_line());
                }
                continue;
            }

            // Opening fence.
            if let Some((fc, flen, info_start)) = opening_fence(&chars) {
                let mut p = Painter::new(line, Style::default());
                p.fill(0, flen, faces.markup);
                if info_start < chars.len() {
                    paint_fence_info(&mut p, info_start, chars.len(), faces);
                }
                out.push(p.into_line());
                fence = Some((fc, flen));
                continue;
            }

            // An HTML comment starting mid-line.
            if let Some(byte) = find_substr(line, "<!--") {
                let start = line[..byte].chars().count();
                let mut p = Painter::new(line, Style::default());
                match find_substr(&line[byte..], "-->") {
                    Some(rel) => {
                        let end = line[..byte + rel + 3].chars().count();
                        p.fill(start, end, faces.comment);
                    }
                    None => {
                        p.fill(start, p.len(), faces.comment);
                        in_comment = true;
                    }
                }
                markdown_inline(&mut p, faces);
                out.push(p.into_line());
                continue;
            }

            // Setext underline (= or -), which also restyles the line above.
            if let Some(uc) = setext_underline(&chars) {
                let prev_nonblank = idx > 0 && !lines[idx - 1].trim().is_empty();
                if prev_nonblank {
                    let mut p = Painter::new(line, Style::default());
                    p.fill(0, p.len(), faces.markup);
                    out.push(p.into_line());
                    let mut hp = Painter::new(&lines[idx - 1], Style::default());
                    hp.fill(0, hp.len(), header_style(faces));
                    markdown_inline(&mut hp, faces);
                    out[idx - 1] = hp.into_line();
                    continue;
                } else if uc == '-' && is_rule(&chars) {
                    let p = Painter::new(line, faces.markup);
                    out.push(p.into_line());
                    continue;
                }
            }

            // Whole-line constructs.
            let mut p = Painter::new(line, Style::default());
            if let Some((hs, he, ts)) = atx_header(&chars) {
                p.fill(hs, he, faces.markup);
                if ts < chars.len() {
                    paint_atx_text(&mut p, ts, chars.len(), faces);
                }
                markdown_inline(&mut p, faces);
                out.push(p.into_line());
                continue;
            }
            if is_rule(&chars) {
                p.fill(0, p.len(), faces.markup);
                out.push(p.into_line());
                continue;
            }
            if let Some((marker_end, content_start)) = blockquote(&chars) {
                p.fill(0, marker_end, faces.markup);
                p.fill(content_start, chars.len(), faces.string); // markdown-blockquote-face
            } else if let Some((ms, me, cs)) = list_marker(&chars) {
                p.fill(ms, me, faces.markup);
                paint_checkbox(&mut p, cs, faces);
            }
            markdown_inline(&mut p, faces);
            out.push(p.into_line());
        }
        out
    }

    fn header_style(faces: &SyntaxFaces) -> Style {
        faces.function.bold()
    }

    fn find_substr(hay: &str, needle: &str) -> Option<usize> {
        hay.find(needle)
    }

    /// Opening code fence: ```` ``` ```` or `~~~` (>= 3) after <= 3 spaces.
    fn opening_fence(chars: &[char]) -> Option<(char, usize, usize)> {
        let mut i = 0;
        while i < chars.len() && chars[i] == ' ' && i < 3 {
            i += 1;
        }
        let fc = *chars.get(i)?;
        if fc != '`' && fc != '~' {
            return None;
        }
        let start = i;
        while i < chars.len() && chars[i] == fc {
            i += 1;
        }
        let len = i - start;
        if len < 3 {
            return None;
        }
        Some((fc, len, i))
    }

    fn is_closing_fence(chars: &[char], fc: char, _flen: usize) -> bool {
        let trimmed: Vec<char> = chars.iter().copied().skip_while(|c| *c == ' ').collect();
        !trimmed.is_empty() && trimmed.iter().all(|c| *c == fc)
    }

    fn paint_fence_info(p: &mut Painter, start: usize, end: usize, faces: &SyntaxFaces) {
        // Skip whitespace, then the info string: first word is the language.
        let chars = p.chars().to_vec();
        let mut i = start;
        while i < end && (chars[i] == ' ' || chars[i] == '\t') {
            i += 1;
        }
        let lang_end = (i..end)
            .find(|&k| chars[k] == ' ' || chars[k] == '\t')
            .unwrap_or(end);
        p.fill(i, lang_end, faces.r#type); // markdown-language-keyword-face
        if lang_end < end {
            p.fill(lang_end, end, faces.string); // markdown-language-info-face
        }
    }

    /// A setext underline (`===` or `---`) on its own line.
    fn setext_underline(chars: &[char]) -> Option<char> {
        let t: Vec<char> = chars.iter().copied().filter(|c| *c != ' ').collect();
        if t.is_empty() {
            return None;
        }
        let c = t[0];
        if (c == '=' || c == '-') && t.iter().all(|x| *x == c) {
            Some(c)
        } else {
            None
        }
    }

    /// A horizontal rule: three or more `*`, `-`, or `_` (spaces allowed).
    fn is_rule(chars: &[char]) -> bool {
        let t: Vec<char> = chars.iter().copied().filter(|c| *c != ' ').collect();
        if t.len() < 3 {
            return false;
        }
        let c = t[0];
        (c == '*' || c == '-' || c == '_') && t.iter().all(|x| *x == c)
    }

    /// An ATX header: returns (hash_start, hash_end, text_start).
    fn atx_header(chars: &[char]) -> Option<(usize, usize, usize)> {
        let mut i = 0;
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        if i > 3 {
            return None;
        }
        let hs = i;
        while i < chars.len() && chars[i] == '#' {
            i += 1;
        }
        let level = i - hs;
        if level == 0 || level > 6 {
            return None;
        }
        if i < chars.len() && chars[i] != ' ' && chars[i] != '\t' {
            return None;
        }
        let mut t = i;
        while t < chars.len() && (chars[t] == ' ' || chars[t] == '\t') {
            t += 1;
        }
        Some((hs, i, t))
    }

    fn paint_atx_text(p: &mut Painter, start: usize, end: usize, faces: &SyntaxFaces) {
        // Strip a trailing closing run of '#' (optionally space-separated).
        let chars = p.chars().to_vec();
        let mut k = end;
        while k > start && (chars[k - 1] == ' ' || chars[k - 1] == '\t') {
            k -= 1;
        }
        let mut ce = k;
        while ce > start && chars[ce - 1] == '#' {
            ce -= 1;
        }
        let closing = ce < k && (ce == start || chars[ce - 1] == ' ' || chars[ce - 1] == '\t');
        p.fill(start, if closing { ce } else { end }, header_style(faces));
        if closing {
            p.fill(ce, k, faces.markup);
        }
    }

    /// Leading `>` blockquote markers; returns (marker_end, content_start).
    fn blockquote(chars: &[char]) -> Option<(usize, usize)> {
        let mut i = 0;
        let mut seen = false;
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        while i < chars.len() && chars[i] == '>' {
            seen = true;
            i += 1;
            if i < chars.len() && chars[i] == ' ' {
                i += 1;
            }
        }
        if !seen {
            return None;
        }
        Some((i, i))
    }

    /// A list marker; returns (marker_start, marker_end, content_start).
    fn list_marker(chars: &[char]) -> Option<(usize, usize, usize)> {
        let mut i = 0;
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        let start = i;
        let c = *chars.get(i)?;
        if c == '-' || c == '*' || c == '+' {
            i += 1;
        } else if c.is_ascii_digit() {
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i >= chars.len() || (chars[i] != '.' && chars[i] != ')') {
                return None;
            }
            i += 1;
        } else {
            return None;
        }
        if i >= chars.len() || (chars[i] != ' ' && chars[i] != '\t') {
            return None;
        }
        let marker_end = i;
        while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
            i += 1;
        }
        Some((start, marker_end, i))
    }

    /// A GFM task checkbox (`[ ]` / `[x]`) right after a list marker.
    fn paint_checkbox(p: &mut Painter, start: usize, faces: &SyntaxFaces) {
        let chars = p.chars().to_vec();
        if start + 3 <= chars.len()
            && chars[start] == '['
            && (chars[start + 1] == ' ' || chars[start + 1] == 'x' || chars[start + 1] == 'X')
            && chars[start + 2] == ']'
        {
            p.fill(start, start + 3, faces.builtin);
        }
    }

    /// Inline markup: code spans, emphasis, and inline links.
    fn markdown_inline(p: &mut Painter, faces: &SyntaxFaces) {
        let n = p.len();
        let mut i = 0;
        while i < n {
            match p.char_at(i) {
                '`' => {
                    let chars = p.chars().to_vec();
                    let mut j = i;
                    while j < n && chars[j] == '`' {
                        j += 1;
                    }
                    let run = j - i;
                    if let Some((cs, ce)) = find_run(&chars, j, '`', run) {
                        p.fill(i, j, faces.markup);
                        p.fill(j, cs, faces.constant); // markdown-inline-code-face
                        p.fill(cs, ce, faces.markup);
                        i = ce;
                    } else {
                        i = j;
                    }
                }
                '*' | '_' => {
                    let ch = p.char_at(i);
                    let chars = p.chars().to_vec();
                    let mut j = i;
                    while j < n && chars[j] == ch {
                        j += 1;
                    }
                    let run = j - i;
                    if run >= 2 {
                        if let Some((cs, ce)) = find_run(&chars, j, ch, 2) {
                            p.fill(i, j, faces.markup);
                            p.modify(j, cs, |s| s.bold = true);
                            p.fill(cs, ce, faces.markup);
                            i = ce;
                            continue;
                        }
                    }
                    if let Some((cs, ce)) = find_run(&chars, j, ch, 1) {
                        p.fill(i, j, faces.markup);
                        p.modify(j, cs, |s| s.italic = true);
                        p.fill(cs, ce, faces.markup);
                        i = ce;
                    } else {
                        i = j;
                    }
                }
                '[' => {
                    if let Some(next) = link(p, i, faces) {
                        i = next;
                    } else {
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        }
    }

    /// An inline link `[text](url)`; returns the index past it.
    fn link(p: &mut Painter, i: usize, faces: &SyntaxFaces) -> Option<usize> {
        let chars = p.chars().to_vec();
        let n = chars.len();
        let close = find_char(&chars, i + 1, ']')?;
        if close + 1 >= n || chars[close + 1] != '(' {
            return None;
        }
        let paren = find_char(&chars, close + 2, ')')?;
        p.fill(i, i + 1, faces.markup);
        p.fill(i + 1, close, faces.link); // markdown-link-face
        p.fill(close, close + 1, faces.markup);
        p.fill(close + 1, close + 2, faces.markup);
        p.fill(close + 2, paren, faces.string); // markdown-url-face
        p.fill(paren, paren + 1, faces.markup);
        Some(paren + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Background, Theme};

    fn faces() -> SyntaxFaces {
        Theme::for_background(Background::Dark).syntax
    }

    fn hl(lang: Lang, lines: &[&str]) -> Vec<Line> {
        let owned: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        highlight(lang, &owned, &faces())
    }

    /// The style at char column `col` of the rendered line.
    fn style_at(line: &Line, col: usize) -> Style {
        let mut c = 0;
        for span in &line.spans {
            let len = span.text.chars().count();
            if col < c + len {
                return span.style;
            }
            c += len;
        }
        Style::default()
    }

    #[test]
    fn detect_by_extension() {
        assert_eq!(detect(Path::new("a.md")), Some(Lang::Markdown));
        assert_eq!(detect(Path::new("A.MARKDOWN")), Some(Lang::Markdown));
        assert_eq!(detect(Path::new("c.toml")), Some(Lang::Toml));
        assert_eq!(detect(Path::new("c.INI")), Some(Lang::Ini));
        assert_eq!(detect(Path::new("x.rs")), None);
    }

    #[test]
    fn detect_by_name() {
        // Emacs `files.el` conf-mode associations.
        assert_eq!(detect(Path::new("/home/u/.git/config")), Some(Lang::Ini));
        assert_eq!(detect(Path::new("/home/u/.gitconfig")), Some(Lang::Ini));
        assert_eq!(detect(Path::new("/etc/foo.conf")), Some(Lang::Ini));
        assert_eq!(detect(Path::new("app.cfg")), Some(Lang::Ini));
        assert_eq!(detect(Path::new("settings.config")), Some(Lang::Ini));
        // `.xml` is left alone (conf-mode-maybe defers to xml-mode).
        assert_eq!(detect(Path::new("config.xml")), None);
        assert_eq!(detect(Path::new("main.rs")), None);
    }

    #[test]
    fn ini_git_config_style() {
        let f = faces();
        let lines = hl(
            Lang::Ini,
            &[
                "[core]",
                "\trepositoryformatversion = 0",
                "\tbare = false",
                "[remote \"origin\"]",
                "\turl = https://example.com/x.git",
            ],
        );
        assert_eq!(style_at(&lines[0], 1), f.r#type); // core
        assert_eq!(style_at(&lines[1], 1), f.variable); // key after a tab
        assert_eq!(style_at(&lines[3], 1), f.r#type); // remote "origin"
        assert_eq!(style_at(&lines[4], 1), f.variable); // url
    }

    #[test]
    fn ini_windows_style() {
        let f = faces();
        let lines = hl(
            Lang::Ini,
            &[
                "; a comment",
                "[server]",
                "host = \"localhost\"; note",
                "port = 8080",
            ],
        );
        assert_eq!(style_at(&lines[0], 0), f.comment);
        assert_eq!(style_at(&lines[1], 1), f.r#type); // "server"
        assert_eq!(style_at(&lines[2], 0), f.variable); // "host"
        assert_eq!(style_at(&lines[2], 7), f.string); // quoted value
        let semi = lines[2].text().find(';').unwrap();
        assert_eq!(style_at(&lines[2], semi), f.comment); // trailing "; note"
        assert_eq!(style_at(&lines[3], 0), f.variable); // "port"
    }

    #[test]
    fn ini_unix_style_uses_hash() {
        let f = faces();
        let lines = hl(
            Lang::Ini,
            &["# one", "# two", "a = 1", "; not a comment here"],
        );
        assert_eq!(style_at(&lines[0], 0), f.comment);
        assert_eq!(style_at(&lines[2], 0), f.variable);
        // In Unix style, ';' is not a comment character.
        assert_ne!(style_at(&lines[3], 0), f.comment);
    }

    #[test]
    fn toml_comment_string_section_key_bool() {
        let f = faces();
        let lines = hl(
            Lang::Toml,
            &[
                "# a comment",
                "[server]",
                "host = \"localhost\" # here",
                "enabled = true",
            ],
        );
        assert_eq!(style_at(&lines[0], 0), f.comment);
        assert_eq!(style_at(&lines[1], 1), f.r#type); // "server"
        assert_eq!(style_at(&lines[2], 0), f.variable); // "host"
        assert_eq!(style_at(&lines[2], 7), f.string); // the quoted value
        assert_eq!(style_at(&lines[2], 20), f.comment); // trailing comment
        assert_eq!(style_at(&lines[3], 10), f.keyword); // true
    }

    #[test]
    fn toml_array_index_is_constant() {
        let f = faces();
        let lines = hl(Lang::Toml, &["ports[0] = 1"]);
        assert_eq!(style_at(&lines[0], 0), f.variable); // "ports"
        assert_eq!(style_at(&lines[0], 6), f.constant); // "0"
    }

    #[test]
    fn markdown_header_and_inline() {
        let f = faces();
        let lines = hl(Lang::Markdown, &["## Title *x*", "plain `code` text"]);
        assert_eq!(style_at(&lines[0], 0), f.markup); // "##"
        let title = style_at(&lines[0], 3);
        assert!(title.bold); // header text
        assert_eq!(style_at(&lines[1], 6), f.markup); // backtick
        assert_eq!(style_at(&lines[1], 8), f.constant); // inline code
    }

    #[test]
    fn markdown_fenced_code_block() {
        let f = faces();
        let lines = hl(Lang::Markdown, &["```rust", "let x = 1;", "```", "after"]);
        assert_eq!(style_at(&lines[0], 0), f.markup); // fence
        assert_eq!(style_at(&lines[0], 3), f.r#type); // language
        assert_eq!(style_at(&lines[1], 0), f.constant); // code line
        assert_eq!(style_at(&lines[2], 0), f.markup); // closing fence
    }

    #[test]
    fn markdown_link() {
        let f = faces();
        let lines = hl(Lang::Markdown, &["see [docs](http://example.com)"]);
        assert_eq!(style_at(&lines[0], 4), f.markup); // "["
        assert_eq!(style_at(&lines[0], 5), f.link); // "docs"
        let text = lines[0].text();
        let url_col = text.find("http").unwrap();
        assert_eq!(style_at(&lines[0], url_col), f.string); // URL
    }

    #[test]
    fn markdown_setext_header() {
        let f = faces();
        let lines = hl(Lang::Markdown, &["Title", "====="]);
        assert!(style_at(&lines[0], 0).bold); // text restyled as header
        assert_eq!(style_at(&lines[1], 0), f.markup); // underline
    }
}
