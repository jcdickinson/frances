use std::borrow::Cow;
use std::cell::{OnceCell, Ref, RefCell};
use std::ops::Range;
use std::sync::Arc;

use frances_core::CountingSink;

use crate::tui::status::{StatusTone, status_prefix};
use frances_session::events::{
    BlockKind as WireBlockKind, ReasoningState, ShellState, Source, TailedHeader,
};
use frances_tui::block::Sigil;
use frances_tui::widget::{EventContext, EventOutcome, Input};
use frances_tui::{Block, BlockMeasureContext, BlockRenderContext};
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar;

/// Maximum body lines (post trailing-newline strip) shown for a
/// tailed block in its compact (unfocused) state. Earlier lines are
/// collapsed into a single `… [N earlier lines]` marker so the visible
/// tail tracks the action.
const TAIL_LINES: usize = 10;
const DIFF_LINE_NUMBER_WIDTH: usize = 4;
/// Expanded tail height when a tailed block is the alt-view inspector's
/// selection. Doubles the visible body so the user has room to see more
/// of the source while paging with `j`/`k`/`u`/`d`.
const TAIL_LINES_FOCUSED: usize = 20;

fn tail_lines_for(selected: bool) -> usize {
    if selected {
        TAIL_LINES_FOCUSED
    } else {
        TAIL_LINES
    }
}

/// Build the right [`Block`] impl for a wire `BlockKind` + accumulated
/// text. Most kinds map onto a generic [`LabelledBlock`]; `Tailed`
/// (shell output, reasoning) has its own structural shape (header +
/// body tail) and gets a dedicated [`TailedBlock`].
pub fn block_for_kind(kind: WireBlockKind, text: String) -> Box<dyn Block> {
    match kind {
        WireBlockKind::Tailed { header } => Box::new(TailedBlock::new(header, text)),
        WireBlockKind::Diff { lines } => Box::new(DiffBlock::new(lines)),
        WireBlockKind::ToolUse {
            name,
            detail: Some(detail),
        } => Box::new(ToolUseBlock::new(name, detail)),
        other => Box::new(LabelledBlock::new(other, text)),
    }
}

#[derive(Serialize, Deserialize)]
pub struct DiffBlock {
    lines: Vec<frances_session::events::DiffLine>,
}

impl DiffBlock {
    pub fn new(lines: Vec<frances_session::events::DiffLine>) -> Self {
        Self { lines }
    }
}

impl Input for DiffBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> EventOutcome {
        EventOutcome::Pass
    }
}

struct DiffDisplayLine {
    text: String,
    style: Style,
}

fn diff_display_line(line: &frances_session::events::DiffLine) -> DiffDisplayLine {
    match line {
        frances_session::events::DiffLine::Context { text, line } => DiffDisplayLine {
            text: format!("{:width$} {}", line, text, width = DIFF_LINE_NUMBER_WIDTH),
            style: Style::default(),
        },
        frances_session::events::DiffLine::Added(text) => DiffDisplayLine {
            text: format!("+{:width$}{}", "", text, width = DIFF_LINE_NUMBER_WIDTH),
            style: Style::default().bg(Color::Green).fg(Color::Black),
        },
        frances_session::events::DiffLine::Removed(text) => DiffDisplayLine {
            text: format!("-{:width$}{}", "", text, width = DIFF_LINE_NUMBER_WIDTH),
            style: Style::default().bg(Color::Red).fg(Color::Black),
        },
    }
}

impl Block for DiffBlock {
    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        let max = ctx.width.max(1) as usize;
        let mut count = 0;
        for line in &self.lines {
            let content = diff_display_line(line).text;
            let mut sink = CountingSink(0);
            wrap_rows(&content, 0, max, &mut sink);
            count += sink.0 as u16;
        }
        count
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
        let max = ctx.area.width.max(1) as usize;
        let mut row_writer = RowWriter::new(ctx);
        for line in &self.lines {
            let display = diff_display_line(line);

            let mut out = Vec::new();
            wrap_into("", &display.text, max, &mut out);

            for wrapped_line in out {
                let written = row_writer.write_styled(&wrapped_line, display.style);
                if !written && row_writer.finished() {
                    paint_truncation_marker_if_set(row_writer.ctx);
                    return Sigil::blank();
                }
            }
        }
        paint_truncation_marker_if_set(row_writer.ctx);
        Sigil::blank()
    }
}

/// History row for a labelled (kind + text) block. Wraps to the
/// available width with the kind prefix on the first row and a
/// matching-width indent on continuation rows.
#[derive(Serialize, Deserialize)]
pub struct LabelledBlock {
    kind: WireBlockKind,
    text: String,
    /// Wrapped row-ranges into [`Self::body_text`], cached with the width
    /// they were computed for. The body is wrapped on both `measure`
    /// (3–4× per redraw) and `render`; caching keyed on width means a
    /// resize recomputes but repeated draws at a stable width reuse the
    /// ranges — and `render` slices the backing text rather than
    /// allocating a `String` per row. The block is rebuilt from fresh
    /// text on every streaming apply, so width is the only invalidation
    /// axis left.
    #[serde(skip)]
    rows: RefCell<Option<(u16, Vec<Range<usize>>)>>,
}

impl LabelledBlock {
    pub fn new(kind: WireBlockKind, text: String) -> Self {
        Self {
            kind,
            text,
            rows: RefCell::new(None),
        }
    }

    /// Body content rendered into the block area. For plain `ToolUse`
    /// markers the tool name lives in the kind (the `text` payload is
    /// empty), so we surface the name as the body — the `→ ` sigil
    /// already lives in the gutter.
    fn body_text(&self) -> &str {
        match &self.kind {
            WireBlockKind::ToolUse { name, .. } => name,
            _ => &self.text,
        }
    }

