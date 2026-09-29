-- The last sync of each public price list.
CREATE TABLE price_sources (
    source     TEXT PRIMARY KEY,
    fetched_at INTEGER,
    -- ok or error
    status     TEXT    NOT NULL,
    error      TEXT,
    entries    INTEGER NOT NULL DEFAULT 0
);

-- Prices of one model from one source. Rows never change (except the current flag) and are
-- never deleted, so every usage row keeps a valid reference.
CREATE TABLE price_versions (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    -- litellm, models_dev, openrouter or override
    source     TEXT    NOT NULL,
    model_key  TEXT    NOT NULL,
    -- JSON, see the `prices` module
    prices     TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    current    INTEGER NOT NULL
);

CREATE UNIQUE INDEX price_versions_current ON price_versions (source, model_key) WHERE current = 1;

-- Admin prices. An edit points to a new price version; a delete makes the override inactive.
CREATE TABLE price_overrides (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    -- `connection/model`, `chatgpt/model` or a model key such as `openai/gpt-5`
    match_key  TEXT    NOT NULL,
    active     INTEGER NOT NULL DEFAULT 1,
    version_id INTEGER NOT NULL REFERENCES price_versions (id),
    created_by INTEGER REFERENCES admins (id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX price_overrides_active ON price_overrides (match_key) WHERE active = 1;

-- The calculated cost ("API value") in nano-USD, NULL when no price is known.
ALTER TABLE usage ADD COLUMN cost_nano INTEGER;
-- True only when the usage is exact and every used category had a price.
ALTER TABLE usage ADD COLUMN cost_complete INTEGER NOT NULL DEFAULT 0;
-- JSON: nano-USD per category (the usage column names).
ALTER TABLE usage ADD COLUMN cost_parts TEXT;
ALTER TABLE usage ADD COLUMN price_version_id INTEGER REFERENCES price_versions (id);
