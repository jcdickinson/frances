# Phase 1 — Migrator

Confirm the per-entity migrator stays, with one small loosening so
workflow-declared migrations (phase 3) can plug in.

## Current state

`crates/frances-daemon/src/migrations.rs`:

- `Migration { name: &'static str, sql: &'static str }`
- `EntitySchema { entity: Uuid, migrations: &'static [Migration] }`
- `run` enforces strict prefix match (same name, same xxh3 checksum,
  same order) against `_migrations`. Forward-only. One row + the SQL in
  one transaction, so a partial apply can never be recorded.
- Three entities today: `anchor_store::SCHEMA`, `history::SCHEMA`,
  `llm::session_provider::SCHEMA`. Each owns a UUID and an
  `include_str!`'d SQL file.

Design is good. Keep it. Tests already cover renamed / edited / shrunk
rejection, separate-entity isolation, partial-apply rollback.

## What changes

Only one thing: `'static` is wrong for workflows. Workflow migrations
come off disk at runtime — the path is read from
`WorkflowConfig::migrations`, the bytes are loaded when the workflow is
about to run. We can't `include_str!` them.

Switch `Migration` to owned strings (`String`, or
`Cow<'static, str>` if we want the daemon's built-in schemas to stay
zero-copy). Same for `EntitySchema::migrations` — make it `Vec<Migration>`
or `&[Migration]` over owned data.

Built-in schemas keep building from `include_str!` constants — they just
move into the owned shape at static-init time, or wrap the `&'static str`s
in `Cow::Borrowed`.

## Tasks

1. Change `Migration` / `EntitySchema` field types away from `&'static`.
   Pick `Cow<'static, str>` to keep daemon-side `include_str!` zero-copy.
2. Update `anchor_store::SCHEMA`, `history::SCHEMA`,
   `llm::session_provider::SCHEMA` accordingly. No semantic change.
3. Move `migrations.rs` to a place workflow code can reach it. Today it
   lives in `frances-daemon`; `frances-workflow` will need to construct
   an `EntitySchema` for each workflow during phase 3. Easiest: extract
   to a small `frances-storage` (or `frances-migrate`) crate that both
   daemon and workflow depend on. Carries `Migration`, `EntitySchema`,
   `MigrationError`, `ensure_table`, `run`, `run_all`. Pulls in `turso`
   and `twox-hash`. No deps on the rest of the workspace.
4. Update `store.rs::Database::open` to call `run_all` with the new
   crate path. Keep the bootstrap order unchanged.

## Crate name

`frances-storage`. Broad enough that the `Database` wrapper can move in
later without another rename.

## Definition of done

- Daemon-side schemas still apply identically (same checksums in
  `_migrations`).
- A test in the new crate constructs an `EntitySchema` from owned
  `String`s and runs it against an in-memory connection.
- `migrations.rs` is gone from `frances-daemon/src/`; the daemon imports
  the new crate.