    /// Wrapped row-ranges for the body at `width`, recomputed only when
    /// the cached width differs. Ranges index into [`Self::body_text`].
    fn rows(&self, width: u16) -> Ref<'_, Vec<Range<usize>>> {
        let stale = self.rows.borrow().as_ref().is_none_or(|(w, _)| *w != width);
        if stale {
            let rows = body_line_ranges(self.body_text(), width);
            *self.rows.borrow_mut() = Some((width, rows));
        }
        Ref::map(self.rows.borrow(), |c| &c.as_ref().unwrap().1)
    }
}

impl Input for LabelledBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> EventOutcome {
        EventOutcome::Pass
    }
}

impl Block for LabelledBlock {
    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        self.rows(ctx.width).len() as u16
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
        let body = self.body_text();
        let rows = self.rows(ctx.area.width);
        let src_y = ctx.src_y;
        let area = ctx.area;
        for (i, row) in rows.iter().enumerate() {
            let i = i as u16;
            if i < src_y {
                continue;
            }
            let dst_row = i - src_y;
            if dst_row >= area.height {
                break;
            }
            ctx.buf.set_string(
                area.x,
                area.y + dst_row,
                &body[row.clone()],
                Style::default(),
            );
        }
        paint_truncation_marker_if_set(ctx);
        sigil_for(&self.kind)
    }
}

/// History row for a tailed streaming-output block (shell command,
/// model reasoning). Renders as:
///   `[label] header_body` (the body may wrap; continuation rows are unindented)
///   `… [N earlier lines]` (only when the body is longer than the tail)
///   last-`TAIL_LINES` body lines (wrapped, unindented)
///
/// `header` drives both the prefix label/colour and any pinned text
/// after it (shell: the command line; reasoning: empty). It rides on
/// every `BlockDelta` so the header stays pinned even while the body
/// keeps streaming.
#[derive(Serialize, Deserialize)]
pub struct TailedBlock {
    header: TailedHeader,
    text: String,
    /// Alt-view-only scroll offset, measured in *source lines* from
    /// the tail. `0` = the window sits at the tail (the canonical
    /// live-view position). Live-view renders always force this to
    /// `0` before computing the visible window, so the persisted
    /// representation never carries scroll state — hence the
    /// `serde(default, skip)` annotation.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    scroll_y: u16,
    /// Cached source-line byte-ranges into [`Self::text`] (trailing
    /// blanks stripped). The split is width-independent and the block is
    /// rebuilt on every streaming apply, so it's computed once and reused
    /// across the many `is_empty` / `max_scroll_for` / body calls a
    /// single redraw makes — the original re-`split('\n')`'d the whole
    /// (potentially large) body on each.
    #[serde(skip)]
    source: OnceCell<Vec<Range<usize>>>,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

impl TailedBlock {
    pub fn new(header: TailedHeader, text: String) -> Self {
        Self {
            header,
            text,
            scroll_y: 0,
            source: OnceCell::new(),
        }
    }

    fn header_prefix(&self) -> (String, Style) {
        tailed_status(&self.header)
    }

    /// Text pinned after the status prefix on the first header line.
    /// Shell: the command line. Reasoning: empty.
    fn header_body(&self) -> &str {
        match &self.header {
            TailedHeader::Shell { cmd, .. } => cmd,
            TailedHeader::Reasoning { .. } => "",
        }
    }

    /// Paint style for body rows. Reasoning bodies render in DarkGray
    /// so thoughts read as a subordinate aside without relying on the
    /// terminal honouring the DIM SGR (which many don't). Shell output
    /// stays default — it's meant to be scannable.
    fn body_style(&self) -> Style {
        match &self.header {
            TailedHeader::Reasoning { .. } => Style::default().fg(Color::DarkGray),
            TailedHeader::Shell { .. } => Style::default(),
        }
    }

    /// Reasoning blocks with no body are hidden — the empty `[thought]`
    /// pill on its own is just noise. Shell blocks always show their
    /// header (the command line + status) even when the body is empty.
    fn is_empty(&self) -> bool {
        matches!(self.header, TailedHeader::Reasoning { .. }) && self.source_ranges().is_empty()
    }

    fn header_lines(&self, width: u16) -> Vec<String> {
        let (prefix, _style) = self.header_prefix();
        let mut out = Vec::new();
        wrap_into(&prefix, self.header_body(), width.max(1) as usize, &mut out);
        out
    }

    /// Row count for [`Self::header_lines`] without materialising the rows.
    fn header_row_count(&self, width: u16) -> usize {
        let (prefix, _style) = self.header_prefix();
        let mut sink = CountingSink(0);
        wrap_rows(
            self.header_body(),
            display_width(&prefix),
            width.max(1) as usize,
            &mut sink,
        );
        sink.0
    }

    /// Source-line byte-ranges into [`Self::text`], with trailing blanks
    /// from a closing `\n` stripped, computed once and cached. Shared
    /// between [`Self::max_scroll_for`] and the windowing logic in
    /// [`Self::body_rows`].
    fn source_ranges(&self) -> &[Range<usize>] {
        self.source.get_or_init(|| {
            let mut ranges = line_ranges(&self.text);
            while matches!(ranges.last(), Some(r) if r.is_empty()) {
                ranges.pop();
            }
            ranges
        })
    }

    /// Maximum legal `scroll_y` for a given tail height — beyond this
    /// the window would expose negative rows. Returns `0` when the
    /// source is shorter than the tail.
    fn max_scroll_for(&self, tail: usize) -> u16 {
        self.source_ranges().len().saturating_sub(tail) as u16
    }

