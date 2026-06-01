//! [`TextInput`] — bordered, focusable text input. Wraps
//! `ratatui_textarea::TextArea` for the actual text editing
//! (cursor placement, horizontal scroll, paste handling); this
//! widget owns the surrounding border + optional status-title
//! inset.
//!
//! `TextArea` paints its own reversed-style cursor cell, so the
//! terminal cursor stays hidden upstream.

use std::cell::RefCell;

use crossterm::event::Event;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use ratatui_textarea::TextArea;

use super::{
    AnimationLease, EventContext, EventOutcome, FocusManager, Input, RenderContext, Widget,
    WidgetState,
};

/// Top + bottom border rows that frame the text area.
const BORDER_ROWS: u16 = 2;

/// Total rows occupied by a single-line `TextInput`: the border plus one row of text.
pub const TEXT_INPUT_HEIGHT: u16 = BORDER_ROWS + 1;

/// The text area grows one row per line up to this many rows; beyond it
/// the textarea scrolls vertically instead of the box growing further.
const MAX_TEXT_ROWS: u16 = 5;

pub struct TextInput {
    textarea: TextArea<'static>,
    status: Option<(String, Color)>,
    /// Animation lease held while `status` is `Some`. Reconciled in
    /// `render` (which is where we have access to the gate via
    /// [`RenderContext`]); cleared one render after [`set_status`]
    /// drops the status text.
    animation_lease: RefCell<Option<AnimationLease>>,
    state: WidgetState,
}

impl TextInput {
    pub fn new(focus: &mut FocusManager, placeholder: impl Into<String>) -> Self {
        let mut textarea = TextArea::new(vec![String::new()]);
        textarea.set_placeholder_text(placeholder);
        // Use an explicit Gray background for the cursor cell so it is
        // visible in terminals that do not faithfully render REVERSED
        // (inverse video) — notably `foot` and others.
        textarea.set_cursor_style(Style::default().bg(Color::Gray));
        // The cursor cell stays at the widget's default reversed
        // style, but the whole-line underline is too noisy for a
        // single-row input box.
        textarea.set_cursor_line_style(Style::default());
        Self {
            textarea,
            status: None,
            animation_lease: RefCell::new(None),
            state: WidgetState {
                focus_id: Some(focus.allocate()),
                ..WidgetState::default()
            },
        }
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.textarea.lines().iter().all(|l| l.is_empty())
    }

    pub fn clear(&mut self) {
        self.textarea.clear();
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<String>) {
        self.textarea.set_placeholder_text(placeholder);
    }

    /// Set the status text inset on the top border (e.g.
    /// `┌─ [working…] ─────┐`). `None` clears it. The text is rendered
    /// in the dim variant of `color`; a single bright cell pulses
    /// across the line, paced by the [`FrameTime`] in the active
    /// [`RenderContext`](super::RenderContext) so the animation stays
    /// steady regardless of redraw frequency.
    ///
    /// [`FrameTime`]: super::FrameTime
    pub fn set_status(&mut self, status: Option<(impl Into<String>, Color)>) {
        self.status = status.map(|(s, color)| (s.into(), color));
    }
}

impl Input for TextInput {
    fn handle_event(&mut self, _: &mut EventContext<'_>, event: &Event) -> EventOutcome {
        match event {
            Event::Key(k) => {
                // ratatui-textarea ships `From<crossterm::event::KeyEvent>`
                // for its Input type behind the (default-on) `crossterm` feature.
                let _ = self.textarea.input(*k);
                EventOutcome::Consumed
            }
            _ => EventOutcome::Pass,
        }
    }
}

impl Widget for TextInput {
    fn state(&self) -> &WidgetState {
        &self.state
    }
    fn state_mut(&mut self) -> &mut WidgetState {
        &mut self.state
    }

    fn measure(&self, _: u16) -> u16 {
        let text_rows = (self.textarea.lines().len() as u16).clamp(1, MAX_TEXT_ROWS);
        text_rows + BORDER_ROWS
    }

