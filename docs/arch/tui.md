# TUI architecture

This is in-progress and incomplete. It captures the design we've converged
on so far for the new TUI — the terminal rendering layer that lives in the
`frances-tui` crate and (eventually) `frances-blocks`.

## Goals

- **Native scrollback first**, not alt-screen. The user's shell history
  above frances stays put and flows into the terminal's own scrollback as
  frances accumulates output. The terminal is the long-term archive; we
  don't fight it.
- **Bottom-anchored viewport** for interactive UI (input box, dialogs,
  spinners). Resizable at runtime — input area grows when the user types
  multi-line, dialogs expand and contract.
- **Structured re-renderable history** between the top of the terminal
  and the top of the viewport. Stored as blocks (see below); painted
  bottom-up so a terminal resize re-wraps fidelity-preserved.

## Layers

```
┌─────────────────────────────────────┐
│ frances binary (main.rs)            │   event loop, focus, scripting
├─────────────────────────────────────┤
│ frances-tui: BottomBackend          │   bottom-anchored ratatui Backend
│              + history paint        │   + emit/update/scroll primitives
├─────────────────────────────────────┤
│ frances-blocks (new crate)          │   pure data: Block, Frame, layout
├─────────────────────────────────────┤
│ ratatui + crossterm                 │   third-party
└─────────────────────────────────────┘
```

`frances-blocks` is shared with `frances-daemon`: the daemon decides what
blocks to emit, the TUI renders them. No deps trait — pure data types and
render impls.

## `BottomBackend`

Implemented and tested. See `crates/frances-tui/src/bottom_backend.rs`.

- Wraps a `ratatui::backend::Backend` (concretely `CrosstermBackend`).
- Reports a smaller `size()` than the real terminal, translating ratatui's
  cell coordinates down to land in the bottom band.
- Owns the rows above the viewport. Renders them from structured history
  on demand; ratatui never paints there.
- Algorithms:
  - `emit_above(lines)` — row-by-row scroll-and-paint. Each rendered row
    pushes the topmost on-screen row into native scrollback. Handles
    wrap-overflow (single line wider than the screen) emergently.
  - `set_viewport_height(h)` — grow scrolls up by the delta (oldest
    history flows into native scrollback); shrink slides history down
    and may pull older lines back from the buffer.
  - `handle_terminal_resize(size)` — re-wraps and re-paints visible
    history at the new dimensions.

Today the history is `VecDeque<Line<'static>>`. The next step is to
generalise to blocks.

## Blocks

A **block** is the unit of structured history. It owns a chunk of
displayable data and knows how to wrap itself at a given terminal width.

```rust
pub enum Block {
    Paragraph(Vec<Line<'static>>),
    Code { lang: Option<String>, text: String },
    Table { columns: Vec<ColumnSpec>, rows: Vec<Vec<Line<'static>>> },
    Frame { id: FrameId, children: Vec<Block> },
}
```

Application-specific concepts (tool calls, agent messages, …) are *not*
block variants — they're composed at the daemon layer from these
primitives. Keeping the block set small and generic is what lets the
protocol stay clean.

Each block implements:

```rust
fn measure(&self, width: u16) -> u16;          // wrapped row count
fn render(&self, width: u16, dst: &mut Buffer); // paint into a Buffer
```

`paint_history` walks `history` newest-to-oldest, summing `measure()`
results until cumulative rows hit `available_above`. So measurement cost
per repaint is bounded by visible window size, not history length —
chat sessions with thousands of blocks cost the same as ten.

### IDs and streaming updates

Blocks have IDs. The daemon issues `push(id, block)` to create and
`update(id, block)` to replace (or `append(id, fragment)` for streaming
text into an existing block — see open questions). Updates that target
the visible window re-render the tail from that block down. Updates
that target a block already past `visible_rows` (in native scrollback)
are silently dropped — the terminal owns those cells.

### Frames

A `Frame` is an addressable group of child blocks rendered contiguously.
The daemon protocol references frames by ID; child blocks are internal.
This is what makes "the LLM is streaming and just added a code block in
the middle of its message" expressible: update the frame's children.

### Interactivity

Blocks declare whether they're focusable (table: yes; paragraph: no).
The TUI maintains a focus pointer into history. Key events route to the
focused block, which mutates its own state (table horizontal scroll,
code-block collapse/expand). Once a block scrolls past `visible_rows`,
focus advances to the next focusable block; the now-frozen block loses
its state (no one can see it anyway).

## Blocks larger than the visible area

A block — especially a `Frame` containing several children — can measure
larger than `available_above`. Today the bottom-up paint algorithm just
truncates: only the bottom `available_above` rendered rows of the block
are visible, everything above is past the top of the terminal.

The plan is to add **focus-based scrollback navigation**:

- **Input focused** (the viewport input area): terminal scrollback is
  normal — mouse-wheel / shift-PageUp scrolls the *terminal's* buffer,
  which includes our history that has fallen off the top.
- **Custom area focused** (history): two states.
  - **At bottom**: render as normal — newest content flush against the
    viewport.
  - **Scrolled up**: render the history window offset by N rendered
    rows. While in this state, `^^^` / `vvv` markers appear at the top
    and/or bottom of the area to indicate "you're not seeing the live
    state". If the user Ctrl-C's mid-scroll the marker makes the cause
    obvious.

Bonus: temporarily hook terminal scroll events (mouse wheel) so they
drive our scroll position instead of native scrollback while the custom
area has focus.

This is what we plan to implement next — a separate doc / plan.

## Out of scope here

- **UI widgets** for the viewport (input area, dialog widgets,
  status bar). They live in the viewport, are stateful, and don't enter
  history. They'll likely share the same scripting protocol as blocks
  (so the daemon can script both uniformly), but the rendering paths are
  different.
- Image / sixel / kitty graphics in blocks. Punt until needed; render a
  placeholder if encountered.

## Open questions

1. **Append vs replace for streaming updates.** Probably both, with
   append being a special wire op so the protocol can express "the LLM
   added 3 more characters to the bottom Paragraph" cheaply.
2. **Interactivity scope.** Tables for sure; collapsible code blocks,
   clickable links, inline editors are all plausible. Drives focus/event
   model complexity.
3. **Block ID scope.** Globally unique within a session (simpler wire
   protocol) vs per-frame (frames are self-contained). Leaning global.
