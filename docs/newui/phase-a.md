# Phase A — ratatui owns the footer rect

## Goal

Stop maintaining our own footer-side cell diff. ratatui's existing
buffer-pair diff handles the footer rect; we keep our direct-emit path
for the scrollback area only. After Phase A the container has zero
buffer state for the footer.

## Why

Today the container holds three fields purely to diff the footer
against the previous frame:

- `prev_footer_buf: Option<Buffer>` (`scrollback_container.rs:170`)
- `prev_footer_anchor_y: Option<u16>` (`scrollback_container.rs:171`)
- `prev_footer_bottom_y: Option<u16>` (`scrollback_container.rs:160`)

And the natural-scroll path at `scrollback_container.rs:717–822`
hand-rolls the diff loop:

```text
let local_area = Rect::new(0, 0, width, footer_h);
let mut curr_buf = Buffer::empty(local_area);
self.footer.render(local_area, &mut curr_buf);
…
if same_layout {
    for (x, y, cell) in prev.diff(&curr_buf) {
        backend.write_cell(x, screen_y, cell)?;
    }
} else {
    // full repaint, row by row
}
self.prev_footer_buf = Some(curr_buf);
```

ratatui's `Terminal::draw` already does this. We're paying for the
buffer pair twice (once in ratatui's `Terminal`, once on
`ScrollbackContainer`), and the diff logic is duplicated.

## Mechanism

History note: `Viewport::Fixed` was tried and backed out — it can't
change height, and the footer needs to. Don't reach for it again.

We own `InlineBackend` (it's not shaped for a reusable framework
anyway). Rename it `ScrollbackBackend` and teach it the footer rect
directly. The same struct serves both interfaces:

```rust
// crates/frances-tui/src/scrollback_backend.rs (was inline_backend.rs)

pub struct ScrollbackBackend<W> {
    out: W,
    band_size: Size,         // full inline band (scrollback + footer)
    footer_anchor_y: u16,    // screen row where footer's row 0 lives
    footer_height: u16,
    mode: BackendMode,
    cursor: (u16, u16),      // current physical cursor (existing
                             // bookkeeping)
}

enum BackendMode {
    /// Direct-emit interface used by the container (write_row,
    /// newline, move_cursor_abs, write_cell, clear_line). All
    /// coordinates are absolute screen positions.
    Scrollback,

    /// ratatui Backend interface. `size()` returns the footer rect's
    /// dimensions; `draw(cells)` translates `y += footer_anchor_y`
    /// before emitting. Used by ratatui internally when we call
    /// `terminal.draw(...)`.
    Footer,
}
```

The container still holds a `ratatui::Terminal<ScrollbackBackend<W>>`.
To render the footer:

1. Update `backend.footer_anchor_y` / `backend.footer_height` based on
   this frame's layout.
2. If anchor or height changed since the last footer paint:
   - height change → `terminal.resize(Rect::new(0, 0, width, h))`
   - anchor change → `terminal.clear()` to mark every cell dirty
   (Both can change in the same frame; do both.)
3. Set `backend.mode = Footer`.
4. `terminal.draw(|frame| frame.render_widget(footer, frame.area()))`.
   ratatui calls `Backend::size()` (returns footer dims), allocates
   the buffer, diffs against last frame's buffer, calls
   `Backend::draw(iter)` with cells at (x, y) relative to (0, 0). Our
   `Backend::draw` translates `y += footer_anchor_y` and emits at
   absolute screen position via the same machinery as `write_cell`.
5. Set `backend.mode = Scrollback`.

The container's direct-emit calls (history scrolling, eviction) work
in absolute coordinates and don't go through ratatui at all. They use
the existing methods on `ScrollbackBackend` (`write_row`, `newline`,
etc.) which are mode-agnostic.

No `Rc<RefCell<…>>`, no wrapper backend, no split constructor. One
struct, one mode flag.

## Footer slot type

`MeasuredWidget` survives Phase A. Its `render(area, buf)` is now
invoked inside ratatui's draw closure rather than by the container
directly. The container needs the footer expression to satisfy both:

- `MeasuredWidget::measure(width) -> u16` — for layout decisions
  before the draw call.
- Something `Frame::render_widget` can take — i.e. `Widget`.

Cheapest path: add a small private newtype in the container that
implements `Widget` by delegating to a `&dyn MeasuredWidget`. Inside
the draw closure:

```rust
let widget = MeasuredWidgetRef(self.footer.as_ref());
frame.render_widget(widget, frame.area());
```

Phase B kills this when `Widget` becomes the canonical trait.

## Anchor / height bookkeeping (A2)

The footer floats up early in the session (cursor row + content =
anchor) and pins to the bottom once content fills the screen. Both
can change frame to frame.

- **Anchor moved this frame** → `terminal.clear()` before
  `terminal.draw`. Forces a full repaint into the new rect.
