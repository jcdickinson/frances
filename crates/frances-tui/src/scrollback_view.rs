//! Alt-screen view of the structured scrollback held by
//! [`BottomBackend`]. The entry point is [`paint_scrollback`], a pure
//! function that takes a slice of history lines plus a scroll offset
//! (in rendered rows from the bottom) and lays out:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────┐
//! │  ▲ 12 more rows above                               │  status (1 row)
//! │  history line                                       │
//! │  ...                                                │  content
//! │  history line                                       │
//! │  ▼ 3 more rows below             [Esc] back         │  status (1 row)
//! └─────────────────────────────────────────────────────┘
//! ```
//!
//! The `▲` marker is suppressed when the top of history is visible;
//! the `▼` marker is suppressed when the bottom is visible. The hint
//! lives in the bottom status bar.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// Render the scrollback view into `frame`'s full area.
///
/// `history` is the logical row sequence (oldest first). `scroll` is
/// measured in rendered rows from the bottom of the wrapped content —
/// `0` means the most recent rows are flush against the bottom status
/// bar. The function clamps `scroll` against the maximum possible
/// offset so callers can pass `u16::MAX` to jump to the top.
pub fn paint_scrollback(frame: &mut Frame<'_>, history: &[Line<'static>], scroll: u16) {
    let area = frame.area();
    if area.height < 2 {
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    let top_bar = chunks[0];
    let content = chunks[1];
    let bottom_bar = chunks[2];

    let para = Paragraph::new(history.to_vec()).wrap(Wrap { trim: false });
    let total_rows = para.line_count(content.width) as u16;

    // Scroll is rows from the bottom; clamp against what's possible.
    let max_scroll = total_rows.saturating_sub(content.height);
    let scroll = scroll.min(max_scroll);

    // Visible window inside the wrapped paragraph: [y_offset, y_offset + visible_rows).
    let y_offset = max_scroll.saturating_sub(scroll);
    let above = y_offset;
    let below = scroll;

    // Render content. If the history is shorter than the content area,
    // bottom-align it inside the area (history "sits" against the
    // bottom status bar, like a shell terminal). Otherwise render at
    // the scroll offset to fill the area.
    if total_rows < content.height {
        let pad = content.height - total_rows;
        let aligned = Rect::new(content.x, content.y + pad, content.width, total_rows);
        para.render(aligned, frame.buffer_mut());
    } else {
        para.scroll((y_offset, 0))
            .render(content, frame.buffer_mut());
    }

    // Status bars: the ▲/▼ marker uses the terminal's default
    // foreground (so it's always visible against the user's theme);
    // the trailing text and hint are DIM so they read as chrome.
    let dim = Style::default().add_modifier(Modifier::DIM);

    // Top status bar.
    if above > 0 {
        let line = Line::from(vec![
            Span::raw("  ▲"),
            Span::styled(format!(" {above} more row{} above", plural(above)), dim),
        ]);
        frame.render_widget(Paragraph::new(line), top_bar);
    }

    // Bottom status bar: marker on the left, [Esc] hint on the right.
    let left = if below > 0 {
        Line::from(vec![
            Span::raw("  ▼"),
            Span::styled(format!(" {below} more row{} below", plural(below)), dim),
        ])
    } else {
        Line::from(Span::styled("  (bottom)", dim))
    };
    let hint_text = "[Esc] back  ";
    let hint_w = hint_text.chars().count() as u16;
    let (left_area, hint_area) = split_right(bottom_bar, hint_w);
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint_text, dim))),
        hint_area,
    );
}

/// Total wrapped row count for `history` at `width`. Useful for
/// computing the max scroll offset outside the paint function.
pub fn total_wrapped_rows(history: &[Line<'static>], width: u16) -> u16 {
    if history.is_empty() || width == 0 {
        return 0;
    }
    Paragraph::new(history.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width) as u16
}

fn plural(n: u16) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn split_right(area: Rect, right_w: u16) -> (Rect, Rect) {
    let right_w = right_w.min(area.width);
    let left_w = area.width - right_w;
    let left = Rect::new(area.x, area.y, left_w, area.height);
    let right = Rect::new(area.x + left_w, area.y, right_w, area.height);
    (left, right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn lines(n: u16) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::raw(format!("line {i}"))).collect()
    }

    fn row_text(term: &Terminal<TestBackend>, y: u16) -> String {
        let buf = term.backend().buffer();
        let mut s = String::new();
        for x in 0..buf.area.width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.trim_end().to_string()
    }

    fn render(
        width: u16,
        height: u16,
        history: &[Line<'static>],
        scroll: u16,
    ) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| paint_scrollback(f, history, scroll)).unwrap();
        term
    }

    #[test]
    fn scroll_zero_shows_bottom_no_below_marker() {
        // 10 history rows, 7-row content area (1 top + 7 content + 1 bottom = 9 rows... wait we need height >=2).
        // Let's use height 9 so content_area = 7. 10 history rows -> 3 above, 0 below.
        let term = render(40, 9, &lines(10), 0);
        let top = row_text(&term, 0);
        let bottom = row_text(&term, 8);
        assert!(top.contains("▲"), "top bar should show ▲, got {top:?}");
        assert!(top.contains("3"), "top bar count should be 3, got {top:?}");
        assert!(
            !bottom.contains("▼"),
            "bottom bar should NOT show ▼ at scroll=0, got {bottom:?}"
        );
        assert!(bottom.contains("(bottom)"));
        // Last content row (y=7) should show the most recent line.
        assert_eq!(row_text(&term, 7), "line 9");
    }

    #[test]
    fn scroll_max_shows_top_no_above_marker() {
        // 10 rows, height 9 → content 7. max_scroll = 10 - 7 = 3.
        let term = render(40, 9, &lines(10), 3);
        let top = row_text(&term, 0);
        let bottom = row_text(&term, 8);
        assert!(
            !top.contains("▲"),
            "top bar should NOT show ▲ at max scroll, got {top:?}"
        );
        assert!(bottom.contains("▼"));
        assert!(bottom.contains("3"));
        // First content row should be the oldest line.
        assert_eq!(row_text(&term, 1), "line 0");
    }

    #[test]
    fn scroll_middle_shows_both_markers() {
        // 10 rows, height 9 → content 7. scroll=1 -> above=2, below=1.
        let term = render(40, 9, &lines(10), 1);
        let top = row_text(&term, 0);
        let bottom = row_text(&term, 8);
        assert!(top.contains("▲"));
        assert!(top.contains("2"));
        assert!(bottom.contains("▼"));
        assert!(bottom.contains("1"));
    }

    #[test]
    fn scroll_clamps_above_max() {
        // History smaller than the area — no scroll possible.
        let term = render(40, 10, &lines(3), u16::MAX);
        let top = row_text(&term, 0);
        let bottom = row_text(&term, 9);
        assert!(!top.contains("▲"));
        assert!(!bottom.contains("▼"));
        assert!(bottom.contains("(bottom)"));
    }

    #[test]
    fn esc_hint_is_present() {
        let term = render(40, 10, &lines(5), 0);
        let bottom = row_text(&term, 9);
        assert!(bottom.contains("[Esc] back"));
    }
}
