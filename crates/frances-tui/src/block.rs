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
//! Blocks are not persisted directly. The transcript is persisted at
//! the section level (see [`crate::section`]); the TUI's section
//! dispatcher reconstructs the inner blocks on replay by running each
//! section's `apply` over the persisted append-stream. If a `Block`
//! impl needs replay-stable state beyond what the section gives it,
//! the section should hold that state itself.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::widget::{FrameTime, Input, Theme};

/// Container-reserved left gutter width, in columns. Every block's
/// rendered content lives at `area.x + SIGIL_WIDTH..area.x + area.width`;
/// the container paints the block's [`Block::sigil`] into the first
/// `SIGIL_WIDTH` cells of the block's topmost row.
pub const SIGIL_WIDTH: u16 = 2;

/// A glyph painted by the container into the left gutter at the
/// block's first on-screen row. The container always reserves
/// [`SIGIL_WIDTH`] cells regardless of `text`'s display width — `text`
/// is just what (if anything) gets drawn at column 0 of the gutter.
/// Blank == empty string, not whitespace; the gutter is reserved by
/// the container, not by the sigil's content.
#[derive(Debug, Clone, Default)]
pub struct Sigil {
    pub text: String,
    pub style: Style,
}

impl Sigil {
    /// No glyph — the container's reserved gutter stays empty for this
    /// block's first row.
    pub fn blank() -> Self {
        Self::default()
    }

    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// Inputs threaded through [`Block::measure`].
///
/// `selected` is `true` when this block is the alt-view inspector's
/// current selection. Blocks may grow (return a larger row count) in
/// that state to reveal more content while focused — `ShellOutputBlock`
/// uses this to expand its body tail when the user has chosen it.
/// Outside the inspector, callers pass `false`.
///
/// `selected_part` is meaningful only to composite blocks (a section
/// view, see [`Block::parts`]): the index of the selected inner block,
/// or `None`. Leaf blocks ignore it and read `selected`.
pub struct BlockMeasureContext<'a> {
    pub width: u16,
    pub selected: bool,
    pub selected_part: Option<usize>,
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
    /// Index of the selected inner block, for composite blocks (a
    /// section view, see [`Block::parts`]). `None` outside the inspector
    /// or when the selection isn't inside this block. Leaf blocks ignore
    /// it and read `selected`. Mirrored from
    /// [`BlockMeasureContext::selected_part`].
    pub selected_part: Option<usize>,
    pub theme: &'a Theme,
    /// Animation clock for renderables that paint a moving glyph (e.g.
    /// a live section's streaming indicator). Frame index at 60fps; the
    /// epoch is arbitrary, only deltas matter. The container marks
    /// animated entries damaged every frame so this advances.
    pub frame_time: &'a dyn FrameTime,
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

    /// The block's addressable inner blocks, for the inspector's
    /// per-block selection and input routing. A composite block (a
    /// section view) returns its children, in display order; a leaf
    /// block returns an empty slice, meaning "I am my own selectable
    /// unit." The container flattens these into the flat selection
    /// ordinal so behaviours that differ per block — e.g.
    /// `ShellOutputBlock`'s scroll — reach the right block.
    fn parts(&self) -> &[Box<dyn Block>] {
        &[]
    }

    /// Mutable sibling of [`Self::parts`] for input dispatch. Same
    /// contract: composite blocks return their children, leaf blocks
    /// return an empty slice.
    fn parts_mut(&mut self) -> &mut [Box<dyn Block>] {
        &mut []
    }

    /// Total rendered row count if wrapped at `ctx.width`. Note that
    /// `ctx.width` is already the *body* width — the container has
    /// deducted [`SIGIL_WIDTH`] for the left gutter, so blocks compute
    /// wrap exclusively against the body region. Must be deterministic
    /// for a given context — the container caches layout decisions on
    /// this.
    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16;

    /// Paint the block's body into `ctx.area` (which already excludes
    /// the container's left gutter), and return the [`Sigil`] the
    /// container should paint into the gutter at the block's topmost
    /// row. Returning the sigil from `render` (instead of a separate
    /// method) keeps the body content and its sigil in lockstep — they
    /// can't drift on streaming updates.
    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil;
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
    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        // line_count requires the `unstable-rendered-line-info` cargo
        // feature, which is enabled in our Cargo.toml.
        self.line_count(ctx.width) as u16
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
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
        Sigil::blank()
    }
}
