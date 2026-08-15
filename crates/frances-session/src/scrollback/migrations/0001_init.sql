-- Per-workflow scrollback sections. One row per pushed section. The UI
-- replays these straight into the alt-screen scrollback inspector on
-- attach and whenever the active workflow changes.
--
-- The payload column carries the full `SectionKind` variant inline
-- via `#[serde(tag = "type")]`, so there's no parallel discriminator
-- column.

CREATE TABLE IF NOT EXISTS scrollback_sections (
    -- AUTOINCREMENT so order strictly grows across the session's
    -- lifetime; replay reads `ORDER BY id`.
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Workflow instance UUID (currently the session id).
    -- Replay filters on this so each workflow has its own scrollback.
    instance_id  BLOB    NOT NULL,
    -- `SectionKind`-shaped JSON; serde-on-read decides the variant via
    -- the embedded `type` tag.
    payload      JSONB   NOT NULL,
    created_at   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS scrollback_sections_by_instance
    ON scrollback_sections (instance_id, id);
