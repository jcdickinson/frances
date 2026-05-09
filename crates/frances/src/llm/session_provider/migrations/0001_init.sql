CREATE TABLE IF NOT EXISTS session_config (
    path_hash INTEGER PRIMARY KEY,
    path      JSONB   NOT NULL,
    kind      TEXT    NOT NULL,
    value     TEXT    NOT NULL
);