- **Height changed this frame** → `terminal.resize(Rect::new(0, 0,
  width, h))`. ratatui handles buffer-pair resize; the next draw
  emits all cells.
- **Width changed** (terminal resize) → same as height change. The
  outer terminal-resize path (existing) already invalidates state;
  align with that.
- **Anchor stayed put, height stayed put, content changed** →
  `terminal.draw(...)`. ratatui diffs against last frame's buffer,
  emits only changed cells.

The `SyncGuard` (DEC 2026 synchronized output) already wraps each
frame — well-behaved terminals composite the repaint atomically. The
expected flicker on a full repaint is only visible on legacy
terminals.

## What gets deleted

In `scrollback_container.rs`:

- Fields `prev_footer_buf`, `prev_footer_anchor_y`,
  `prev_footer_bottom_y` (~lines 160–171).
- Their inits in `new` (~lines 222–224).
- Their resets in `clear` (~lines 387–389).
- Their resets in the size-change branch of `draw` (~lines 616–618).
- The footer render block in `draw` (~lines 717–822) — replaced with
  the mode-toggle + `terminal.draw` sequence above.
- The footer render block in `draw_active_overflow` (~lines 1027–1039)
  and the trailing state cleanup at lines 1083–1086 — replaced with
  the same sequence.
- The footer render block in `paint_scrollback` (~lines 1166–1188),
  including the top-clip-into-scratch-buffer branch — `terminal.draw`
  handles the alt-screen footer too. (The alt-screen layout still
  decides where the footer rect goes; the difference is who paints
  it.)

In `inline_backend.rs` → `scrollback_backend.rs`:

- Add the `mode: BackendMode` field + state for `footer_anchor_y` and
  `footer_height`.
- Inside the existing `Backend::draw` impl, branch on `mode`. In
  `Footer` mode, translate cell `y` and emit. In `Scrollback` mode,
  the impl already does very little (we mostly drive via direct-emit
  methods); confirm what the existing `Backend::draw` does for
  scrollback flow and preserve it.
- `Backend::size()` returns `(band_size.width, footer_height)` in
  `Footer` mode, full `band_size` in `Scrollback` mode. (Ratatui only
  asks for size during `draw`, so Footer mode is what matters.)

## Files

- `crates/frances-tui/src/inline_backend.rs` →
  `crates/frances-tui/src/scrollback_backend.rs`. Rename file +
  symbol. Add mode + footer state + offset logic.
- `crates/frances-tui/src/scrollback_container.rs`. Strip the footer
  diff state; rewire three render paths.
- `crates/frances-tui/src/lib.rs`. Re-export the renamed type. Keep
  the old name re-exported behind a `pub use ScrollbackBackend as
  InlineBackend` only if downstream impact is large; otherwise rename
  cleanly.
- `crates/frances/src/ui.rs`. Update the type name at the construction
  site. No shape changes.

## Tests + verification

- All existing scrollback container tests (~42) must pass without
  expectation edits. They observe terminal state through a real
  alacritty buffer (see `mk_term_terminal`), so they're the truth
  source for "ratatui + direct-emit cohabit cleanly."
- Add unit tests:
  - Anchor change between frames triggers a full footer repaint.
  - Height change between frames triggers a full footer repaint.
  - Stable anchor + stable height + content change → ratatui's diff
    only emits the changed cells. (Observe via the alacritty harness
    or by mocking the backend.)
  - `Backend::size()` in `Footer` mode returns footer dimensions; in
    `Scrollback` mode returns band dimensions.
- Manual: `cargo run -p frances`. Type into the textarea (per-cell
  diff should keep emission small). Resize the terminal during a
  streaming response. Scroll into the alt view; back out.

## Out of scope

- Removing `MeasuredWidget`. It survives Phase A unchanged; Phase B
  promotes it to a real `Widget`.
- Changing footer composition (still one ad-hoc `FooterBlock`). Phase
  B introduces the Border ▸ VStack ▸ … tree.
- Anything block-side (`Block` trait, clip direction, serde).

## Order of operations during implementation

1. Rename `inline_backend.rs` → `scrollback_backend.rs`, `InlineBackend`
   → `ScrollbackBackend`. One commit-sized step; no behaviour change.
   Re-export from `lib.rs`. Update `ui.rs` and the scratch binary.
   Run tests.
2. Add `BackendMode` + footer state fields, default to `Scrollback`
   everywhere. No behaviour change yet. Run tests.
3. Add the offset logic inside `Backend::draw` + `Backend::size`.
   Still no callers; tests pass.
4. Switch the natural-scroll path's footer render to mode-toggle +
   `terminal.draw`. Delete `prev_footer_buf` references gated on this
   path only (the active-overflow + alt-screen paths still write to
   them, so this step probably needs the field gone in one go — fine,
   do it together).
5. Switch `draw_active_overflow` and `paint_scrollback` footer paths.
6. Delete the now-unused fields and their resets.
7. Add the new unit tests.
8. Manual smoke test; commit.
