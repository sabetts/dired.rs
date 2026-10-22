//! Rendering with ratatui.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color as RColor, Modifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan, Text};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, Prompt, SplitAxis, WindowId, WindowTree, WindowView};
use crate::buffer::{Color, Line, Pos, Style};
use crate::search::{Search, SearchDirection};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let minibuffer = app.minibuffer_height().max(1);
    let rows =
        Layout::vertical([Constraint::Min(0), Constraint::Length(minibuffer)]).split(frame.area());

    let mut panes = Vec::new();
    let mut dividers = Vec::new();
    collect_panes(app.window_tree(), rows[0], &mut panes, &mut dividers);
    app.set_window_areas(
        panes
            .iter()
            .map(|(id, buffer_area, _)| (*id, *buffer_area))
            .collect(),
    );
    let active_id = app.active_window_id();
    let active_area = panes
        .iter()
        .find(|(id, _, _)| *id == active_id)
        .map(|(_, buffer_area, _)| *buffer_area)
        .unwrap_or(rows[0]);
    app.prepare_active_view(active_area.width as usize, active_area.height as usize);

    for &(id, buffer_area, mode_area) in &panes {
        let Some(window) = app.window(id) else {
            continue;
        };
        let active = id == active_id;
        let view = if active {
            let buffer = &app.buffers[window.buffer].buffer;
            WindowView {
                point: buffer.point(),
                scroll: buffer.scroll(),
                scroll_sub: buffer.scroll_sub(),
                width: buffer.width(),
            }
        } else {
            window.view
        };
        draw_buffer(frame, app, window.buffer, buffer_area, view, active);
        draw_mode_line(
            frame,
            app,
            window.buffer,
            app.window_number(id),
            view.point.row + 1,
            mode_area,
            active,
        );
    }
    for divider in dividers {
        draw_vertical_border(frame, divider);
    }
    if app.show_window_numbers() {
        draw_window_numbers(frame, app, &panes);
    }
    draw_minibuffer(frame, app, rows[1]);
}

fn collect_panes(
    tree: &WindowTree,
    area: Rect,
    out: &mut Vec<(WindowId, Rect, Rect)>,
    dividers: &mut Vec<Rect>,
) {
    match tree {
        WindowTree::Leaf(id) => {
            let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
            out.push((*id, rows[0], rows[1]));
        }
        WindowTree::Split {
            axis,
            first,
            second,
        } => {
            let chunks = match axis {
                SplitAxis::Below => {
                    Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(area)
                }
                SplitAxis::Right => Layout::horizontal([
                    Constraint::Fill(1),
                    Constraint::Length(1),
                    Constraint::Fill(1),
                ])
                .split(area),
            };
            match axis {
                SplitAxis::Below => {
                    collect_panes(first, chunks[0], out, dividers);
                    collect_panes(second, chunks[1], out, dividers);
                }
                SplitAxis::Right => {
                    collect_panes(first, chunks[0], out, dividers);
                    dividers.push(chunks[1]);
                    collect_panes(second, chunks[2], out, dividers);
                }
            }
        }
    }
}

fn draw_vertical_border(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = vec![RLine::from("│"); area.height as usize];
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

const DIGIT_PATTERNS: [[&str; 5]; 10] = [
    ["███", "█ █", "█ █", "█ █", "███"],
    [" █ ", "██ ", " █ ", " █ ", "███"],
    ["███", "  █", "███", "█  ", "███"],
    ["███", "  █", "███", "  █", "███"],
    ["█ █", "█ █", "███", "  █", "  █"],
    ["███", "█  ", "███", "  █", "███"],
    ["███", "█  ", "███", "█ █", "███"],
    ["███", "  █", "  █", "  █", "  █"],
    ["███", "█ █", "███", "█ █", "███"],
    ["███", "█ █", "███", "  █", "███"],
];

fn draw_window_numbers(frame: &mut Frame, app: &App, panes: &[(WindowId, Rect, Rect)]) {
    let style = rstyle(app.cursor_style);
    for &(id, area, _) in panes {
        let Some(number) = app.window_number(id) else {
            continue;
        };
        let pattern = &DIGIT_PATTERNS[usize::from(number)];
        if area.width >= 3 && area.height >= 5 {
            let x = area.x + (area.width - 3) / 2;
            let y = area.y + (area.height - 5) / 2;
            let lines: Vec<RLine> = pattern
                .iter()
                .map(|row| RLine::from(RSpan::styled((*row).to_string(), style)))
                .collect();
            frame.render_widget(Paragraph::new(Text::from(lines)), Rect::new(x, y, 3, 5));
        } else if area.width > 0 && area.height > 0 {
            let x = area.x + (area.width - 1) / 2;
            let y = area.y + (area.height - 1) / 2;
            frame.render_widget(
                Paragraph::new(number.to_string()).style(style),
                Rect::new(x, y, 1, 1),
            );
        }
    }
}

fn draw_buffer(
    frame: &mut Frame,
    app: &App,
    idx: usize,
    area: Rect,
    view: WindowView,
    active: bool,
) {
    let height = area.height as usize;
    let width = area.width as usize;
    if height == 0 || width == 0 {
        return;
    }
    let buf = &app.buffers[idx].buffer;
    let cursor_point = display_point(&app.search, view.point);

    let highlights = if active && app.search.active && !app.search.query.is_empty() {
        buf.matches(&app.search.query)
    } else {
        Vec::new()
    };
    let current_match = if active && app.search.active {
        app.search.match_end.and_then(|end| {
            (end.row == view.point.row && end.col > view.point.col)
                .then(|| (view.point, end.col - view.point.col))
        })
    } else {
        None
    };
    // Hex view: the byte under point, highlighted in its hex and ASCII forms
    // with a background colour (Emacs's `highlight` face), not reverse video.
    let hex_highlights = if active {
        app.hexl_highlights()
    } else {
        Vec::new()
    };

    // Rendering starts at the persisted view top; the buffer keeps the point
    // visible (wrapping-aware), so the view does not drift as point moves.
    let last_line = buf.len().saturating_sub(1);
    let mut lines: Vec<RLine> = Vec::with_capacity(height);
    let mut row = view.scroll.min(last_line);
    let mut sub = view.scroll_sub;
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
        if active && row == cursor_point.row && matches!(app.prompt, Prompt::None) {
            place_cursor(&mut cells, cursor_point.col, app.cursor_style);
        }

        let total = cells.len();
        if total == 0 {
            lines.push(to_rline(&[]));
        } else {
            let sub = sub.min(total.saturating_sub(1) / width);
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

fn draw_mode_line(
    frame: &mut Frame,
    app: &App,
    idx: usize,
    number: Option<u8>,
    line: usize,
    area: Rect,
    active: bool,
) {
    let base = if active {
        RStyle::default().add_modifier(Modifier::REVERSED)
    } else {
        RStyle::default().add_modifier(Modifier::REVERSED | Modifier::DIM)
    };
    let number = number
        .map(|number| number.to_string())
        .unwrap_or_else(|| "-".to_string());
    let text = format!(
        " {}  {}  ({})  L{} ",
        number,
        app.mode_line_name_for(idx),
        app.mode_line_mode_for(idx),
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
