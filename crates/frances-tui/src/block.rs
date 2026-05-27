//! [`Block`] — the unit of content for the scrollback container.
//!
//! A block measures itself at a given width and paints itself into a
//! region of a `Buffer`. Both history rows above the live area and the
//! scrollback inspector use the same trait. The container knows about
//! *the shape* of a block (measurable, renderable, optionally
//! interactive) but not *the substance* — a block can be a paragraph,
//! a code listing, a shell-output tail, anything that fits the trait.
//!
//! The trait is ratatui-coupled: `Rect` and `Buffer` show up in
//! [`BlockRenderContext`]. That's a deliberate trade — we get any
//! ratatui-styled cell to participate cheaply, at the cost of needing
//! to redo this layer if we ever port to a non-ratatui frontend.
//!
//! ## Trait split
//!
//! [`Block`] is a [`crate::widget::Input`] sub-trait so the alt-view
//! inspector can dispatch events to whichever block has focus
//! (hscroll/vscroll, expand/collapse). Concrete blocks ship a no-op
//! `Input` impl until Phase D wires up per-block keymaps.
//!
//! ## Persistence
//!
//! [`BlockKind`] is a closed enum mirroring what concrete block types
//! exist in the binary. Persistence is per-variant serde on the
//! concrete blocks — `Box<dyn Block>` itself isn't `Serialize` because
//! that would block dyn dispatch (Serialize is not object-safe). The
//! binary's `block_for_kind` reconstructs concrete blocks from a wire
//! `frances_session::events::BlockKind` + text payload; the same path
//! is intended to drive scrollback restore. If we later need open-set
//! extensibility, swap to `typetag`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use crate::widget::{Input, Theme};

/// Closed tag for the concrete block types this binary ships. Returned
/// by [`Block::kind`] so persistence layers can route a `Box<dyn Block>`
/// into a tagged-enum serde wrapper without touching the trait object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockKind {
    Text,
    ToolUse,
    ShellOutput,
    Diff,
    Raw,
}

/// Inputs threaded through [`Block::measure`].
///
/// `selected` is `true` when this block is the alt-view inspector's
/// current selection. Blocks may grow (return a larger row count) in
/// that state to reveal more content while focused — `ShellOutputBlock`
/// uses this to expand its body tail when the user has chosen it.
/// Outside the inspector, callers pass `false`.
pub struct BlockMeasureContext<'a> {
    pub width: u16,
    pub selected: bool,
    pub theme: &'a Theme,
}

/// Inputs threaded through [`Block::render`].
///
/// `src_y` is the row offset inside the block's natural content where
/// painting starts — used by the scrollback inspector's straddle path
/// so blocks can render directly into the destination buffer when only
/// the bottom portion of a tall block is on screen. The block paints
/// rows in source range `[src_y, src_y + area.height)` into
/// `[area.y, area.y + area.height)` of `buf`.
///
/// `truncated` is set by the container when the block was dehydrated
/// mid-stream (replay path). The block decides how — and whether — to
/// represent that; the trailing "⋯ truncated ⋯" indicator is one block's
/// choice, not a trait-level convention.
///
/// `alt_view` is `true` when the block is being painted inside the
/// scrollback inspector (`paint_scrollback`) and `false` for every
/// live-view render path. Blocks that hold UI-only state (e.g.
/// `ShellOutputBlock::scroll_y`) consult this flag to decide whether
/// to honour the state or render the canonical "live" view.
pub struct BlockRenderContext<'a> {
    pub area: Rect,
    pub buf: &'a mut Buffer,
    pub src_y: u16,
    pub truncated: bool,
    pub alt_view: bool,
    /// `true` when this block is the alt-view inspector's current
    /// selection. Mirrored from [`BlockMeasureContext::selected`] so
    /// `measure` + `render` stay in sync — a block that returns a
    /// taller measure when selected must render a body that fills
    /// those extra rows.
    pub selected: bool,
    pub theme: &'a Theme,
}

pub trait Block: Input + 'static {
    /// True when an instance of this block always arrives complete on
    /// push — no streaming deltas to follow. The container promotes it
    /// straight to `safe` without waiting for [`mark_safe`].
    ///
    /// [`mark_safe`]: crate::ScrollbackContainer::mark_safe
    fn safe_on_push(&self) -> bool {
        false
    }

    fn kind(&self) -> BlockKind;

    /// Total rendered row count if wrapped at `ctx.width`. Must be
    /// deterministic for a given context — the container caches layout
    /// decisions on this.
    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16;

    fn render(&self, ctx: &mut BlockRenderContext<'_>);
}

/// Adapter so a `ratatui::widgets::Paragraph` can be used as a `Block`
/// directly. `Paragraph::render` consumes self upstream, so we clone —
/// Paragraph clones are cheap (owned Lines, no internal handles).
///
/// Kept for the binary's `container_scratch` example and the container
/// tests; the binary's real blocks live in `frances/src/tui/blocks.rs`.
impl Input for ratatui::widgets::Paragraph<'static> {
    fn handle_event(
        &mut self,
        _ctx: &mut crate::widget::EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> crate::widget::EventOutcome {
        crate::widget::EventOutcome::Pass
    }
}

impl Block for ratatui::widgets::Paragraph<'static> {
    fn kind(&self) -> BlockKind {
        BlockKind::Raw
    }

    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        // line_count requires the `unstable-rendered-line-info` cargo
        // feature, which is enabled in our Cargo.toml.
        self.line_count(ctx.width) as u16
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) {
        use ratatui::widgets::Widget;
        // Paragraph has no native src_y; for the straddle path the
        // container shouldn't be calling Paragraph directly. If src_y
        // is non-zero, paint shifted up by src_y so the visible window
        // lines up — ratatui clips at the buffer edge for us.
        let shifted = Rect::new(
            ctx.area.x,
            ctx.area.y.saturating_sub(ctx.src_y),
            ctx.area.width,
            ctx.area.height + ctx.src_y,
        );
        Widget::render(self.clone(), shifted, ctx.buf);
    }
}