    /// Body rows for a window of height `tail` whose right edge sits
    /// `window_start` source-lines *before* the natural tail. The row
    /// count is invariant in `window_start` (clamped via
    /// [`Self::max_scroll_for`]) so `measure` and `render` agree on height.
    /// Source-line rows are borrowed slices of [`Self::text`]; only the
    /// marker is owned.
    ///
    /// - When `source.len() > tail`: 1 marker row + `tail` body rows.
    ///   The marker reports how many source lines remain hidden above
    ///   the window (`0` when scrolled to the top).
    /// - When `source.len() <= tail`: marker suppressed, output is the
    ///   source verbatim.
    fn body_rows(&self, width: u16, window_start: u16, tail: usize) -> Vec<Cow<'_, str>> {
        let source = self.source_ranges();
        if source.is_empty() {
            return Vec::new();
        }
        let max = width.max(1) as usize;
        let (marker, start, end) = body_window(source.len(), window_start, tail);
        let mut out: Vec<Cow<'_, str>> = Vec::new();
        if let Some(marker) = marker {
            let mut wrapped = Vec::new();
            wrap_into("", &marker, max, &mut wrapped);
            out.extend(wrapped.into_iter().map(Cow::Owned));
        }
        for r in &source[start..end] {
            let line = &self.text[r.clone()];
            let mut ranges = Vec::new();
            wrap_rows(line, 0, max, &mut ranges);
            out.extend(ranges.into_iter().map(|rr| Cow::Borrowed(&line[rr])));
        }
        out
    }

    /// Row count for [`Self::body_rows`] at the live-view window
    /// (`window_start = 0`) without materialising the rows. The count is
    /// invariant in `window_start`, so the live window is the right basis
    /// for `measure`.
    fn body_row_count(&self, width: u16, tail: usize) -> usize {
        let source = self.source_ranges();
        if source.is_empty() {
            return 0;
        }
        let max = width.max(1) as usize;
        let (marker, start, end) = body_window(source.len(), 0, tail);
        let mut sink = CountingSink(0);
        if let Some(marker) = &marker {
            wrap_rows(marker, 0, max, &mut sink);
        }
        for r in &source[start..end] {
            wrap_rows(&self.text[r.clone()], 0, max, &mut sink);
        }
        sink.0
    }
}

/// The visible body window for a tailed block of height `tail` whose
/// right edge sits `window_start` source-lines before the natural tail.
/// Returns `(marker, start, end)` where `source[start..end]` are the
/// visible source lines and `marker` is the `… [N earlier lines]` row
/// shown above them (`None` when the source fits inside the tail, in
/// which case the whole source is visible and no marker is shown).
///
/// `window_start` is clamped so the count of returned lines is invariant
/// in it — `measure` (window_start 0) and a scrolled `render` agree on
/// height.
fn body_window(
    source_len: usize,
    window_start: u16,
    tail: usize,
) -> (Option<String>, usize, usize) {
    if source_len > tail {
        let max_scroll = source_len.saturating_sub(tail);
        let start_offset = (window_start as usize).min(max_scroll);
        let end = source_len - start_offset;
        let start = end - tail;
        let marker = format!(
            "… [{start} earlier line{}]",
            if start == 1 { "" } else { "s" }
        );
        (Some(marker), start, end)
    } else {
        (None, 0, source_len)
    }
}

impl Input for TailedBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        event: &crossterm::event::Event,
    ) -> EventOutcome {
        let crossterm::event::Event::Key(key) = event else {
            return EventOutcome::Pass;
        };
        if key.kind != crossterm::event::KeyEventKind::Press {
            return EventOutcome::Pass;
        }
        // Input events only reach this block when it's the alt-view
        // selection, so the focused tail is the right clamp.
        let max = self.max_scroll_for(TAIL_LINES_FOCUSED);
        let half: u16 = (TAIL_LINES_FOCUSED as u16) / 2;
        match key.code {
            crossterm::event::KeyCode::Char('j') => {
                self.scroll_y = self.scroll_y.saturating_sub(1);
                EventOutcome::Consumed
            }
            crossterm::event::KeyCode::Char('k') => {
                self.scroll_y = self.scroll_y.saturating_add(1).min(max);
                EventOutcome::Consumed
            }
            crossterm::event::KeyCode::Char('d') => {
                self.scroll_y = self.scroll_y.saturating_sub(half);
                EventOutcome::Consumed
            }
            crossterm::event::KeyCode::Char('u') => {
                self.scroll_y = self.scroll_y.saturating_add(half).min(max);
                EventOutcome::Consumed
            }
            _ => EventOutcome::Pass,
        }
    }
}

impl Block for TailedBlock {
    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        if self.is_empty() {
            return 0;
        }
        let tail = tail_lines_for(ctx.selected);
        (self.header_row_count(ctx.width) + self.body_row_count(ctx.width, tail)) as u16
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
        if self.is_empty() {
            return Sigil::blank();
        }
        // Live view always shows the tail (window_start = 0).
        // Alt view honours the block's internal scroll position so the
        // user can page through earlier source lines. The tail grows
        // when this block is the alt-view selection.
        let window_start = if ctx.alt_view { self.scroll_y } else { 0 };
        let tail = tail_lines_for(ctx.selected);
        let header = self.header_lines(ctx.area.width);
        let body = self.body_rows(ctx.area.width, window_start, tail);
        let (prefix, prefix_style) = self.header_prefix();
        let prefix_bytes = prefix.len();
        let prefix_cols = display_width(&prefix) as u16;

        let src_y = ctx.src_y;
        let area = ctx.area;
        let mut src_idx: u16 = 0;
        for (i, line) in header.iter().enumerate() {
            if src_idx >= src_y {
                let dst_row = src_idx - src_y;
                if dst_row >= area.height {
                    paint_truncation_marker_if_set(ctx);
                    return Sigil::blank();
                }
                let y = area.y + dst_row;
                if i == 0 && line.starts_with(&prefix) {
                    ctx.buf
                        .set_string(area.x, y, &line[..prefix_bytes], prefix_style);
                    if line.len() > prefix_bytes {
                        ctx.buf.set_string(
                            area.x + prefix_cols,
                            y,
                            &line[prefix_bytes..],
                            Style::default(),
                        );
                    }
                } else {
                    ctx.buf.set_string(area.x, y, line, Style::default());
                }
            }
            src_idx = src_idx.saturating_add(1);
        }
        let body_style = self.body_style();
        for line in body.iter() {
            if src_idx >= src_y {
                let dst_row = src_idx - src_y;
                if dst_row >= area.height {
                    paint_truncation_marker_if_set(ctx);
                    return Sigil::blank();
                }
                ctx.buf
                    .set_string(area.x, area.y + dst_row, line.as_ref(), body_style);
            }
            src_idx = src_idx.saturating_add(1);
        }
        paint_truncation_marker_if_set(ctx);
        Sigil::blank()
    }
}

