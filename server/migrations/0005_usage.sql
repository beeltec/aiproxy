-- One row per billing component of a request (usually one). Token columns are billing
-- categories that do not overlap (see the plan, section 10.2).
CREATE TABLE usage (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id           TEXT    NOT NULL,
    component            TEXT    NOT NULL DEFAULT 'model',
    time                 INTEGER NOT NULL,
    api_key_id           INTEGER REFERENCES api_keys (id) ON DELETE SET NULL,
    route                TEXT    NOT NULL,
    client_format        TEXT    NOT NULL,
    upstream             TEXT    NOT NULL,
    chatgpt_account_id   INTEGER REFERENCES chatgpt_accounts (id) ON DELETE SET NULL,
    requested_model      TEXT    NOT NULL,
    resolved_model       TEXT,
    effort               TEXT,
    service_tier_requested TEXT,
    service_tier_reported  TEXT,
    streamed             INTEGER NOT NULL,
    status_code          INTEGER NOT NULL,
    error_kind           TEXT,
    latency_ms           INTEGER NOT NULL,
    first_token_ms       INTEGER,
    -- reported, estimated, none
    usage_status         TEXT    NOT NULL,
    usage_exact          INTEGER NOT NULL DEFAULT 1,
    input_text           INTEGER NOT NULL DEFAULT 0,
    input_text_cached    INTEGER NOT NULL DEFAULT 0,
    input_audio          INTEGER NOT NULL DEFAULT 0,
    input_audio_cached   INTEGER NOT NULL DEFAULT 0,
    input_image          INTEGER NOT NULL DEFAULT 0,
    input_image_cached   INTEGER NOT NULL DEFAULT 0,
    cache_write_5m       INTEGER NOT NULL DEFAULT 0,
    cache_write_1h       INTEGER NOT NULL DEFAULT 0,
    output_text          INTEGER NOT NULL DEFAULT 0,
    output_reasoning     INTEGER NOT NULL DEFAULT 0,
    output_audio         INTEGER NOT NULL DEFAULT 0,
    output_image         INTEGER NOT NULL DEFAULT 0,
    web_search_calls     INTEGER NOT NULL DEFAULT 0,
    failover_attempts    INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX usage_time ON usage (time);
CREATE INDEX usage_key_time ON usage (api_key_id, time);
CREATE INDEX usage_model_time ON usage (resolved_model, time);

-- Last known usage limits of a ChatGPT account, from the response headers.
CREATE TABLE chatgpt_quota (
    account_id               INTEGER PRIMARY KEY REFERENCES chatgpt_accounts (id) ON DELETE CASCADE,
    primary_used_percent     REAL,
    primary_window_minutes   INTEGER,
    primary_reset_at         INTEGER,
    secondary_used_percent   REAL,
    secondary_window_minutes INTEGER,
    secondary_reset_at       INTEGER,
    updated_at               INTEGER NOT NULL
);

-- A usage-limit error blocks the account until this time.
ALTER TABLE chatgpt_accounts ADD COLUMN limited_until INTEGER;
