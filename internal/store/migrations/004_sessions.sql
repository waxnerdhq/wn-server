-- wn-server sessions of first-party clients. POST /api/v1/session issues a
-- session after wn-cloud has verified a device code. The client sees the
-- token once. The store keeps only its SHA-256 hash, in hex. device is the
-- client's label, or the empty string.
CREATE TABLE sessions (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES accounts (id),
    token_hash   TEXT NOT NULL UNIQUE,
    device       TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL,
    last_seen_at TEXT NOT NULL
);

CREATE INDEX sessions_account_id ON sessions (account_id);
