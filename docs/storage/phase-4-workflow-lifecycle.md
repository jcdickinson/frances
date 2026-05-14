# Phase 4 — Workflow lifecycle (resume, not clear)

Workflows persist across daemon restarts. Each instance has a stable id
the workflow sees as `import.meta.instance`. The daemon remembers which
workflow (by config id) was running, restarts it on boot, and feeds it
its previous instance id so it can resume from its own state.

## Current state

`WorkflowStack` (`crates/frances-daemon/src/workflows.rs`):

- In-memory only. Constructed fresh on every daemon start as
  `[Frame::LegacyLlmTurn]`.
- `Frame::Js(JsFrame { handle, emit })` is purely runtime state — a
  `WorkflowHandle` plus channel-tracking. None of it survives a daemon
  restart.
- `JsFrame` holds no concept of an "instance id." The script's
  `import.meta` exposes only `args`.

The top-level `LegacyLlmTurn` frame is being phased out in favor of
`assets/workflows/main.ts` (see the agentic-loop docs). That migration
is independent of this phase; this phase only needs a stack-bottom that
can be persisted alongside the JS frames.

## Desired state

- Every push of a JS workflow onto the stack creates a fresh `instance_id`
  (UUID). That id is stable for the life of the frame — pop and re-push
  is a new instance, but a daemon restart preserves it.
- The active stack is persisted to the per-session DB. After a restart,
  the daemon reads the rows back, starts each workflow from its TOML
  config, and exposes the old `instance_id` to the body via
  `import.meta.instance`.
- A workflow's persisted state (its tables from phase 3) is keyed off
  `instance_id` by the workflow itself. The host does not interpret it.
- `/clear`-style nuking of a workflow's state is *not* a host feature.
  A workflow that wants a clear command implements it itself (e.g.
  `if (input === "/clear") { await db.exec("DELETE FROM ..."); ... }`).

## Schema

New entity in the per-session DB, owned by the daemon (its own UUID, no
relation to any workflow's UUID):

```sql
CREATE TABLE workflow_stack (
    -- 0 = bottom of stack, monotonically increasing toward top.
    position    INTEGER PRIMARY KEY,
    -- TOML workflow id (the key under [workflows.<id>]). Identifies
    -- which config row to look up.
    config_key  TEXT    NOT NULL,
    -- UUID of the running instance. Stable across daemon restarts;
    -- changes only on pop+push.
    instance_id BLOB    NOT NULL,
    -- Args this instance was invoked with. JSON array of strings.
    args        JSONB   NOT NULL,
    created_at  INTEGER NOT NULL
);
```

Three operations:

- **Push**: insert at `MAX(position) + 1`. Records the new instance.
- **Pop**: delete `MAX(position)`. The instance's tables are not
  touched — the workflow is responsible for cleanup if it cares.
- **Restore**: `SELECT * ORDER BY position ASC`.

The bottom of the stack is whatever the host wires up by default
(currently `LegacyLlmTurn`, later `main`); it's *not* in
`workflow_stack`. We persist only the JS frames the user pushed.

## Restart flow

1. Daemon starts. Bootstrap runs as today.
2. After `WorkflowStack::new()` (which still seeds `LegacyLlmTurn`),
   read `workflow_stack` rows in order.
3. For each row: look up `cfg = workflows[row.config_key]`. If the
   config no longer exists (user removed the entry), log a warning and
   drop the row. If it exists, call `WorkflowRuntime::start(invocation)`
   with `instance_id = Some(row.instance_id)`.
4. Push the resulting frames onto the in-memory `WorkflowStack`.
5. Each restored workflow starts running. It parks on `inbox.next()` as
   usual — there is no synthetic input on resume. The workflow's own
   logic (likely a `for await (const input of inbox)` loop with state
   loaded from `db.query`) reconstitutes whatever it needs.

## `import.meta.instance`

The runtime sets `meta.instance` to the instance UUID string before
evaluating the user module. Workflows that don't care can ignore it;
the storage-aware ones key their rows off it:

```ts
const instance = import.meta.instance as string;
const state = await db.query(
  `SELECT * FROM agent_state WHERE instance = ?`,
  [instance],
);
```

`import.meta.args` keeps its current behavior. On a *restored* instance,
`args` is whatever it was originally invoked with — we round-trip
through the row.

## Tasks

1. `Invocation` (in `frances-workflow`) gains an optional
   `instance_id: Option<Uuid>`. `WorkflowRuntime::start` allocates a
   fresh UUID when `None`, threads the chosen id back to the caller
   (return it on `WorkflowHandle`).
2. Runtime: in `run_workflow`, set `meta.instance` to the UUID's
   `to_string()` (alongside the existing `meta.args` setter).
3. New entity schema in the daemon for `workflow_stack`. Wire into
   `Database::open`.
4. `WorkflowStack` gains a handle to the DB. `push_and_drive` inserts
   on push and deletes on `Frame::Js` pop (i.e. when `drive` returns
   `exited`). The legacy frame never inserts.
5. Boot: after `WorkflowStack::new()`, run restore. Restore goes
   strictly after the listeners are up so a workflow that emits during
   eval has somewhere to send it — but before the daemon waits on
   `shutdown.notified()`.
6. Tests:
   - Push a JS workflow that writes a row to its own table keyed on
     `import.meta.instance`. Stop and restart the daemon. The same
     workflow comes back up with the same instance id; querying its
     table finds the row.
   - Push two workflows, restart, both come back in order.
   - Remove a workflow from config, restart, the matching row is
     dropped with a warning and the daemon continues.

## Open questions

- **Eager restart vs lazy.** Restoring all frames on boot could mean
  rehydrating a tall stack with active timers, file handles, etc.
  Workflows shouldn't be doing long-running side effects at top-level
  anyway — they should park on `inbox.next()` and resume from `db`. If
  this turns out to be expensive, lazily restart only the top frame and
  defer lower frames until they become top. Defer the call until we
  have a workflow that hurts.
- **Crash semantics.** If a workflow crashes during restore (script
  syntax error, missing dep, migration drift from phase 3), do we drop
  it or refuse to start the daemon? Lean toward "drop it with a loud
  error in the events stream so the user sees it." Daemon stays usable.
- **Bottom frame.** Today's `LegacyLlmTurn` is special-cased and not
  persisted. Once `main.ts` takes over as the implicit bottom, do we
  treat it the same (host-controlled, no row) or persist it as the
  first row? Probably stay host-controlled — the bottom is a config
  choice, not a user push.
- **`frances new` semantics.** It currently stops the daemon and unlinks
  the TTY link, so the next attach creates a fresh session — and a
  fresh session has an empty `workflow_stack` automatically. No new
  work here. Document.

## Definition of done

- Pushing a JS workflow records a row; popping deletes it.
- Daemon restart restores active frames with their original
  `instance_id` and `args`.
- A workflow can read `import.meta.instance` and use it as the key for
  its own `db.query` calls — survives a restart end-to-end in a test.
- No host-side `/clear`. Documented in the workflow author guide
  (whenever that exists; for now the rule lives in this doc until
  phase 4 ships and we promote the bits into `docs/arch/`).
