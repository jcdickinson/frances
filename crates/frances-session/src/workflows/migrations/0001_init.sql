-- Per-session workflow stack. Append-only: pops mark rows with a
-- `completed_at` timestamp, push truncates any non-completed rows above
-- the current top. The single `active=1` row is the current top.

CREATE TABLE IF NOT EXISTS workflow_stack (
    -- AUTOINCREMENT so positions strictly grow across the lifetime of
    -- the session; popped rows never free their slot, so a future
    -- push always lands above whatever's been popped before.
    position     INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Key under [workflows.<id>] in config; identifies which config
    -- entry to look up when rehydrating.
    config_key   TEXT    NOT NULL,
    -- Instance UUID exposed to JS as `import.meta.instance`. Stable
    -- across daemon restarts; allocated once per push.
    instance_id  BLOB    NOT NULL UNIQUE,
    -- Args this instance was invoked with. JSON array of strings,
    -- stored as TEXT (we never query into the array — it round-trips
    -- through `serde_json` on the host side).
    args         TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    -- 1 for the current top of the stack, 0 otherwise. At most one
    -- row holds active=1 at any time (partial unique index below).
    active       INTEGER NOT NULL DEFAULT 0,
    -- NULL while the row is alive (the active top, or in-stack below
    -- the top). Set to epoch-ns when popped or truncated by a later
    -- push.
    completed_at INTEGER
);

-- "Active is unique": at most one row is the top at any moment.
CREATE UNIQUE INDEX IF NOT EXISTS workflow_stack_one_active
    ON workflow_stack(active) WHERE active = 1;