    fn render(&self, ctx: &mut RenderContext<'_>) {
        if ctx.area.width == 0 || ctx.area.height == 0 {
            return;
        }
        let want_lease = self.status.as_ref().is_some_and(|(s, _)| !s.is_empty());
        let mut lease_slot = self.animation_lease.borrow_mut();
        match (want_lease, lease_slot.is_some()) {
            (true, false) => *lease_slot = Some(ctx.animation_lease()),
            (false, true) => *lease_slot = None,
            _ => {}
        }
        drop(lease_slot);

        // Build the border (with the optional status-title inset)
        // fresh each frame so the title text can change between
        // frames. We paint it ourselves and render the textarea into
        // the inner rect rather than handing it to the textarea via
        // `set_block` — that would force a full clone of the textarea
        // every frame, since `render` only has `&self`.
        let block = match self.status.as_ref().filter(|(s, _)| !s.is_empty()) {
            Some((s, color)) => {
                let mut spans = vec![Span::raw("─ ")];
                spans.extend(pulse_spans(s, *color, ctx.frame_time.get_frame()));
                spans.push(Span::raw(" "));
                Block::default()
                    .borders(Borders::ALL)
                    .style(ctx.theme.border)
                    .title(Line::from(spans))
            }
            None => Block::default()
                .borders(Borders::ALL)
                .style(ctx.theme.border),
        };
        let inner = block.inner(ctx.area);
        ratatui::widgets::Widget::render(block, ctx.area, &mut *ctx.buf);
        ratatui::widgets::Widget::render(&self.textarea, inner, &mut *ctx.buf);
    }
}

/// Render `text` as a `DarkGray` base with a two-cell "comet" walking
/// right-to-left: a bright cell (the bright ANSI variant of `color`)
/// followed by a regular `color` cell. The pair wraps around the
/// right edge with no rest gap.
///
/// Cell 0 is special — it's painted at the bright variant regardless
/// of the comet's position. Callers prefix a spinner glyph there so
/// the indicator's lead stays visible at all times.
///
/// `frame` is in 60fps units (see [`FrameTime`](super::FrameTime)); the
/// comet steps one cell every six frames (~10 Hz) regardless of host
/// redraw frequency.
fn pulse_spans(text: &str, color: Color, frame: f64) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n == 0 {
        return Vec::new();
    }
    let bright = Style::default().fg(brighter(color));
    let normal = Style::default().fg(color);
    let bg = Style::default().fg(Color::DarkGray);

    // Cell 0 is the always-bright spinner; the comet rides over cells
    // 1..n. With one comet cell (text of length 2) we collapse the
    // pair onto a single position.
    let comet_len = n.saturating_sub(1);
    let (head_pos, tail_pos) = if comet_len == 0 {
        (None, None)
    } else {
        let step = (frame / 6.0).rem_euclid(comet_len as f64) as usize;
        let head_rel = (comet_len - 1) - step;
        let tail_rel = (head_rel + 1) % comet_len;
        (Some(head_rel + 1), Some(tail_rel + 1))
    };

    chars
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let style = if i == 0 || Some(i) == head_pos {
                bright
            } else if Some(i) == tail_pos {
                normal
            } else {
                bg
            };
            Span::styled(c.to_string(), style)
        })
        .collect()
}