/// History row for a one-shot tool call with a detail suffix. Renders
/// as `→ {name}  {detail}` on one line — the prefix in yellow, the
/// detail in dim — wrapping `detail` to subsequent rows (still dim,
/// indented to match the prefix column) when the line overflows.
///
/// The plain `BlockKind::ToolUse` variant (no detail) still routes
/// through [`LabelledBlock`]; only the `Some(detail)` shape comes here.
#[derive(Serialize, Deserialize)]
pub struct ToolUseBlock {
    name: Arc<str>,
    detail: Arc<str>,
}

impl ToolUseBlock {
    pub fn new(name: Arc<str>, detail: Arc<str>) -> Self {
        Self { name, detail }
    }

    /// Lead text on the first body row — the tool name plus a two-space
    /// gap before the detail. The `→` glyph lives in the container's
    /// gutter as the [`Sigil`]; the body owns just the name + detail.
    fn name_prefix(&self) -> String {
        format!("{}  ", self.name)
    }

    fn wrapped_lines(&self, width: u16) -> Vec<String> {
        let max = width.max(1) as usize;
        let prefix = self.name_prefix();
        let indent = " ".repeat(display_width(&prefix));
        let mut out = Vec::new();
        for (i, source_line) in self.detail.split('\n').enumerate() {
            let lead = if i == 0 {
                prefix.as_str()
            } else {
                indent.as_str()
            };
            wrap_into(lead, source_line, max, &mut out);
        }
        if out.is_empty() {
            out.push(prefix);
        }
        out
    }

    /// Row count for [`Self::wrapped_lines`] without materialising the
    /// rows. The prefix and continuation indent share a width, so both
    /// pass `prefix_width` as the lead.
    fn wrapped_line_count(&self, width: u16) -> u16 {
        let max = width.max(1) as usize;
        let prefix_width = display_width(&self.name_prefix());
        let mut sink = CountingSink(0);
        for source_line in self.detail.split('\n') {
            wrap_rows(source_line, prefix_width, max, &mut sink);
        }
        sink.0 as u16
    }
}

impl Input for ToolUseBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> EventOutcome {
        EventOutcome::Pass
    }
}

impl Block for ToolUseBlock {
    /// One-shot block — emitted as `BlockDelta` + `BlockStop`
    /// back-to-back by the runtime. The container promotes it straight
    /// to `safe`, so it never sits in `active` as a streaming section.
    fn safe_on_push(&self) -> bool {
        true
    }

    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        self.wrapped_line_count(ctx.width)
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
        let lines = self.wrapped_lines(ctx.area.width);
        let prefix = self.name_prefix();
        // Split the prefix into the colored name segment (yellow,
        // matching the gutter `→`) and the trailing two spaces that
        // bridge into the dim detail. The name byte count is
        // `prefix.len() - 2` because the suffix is exactly `"  "`.
        let name_bytes = prefix.len() - 2;
        let prefix_cols = display_width(&prefix) as u16;
        let name_style = Style::default().fg(Color::Yellow);
        let dim_style = Style::default().add_modifier(Modifier::DIM);
        let src_y = ctx.src_y;
        let area = ctx.area;
        for (i, line) in lines.iter().enumerate() {
            let i = i as u16;
            if i < src_y {
                continue;
            }
            let dst_row = i - src_y;
            if dst_row >= area.height {
                break;
            }
            let y = area.y + dst_row;
            if i == 0 && line.starts_with(&prefix) {
                ctx.buf
                    .set_string(area.x, y, &line[..name_bytes], name_style);
                if line.len() > prefix.len() {
                    ctx.buf
                        .set_string(area.x + prefix_cols, y, &line[prefix.len()..], dim_style);
                }
            } else {
                ctx.buf.set_string(area.x, y, line, dim_style);
            }
        }
        paint_truncation_marker_if_set(ctx);
        Sigil::new("→ ", Style::default().fg(Color::Yellow))
    }
}

fn tailed_status(header: &TailedHeader) -> (String, Style) {
    match header {
        TailedHeader::Shell { state, .. } => match state {
            ShellState::Running => status_prefix("…", StatusTone::Pending),
            ShellState::Success => status_prefix("ok", StatusTone::Success),
            ShellState::Exit(n) => status_prefix(&format!("exit {n}"), StatusTone::Failure),
        },
        TailedHeader::Reasoning { state } => match state {
            ReasoningState::Streaming => status_prefix("thinking…", StatusTone::Pending),
            ReasoningState::Done => status_prefix("thought", StatusTone::Settled),
        },
    }
}

/// History row that holds raw, pre-formatted lines and renders them
/// verbatim (no kind prefix, no re-wrap). Used for banner rows, usage
/// summaries, error / approval messages — anything not driven by the
/// runtime's [`WireBlockKind`] vocabulary that still wants to live in
/// the container's scrollback. `style` paints the whole block uniformly;
/// ANSI variants only by convention (RGB stays available for future
/// syntax-highlighted block types).
///
/// `RawBlock` does not derive serde — ratatui's `Style` doesn't ship
/// `Serialize` in our build, and `RawBlock` content is UI-side state
/// (banners, error overlays) that never round-trips through scrollback
/// persistence anyway.
pub struct RawBlock {
    pub lines: Vec<String>,
    pub style: Style,
}

impl RawBlock {
    pub fn single_styled(line: String, style: Style) -> Self {
        Self {
            lines: vec![line],
            style,
        }
    }
}

impl Input for RawBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> EventOutcome {
        EventOutcome::Pass
    }
}

impl Block for RawBlock {
    /// One-shot block — banners and error overlays arrive fully
    /// formed; the container can promote them straight to `safe`.
    fn safe_on_push(&self) -> bool {
        true
    }

