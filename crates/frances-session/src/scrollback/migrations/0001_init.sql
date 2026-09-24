-- One complete section per row in the session transcript.
CREATE TABLE IF NOT EXISTS scrollback_sections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    payload JSONB NOT NULL,
    created_at INTEGER NOT NULL
);
