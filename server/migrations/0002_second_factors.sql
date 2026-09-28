-- A session with pending_second_factor = 1 only allows the second-factor check (for 5 minutes).
ALTER TABLE sessions ADD COLUMN pending_second_factor INTEGER NOT NULL DEFAULT 0;

-- WebAuthn user handle. Set when the admin adds the first passkey.
ALTER TABLE admins ADD COLUMN webauthn_id TEXT;
CREATE UNIQUE INDEX admins_webauthn_id ON admins (webauthn_id);

CREATE TABLE admin_totp (
    admin_id       INTEGER PRIMARY KEY REFERENCES admins (id) ON DELETE CASCADE,
    -- AES-GCM encrypted secret, associated data "admin_totp:<admin_id>".
    secret_enc     BLOB    NOT NULL,
    confirmed      INTEGER NOT NULL DEFAULT 0,
    -- Time step of the last accepted code, so a code cannot be used twice.
    last_used_step INTEGER NOT NULL DEFAULT 0,
    created_at     INTEGER NOT NULL
);

CREATE TABLE admin_recovery_codes (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    admin_id   INTEGER NOT NULL REFERENCES admins (id) ON DELETE CASCADE,
    code_hash  BLOB    NOT NULL,
    used_at    INTEGER
);

CREATE INDEX admin_recovery_codes_admin_id ON admin_recovery_codes (admin_id);

CREATE TABLE admin_passkeys (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    admin_id      INTEGER NOT NULL REFERENCES admins (id) ON DELETE CASCADE,
    name          TEXT    NOT NULL,
    credential_id BLOB    NOT NULL UNIQUE,
    -- Serialized webauthn-rs Passkey (public key and sign counter, no secrets).
    credential    TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER
);

CREATE INDEX admin_passkeys_admin_id ON admin_passkeys (admin_id);