    fn measure(&self, _ctx: &BlockMeasureContext<'_>) -> u16 {
        self.lines.len() as u16
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
        let src_y = ctx.src_y;
        let area = ctx.area;
        for (i, line) in self.lines.iter().enumerate() {
            let i = i as u16;
            if i < src_y {
                continue;
            }
            let dst_row = i - src_y;
            if dst_row >= area.height {
                break;
            }
            ctx.buf
                .set_string(area.x, area.y + dst_row, line, self.style);
        }
        paint_truncation_marker_if_set(ctx);
        Sigil::blank()
    }
}

/// Overlay the dim "⋯ truncated ⋯" indicator on the bottom row of
/// `ctx.area` when the container flagged this entry as truncated.
fn paint_truncation_marker_if_set(ctx: &mut BlockRenderContext<'_>) {
    if !ctx.truncated || ctx.area.height == 0 || ctx.area.width == 0 {
        return;
    }
    let y = ctx.area.y + ctx.area.height - 1;
    let marker = "  ⋯ truncated ⋯";
    ctx.buf
        .set_string(ctx.area.x, y, marker, Style::default().fg(Color::DarkGray));
}

/// Linear row writer used by the diff block — its iteration emits
/// already-wrapped lines without keeping an explicit row index. Walks
/// `ctx.src_y` for the skip phase, then `ctx.area.height` for the
/// emit phase, transparently for the caller.
struct RowWriter<'a, 'b> {
    ctx: &'a mut BlockRenderContext<'b>,
    src_idx: u16,
    finished: bool,
}

impl<'a, 'b> RowWriter<'a, 'b> {
    fn new(ctx: &'a mut BlockRenderContext<'b>) -> Self {
        Self {
            ctx,
            src_idx: 0,
            finished: false,
        }
    }

    /// Write `line` with `style`. Returns `true` if the line was
    /// emitted, `false` if it was skipped (above src_y) or dropped
    /// (below area.height).
    fn write_styled(&mut self, line: &str, style: Style) -> bool {
        let i = self.src_idx;
        self.src_idx = self.src_idx.saturating_add(1);
        if i < self.ctx.src_y {
            return false;
        }
        let dst_row = i - self.ctx.src_y;
        if dst_row >= self.ctx.area.height {
            self.finished = true;
            return false;
        }
        let area = self.ctx.area;
        self.ctx
            .buf
            .set_string(area.x, area.y + dst_row, line, style);
        let w = display_width(line) as u16;
        if w < area.width {
            self.ctx.buf.set_string(
                area.x + w,
                area.y + dst_row,
                " ".repeat((area.width - w) as usize),
                style,
            );
        }
        true
    }

    fn finished(&self) -> bool {
        self.finished
    }
}

/// Sigil for a [`LabelledBlock`] of the given wire kind. The renderer
/// reserves the 2-col gutter; this just declares what glyph (if any)
/// belongs there.
pub fn sigil_for(kind: &WireBlockKind) -> Sigil {
    match kind {
        WireBlockKind::Text {
            source: Source::User,
        } => Sigil::new("> ", Style::default()),
        WireBlockKind::Text {
            source: Source::Assistant,
        } => Sigil::new("◆ ", Style::default()),
        WireBlockKind::Text {
            source: Source::Internal,
        } => Sigil::blank(),
        // The `detail`-bearing variant routes through `ToolUseBlock`; the
        // `LabelledBlock` path only sees plain tool-use markers (no detail).
        WireBlockKind::ToolUse { .. } => Sigil::new("→ ", Style::default().fg(Color::Yellow)),
        WireBlockKind::Tailed { .. } => Sigil::blank(),
        WireBlockKind::Diff { .. } => Sigil::blank(),
    }
}

/// Wrap `text` into rows at `max_width` columns, emitting one
/// byte-[`Range`] per row into `sink`. `lead_width` is the column cost
/// of a prefix that occupies the start of the first row (the prefix
/// bytes are *not* part of `text`, so they never appear in a range); it
/// only shifts where the first break falls. Continuation rows start at
/// column 0.
///
/// Always emits at least one range (`0..0` for empty `text`), matching
/// the "one blank row" floor the string builders rely on.
/// An [`Extend`] sink that shifts each row range by `base` before
/// forwarding it to `out`. Lets [`wrap_rows`] (which emits ranges
/// relative to the slice it was given) write ranges relative to a larger
/// backing string — e.g. wrapping one `\n`-separated line but recording
/// the rows as ranges into the whole body.
struct OffsetSink<'a> {
    out: &'a mut Vec<Range<usize>>,
    base: usize,
}

impl Extend<Range<usize>> for OffsetSink<'_> {
    fn extend<I: IntoIterator<Item = Range<usize>>>(&mut self, iter: I) {
        let base = self.base;
        self.out
            .extend(iter.into_iter().map(|r| base + r.start..base + r.end));
    }
}

/// Byte-ranges of `text` split on `\n`, the range analogue of
/// `text.split('\n').collect()` (no trailing-empty stripping). Always
/// yields at least one range.
fn line_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (nl, _) in text.match_indices('\n') {
        ranges.push(start..nl);
        start = nl + 1;
    }
    ranges.push(start..text.len());
    ranges
}

/// Wrapped row-ranges for a labelled-block body at `width`. The body is
/// trimmed of trailing `\n` (a closing newline shouldn't paint a blank
/// row), then each `\n`-separated line is wrapped; embedded blank lines
/// survive as real paragraph breaks. Ranges index into `text`. Always
/// non-empty — empty/blank input yields a single `0..0` row.
fn body_line_ranges(text: &str, width: u16) -> Vec<Range<usize>> {
    let max = width.max(1) as usize;
    let text = text.trim_end_matches('\n');
    let mut rows = Vec::new();
    for line in line_ranges(text) {
        let mut sink = OffsetSink {
            out: &mut rows,
            base: line.start,
        };
        wrap_rows(&text[line.clone()], 0, max, &mut sink);
    }
    rows
}

