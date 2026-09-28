CREATE TABLE api_keys (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    name              TEXT    NOT NULL,
    -- First characters of the key, shown in the dashboard to recognize it.
    prefix            TEXT    NOT NULL,
    key_hash          BLOB    NOT NULL UNIQUE,
    created_by        INTEGER REFERENCES admins (id) ON DELETE SET NULL,
    created_at        INTEGER NOT NULL,
    expires_at        INTEGER,
    revoked_at        INTEGER,
    last_used_at      INTEGER,
    -- NULL means no limit (for concurrency: the default).
    rpm_limit         INTEGER,
    tpm_limit         INTEGER,
    concurrency_limit INTEGER,
    -- JSON array of model patterns. Empty: all models.
    allowlist         TEXT    NOT NULL DEFAULT '[]'
);

-- Requests that the gateway refused before any work, counted per hour and reason.
CREATE TABLE rejected_requests (
    hour   INTEGER NOT NULL,
    reason TEXT    NOT NULL,
    count  INTEGER NOT NULL,
    PRIMARY KEY (hour, reason)
);
