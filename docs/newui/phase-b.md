# Phase B — widget framework

> **Sketch.** Detailed mechanics get filled in once Phase A is landed
> and we can read concrete call-site shapes.

## Goal

Stand up our own widget hierarchy on top of taffy. The footer becomes
a `Border ▸ VStack ▸ (Input, StatusRow)` tree. Layout and focus are
first-class.

## Trait split

```rust
pub trait Input {
    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event)
        -> EventOutcome;
}

pub trait Widget: Input {
    fn measure(&self, ctx: &MeasureContext) -> taffy::Size<f32>;
    fn layout(&mut self, ctx: &mut LayoutContext, rect: Rect);
    fn render(&self, ctx: &mut RenderContext);
}
```

`Input` is split out so blocks (Phase C) can reuse the event surface
without becoming widgets.

## Contexts (to flesh out)

- `MeasureContext` — terminal size, theme, taffy node-tree handle.
- `LayoutContext` — resolved `Rect` for this widget + children.
- `RenderContext` — target `Buffer`, theme, focus state, frame counter.
- `EventContext` — event queue, focus router, "redraw needed" signal.

Exact field set gets enumerated from the call sites that today take
ad-hoc args (`terminal_h`, `width`, `cursor`, …).

## Widgets

Containers:
- `Border { child, style, title }`
- `VStack { children, gap }`
- `HStack { children, gap }`
- `Flex` (taffy-driven)
- `Grid` (taffy-driven)

Primitives:
- `TextLine` (single styled line — the StatusRow lives here).
- `Input` (thin wrapper around `ratatui_textarea::TextArea`).
- `Spinner`, `ProgressBar` later.

## Focus

Framework-level concern, not a container widget. There is no
`FocusGroup`.

- `EventContext` carries a `FocusPath` + a `FocusManager` handle.
- Containers route events to their focused child based on the path.
- `FocusManager` exposes `focus_next`, `focus_prev`, `focus_set(path)`.
- Containers expose `focusable_children(&self) -> Vec<FocusPath>` for
  traversal; default is empty.

A Flex with multiple focusable children IS the focus group — no
separate utility needed.

## Integration with Phase A

Footer slot becomes `Box<dyn Widget>`. The container's `measure` of
its row budget uses the widget's 2D `measure` and reads `.height`.
The `MeasuredWidgetRef` shim from Phase A goes away.

## Files (anticipated)

- `crates/frances-tui/src/widget/mod.rs` + per-widget files.
- `crates/frances-tui/src/input.rs` — the `Input` trait + `EventContext`.
- `crates/frances-tui/src/context.rs` — Measure / Layout / Render.
- `crates/frances-tui/Cargo.toml` — add `taffy`.
- `crates/frances/src/tui/blocks.rs` — `FooterBlock` becomes a widget
  tree.

## Open items

- Exact field set of each context.
- `FocusPath` shape — `Vec<u16>` of child indices, or a flat handle
  into a side-table?
