-- The match queue: one row per album folder that wn-server match has seen in
-- the wn_candidates output. folder is the absolute path the plugin wrote.
-- state is pending, applied, dismissed, or duplicate. The tag columns hold
-- the album's existing tags. The applied columns are set while state is
-- applied and are NULL otherwise. last_error holds the message of the last
-- error line for the folder.
CREATE TABLE match_queue (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    folder             TEXT NOT NULL UNIQUE,
    first_seen         TEXT NOT NULL,
    last_seen          TEXT NOT NULL,
    state              TEXT NOT NULL CHECK (state IN ('pending', 'applied', 'dismissed', 'duplicate')),
    artist             TEXT NOT NULL DEFAULT '',
    album              TEXT NOT NULL DEFAULT '',
    year               INTEGER NOT NULL DEFAULT 0,
    track_count        INTEGER NOT NULL DEFAULT 0,
    applied_release_id TEXT NULL,
    copied_to          TEXT NULL,
    applied_at         TEXT NULL,
    last_error         TEXT NULL
);

CREATE INDEX match_queue_state ON match_queue (state);

-- The MusicBrainz candidates of a queue entry from its latest line, in
-- beets' order. position starts at 0.
CREATE TABLE match_candidates (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    queue_id   INTEGER NOT NULL REFERENCES match_queue (id) ON DELETE CASCADE,
    position   INTEGER NOT NULL,
    release_id TEXT NOT NULL,
    artist     TEXT NOT NULL,
    album      TEXT NOT NULL,
    year       INTEGER NOT NULL,
    label      TEXT NOT NULL,
    distance   REAL NOT NULL,
    UNIQUE (queue_id, position)
);
