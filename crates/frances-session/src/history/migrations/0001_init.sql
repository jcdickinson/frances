CREATE TABLE IF NOT EXISTS chat_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Opaque per-session identifier (UUID). Threaded into
    -- `ProviderRequest::session_id` so token caching scopes to this chat
    -- and not the whole session runtime.
    session_id TEXT NOT NULL UNIQUE,
    -- JSON array of model intent names. Each entry keys into a
    -- `models::<intent>` config table; the session walks them in order
    -- when picking a model. The implicit `models::default` (a required
    -- binding) is the always-on final fallback.
    model_intents JSONB NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS chat_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_session_id INTEGER NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    -- 'user' | 'assistant' | 'tool_call' | 'tool_result' | 'history'
    type TEXT NOT NULL,
    primitive JSONB,
    history JSONB,
    kind TEXT,
    provider_id TEXT,
    UNIQUE (chat_session_id, seq),
    CHECK (
        (type = 'history' AND history IS NOT NULL AND primitive IS NULL
                          AND kind IS NOT NULL AND provider_id IS NOT NULL)
        OR
        (type != 'history' AND primitive IS NOT NULL AND history IS NULL
                           AND kind IS NULL AND provider_id IS NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_chat_messages_history
    ON chat_messages(chat_session_id, seq) WHERE history IS NOT NULL;
