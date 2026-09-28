-- Instance settings as one JSON document (time zone, refresh schedule, failover).
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE chatgpt_accounts (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    -- From the id token claim "https://api.openai.com/auth".
    chatgpt_account_id TEXT    NOT NULL UNIQUE,
    email              TEXT,
    plan_type          TEXT,
    label              TEXT,
    -- AES-GCM, associated data "chatgpt:<chatgpt_account_id>:<field>".
    access_token_enc   BLOB    NOT NULL,
    refresh_token_enc  BLOB    NOT NULL,
    id_token_enc       BLOB    NOT NULL,
    access_expires_at  INTEGER,
    -- Goes up on every re-link. A refresh that started before a re-link does not write.
    credential_generation INTEGER NOT NULL DEFAULT 1,
    last_refresh_at    INTEGER NOT NULL,
    last_refresh_error TEXT,
    last_refresh_failed_at INTEGER,
    -- active, needs_relogin
    status             TEXT    NOT NULL DEFAULT 'active',
    is_primary         INTEGER NOT NULL DEFAULT 0,
    failover_enabled   INTEGER NOT NULL DEFAULT 0,
    failover_order     INTEGER NOT NULL DEFAULT 0,
    -- inherit, custom, disabled
    refresh_mode       TEXT    NOT NULL DEFAULT 'inherit',
    refresh_cron       TEXT,
    created_at         INTEGER NOT NULL
);

-- Models of all upstreams. source is "chatgpt" or a connection id (later).
CREATE TABLE models (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    source       TEXT    NOT NULL,
    upstream_id  TEXT    NOT NULL,
    display_name TEXT,
    enabled      INTEGER NOT NULL DEFAULT 0,
    -- JSON: input kinds, efforts, fast support, context window.
    capabilities TEXT    NOT NULL DEFAULT '{}',
    last_seen_at INTEGER NOT NULL,
    UNIQUE (source, upstream_id)
);

-- Which ChatGPT account can use which model.
CREATE TABLE chatgpt_account_models (
    account_id   INTEGER NOT NULL REFERENCES chatgpt_accounts (id) ON DELETE CASCADE,
    model_id     INTEGER NOT NULL REFERENCES models (id) ON DELETE CASCADE,
    last_seen_at INTEGER NOT NULL,
    PRIMARY KEY (account_id, model_id)
);
