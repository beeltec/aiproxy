-- Keyed provider connections. The slug is the model prefix.
CREATE TABLE connections (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    slug           TEXT    NOT NULL UNIQUE,
    -- openai, anthropic, openrouter
    kind           TEXT    NOT NULL,
    display_name   TEXT    NOT NULL,
    -- AES-GCM, associated data "connection:<id>:api_key".
    api_key_enc    BLOB    NOT NULL,
    api_key_last4  TEXT    NOT NULL,
    base_url       TEXT,
    created_at     INTEGER NOT NULL,
    last_sync_at   INTEGER,
    last_error     TEXT
);

-- Admin changes to the capabilities of a model. They win over the synced values.
ALTER TABLE models ADD COLUMN capability_overrides TEXT NOT NULL DEFAULT '{}';

-- Model names for clients. The target is `chatgpt/<model>` or `<slug>/<model>`.
CREATE TABLE aliases (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT    NOT NULL UNIQUE,
    target          TEXT    NOT NULL,
    default_effort  TEXT,
    default_fast    INTEGER NOT NULL DEFAULT 0,
    -- auto, concise, detailed
    default_summary TEXT,
    description     TEXT,
    created_at      INTEGER NOT NULL
);

-- Usage rows of connections, and the alias that a client used.
ALTER TABLE usage ADD COLUMN connection_id INTEGER REFERENCES connections (id) ON DELETE SET NULL;
ALTER TABLE usage ADD COLUMN alias TEXT;
