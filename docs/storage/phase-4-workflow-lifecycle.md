# Phase 4 — Workflow lifecycle (resume, not clear)

Workflows persist across daemon restarts. Every push assigns a stable
`instance_id` UUID that the JS body sees as `import.meta.instance`. The
daemon remembers which workflow (by `[workflows.<config_key>]` entry)
is on top, restores it on boot with the same `instance_id`, and feeds
that id back to the body so the workflow can resume from its own
tables.

Only one workflow runs at a time. Pushing on top of A causes A to be
**dehydrated**: a graceful-shutdown signal pulses, the body's
`frances:v1/lifecycle` hook runs (if registered), the inbox closes,
and A's task ends. A's row stays in the DB. When B exits, A is
**rehydrated**: a fresh runtime instance starts with A's original
`instance_id` so the body's storage reads its own prior state.

## Storage

Per-session DB table owned by the daemon (its own entity UUID,
independent of any workflow's UUID):

```sql
CREATE TABLE workflow_stack (
    -- AUTOINCREMENT so positions strictly grow across the lifetime
    -- of the session; pops never free a slot.
    position     INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Key under [workflows.<id>] in config.
    config_key   TEXT    NOT NULL,
    -- Instance UUID exposed as import.meta.instance.
    instance_id  BLOB    NOT NULL UNIQUE,
    -- JSON array of strings. Round-trips via serde_json.
    args         TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    -- 1 = current top of the stack. At most one row has active=1
    -- at any time (partial unique index below).
    active       INTEGER NOT NULL DEFAULT 0,
    -- NULL while alive (top OR in-stack below top). Epoch-ns when
    -- popped or truncated by a later push.
    completed_at INTEGER
);

CREATE UNIQUE INDEX workflow_stack_one_active
    ON workflow_stack(active) WHERE active = 1;
```

The table is **append-only**: pop sets `completed_at`, push truncates
any non-completed rows above the current top (defensive sweep against
crash-mid-pop), then inserts a new row. Rows accumulate; a future
"resume previously-popped workflow" feature is unblocked at the schema
level (the row is still there) but not built here.

## Operations

**Push** (slash command or default-workflow seeding):

```sql
BEGIN;
-- Defensive truncation. Normal flow matches nothing.
UPDATE workflow_stack
   SET completed_at = ?now
 WHERE completed_at IS NULL
   AND position > COALESCE(
     (SELECT MAX(position) FROM workflow_stack WHERE active = 1),
     -1
   );
-- Demote current top.
UPDATE workflow_stack SET active = 0 WHERE active = 1;
-- Insert new top.
INSERT INTO workflow_stack
    (config_key, instance_id, args, created_at, active)
    VALUES (?, ?, ?, ?now, 1);
COMMIT;
```

**Pop** (current top exited):

```sql
BEGIN;
UPDATE workflow_stack
   SET active = 0, completed_at = ?now
 WHERE instance_id = ?;
UPDATE workflow_stack
   SET active = 1
 WHERE position = (
   SELECT MAX(position)
     FROM workflow_stack
    WHERE completed_at IS NULL
 );
COMMIT;
```

## Boot

1. Daemon starts. Listeners spawn, runtime is constructed.
2. `restore_or_seed`:
   - If `COUNT(*) = 0` ⇒ table has never been used. Push the
     configured `default_workflow` (if any) via the normal push path,
     which inserts a row and hydrates the runtime.
   - Else ⇒ find the row with `active = 1` and hydrate it. If no row
     is active (the user previously popped everything to zero live
     rows), leave the in-memory stack empty — the default workflow
     is **not** re-seeded.
3. Hydration failures (missing `[workflows.*]` entry, migration
   drift, runtime error) cascade: the failing row + any rows above it
   are tombstoned, the next live row is promoted, retry. Loops until
   a row hydrates cleanly or the live stack runs dry. The daemon
   always reaches its `shutdown.notified()` wait in a usable state.

## `import.meta.instance`

The runtime sets `meta.instance` to the instance UUID string before
evaluating the user module. Workflows that don't care can ignore it;
the storage-aware ones key off it:

```ts
const instance = import.meta.instance as string;
const state = await db.query(
  `SELECT * FROM agent_state WHERE instance = ?`,
  [instance],
);
```

`import.meta.args` keeps its previous behavior — and round-trips
through the row, so a restored instance sees the same args it was
originally invoked with.

Cleanup of a popped workflow's tables is **not** a host feature. A
workflow that wants a `/clear` command implements it itself
(`if (input === "/clear") { await db.exec("DELETE FROM ..."); ... }`).

## `frances:v1/lifecycle`

Workflows opt into a graceful-shutdown hook by importing the new
`frances:v1/lifecycle` module and assigning a function:

```ts
import { lifecycle } from "frances:v1/lifecycle";

lifecycle.shutdown = async () => {
  // Save final state, emit a farewell frame, etc.
};
```

Mechanics:

- One signal: `shutdown_notify: Arc<Notify>` plumbed into the runtime.
- The lifecycle module body kicks off a background IIFE that awaits
  the signal, runs the registered handler (best-effort, errors
  swallowed), and then closes the inbox so any
  `for await (const input of inbox)` loop in user code unwinds.
- `workflow.exit()` now routes through the same signal rather than
  closing the inbox directly — so the hook fires for both
  user-initiated `exit()` and daemon-driven dehydration. A workflow
  with no registered handler still terminates promptly (the IIFE
  closes the inbox unconditionally).
- The daemon calls `WorkflowHandle::request_shutdown()` when
  dehydrating; it then drains remaining frames (forwarding any final
  emissions to the wire) and awaits the body's exit. Bounded by a
  5-second timeout, after which the handle is force-dropped (aborts
  the spawned task).

## In-memory shape

`WorkflowStack` holds:

```rust
pub struct WorkflowStack {
    top:  AsyncMutex<Option<WorkflowInstance>>,
    conn: Connection,
}

struct WorkflowInstance {
    handle:     WorkflowHandle,   // carries `instance: Uuid`
    emit:       EmitState,
    config_key: String,
}
```

Single slot. The layered stack lives entirely in `workflow_stack`
rows.

## Open questions / future work

- **Resume a popped workflow.** Schema permits it (the row is still
  there with `completed_at IS NOT NULL`). A future feature could
  clear `completed_at`, set `active = 1`, and restart the runtime
  with the original `instance_id`. Not built here.
- **Re-seed default when live stack is empty.** Currently the rule is
  "seed only if `COUNT(*) = 0`". Once the user pops everything down
  to zero live rows, the daemon stays empty until they push
  something explicit. Could grow to "seed if no live rows" if the UX
  turns out poorly; for now we accept the literal reading.
- **Crash mid-restore surfacing.** Today a failed restore logs a
  warning and tombstones the row. Future work could push an error
  frame onto the events socket so the user sees what happened.
- **Scrollback restoration.** Tracked separately in
  `phase-5-wip.md`.

## Definition of done

- Pushing a workflow records a row with `active = 1`; popping
  tombstones it and promotes the next live row.
- Dehydration runs `lifecycle.shutdown` (if registered) before the
  body terminates; rehydration restarts the runtime with the same
  `instance_id`.
- Daemon restart restores the active row (or seeds the default if the
  table is empty); orphan/broken rows tombstone and the daemon stays
  usable.
- Workflows can read `import.meta.instance` and use it as the key
  for their own `db.query` calls; survives a restart end-to-end.
- No host-side `/clear`.
