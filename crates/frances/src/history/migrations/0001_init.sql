CREATE TABLE IF NOT EXISTS rows (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    seq INTEGER NOT NULL UNIQUE,
    -- 'user' | 'assistant' | 'tool_call' | 'tool_result' | 'history'
    type TEXT NOT NULL,
    -- Set on non-history rows only; carries the primitive content typed by `type`.
    primitive JSONB,
    -- Set on history rows only; the wire JSON the provider emitted.
    history JSONB,
    -- Set on history rows only.
    kind TEXT,
    provider_id TEXT,
    CHECK (
        (type = 'history' AND history IS NOT NULL AND primitive IS NULL
                          AND kind IS NOT NULL AND provider_id IS NOT NULL)
        OR
        (type != 'history' AND primitive IS NOT NULL AND history IS NULL
                           AND kind IS NULL AND provider_id IS NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_rows_history ON rows(seq) WHERE history IS NOT NULL;
