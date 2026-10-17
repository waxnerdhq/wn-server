-- The local rating store: the owner's ratings of MusicBrainz subjects, on
-- waxnerd's 10-point scale. kind is release or track. half_stars is 1 to 10
-- or NULL. like_level is like, love, or NULL. A rating with both values NULL
-- is not stored: clearing a rating deletes its row. updated_at is the time
-- of the last local change.
CREATE TABLE ratings (
    account_id TEXT NOT NULL REFERENCES accounts (id),
    kind       TEXT NOT NULL CHECK (kind IN ('release_group', 'release', 'track', 'artist')),
    mbid       TEXT NOT NULL,
    half_stars INTEGER NULL CHECK (half_stars BETWEEN 1 AND 10),
    like_level TEXT NULL CHECK (like_level IN ('like', 'love')),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (account_id, kind, mbid),
    CHECK (half_stars IS NOT NULL OR like_level IS NOT NULL)
);

-- Rating writes that wn-cloud has not accepted yet. One entry per subject:
-- a later change replaces the values of the earlier entry. deleted is 1 for
-- a cleared rating, which is sent as DELETE. next_at is the earliest time of
-- the next attempt, or NULL for an entry that wn-cloud rejected and that is
-- not retried. version grows with every change of the entry, so that the
-- drain does not remove or delay an entry that changed while it was being
-- sent. Times are UTC with microseconds, so they sort as text.
CREATE TABLE rating_outbox (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    kind       TEXT NOT NULL,
    mbid       TEXT NOT NULL,
    half_stars INTEGER NULL,
    like_level TEXT NULL,
    deleted    INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    next_at    TEXT NULL,
    last_error TEXT NULL,
    version    INTEGER NOT NULL DEFAULT 0,
    UNIQUE (kind, mbid)
);

CREATE INDEX rating_outbox_next_at ON rating_outbox (next_at);
