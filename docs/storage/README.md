# Storage formalization

Working notes for the storage pass. Each phase is a self-contained piece
of work that can land independently. Delete this directory once phase 4
is merged — it is not architecture documentation, it is a plan.

Nothing here is set in stone, no shipped compatibility to preserve.

## Phases

1. [phase-1-migrator.md](phase-1-migrator.md) — confirm the migrator
   keeps its current shape; loosen `Migration`'s `'static` requirement so
   workflows can declare migrations from disk.
2. [phase-2-chat-sessions.md](phase-2-chat-sessions.md) — honor an
   `ephemeral: true` flag on `new ChatSession({...})`. Ephemeral sessions
   never touch the DB.
3. [phase-3-workflow-storage.md](phase-3-workflow-storage.md) — let
   workflows declare migration files in their config row, run them under
   the per-session DB, and expose a small SQL handle to JS. Browser-shape
   without being a slave to the IndexedDB / Web SQL surface; use
   `AbortSignal` where it fits.
4. [phase-4-workflow-lifecycle.md](phase-4-workflow-lifecycle.md) —
   persist the workflow stack across daemon restarts. Workflows are
   created and resumed, not cleared. Each instance gets a stable id
   exposed as `import.meta.instance`.

## Out of scope (for now)

- Cross-session / app-wide storage. The directory-binding store in
  [docs/todo/directory-binding.md](../todo/directory-binding.md) is the
  first user of that surface; this pass leaves the per-session DB
  partitioning rule from [docs/arch/daemon.md](../arch/daemon.md)
  intact.
- Workflow-to-workflow storage sharing. Each workflow owns its own
  entity UUID and its own tables. If two workflows need to share, that's
  a future "shared entity" feature.
- Migration *rollback*. The migrator is forward-only and that's not
  changing.
