# Edit engine

The full anchor design is in [`anchors.md`](anchors.md) — read it before changing anything in `frances-edit` or `frances-anchors`. This file covers how the engine is wired into the rest of the codebase.

## Crate split

- **`frances-anchors`** — anchor word dictionary (`words.txt`, ~8200 BPE-friendly words), FNV/xxhash line hashing, and the word↔index encoding used to serialize anchors. No I/O.
- **`frances-edit`** — `EditEngine`, `WorkingFile`, patch parser, reconciler, renderer, anchor pool. Filesystem-agnostic: callers supply file content; the engine never reads disk itself. `test-utils` feature exposes `FakeStore`.
- **`frances::edit_session`** — the layer that wires `EditEngine` to disk + formatter + the in-process per-file working cache. This is what tools call.
- **`frances::anchor_store`** — `AnchorStore` impl backed by the per-session turso db.

Public surface of `frances-edit` is re-exported from `lib.rs`; check there before adding new exports.

## Quick rules from anchors.md

- Anchors are **per-file coordinates**, always paired with a path.
- Internal line hashes (xxhash3-64 of trimmed content) are for change detection only — never shown to the model.
- Two reconciliation paths: **direct anchor transforms** for our own edits (no diff needed), **Myers diff over hash arrays** for external drift (formatter, user edits, `git checkout`).
- Blank-line hashes are salted by nth-blank-in-file so adjacent blanks don't collide under Myers.
- Indentation on insertions: if the model's payload starts with whitespace, respect it verbatim; otherwise inherit leading whitespace from the anchor line.
