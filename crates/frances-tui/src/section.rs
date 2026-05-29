//! `Section` trait + `InactiveBlock` — the live-and-sealed vs.
//! snapshotted-into-committed dual representation that drives the
//! container's per-section commit lifecycle.
//!
//! Sections are produced by workflows (via JS classes that map to
//! [`frances_models_tui::SectionKind`] variants) and dispatched by
//! the TUI's section dispatcher (in the binary's `frances::tui`
//! module). The container holds `Box<dyn Section>` in its `active`
//! and `safe` collections; on commit (first row off-screen) the
//! container snapshots `section.blocks() + section.sigil()` into a
//! single [`InactiveBlock`], pushes it into `committed`, and drops
//! the trait object. The section's identity is gone after that.
//!
//! See `docs/plan/section-and-markdown.md` for the full lifecycle.

use ratatui::layout::Rect;

use crate::block::{Block, BlockMeasureContext, BlockRenderContext, Sigil};

pub use frances_models_tui::SectionApply;

/// A live section — owns its own list of inner [`Block`]s plus the
/// section's sigil (gutter glyph) and gap policy. The container
/// interacts with sections only through this trait; concrete impls
/// (Markdown, ShellOutput, Reasoning, ToolUse, Diff, Json, Error)
/// live in `frances-markdown` and the binary's `tui::sections`
/// module.
pub trait Section: 'static {
    /// Apply a post-construction event. Construction is handled by
    /// the dispatcher's `make_section` factory; the first
    /// `SectionAppend` with a new id triggers construction, and the
    /// same Append is dispatched to `apply` so the section absorbs
    /// the initial delta uniformly.
    fn apply(&mut self, event: SectionApply<'_>);

    /// Inner blocks in display order. Borrowed; lifetime tied to
    /// `&self`. Most impls return a single-element slice; the
    /// MarkdownSection returns one element per paragraph.
    fn blocks(&self) -> &[Box<dyn Block>];

    /// Glyph painted by the container in the left gutter at this
    /// section's first on-screen row. Inner blocks inherit the
    /// section's sigil — their own `Block::render` sigil return is
    /// ignored when they're rendered as part of a section.
    fn sigil(&self) -> Sigil;
}

/// Snapshot of a sealed section's inner blocks. Produced when a
/// `Box<dyn Section>` transitions safe → committed: the container
/// takes the section's current `blocks() + sigil()`, packs them into
/// one `InactiveBlock`, and drops the trait object. From the
/// container's perspective the entry is now a single `Box<dyn Block>`
/// living in `committed`, paintable by the alt-view inspector for the
/// rest of the session.
pub struct InactiveBlock {
    blocks: Vec<Box<dyn Block>>,
    sigil: Sigil,
}

impl InactiveBlock {
    pub fn new(blocks: Vec<Box<dyn Block>>, sigil: Sigil) -> Self {
        Self { blocks, sigil }
    }
}

impl crate::widget::Input for InactiveBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut crate::widget::EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> crate::widget::EventOutcome {
        crate::widget::EventOutcome::Pass
    }
}

impl Block for InactiveBlock {
    fn safe_on_push(&self) -> bool {
        true
    }

    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        self.blocks
            .iter()
            .map(|b| b.measure(ctx))
            .fold(0u16, u16::saturating_add)
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
        let measure_ctx = BlockMeasureContext {
            width: ctx.area.width,
            selected: ctx.selected,
            theme: ctx.theme,
        };
        let src_window_top = ctx.src_y;
        let src_window_bot = ctx.src_y.saturating_add(ctx.area.height);
        let mut consumed: u16 = 0;
        let mut painted: u16 = 0;
        for block in &self.blocks {
            let h = block.measure(&measure_ctx);
            if h == 0 {
                continue;
            }
            let block_top = consumed;
            let block_bot = consumed.saturating_add(h);
            consumed = block_bot;
            let overlap_top = block_top.max(src_window_top);
            let overlap_bot = block_bot.min(src_window_bot);
            if overlap_top >= overlap_bot {
                continue;
            }
            let block_src_y = overlap_top - block_top;
            let block_height = overlap_bot - overlap_top;
            let block_area = Rect::new(
                ctx.area.x,
                ctx.area.y + painted,
                ctx.area.width,
                block_height,
            );
            painted = painted.saturating_add(block_height);
            let mut child_ctx = BlockRenderContext {
                area: block_area,
                buf: &mut *ctx.buf,
                src_y: block_src_y,
                truncated: ctx.truncated,
                alt_view: ctx.alt_view,
                selected: ctx.selected,
                theme: ctx.theme,
            };
            let _ = block.render(&mut child_ctx);
        }
        self.sigil.clone()
    }
}
