-- Tombstones for cleared ratings, and the cursor of the rating reconcile.
--
-- A cleared rating stays in the table with deleted = 1 and both values NULL,
-- so that the change feed for first-party clients can report the deletion.
-- updated_at is the time of the change as the rating's owner made it: the
-- time of the local write, or the updated_at that wn-cloud reported.
-- changed_at is the time this database last wrote the row. The change feed
-- orders by changed_at and id, so a change that came from wn-cloud with an
-- old updated_at still comes after the rows a client has already read.
-- The id is the second part of the feed cursor.
CREATE TABLE ratings_new (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id TEXT NOT NULL REFERENCES accounts (id),
    kind       TEXT NOT NULL CHECK (kind IN ('release_group', 'release', 'track', 'artist')),
    mbid       TEXT NOT NULL,
    half_stars INTEGER NULL CHECK (half_stars BETWEEN 1 AND 10),
    like_level TEXT NULL CHECK (like_level IN ('like', 'love')),
    deleted    INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1)),
    updated_at TEXT NOT NULL,
    changed_at TEXT NOT NULL,
    UNIQUE (account_id, kind, mbid),
    CHECK (deleted = 1 OR half_stars IS NOT NULL OR like_level IS NOT NULL),
    CHECK (deleted = 0 OR (half_stars IS NULL AND like_level IS NULL))
);

INSERT INTO ratings_new (account_id, kind, mbid, half_stars, like_level, deleted, updated_at, changed_at)
    SELECT account_id, kind, mbid, half_stars, like_level, 0, updated_at, updated_at
    FROM ratings ORDER BY updated_at, rowid;

DROP TABLE ratings;
ALTER TABLE ratings_new RENAME TO ratings;

CREATE INDEX ratings_changed ON ratings (account_id, changed_at, id);

-- The cursor of GET /ratings?since= at wn-cloud, one row. The reconcile loop
-- sends it on its next call. A login of another waxnerd user removes it.
CREATE TABLE rating_sync (
    id     INTEGER PRIMARY KEY CHECK (id = 1),
    cursor TEXT NOT NULL
);
