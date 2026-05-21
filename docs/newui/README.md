# newui — phased rendering framework rebuild

These docs exist so a fresh context can pick up between phases without
re-deriving the design. Phase docs are self-contained.

## Where we are

- [x] Phase A — ratatui owns the footer rect; drop our own footer diff.
- [ ] Phase B — widget framework on taffy; `Input` + `Widget` traits;
      footer becomes a widget tree.
- [ ] Phase C — Block trait refinement (`Input`, `safe_on_push`, serde,
      truncation flag).
- [ ] Phase D — alt-view per-block interactivity (hscroll/vscroll
      inside the inspector).

Tick the box when a phase is landed on `main`.

## End-state vision (one paragraph)

`frances-tui` owns a small but real rendering framework. The
scrollback container keeps doing what only it can do — emit history
rows directly so they spill into the terminal's native scrollback —
but the footer rect is a normal ratatui-managed widget tree. Widgets
have a 2D measure (via taffy) and compose; an `Input` trait is shared
between widgets and blocks so the alt-view inspector can dispatch
events to whichever block has focus. Blocks declare their own
`safe_on_push`, serialise to/from disk via `BlockKind`, and handle
their own clipping + truncation rendering.

## Files

- `phase-a.md` — fully spec'd, ready to implement.
- `phase-b.md` — sketch; details get filled in as we land A.
- `phase-c.md` — sketch.
- `phase-d.md` — sketch.

These docs are scratch. They get deleted once the work lands. Nothing
else in the repo links to them.
