-- Listener accounts and the cached member list (docs/relay.md, section 5).
--
-- kind is owner or listener. The owner is the account that init created.
-- A listener account is created at the first device code sign-in of a
-- member of the server, with waxnerd_user_id set to the member's user id.
-- disabled is 1 while the member is not in the cached list; wn-server then
-- refuses every session of the account.
ALTER TABLE accounts ADD COLUMN kind TEXT NOT NULL DEFAULT 'owner' CHECK (kind IN ('owner', 'listener'));
ALTER TABLE accounts ADD COLUMN disabled INTEGER NOT NULL DEFAULT 0;
CREATE UNIQUE INDEX accounts_listener_user ON accounts (waxnerd_user_id) WHERE kind = 'listener';

-- The member list of GET /servers/:id/members, replaced at each successful
-- refresh. seen_at is the time of the last refresh that held the row.
-- created_at is wn-cloud's. Times are UTC.
CREATE TABLE members (
    user_id    TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    role       TEXT NOT NULL,
    created_at TEXT NOT NULL,
    seen_at    TEXT NOT NULL
);

-- The time of the last successful refresh of the member list, in UTC. At
-- most one row. No row means no refresh has succeeded yet.
CREATE TABLE members_refresh (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    refreshed_at TEXT NOT NULL
);
