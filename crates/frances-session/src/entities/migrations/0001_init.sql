CREATE TABLE IF NOT EXISTS entities (
    entity_id  BLOB    PRIMARY KEY,
    kind       TEXT    NOT NULL,
    lifecycle  INTEGER NOT NULL DEFAULT 0,
    snapshot   JSONB   NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS entity_stream (
    entity_id  BLOB    NOT NULL,
    seq        INTEGER NOT NULL,
    payload    JSONB   NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (entity_id, seq)
);

CREATE TABLE IF NOT EXISTS entity_artifacts (
    entity_id  BLOB    NOT NULL,
    tag        TEXT    NOT NULL,
    payload    JSONB   NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (entity_id, tag)
);
