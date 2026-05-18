# Phase 3 — Workflow-owned storage

Let a workflow persist its own state in the per-session DB:

- Migrations are declared up-front in `[workflows.<id>]` and applied
  under the workflow's UUID via the migrator from phase 1.
- A small SQL handle is exposed to JS. Browser-shape (Promises,
  `AbortSignal`) without slavishly cloning IndexedDB or Web SQL (but heavy
  inspiration desired).
- The host doesn't model the workflow's data. Workflows own their
  schema and their queries.

## Current state

- `WorkflowConfig` already has `id: Uuid` and
  `migrations: Vec<PathBuf>` (paths relative to the script's directory).
- Nothing reads `migrations` yet. No SQL handle on the JS side.
- The per-session `Database` is constructed in `Database::open` with a
  fixed list of `&EntitySchema`s.

## Desired shape — Rust side

Workflow start picks up the workflow's migrations and runs them once,
on first use within this session:

1. `frances_daemon::server::bootstrap` already binds
   `workflows: ConfigBinding<HashMap<String, WorkflowConfig>>`. Pass
   that, plus the `Database`, into the `WorkflowRuntime`.
2. The runtime keeps a `DashMap<Uuid, Arc<WorkflowDb>>` of per-workflow
   storage handles. First touch:
   - Read each `migrations` path (relative to `cfg.file.parent()`) into
     memory.
   - Build an `EntitySchema { entity: cfg.id, migrations: ... }` and
     call `frances_storage::run(conn, &schema)`.
   - Cache the handle.
3. `WorkflowDb` is a thin wrapper around the shared `turso::Connection`
   plus the workflow's UUID. It does not partition rows by UUID — the
   workflow's tables are its own, named whatever it wants.

The migrator's strict-prefix rule does the work of "you shipped a
workflow once, don't edit migration 1 later." Migration name + checksum
mismatch fails the workflow's start with a clear error surfaced as a
`StreamFrame::Error` to the client.

## Desired shape — JS side

May need rework here.

A new virtual module: `frances:v1/storage`. Imported like every other v1
surface.

```ts
import { db } from "frances:v1/storage";

await db.exec(`INSERT INTO notes(text) VALUES (?)`, [text]);

const rows = await db.query(
  `SELECT id, text FROM notes WHERE created_at > ?`,
  [since],
  { signal: ac.signal },
);
for (const row of rows) { /* row.id, row.text */ }

await db.transaction(async (tx) => {
  await tx.exec(`UPDATE notes SET text = ? WHERE id = ?`, [t, id]);
  await tx.exec(`DELETE FROM tombstones WHERE note_id = ?`, [id]);
});
```

Shape rules:

- One singleton `db` per workflow. Bound at module evaluation; it points
  at this workflow's entity (i.e. its connection + UUID; same physical
  connection as every other workflow in the session, no row-level
  partitioning).
- `exec(sql, params)` → `Promise<{ rowsAffected, lastInsertRowid }>`.
  Use for writes / DDL-free statements.
- `query(sql, params, opts?)` → `Promise<Row[]>`. Returns rows as plain
  objects keyed by column name. JSON / blob columns are passed through
  as the raw value turso gives us — workflows decode their own shapes.
- `queryStream(sql, params, opts?)` → `ReadableStream<Row>`. For result
  sets large enough to want backpressure. Both `query` and
  `queryStream` accept `{ signal }`.
- `transaction(fn)` runs `fn` inside a turso transaction; `tx` shadows
  `db.exec`/`db.query`/`db.queryStream`. Throws roll back; returning
  resolves commit. No `BEGIN IMMEDIATE` vs deferred knob until we have
  a use case.
- `AbortSignal` aborts in-flight statements where turso supports it,
  and rejects the surrounding `Promise` / errors the `ReadableStream`
  with a `DOMException("AbortError")`.

Things we are deliberately not doing:

- No object-store / index DSL. Workflows write SQL.
- No schema migrations from JS. Migrations are declared in TOML and
  run by the host; the JS surface is read/write only.
- No DB-level event subscription. Workflows poll if they need it.
- No prepared-statement object exposed to JS. Internally we cache
  statements off the SQL string, but the JS surface stays statement-
  free — keeps the API small and the ownership story trivial.

## Tasks

1. **Plumbing.** Hand the `Database` and the workflows binding into
   `WorkflowRuntime::new`. `WorkflowDeps` gains a `workflow_db(id) ->
   Future<Arc<WorkflowDb>>` accessor that runs the entity's migrations
   on first call (or returns the cached handle).
2. **WorkflowDb.** New type in `frances-workflow` (or a small
   `frances-workflow-storage`). Holds `Connection` + `Uuid` for error
   reporting. Implements `exec`, `query`, `queryStream`, `transaction`.
3. **JS module.** `crates/frances-workflow/src/modules/storage.rs` +
   `js/storage.js`. The rs side exposes a `Db` rquickjs class plus a
   `Transaction` class. The js side wraps them with the WHATWG idioms
   (`AbortSignal`, `ReadableStream` for streamed results).
4. **Cancellation.** Turso `Statement` does not expose a cancel API
   today. For v1, `signal.abort()` after a row arrives stops iteration
   (we check the signal between rows). A statement that has not yet
   produced its first row will not be cancelled mid-call. Document this
   limitation; revisit when turso grows interrupt support.
5. **Param binding.** Bind `null` / number / string / bool / Uint8Array.
   Reject everything else with a clear JS error (`TypeError`).
6. **Tests.** A workflow with a `0001_init.sql` that creates a `notes`
   table; calls `exec` then `query` then iterates `queryStream`; asserts
   the rows. A second test: declare an edited migration after the
   first run and confirm the workflow start fails with a
   `ChecksumMismatch`.

## Open questions

- Path of migration files. Today `WorkflowConfig::migrations: Vec<PathBuf>`
  resolves relative to `cfg.file.parent()`. Stick with that — workflow
  authors are already used to it from `file =`.
- Per-workflow connection vs shared. Turso uses a single connection
  cloned around; we already do that for the daemon. Workflows share
  the connection; no need for a fresh one per workflow.
- Backpressure on `queryStream`. WHATWG streams already do it; we just
  need to honor the controller's `desiredSize`. Implementation note,
  not a design question.

## Definition of done

- A workflow can declare `migrations = ["0001_init.sql"]`, the table
  appears in the per-session DB scoped to the workflow's UUID in
  `_migrations`, and JS can `exec`/`query` against it.
- Editing the SQL of an already-applied migration fails the workflow's
  start with a clear `ChecksumMismatch` error surfaced to the TUI.
- Re-starting the daemon does not re-run migrations (the `_migrations`
  row prevents it; this is just confirming the path works for
  workflow-owned entities).
