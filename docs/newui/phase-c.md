# Phase C — Block contract refinement

> **Sketch.** Lands after Phase B because the trait reuses Phase B's
> `Input` + `EventContext`.

## Goal

Promote `Block` from "ad-hoc trait object" to a richer contract that
supports persistence, type-level declarations of behaviour, and
reuses the framework's event hook.

## Trait surface

```rust
#[derive(Serialize, Deserialize)]
pub enum BlockKind { Text, ToolUse, ShellOutput, Diff, Raw }

pub trait Block: Input + Serialize + DeserializeOwned + 'static {
    /// True when an instance of this block always arrives complete
    /// on push — no streaming deltas to follow. The container
    /// promotes it straight to `safe` without waiting for
    /// `mark_safe`. Defaults to false (streaming).
    fn safe_on_push(&self) -> bool { false }

    fn kind(&self) -> BlockKind;
    fn measure(&self, ctx: &BlockMeasureContext) -> u16;
    fn render(&self, ctx: &mut BlockRenderContext);
}
```

## `safe_on_push`

Replaces the runtime decision in `crates/frances/src/ui.rs` (~lines
117–127) where the caller flags `ToolUse` blocks safe-to-commit
immediately because "no streaming body — `ToolUse` is emitted as
`BlockDelta` + `BlockStop` back-to-back". That property belongs to
the block type, not the caller.

Concrete overrides:
- `ToolUseBlock` returns `true` (one-shot).
- `RawBlock` returns `true` (banner / status, never streams).
- `DiffBlock`, `LabelledBlock` (Text), `ShellOutputBlock` keep default
  `false`.

The container's `push_active` calls `block.safe_on_push()` and either
auto-`mark_safe`s or waits for `BlockStop` as today.

## Contexts

- `BlockMeasureContext` — width + theme.
- `BlockRenderContext` — target `Buffer` + area + `src_y` offset (for
  straddle clipping) + `truncated: bool` flag + theme.

## Clipping

No `ClipDirection`. Each block decides clipping inside `render` given
`area.height` + `src_y`. A shell output block can clip its tail; a
diff block can clip its head; the trait does not constrain.

## Truncation

No `BlockKind::Truncated` and no `TruncatedBlock` wrapper. Truncation
is the `truncated: bool` flag on `BlockRenderContext`, set by the
container when a block was dehydrated mid-stream. Each block decides
how — and whether — to represent incomplete content. The trailing
"⋯ truncated ⋯" indicator becomes one specific block's choice.

## Persistence

`BlockKind` is a closed enum mirroring what the runtime emits — gives
us trivial enum-tagged serde. If we later need extensibility, swap to
typetag.

## Event handling

Reuses Phase B's `Input` trait. Phase C ships a no-op default; real
handlers come in Phase D.

## Files

- `crates/frances-tui/src/block.rs` — rewrite the trait; delete
  `TruncatedBlock`.
- `crates/frances-tui/src/context.rs` — block contexts.
- `crates/frances/src/tui/blocks.rs` — migrate each concrete block;
  derive serde; each that wants a truncation marker draws it inside
  its own `render`.
- `crates/frances-tui/src/scrollback_container.rs` — straddle path
  passes `src_y`; truncation path passes `truncated = true`. No
  ClipDirection or marker glyph in the container.

## Verification

- Round-trip serde test per block variant.
- Existing block render tests adapt to the new context shape.
- Manual: long shell output straddles the visible window correctly
  (top straddle: head clipped; bottom straddle: tail clipped).
