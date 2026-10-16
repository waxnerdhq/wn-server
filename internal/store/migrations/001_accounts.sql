CREATE TABLE accounts (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE COLLATE NOCASE,
    app_password    TEXT NOT NULL,
    waxnerd_user_id TEXT NULL,
    created_at      TEXT NOT NULL
);