/// Bright variant of an ANSI 16-colour foreground. The standard
/// `Color::Red` etc. map to their `Light*` counterparts; anything
/// already bright (or outside the named palette) is returned as-is.
fn brighter(color: Color) -> Color {
    match color {
        Color::Black => Color::DarkGray,
        Color::Red => Color::LightRed,
        Color::Green => Color::LightGreen,
        Color::Yellow => Color::LightYellow,
        Color::Blue => Color::LightBlue,
        Color::Magenta => Color::LightMagenta,
        Color::Cyan => Color::LightCyan,
        Color::Gray => Color::White,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::{Focus, Theme};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn type_chars(input: &mut TextInput, text: &str) {
        let mut focus = Focus::new();
        let mut redraw = false;
        let mut ctx = EventContext {
            focus: &mut focus,
            redraw: &mut redraw,
        };
        for c in text.chars() {
            let ev = Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char(c),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            ));
            input.handle_event(&mut ctx, &ev);
        }
    }

    #[test]
    fn empty_input_text_round_trips() {
        let mut mgr = FocusManager::new();
        let input = TextInput::new(&mut mgr, "type a message");
        assert!(input.is_empty());
        assert_eq!(input.text(), "");
    }

    #[test]
    fn typed_chars_appear_in_text() {
        let mut mgr = FocusManager::new();
        let mut input = TextInput::new(&mut mgr, "hi");
        type_chars(&mut input, "hello");
        assert_eq!(input.text(), "hello");
        assert!(!input.is_empty());
    }

    #[test]
    fn clear_empties_textarea() {
        let mut mgr = FocusManager::new();
        let mut input = TextInput::new(&mut mgr, "hi");
        type_chars(&mut input, "abc");
        input.clear();
        assert!(input.is_empty());
        assert_eq!(input.text(), "");
    }

    #[test]
    fn empty_input_is_three_rows() {
        let mut mgr = FocusManager::new();
        let input = TextInput::new(&mut mgr, "hi");
        assert_eq!(input.measure(80), TEXT_INPUT_HEIGHT);
    }

    #[test]
    fn measure_grows_per_line_then_caps() {
        let mut mgr = FocusManager::new();
        let mut input = TextInput::new(&mut mgr, "hi");
        // Three lines → three text rows + border.
        input.textarea.insert_newline();
        input.textarea.insert_newline();
        assert_eq!(input.measure(80), 3 + BORDER_ROWS);
        // Past the cap the box stops growing.
        for _ in 0..10 {
            input.textarea.insert_newline();
        }
        assert_eq!(input.measure(80), MAX_TEXT_ROWS + BORDER_ROWS);
    }

    #[test]
    fn focus_id_is_allocated_on_construction() {
        let mut mgr = FocusManager::new();
        let input = TextInput::new(&mut mgr, "hi");
        assert!(input.state().focus_id.is_some());
    }

    // Cursor visibility test lives in scrollback_container.rs where it
    // renders through the full terminal pipeline (ScrollbackContainer +
    // TermBackend).

    #[test]
    fn cursor_cell_has_gray_bg_in_buffer() {
        let mut mgr = FocusManager::new();
        let input = TextInput::new(&mut mgr, "type a message");
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        let theme = Theme::default();
        let focus = Focus::new();
        let frame_time = crate::widget::FixedFrameTime(0.0);
        let animation = crate::widget::AnimationGate::new();
        let mut ctx = RenderContext {
            area,
            buf: &mut buf,
            theme: &theme,
            focus: &focus,
            frame_time: &frame_time,
            animation: &animation,
        };
        input.render(&mut ctx);
        // Inner rect starts at col 1, row 1 (inside borders). Cursor
        // cell should have a Gray background.
        let cell = &buf[(1, 1)];
        assert_eq!(
            cell.bg,
            Color::Gray,
            "cursor cell bg should be Gray, got {:?}. cell: symbol={:?}, fg={:?}",
            cell.bg,
            cell.symbol(),
            cell.fg,
        );
    }

    #[test]
    fn render_paints_a_border() {
        let mut mgr = FocusManager::new();
        let mut input = TextInput::new(&mut mgr, "hi");
        let area = Rect::new(0, 0, 10, 3);
        input.layout(area);
        let mut buf = Buffer::empty(area);
        let theme = Theme::default();
        let focus = Focus::new();
        let frame_time = crate::widget::FixedFrameTime(0.0);
        let animation = crate::widget::AnimationGate::new();
        let mut ctx = RenderContext {
            area,
            buf: &mut buf,
            theme: &theme,
            focus: &focus,
            frame_time: &frame_time,
            animation: &animation,
        };
        input.render(&mut ctx);
        assert_eq!(buf[(0, 0)].symbol(), "┌");
        assert_eq!(buf[(9, 0)].symbol(), "┐");
        assert_eq!(buf[(0, 2)].symbol(), "└");
        assert_eq!(buf[(9, 2)].symbol(), "┘");
    }

    #[test]
    fn status_title_lands_on_top_border() {
        let mut mgr = FocusManager::new();
        let mut input = TextInput::new(&mut mgr, "hi");
        input.set_status(Some(("streaming", Color::Cyan)));
        let area = Rect::new(0, 0, 20, 3);
        input.layout(area);
        let mut buf = Buffer::empty(area);
        let theme = Theme::default();
        let focus = Focus::new();
        let frame_time = crate::widget::FixedFrameTime(0.0);
        let animation = crate::widget::AnimationGate::new();
        let mut ctx = RenderContext {
            area,
            buf: &mut buf,
            theme: &theme,
            focus: &focus,
            frame_time: &frame_time,
            animation: &animation,
        };
        input.render(&mut ctx);
        // Top border row contains "─ streaming " somewhere after the
        // left corner. Concatenate the row and check.
        let row: String = (0..area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            row.contains("streaming"),
            "expected status text on top border, got `{row}`"
        );
    }

    #[test]
    fn animation_lease_tracks_status_presence() {
        let mut mgr = FocusManager::new();
        let mut input = TextInput::new(&mut mgr, "hi");
        let area = Rect::new(0, 0, 20, 3);
        input.layout(area);
        let theme = Theme::default();
        let focus = Focus::new();
        let frame_time = crate::widget::FixedFrameTime(0.0);
        let animation = crate::widget::AnimationGate::new();

        let render = |input: &TextInput| {
            let mut buf = Buffer::empty(area);
            let mut ctx = RenderContext {
                area,
                buf: &mut buf,
                theme: &theme,
                focus: &focus,
                frame_time: &frame_time,
                animation: &animation,
            };
            input.render(&mut ctx);
        };

        // No status → no lease taken.
        render(&input);
        assert_eq!(animation.active(), 0);

        // Setting status → render acquires a lease.
        input.set_status(Some(("streaming", Color::Cyan)));
        render(&input);
        assert_eq!(animation.active(), 1);

        // Re-rendering doesn't double-acquire.
        render(&input);
        assert_eq!(animation.active(), 1);

        // Clearing status → next render drops the lease.
        input.set_status(Option::<(&str, Color)>::None);
        render(&input);
        assert_eq!(animation.active(), 0);
    }

    #[test]
    fn pulse_spans_walks_comet_right_to_left() {
        use crate::widget::{AtomicFrameTime, FrameTime};

        let clock = AtomicFrameTime::new(0.0);
        // 6-char text → spinner at 0, comet over 1..6 (5 positions).
        // At frame 0 the head sits at the rightmost comet cell.
        let spans = pulse_spans("Sabcde", Color::Red, clock.get_frame());
        let styles: Vec<Style> = spans.iter().map(|s| s.style).collect();

        let bright = Style::default().fg(Color::LightRed);
        let normal = Style::default().fg(Color::Red);
        let bg = Style::default().fg(Color::DarkGray);

        // Spinner column is always bright.
        assert_eq!(styles[0], bright);
        // Comet head at the right edge; tail wraps to position 1.
        assert_eq!(styles[5], bright);
        assert_eq!(styles[1], normal);
        assert_eq!(styles[2], bg);
        assert_eq!(styles[3], bg);
        assert_eq!(styles[4], bg);

        // One step later (6 frames at 60fps == one cell): head moves
        // one cell left, tail follows.
        clock.set(6.0);
        let styles: Vec<Style> = pulse_spans("Sabcde", Color::Red, clock.get_frame())
            .iter()
            .map(|s| s.style)
            .collect();
        assert_eq!(styles[0], bright);
        assert_eq!(styles[4], bright);
        assert_eq!(styles[5], normal);
        assert_eq!(styles[1], bg);
    }
}