fn wrap_rows(
    text: &str,
    lead_width: usize,
    max_width: usize,
    sink: &mut impl Extend<Range<usize>>,
) {
    let mut start = 0;
    // Width of the row so far, including `lead_width` on the first row.
    // `current_width > 0` doubles as "this row has visible content": a
    // break is only taken once the row holds something, so we never emit
    // an empty row before consuming at least one character.
    let mut current_width = lead_width;

    for (idx, ch) in text.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + w > max_width && current_width > 0 {
            sink.extend(std::iter::once(start..idx));
            start = idx;
            current_width = 0;
        }
        current_width += w;
    }
    sink.extend(std::iter::once(start..text.len()));
}

/// Wrap `text` to owned `String` rows, prepending `lead` to the first
/// row. The render paths use this; `measure` uses [`wrap_rows`] with a
/// [`CountingSink`] instead.
fn wrap_into(lead: &str, text: &str, max_width: usize, out: &mut Vec<String>) {
    let mut rows = Vec::new();
    wrap_rows(text, display_width(lead), max_width, &mut rows);
    for (i, row) in rows.iter().enumerate() {
        let mut s = if i == 0 {
            String::from(lead)
        } else {
            String::new()
        };
        s.push_str(&text[row.clone()]);
        out.push(s);
    }
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use frances_session::events::DiffLine;

    /// Materialise [`body_line_ranges`] back to the rows it describes —
    /// the labelled-block body the cache stores and `render` slices.
    fn wrap_body(text: &str, width: u16) -> Vec<&str> {
        body_line_ranges(text, width)
            .iter()
            .map(|r| &text[r.clone()])
            .collect()
    }

    #[test]
    fn no_trailing_newline_is_unchanged() {
        assert_eq!(wrap_body("Hello", 80), vec!["Hello"]);
    }

    #[test]
    fn single_trailing_newline_is_stripped() {
        assert_eq!(
            wrap_body("Hello\n", 80),
            vec!["Hello"],
            "trailing `\\n` should not produce a blank continuation row"
        );
    }

    #[test]
    fn multiple_trailing_newlines_are_stripped() {
        assert_eq!(wrap_body("Hello\n\n\n", 80), vec!["Hello"]);
    }

    #[test]
    fn mid_text_paragraph_break_is_preserved() {
        assert_eq!(
            wrap_body("One\n\nTwo", 80),
            vec!["One", "", "Two"],
            "an internal `\\n\\n` is a real paragraph break and stays"
        );
    }

    #[test]
    fn mid_text_paragraph_break_with_trailing_newline_keeps_only_the_break() {
        assert_eq!(wrap_body("One\n\nTwo\n", 80), vec!["One", "", "Two"]);
    }

    #[test]
    fn newline_only_text_collapses_to_one_blank_row() {
        assert_eq!(wrap_body("\n", 80), vec![""]);
    }

    #[test]
    fn empty_text_yields_one_blank_row() {
        assert_eq!(wrap_body("", 80), vec![""]);
    }

    #[test]
    fn sigil_for_each_text_source() {
        assert_eq!(
            sigil_for(&WireBlockKind::Text {
                source: Source::User,
            })
            .text,
            "> ",
        );
        assert_eq!(
            sigil_for(&WireBlockKind::Text {
                source: Source::Assistant,
            })
            .text,
            "◆ ",
        );
        assert_eq!(
            sigil_for(&WireBlockKind::Text {
                source: Source::Internal,
            })
            .text,
            "",
        );
    }

    #[test]
    fn diff_display_line_reserves_sign_and_line_number_gutters() {
        let context = diff_display_line(&DiffLine::Context {
            text: "context".into(),
            line: 12,
        });
        let added = diff_display_line(&DiffLine::Added("added".into()));
        let removed = diff_display_line(&DiffLine::Removed("removed".into()));

        assert_eq!(context.text, "  12 context");
        assert_eq!(added.text, "+    added");
        assert_eq!(removed.text, "-    removed");

        let content_start = DIFF_LINE_NUMBER_WIDTH + 1;
        assert_eq!(&context.text[content_start..], "context");
        assert_eq!(&added.text[content_start..], "added");
        assert_eq!(&removed.text[content_start..], "removed");

        assert_eq!(context.style, Style::default());
        assert_eq!(
            added.style,
            Style::default().bg(Color::Green).fg(Color::Black)
        );
        assert_eq!(
            removed.style,
            Style::default().bg(Color::Red).fg(Color::Black)
        );
    }

    /// One round-trip-serde test per serializable block variant.
    mod serde_roundtrip {
        use super::*;

        fn round_trip<T: serde::Serialize + for<'de> serde::Deserialize<'de>>(value: &T) -> T {
            let s = serde_json::to_string(value).expect("serialize");
            serde_json::from_str(&s).expect("deserialize")
        }

        #[test]
        fn diff_block() {
            let b = DiffBlock::new(vec![
                DiffLine::Context {
                    text: "ctx".into(),
                    line: 7,
                },
                DiffLine::Added("added".into()),
                DiffLine::Removed("removed".into()),
            ]);
            let r = round_trip(&b);
            assert_eq!(r.lines.len(), 3);
        }

        #[test]
        fn labelled_block_text() {
            let b = LabelledBlock::new(
                WireBlockKind::Text {
                    source: Source::Assistant,
                },
                "hello".into(),
            );
            let r = round_trip(&b);
            assert_eq!(r.text, "hello");
            match r.kind {
                WireBlockKind::Text { source } => assert_eq!(source, Source::Assistant),
                _ => panic!("kind not preserved"),
            }
        }

        #[test]
        fn tailed_shell_block() {
            let b = TailedBlock::new(
                TailedHeader::Shell {
                    state: ShellState::Success,
                    cmd: "ls".into(),
                },
                "out".into(),
            );
            let r = round_trip(&b);
            assert_eq!(r.text, "out");
            match r.header {
                TailedHeader::Shell { state, cmd } => {
                    assert_eq!(&*cmd, "ls");
                    assert!(matches!(state, ShellState::Success));
                }
                other => panic!("unexpected header: {other:?}"),
            }
        }

        #[test]
        fn tailed_reasoning_block() {
            let b = TailedBlock::new(
                TailedHeader::Reasoning {
                    state: ReasoningState::Streaming,
                },
                "thinking…".into(),
            );
            let r = round_trip(&b);
            assert_eq!(r.text, "thinking…");
            assert!(matches!(
                r.header,
                TailedHeader::Reasoning {
                    state: ReasoningState::Streaming,
                }
            ));
        }

        #[test]
        fn tool_use_block() {
            let b = ToolUseBlock::new("shell".into(), "ls -la".into());
            let r = round_trip(&b);
            assert_eq!(&*r.name, "shell");
            assert_eq!(&*r.detail, "ls -la");
        }
    }

    mod shell_scroll {
        use super::*;
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        use frances_tui::widget::{EventContext, EventOutcome, Focus, Theme};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        fn block_with_lines(n: usize) -> TailedBlock {
            let body: String = (0..n).map(|i| format!("line{}\n", i + 1)).collect();
            TailedBlock::new(
                TailedHeader::Shell {
                    state: ShellState::Success,
                    cmd: "cmd".into(),
                },
                body,
            )
        }

        fn press(c: char) -> Event {
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: crossterm::event::KeyEventState::NONE,
            })
        }

        fn dispatch(block: &mut TailedBlock, event: &Event) -> EventOutcome {
            let mut focus = Focus::new();
            let mut redraw = false;
            let mut ctx = EventContext {
                focus: &mut focus,
                redraw: &mut redraw,
            };
            block.handle_event(&mut ctx, event)
        }

        #[test]
        fn j_decrements_scroll_y() {
            let mut b = block_with_lines(30);
            b.scroll_y = 5;
            assert!(matches!(
                dispatch(&mut b, &press('j')),
                EventOutcome::Consumed
            ));
            assert_eq!(b.scroll_y, 4);
        }

        #[test]
        fn j_at_zero_is_noop() {
            let mut b = block_with_lines(30);
            assert_eq!(b.scroll_y, 0);
            dispatch(&mut b, &press('j'));
            assert_eq!(b.scroll_y, 0);
        }

        #[test]
        fn k_clamps_at_source_minus_focused_tail() {
            // 25 lines, focused tail = 20 → max_scroll = 5.
            let mut b = block_with_lines(25);
            for _ in 0..20 {
                dispatch(&mut b, &press('k'));
            }
            assert_eq!(b.scroll_y, 5);
        }

        #[test]
        fn d_u_step_by_half_focused_tail() {
            // 40 lines, focused tail = 20 → max_scroll = 20, half = 10.
            let mut b = block_with_lines(40);
            dispatch(&mut b, &press('u'));
            assert_eq!(b.scroll_y, 10);
            dispatch(&mut b, &press('u'));
            assert_eq!(b.scroll_y, 20);
            dispatch(&mut b, &press('u'));
            assert_eq!(b.scroll_y, 20, "u saturates at max_scroll");
            dispatch(&mut b, &press('d'));
            assert_eq!(b.scroll_y, 10);
            dispatch(&mut b, &press('d'));
            assert_eq!(b.scroll_y, 0);
            dispatch(&mut b, &press('d'));
            assert_eq!(b.scroll_y, 0, "d saturates at zero");
        }

        #[test]
        fn body_lines_shift_with_scroll_y() {
            let b0 = block_with_lines(30);
            let mut b5 = block_with_lines(30);
            b5.scroll_y = 5;
            let lines0 = b0.body_rows(80, 0, TAIL_LINES);
            let lines5 = b5.body_rows(80, 5, TAIL_LINES);
            // Both runs have 1 marker + TAIL_LINES body rows.
            assert_eq!(lines0.len(), 1 + TAIL_LINES);
            assert_eq!(lines5.len(), 1 + TAIL_LINES);
            assert_eq!(&*lines0[0], "… [20 earlier lines]");
            assert_eq!(&*lines5[0], "… [15 earlier lines]");
            // Window shifts back by 5 source lines.
            assert_eq!(&*lines0[1], "line21");
            assert_eq!(&*lines5[1], "line16");
        }

        #[test]
        fn live_view_always_shows_tail_regardless_of_scroll_y() {
            let mut b = block_with_lines(30);
            b.scroll_y = 7;
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
            let theme = Theme::default();
            let mut ctx = BlockRenderContext {
                area: Rect::new(0, 0, 40, 12),
                buf: &mut buf,
                src_y: 0,
                truncated: false,
                alt_view: false,
                selected: false,
                selected_part: None,
                theme: &theme,
                frame_time: &frances_tui::FixedFrameTime(0.0),
            };
            b.render(&mut ctx);
            // First body row = first row after the header (1 row).
            // Live view forces window_start = 0 → marker "20 earlier lines".
            let marker_row: String = (0..40)
                .map(|x| buf[(x, 1)].symbol().chars().next().unwrap_or(' '))
                .collect();
            assert!(
                marker_row.starts_with("… [20 earlier lines]"),
                "live view ignored scroll_y; got marker row: {marker_row:?}",
            );
        }

        #[test]
        fn alt_view_honours_scroll_y() {
            let mut b = block_with_lines(30);
            b.scroll_y = 7;
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
            let theme = Theme::default();
            let mut ctx = BlockRenderContext {
                area: Rect::new(0, 0, 40, 12),
                buf: &mut buf,
                src_y: 0,
                truncated: false,
                alt_view: true,
                selected: false,
                selected_part: None,
                theme: &theme,
                frame_time: &frances_tui::FixedFrameTime(0.0),
            };
            b.render(&mut ctx);
            let marker_row: String = (0..40)
                .map(|x| buf[(x, 1)].symbol().chars().next().unwrap_or(' '))
                .collect();
            assert!(
                marker_row.starts_with("… [13 earlier lines]"),
                "alt view ignored scroll_y; got marker row: {marker_row:?}",
            );
        }
    }

    /// `wrap_rows` (the allocation-free measure core) and `wrap_into`
    /// (the render path) must agree: same row count, and the ranges must
    /// reconstruct the materialised strings.
    mod wrap_equivalence {
        use super::*;

        const CASES: &[(&str, &str)] = &[
            ("", ""),
            ("", "short"),
            ("", "a string that is definitely wider than the wrap width"),
            ("", "exact"),
            ("→ ", "tool detail that wraps across several rows for sure"),
            (
                "… ",
                "wide  →  glyphs … and ✓ marks mixed in with ascii text here",
            ),
            ("prefix ", ""),
            ("", "trailing space test "),
        ];

        fn rows(text: &str, lead_width: usize, max: usize) -> Vec<Range<usize>> {
            let mut out = Vec::new();
            wrap_rows(text, lead_width, max, &mut out);
            out
        }

        #[test]
        fn count_matches_wrap_into() {
            for &(lead, text) in CASES {
                for max in [1usize, 3, 8, 20, 200] {
                    let lead_width = display_width(lead);
                    let ranges = rows(text, lead_width, max);
                    let mut materialised = Vec::new();
                    wrap_into(lead, text, max, &mut materialised);
                    assert_eq!(
                        ranges.len(),
                        materialised.len(),
                        "row count diverged for lead={lead:?} text={text:?} max={max}",
                    );
                }
            }
        }

        #[test]
        fn ranges_reconstruct_wrap_into_strings() {
            for &(lead, text) in CASES {
                for max in [1usize, 3, 8, 20, 200] {
                    let lead_width = display_width(lead);
                    let ranges = rows(text, lead_width, max);
                    let mut materialised = Vec::new();
                    wrap_into(lead, text, max, &mut materialised);
                    let rebuilt: Vec<String> = ranges
                        .iter()
                        .enumerate()
                        .map(|(i, r)| {
                            let mut s = if i == 0 {
                                String::from(lead)
                            } else {
                                String::new()
                            };
                            s.push_str(&text[r.clone()]);
                            s
                        })
                        .collect();
                    assert_eq!(
                        rebuilt, materialised,
                        "ranges didn't reconstruct rows for lead={lead:?} text={text:?} max={max}",
                    );
                }
            }
        }
    }

    /// Each block's `measure` (count helpers) must equal the length of
    /// the rows its `render` path materialises, at the same width.
    mod measure_matches_render {
        use super::*;

        fn mctx(width: u16) -> BlockMeasureContext<'static> {
            // Theme is never read during measure today; leak one default
            // so the context can borrow a `'static`.
            let theme: &'static frances_tui::widget::Theme =
                Box::leak(Box::new(frances_tui::widget::Theme::default()));
            BlockMeasureContext {
                width,
                selected: false,
                selected_part: None,
                theme,
            }
        }

        #[test]
        fn labelled_block() {
            let text = "a longish assistant reply\nwith two paragraphs that each wrap a bit";
            let b = LabelledBlock::new(
                WireBlockKind::Text {
                    source: Source::Assistant,
                },
                text.into(),
            );
            for width in [4u16, 12, 30, 200] {
                assert_eq!(
                    b.measure(&mctx(width)) as usize,
                    wrap_body(b.body_text(), width).len(),
                    "width {width}",
                );
            }
        }

        #[test]
        fn tool_use_block() {
            let b = ToolUseBlock::new(
                "shell".into(),
                "a detail line\nand a second one that is long enough to wrap somewhere".into(),
            );
            for width in [4u16, 12, 30, 200] {
                assert_eq!(
                    b.measure(&mctx(width)) as usize,
                    b.wrapped_lines(width).len(),
                    "width {width}",
                );
            }
        }

        #[test]
        fn tailed_block_short_and_long() {
            for n in [3usize, 12, 40] {
                let body: String = (0..n).map(|i| format!("output line {}\n", i + 1)).collect();
                let b = TailedBlock::new(
                    TailedHeader::Shell {
                        state: ShellState::Success,
                        cmd: "build with a fairly long command line that wraps".into(),
                    },
                    body,
                );
                for width in [6u16, 20, 200] {
                    let tail = tail_lines_for(false);
                    let rendered = b.header_lines(width).len() + b.body_rows(width, 0, tail).len();
                    assert_eq!(
                        b.measure(&mctx(width)) as usize,
                        rendered,
                        "n={n} width={width}"
                    );
                }
            }
        }

        #[test]
        fn diff_block() {
            let b = DiffBlock::new(vec![
                frances_session::events::DiffLine::Context {
                    text: "a context line that is long enough to wrap at small widths".into(),
                    line: 12,
                },
                frances_session::events::DiffLine::Added("added".into()),
            ]);
            for width in [6u16, 20, 200] {
                // Mirror render's per-line wrap to count materialised rows.
                let max = width.max(1) as usize;
                let mut rendered = 0usize;
                for line in &b.lines {
                    let content = diff_display_line(line).text;
                    let mut out = Vec::new();
                    wrap_into("", &content, max, &mut out);
                    rendered += out.len();
                }
                assert_eq!(b.measure(&mctx(width)) as usize, rendered, "width {width}");
            }
        }

        /// The labelled-block row cache is keyed on width: re-measuring at
        /// a previously-seen width after a different one must recompute,
        /// not return the stale narrow/wide layout.
        #[test]
        fn labelled_cache_recomputes_on_width_change() {
            let text = "a reply long enough that the wrap width clearly changes the row count";
            let b = LabelledBlock::new(
                WireBlockKind::Text {
                    source: Source::Assistant,
                },
                text.into(),
            );
            let wide = b.measure(&mctx(200));
            let narrow = b.measure(&mctx(12));
            assert!(narrow > wide, "narrower width should wrap to more rows");
            assert_eq!(b.measure(&mctx(200)), wide);
            assert_eq!(b.measure(&mctx(12)), narrow);
        }
    }
}
