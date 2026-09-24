# Edit engine

Status: current engine design. JS tool bindings described below are being replaced
by Rust platform tools under the
[Model Content + Hooks Protocol](../model-content-hooks-protocol.md). The anchor
algorithms and filesystem-independent engine remain; no JS runtime is required
by this design.

The full anchor design is in [`anchors.md`](anchors.md) — read it before changing anything in `frances-edit` or `frances-anchors`. This file covers how the engine is wired into the rest of the codebase.

## Crate split

- **`frances-anchors`** — anchor word dictionary (`words.txt`, ~8200 BPE-friendly words), FNV/xxhash line hashing, and the word↔index encoding used to serialize anchors. No I/O.
- **`frances-edit`** — `EditEngine`, `WorkingFile`, patch parser, reconciler, renderer, anchor pool. Filesystem-agnostic: callers supply file content; the engine never reads disk itself. `test-utils` feature exposes `FakeStore`.
- **`frances-workflow` file module and JS tool wrappers** — current adapter from
  tools to `EditSession`, worker-backed filesystem I/O, and UI output. This adapter
  is replaced by native Rust tools, not retained as a QuickJS compatibility layer.
- **`frances-session::runtime::SessionEditorFactory`** — creates per-context
  `EditSession` instances over the shared engine.
- **`frances-session::anchor_store`** — `AnchorStore` implementation backed by the
  per-session turso database.

Public surface of `frances-edit` is re-exported from `lib.rs`; check there before adding new exports.

## Context and authorization boundaries

The Rust host owns tool dispatch, file I/O, edit reconciliation, and publishing
file/diff views. These responsibilities must move out of the workflow wrappers
when the embedded JS runtime is deleted. The worker remains the filesystem
boundary; MCP does not replace its internal transport.

Each model context receives a fresh editor read cache and loop guard. Context
replacement retains the shared anchor engine and persisted anchors, but the model
must read files again before editing through the new context. Pending calls settle
or cancel before replacement; the host owns the edit reconciliation boundary.

Platform file tools expose authorization descriptions with the requested `uri`
and resolved `canonicalUri`. Both filesystem schemes are hostless; resolved paths
inside the canonical workspace use `workspace-file:///`. A requested workspace
symlink can resolve outside it without an automatic denial solely for that reason.
The tool's executor enforces approved path constraints when accessing the file.

Planning contexts omit editing tools. Changing tool availability requires a new
context; it is not an in-place toggle. Per-call authorization still checks the
arguments of tools selected for that context.

## Quick rules from anchors.md

- Anchors are **per-file coordinates**, always paired with a path.
- Internal line hashes (xxhash3-64 of trimmed content) are for change detection only — never shown to the model.
- Two reconciliation paths: **direct anchor transforms** for our own edits (no diff needed), **Myers diff over hash arrays** for external drift (formatter, user edits, `git checkout`).
- Blank-line hashes are salted by nth-blank-in-file so adjacent blanks don't collide under Myers.
- Indentation on insertions: if the model's payload starts with whitespace, respect it verbatim; otherwise inherit leading whitespace from the anchor line.
