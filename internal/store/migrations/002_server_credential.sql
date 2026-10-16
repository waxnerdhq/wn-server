-- The server credential: the owner's wn-cloud CLI token, the server id that
-- POST /servers returned, and the waxnerd user the token belongs to. At most
-- one row, written by wn-server login and removed by wn-server logout.
CREATE TABLE server_credential (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    token           TEXT NOT NULL,
    server_id       TEXT NOT NULL,
    waxnerd_user_id TEXT NOT NULL,
    created_at      TEXT NOT NULL
);
