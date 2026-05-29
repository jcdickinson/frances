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

/// A live section — a state machine that consumes section events and
/// emits a fresh list of inner [`Block`]s on each `apply`. The
/// dispatcher (in `frances/src/ui.rs`) diffs the new list against the
/// previous and routes changes into the container's existing
/// block-level API. Concrete impls live in `frances-markdown`
/// (`MarkdownSection`) and the binary's `tui::sections` module
/// (`SingleBlockSection`).
///
/// Returning a fresh `Vec<Box<dyn Block>>` per apply is the
/// load-bearing choice: `Box<dyn Block>` isn't `Clone`, so the
/// section can't store blocks AND expose them by reference for the
/// container to render — the container would need owning copies.
/// Instead the section holds whatever state is needed to materialise
/// blocks on demand (accumulated text, paragraph offsets, etc.) and
/// rebuilds them each apply. For typical PoC payloads the cost is
/// negligible.
pub trait Section: 'static {
    /// Apply an event and return the section's full block list AFTER
    /// the event. The first call (right after construction) processes
    /// the initial `SectionApply::Append` so the section absorbs its
    /// seed delta uniformly. Returning `Vec::new()` means the section
    /// has no renderable blocks yet — the dispatcher tracks the id
    /// but pushes nothing.
    fn apply(&mut self, event: SectionApply<'_>) -> Vec<Box<dyn Block>>;

    /// Glyph painted by the container in the left gutter at this
    /// section's first on-screen row. Today the gutter sigil is
    /// painted per-block by the container; the section-level sigil
    /// is consulted only at commit time when sections snapshot into
    /// an [`InactiveBlock`] (a refinement on the post-PoC backlog).
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
