//! Rendering with ratatui.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color as RColor, Modifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan, Text};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, Prompt};
use crate::buffer::{Color, Line, Pos, Style};
use crate::search::{Search, SearchDirection};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let minibuffer = app.minibuffer_height().max(1);
    let rows = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(minibuffer),
    ])
    .split(frame.area());

    draw_buffer(frame, app, rows[0]);
    draw_mode_line(frame, app, rows[1]);
    draw_minibuffer(frame, app, rows[2]);
}

fn draw_buffer(frame: &mut Frame, app: &App, area: Rect) {
    let height = area.height as usize;
    let width = area.width as usize;
    if height == 0 || width == 0 {
        return;
    }
    let buf = &app.current().buffer;
    let point = buf.point();
    let cursor_point = display_point(&app.search, point);

    let highlights = if app.search.active && !app.search.query.is_empty() {
        buf.matches(&app.search.query)
    } else {
        Vec::new()
    };
    let current_match = if app.search.active {
        app.search.match_end.and_then(|end| {
            (end.row == point.row && end.col > point.col).then(|| (point, end.col - point.col))
        })
    } else {
        None
    };
    // Hex view: the byte under point, highlighted in its hex and ASCII forms
    // with a background colour (Emacs's `highlight` face), not reverse video.
    let hex_highlights = app.hexl_highlights();

    // Rendering starts at the persisted view top; the buffer keeps the point
    // visible (wrapping-aware), so the view does not drift as point moves.
    let last_line = buf.len().saturating_sub(1);
    let mut lines: Vec<RLine> = Vec::with_capacity(height);
    let mut row = buf.scroll();
    let mut sub = buf.scroll_sub();
    while row <= last_line && lines.len() < height {
        let mut cells = line_cells(buf.line(row));
        apply_highlights(
            &mut cells,
            row,
            &highlights,
            current_match,
            app.theme.lazy_highlight,
            app.theme.isearch,
        );
        apply_background(&mut cells, row, &hex_highlights, app.theme.hex_highlight);
        if row == cursor_point.row && matches!(app.prompt, Prompt::None) {
            place_cursor(&mut cells, cursor_point.col, app.cursor_style);
        }

        let total = cells.len();
        if total == 0 {
            lines.push(to_rline(&[]));
        } else {
            let mut start = (sub * width).min(total);
            while start < total && lines.len() < height {
                let end = (start + width).min(total);
                lines.push(to_rline(&cells[start..end]));
                start += width;
            }
        }
        row += 1;
        sub = 0;
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Where to draw the point. During a forward isearch Emacs leaves point at the
/// end of the match; backward searches leave it at the beginning.
fn display_point(search: &Search, point: Pos) -> Pos {
    if search.active && search.direction == SearchDirection::Forward {
        search.match_end.unwrap_or(point)
    } else {
        point
    }
}

fn draw_mode_line(frame: &mut Frame, app: &App, area: Rect) {
    let line = app.current().buffer.point().row + 1;
    let base = RStyle::default().add_modifier(Modifier::REVERSED);
    let text = format!(
        " {}  ({})  L{} ",
        app.mode_line_name(),
        app.mode_line_mode(),
        line
    );
    frame.render_widget(Paragraph::new(RLine::from(text)).style(base), area);
}

fn draw_minibuffer(frame: &mut Frame, app: &App, area: Rect) {
    if let Some((prompt, cursor, candidates, selected)) = app.prompt_view() {
        let mut lines: Vec<RLine> = Vec::with_capacity(candidates.len() + 1);
        let mut cells: Vec<(char, Style)> = prompt.chars().map(|c| (c, Style::default())).collect();
        place_cursor(&mut cells, cursor, app.cursor_style);
        lines.push(to_rline(&cells));
        for (i, candidate) in candidates.iter().enumerate() {
            let style = if i == selected {
                RStyle::default().add_modifier(Modifier::REVERSED)
            } else {
                RStyle::default()
            };
            lines.push(RLine::from(RSpan::styled(format!("  {candidate}"), style)));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    }

    let text = if app.search.active {
        let prefix = match app.search.direction {
            SearchDirection::Forward => "I-search",
            SearchDirection::Backward => "I-search backward",
        };
        let suffix = if app.search.failed { " [failing]" } else { "" };
        format!("{prefix}: {}{suffix}", app.search.query)
    } else {
        app.message.clone().unwrap_or_default()
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn line_cells(line: &Line) -> Vec<(char, Style)> {
    let mut cells = Vec::new();
    for span in &line.spans {
        for ch in span.text.chars() {
            // Control characters (tabs, etc.) occupy no cell in ratatui but
            // the terminal may still act on them; show a space instead so every
            // character is exactly one cell and column math stays aligned.
            let ch = if ch.is_control() { ' ' } else { ch };
            cells.push((ch, span.style));
        }
    }
    cells
}

fn apply_highlights(
    cells: &mut [(char, Style)],
    row: usize,
    highlights: &[(Pos, usize)],
    current: Option<(Pos, usize)>,
    lazy: Style,
    isearch: Style,
) {
    for (pos, len) in highlights {
        if pos.row != row {
            continue;
        }
        let is_current = current
            .is_some_and(|(cpos, clen)| cpos.row == pos.row && cpos.col == pos.col && clen == *len);
        for i in pos.col..(pos.col + len).min(cells.len()) {
            cells[i].1.reverse = false;
            if is_current {
                cells[i].1.color = isearch.color;
                cells[i].1.bg = isearch.bg;
            } else {
                cells[i].1.bg = lazy.bg;
            }
        }
    }
}

/// Set the background of the given ranges, leaving each cell's foreground.
fn apply_background(cells: &mut [(char, Style)], row: usize, ranges: &[(Pos, usize)], bg: Color) {
    for (pos, len) in ranges {
        if pos.row != row {
            continue;
        }
        for i in pos.col..(pos.col + len).min(cells.len()) {
            cells[i].1.bg = bg;
        }
    }
}

fn place_cursor(cells: &mut Vec<(char, Style)>, col: usize, style: Style) {
    if col < cells.len() {
        cells[col].1 = style;
    } else {
        cells.push((' ', style));
    }
}

fn to_rline(cells: &[(char, Style)]) -> RLine<'static> {
    let mut spans: Vec<RSpan> = Vec::new();
    let mut current = String::new();
    let mut current_style: Option<Style> = None;
    for (ch, style) in cells {
        match current_style {
            Some(s) if s == *style => current.push(*ch),
            Some(s) => {
                spans.push(RSpan::styled(std::mem::take(&mut current), rstyle(s)));
                current.push(*ch);
                current_style = Some(*style);
            }
            None => {
                current.push(*ch);
                current_style = Some(*style);
            }
        }
    }
    if let Some(s) = current_style {
        spans.push(RSpan::styled(current, rstyle(s)));
    }
    RLine::from(spans)
}

fn rstyle(style: Style) -> RStyle {
    let mut out = RStyle::default().fg(rcolor(style.color));
    if style.bg != Color::Default {
        out = out.bg(rcolor(style.bg));
    }
    if style.bold {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    if style.dim {
        out = out.add_modifier(Modifier::DIM);
    }
    if style.reverse {
        out = out.add_modifier(Modifier::REVERSED);
    }
    out
}

fn rcolor(color: Color) -> RColor {
    match color {
        Color::Default => RColor::Reset,
        Color::Black => RColor::Black,
        Color::Red => RColor::Red,
        Color::Green => RColor::Green,
        Color::Yellow => RColor::Yellow,
        Color::Blue => RColor::Blue,
        Color::Magenta => RColor::Magenta,
        Color::Cyan => RColor::Cyan,
        Color::Gray => RColor::Gray,
        Color::DarkGray => RColor::DarkGray,
        Color::LightRed => RColor::LightRed,
        Color::LightGreen => RColor::LightGreen,
        Color::LightYellow => RColor::LightYellow,
        Color::LightBlue => RColor::LightBlue,
        Color::LightMagenta => RColor::LightMagenta,
        Color::LightCyan => RColor::LightCyan,
        Color::White => RColor::White,
        Color::Rgb(r, g, b) => RColor::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Line;

    #[test]
    fn control_chars_render_as_spaces() {
        let cells = line_cells(&Line::plain("a\tb"));
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[1].0, ' ');
    }

    #[test]
    fn cursor_style_is_independent_of_the_text_under_it() {
        let mut cells = vec![('a', Style::fg(Color::Green).bold())];
        let cursor = Style::fg(Color::Black).bg(Color::Yellow);
        place_cursor(&mut cells, 0, cursor);
        assert_eq!(cells[0].1, cursor);
    }

    #[test]
    fn isearch_uses_lazy_highlight_for_other_matches() {
        let mut cells = vec![('a', Style::default()); 8];
        let lazy = Style::fg(Color::White).bg(Color::Rgb(0x66, 0x8b, 0x8b));
        let isearch = Style::fg(Color::Rgb(0x8b, 0x23, 0x23)).bg(Color::Rgb(0xee, 0x79, 0x9f));
        let highlights = vec![(Pos::new(0, 0), 2), (Pos::new(0, 4), 2)];
        apply_highlights(
            &mut cells,
            0,
            &highlights,
            Some((Pos::new(0, 0), 2)),
            lazy,
            isearch,
        );
        assert_eq!(cells[0].1.bg, isearch.bg);
        assert_eq!(cells[0].1.color, isearch.color);
        assert_eq!(cells[4].1.bg, lazy.bg);
        assert_eq!(cells[4].1.color, Style::default().color);
    }

    #[test]
    fn isearch_cursor_sits_at_the_match_edge() {
        let mut search = Search {
            active: true,
            direction: SearchDirection::Forward,
            match_end: Some(Pos::new(0, 4)),
            ..Search::default()
        };
        assert_eq!(display_point(&search, Pos::new(0, 0)), Pos::new(0, 4));
        search.direction = SearchDirection::Backward;
        assert_eq!(display_point(&search, Pos::new(0, 0)), Pos::new(0, 0));
    }
}
