-- The relay settings from wn-cloud's GET /servers/:id: the edge's tunnel URL
-- (relay.connect_url), the relay host (relay.host), and the addresses list
-- as JSON text. At most one row, written by serve after each successful call
-- while relay = true. The relay package dials the stored connect_url when
-- wn-cloud is unreachable. fetched_at is the time of the call, in UTC.
CREATE TABLE relay_settings (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    connect_url TEXT NOT NULL,
    host        TEXT NOT NULL,
    addresses   TEXT NOT NULL,
    fetched_at  TEXT NOT NULL
);

-- The relay package's last state, for wn-server status. At most one row,
-- written by serve at start (off or waiting) and on every change of the
-- state. state is a relay.StateName. since is when the package entered the
-- state, host is the relay host while connected, last_error is the last
-- error text, and written_at is the time of the write. Times are UTC.
CREATE TABLE relay_state (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    state      TEXT NOT NULL,
    since      TEXT NOT NULL,
    host       TEXT NOT NULL,
    last_error TEXT NOT NULL,
    written_at TEXT NOT NULL
);
