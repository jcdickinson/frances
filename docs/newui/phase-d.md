# Phase D — alt-view per-block interactivity

> **Sketch.** Builds on Phases B + C — uses the `Input` trait shared
> by widgets and blocks.

## Goal

Make blocks interactive in the alt-screen scrollback inspector. Wide
diff hscrolls; tall shell output vscrolls; focus moves between blocks
with j/k.

## Sketch

- Container holds `inspector_focus: Option<BlockId>` in alt-view mode.
- Key events in `paint_scrollback` route to the focused block's
  `Input::handle_event`.
- Block-internal scroll state lives on the block itself (e.g.
  `ShellOutputBlock { scroll_y: u16, ... }`, `DiffBlock { scroll_x:
  u16, ... }`).
- `EventContext` exposes a "redraw needed" signal so the container
  knows to repaint the alt view after an event.

## Files (anticipated)

- `crates/frances-tui/src/scrollback_container.rs` — focus state +
  alt-view key dispatch.
- `crates/frances/src/tui/blocks.rs` — scroll state + `Input` impls on
  `ShellOutputBlock` and `DiffBlock` first.

## Verification

TBD. Likely:
- Unit tests for per-block scroll state changes via `handle_event`.
- Manual: open alt view on a session with a long shell output and a
  wide diff; navigate, scroll, exit cleanly.

## Open items

- Text selection / copy across blocks — probably yes eventually; out
  of scope for D unless it's free with the focus routing.
