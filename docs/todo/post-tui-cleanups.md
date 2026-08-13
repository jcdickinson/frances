# Post-TUI-port structural cleanups

Leftovers from the TUI → Tauri port that are functional but shaped by the
old renderer. Each is a judgment call, not a straight deletion.

## Collapse `ScrollbackFrame` into `StreamFrame`?

`frances-session/src/events.rs` keeps replay as a closed sub-protocol
(`StreamFrame::Scrollback(ScrollbackFrame)`) so the consumer's replay
handler never sees live-only frames. That mattered for the TUI's
alt-screen replay handler; the Tauri event bridge (`frances/src/app.rs`
`convert_frame`) now flattens both into the *same* `UiEvent` variants —
the frontend only distinguishes replay via the `Reset`/`ReplayEnd`
brackets and the truncated flag on close.

The two-enum split no longer pulls the weight it was designed for.
Collapsing would delete ~40 lines and one layer of match arms. The
counter-argument is the bounded-set property: replay producers can only
emit frames that make sense in a burst. Decide whether that property is
worth the duplication.

## Hand-mirrored TypeScript types

`frontend/src/types.ts` mirrors the Rust serde shapes by eye:
`SectionKind`, `DiffLine` (= `frances_edit::DiffOp`), `SurfaceCommand`,
`UiEvent`, `Usage`, `AppInfo`. Nothing checks they stay in lockstep — a
Rust enum change silently breaks rendering.

Options: adopt codegen (e.g. tauri-specta / specta) to emit the TS types
from the Rust definitions, or add a round-trip test that deserializes
representative JSON fixtures on both sides. Or accept the risk knowingly
and keep the mirror small.

## "Scrollback" terminology

The persisted-transcript layer is named after the terminal concept
throughout: the `frances-session` `scrollback` module, the
`scrollback_blocks` table, `replay_initial_scrollback`, CLAUDE.md /
CONTEXT.md / README, and the shell tool descriptions in
`frances-workflow/assets/frances/v1/tools/shell.js` ("full output in
scrollback"). It works, but the word will read increasingly oddly as the
GUI drifts from the terminal metaphor. If renamed (e.g. "transcript"),
do it in one sweep — module, table, docs, and the JS tool prose that the
model reads.
