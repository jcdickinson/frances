-- Per-workflow scrollback blocks. One row per finished or truncated
-- protocol block plus one row per emitted Error frame. The TUI replays
-- these straight into the alt-screen scrollback inspector on attach and
-- whenever the active workflow changes.

CREATE TABLE IF NOT EXISTS scrollback_blocks (
    -- AUTOINCREMENT so order strictly grows across the session's
    -- lifetime; replay reads `ORDER BY id`.
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Workflow instance UUID (matches `workflow_stack.instance_id`).
    -- Replay filters on this so each workflow has its own scrollback.
    instance_id  BLOB    NOT NULL,
    -- 'text' | 'tool_use' | 'tool_result' | 'error'
    kind         TEXT    NOT NULL,
    -- Kind-shaped JSON; serde-on-read decides the shape.
    payload      JSONB   NOT NULL,
    -- 1 = the block did not reach BlockStop (workflow was dehydrated
    -- mid-stream). DEFAULTs to 1 so the dehydration write path can
    -- omit it. Cleared to 0 on a clean BlockStop. Ignored for
    -- kind='error'.
    truncated    INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS scrollback_blocks_by_instance
    ON scrollback_blocks (instance_id, id);
