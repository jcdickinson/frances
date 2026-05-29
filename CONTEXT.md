# Frances — Glossary

Living document. Terms are resolved as they come up; do not treat this
file as a spec.

## Crate layout (sections-related)

- **`frances-models-tui`** (new) — shared TUI-side wire vocabulary.
  Holds `SectionKind`, `SectionEvent`, `SectionId`, `Source`. No logic,
  no deps traits (per the project's `frances-models-*` convention).
  Both `frances-workflow` and `frances-tui` depend on it.
- **`frances-tui`** — `Block` trait, `Section` trait, `InactiveBlock`,
  the container, the section dispatcher (`make_section(kind, seed)
  -> Box<dyn Section>`).
- **`frances-markdown`** (new) — `MarkdownSection` impl + the inline
  parser + `ParagraphBlock`. Depends on `frances-tui` (Block, Section)
  and `frances-models-tui` (SectionEvent, SectionKind).

## Markdown parsing scope

`MarkdownSection` runs the paragraph splitter for **all** sources, so a
plain text section still renders as N `ParagraphBlock`s (one per
paragraph). The inline parser (CommonMark `*…*` / `**…**` /
`_…_` / `__…__` delimiters) is gated on `source != Source::User`:

- `Source::Assistant` → splits + inline-parses (the LLM emits markdown).
- `Source::Internal` → splits + inline-parses (chrome / plan dumps /
  greetings may include markdown).
- `Source::User` → splits only; inline runs of `*` / `_` are rendered
  literally so the user's `look at the *.rs files` doesn't render
  half of the line italic.

## Sections & blocks

- **Section** — a workflow-emitted, lifecycle-bounded unit of transcript
  content. Replaces the older name "Frame". `Section` is a trait;
  concrete impls are Markdown / ShellOutput / Reasoning / ToolUse /
  Diff / Json / Error. Each impl knows how to apply section events
  (open/append/update/close/truncated) and exposes its current blocks.
  Sections are persisted; blocks are derived. Not the same as a
  markdown "section" (`# heading`).
- **Block** — a TUI-side render primitive that implements
  `frances_tui::Block`. A section owns one or more blocks. Most
  section impls degenerate to a single block; `MarkdownSection`
  expands to many `ParagraphBlock`s via the `frances-markdown` crate.
  Workflows (JS) never emit or address blocks; the wire vocabulary is
  purely section-centric.
- **Custom scrollback** — the container's in-memory mirror of the
  visible screen + sealed-but-still-in-memory blocks. Exists so a
  caller can declare a block sealed (via `mark_safe`, the explicit
  "commit" step) while content above it is still streaming, without
  the sealed block racing into native scrollback prematurely.
- **Section commit lifecycle.**
  1. Workflow opens a section → `Box<dyn Section>` enters `active`.
     Container calls `section.apply(...)` for each subsequent event.
  2. Workflow closes the section (`SectionClose` event) → section is
     flagged sealed. `promote_ready` drains the front of `active` for
     any prefix of sealed sections, moving them to `safe`. A section
     sealed before an older still-active one waits behind it.
  3. Section in `safe` is still a full `Box<dyn Section>` and
     renderable. It only enters native scrollback once its first row
     scrolls past `cumulative_scrolls`; at that moment the container
     snapshots `section.blocks() + sigil` into a built-in
     `InactiveBlock`, pushes it into `committed`, and drops the
     `Box<dyn Section>`.
  4. The `committed` deque is retained in memory for the alt-view
     scrollback explorer, so InactiveBlocks live as long as the
     session.
- **InactiveBlock** — a built-in concrete `Block` impl produced at
  step 3 above. Owns the snapshotted inner blocks + sigil; the
  section's trait identity is gone.
- **Wire event vocabulary.** Three variants, self-describing (every
  Append carries the section's current kind, so any consumer can
  construct or update from a single delta):
  - `SectionAppend { id, kind: SectionKind, delta: String }` — the
    first Append with a new id implicitly constructs the section
    (the dispatcher calls `make_section(kind)`); subsequent Appends
    either grow the text or carry an unchanged-delta + new-kind for
    metadata transitions (e.g. ShellState `Running` → `Success`).
  - `SectionClose { id }` — workflow seals the section. Triggers
    promote-ready (sealed prefix of `active` drains to `safe`).
  - `SectionTruncated { id }` — replay-only sibling of Close: the
    section was in flight when its workflow was dehydrated. Section
    seals; the InactiveBlock snapshot will be marked truncated when
    it lands in `committed`.
- **`Section::apply(&mut self, event)`** receives the same 3-variant
  vocabulary on the trait side (renamed `SectionApply` for clarity if
  it diverges from the wire enum). The trait does not have a separate
  `Open` event; first-Append-as-construct is the dispatcher's concern.
  Inter-block gap inside a section is implicit 0 — only Markdown
  holds multiple blocks, and they're contiguous by design.
- **Frame** (reserved for render-loop only) — one terminal redraw tick.
  Used inside `frances-tui` (`FrameTime`, "this frame"). Never used for
  transcript content.
