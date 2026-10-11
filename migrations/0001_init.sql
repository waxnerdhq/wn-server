CREATE TABLE users (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE COLLATE NOCASE,
  role        TEXT NOT NULL,                   -- 'owner' now; 'guest' later; no default
  password    TEXT NOT NULL,                   -- plain text; token auth needs it recoverable
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);

CREATE TABLE folders (
  id    INTEGER PRIMARY KEY,
  path  TEXT NOT NULL UNIQUE,                   -- absolute, components joined, no trailing separator
  name  TEXT NOT NULL
);

CREATE TABLE artists (
  id          TEXT PRIMARY KEY,
  key         TEXT NOT NULL UNIQUE,             -- normalised name
  name        TEXT NOT NULL,
  sort_name   TEXT NOT NULL,
  album_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE albums (
  id            TEXT PRIMARY KEY,
  artist_id     TEXT NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
  key           TEXT NOT NULL,                  -- normalised title
  name          TEXT NOT NULL,
  artist_name   TEXT NOT NULL,
  year          INTEGER,
  genre         TEXT,
  song_count    INTEGER NOT NULL DEFAULT 0,
  duration_ms   INTEGER NOT NULL DEFAULT 0,
  compilation   INTEGER NOT NULL DEFAULT 0,
  cover_source  TEXT,                           -- absolute image path, 'embedded:<track id>', or NULL
  cover_mtime   INTEGER,                        -- mtime of the image file, or of the track for embedded art
  created_at    INTEGER NOT NULL,               -- first seen
  UNIQUE(artist_id, key)
);
CREATE INDEX albums_key ON albums(key);
CREATE INDEX albums_created ON albums(created_at);
CREATE INDEX albums_year ON albums(year);
CREATE INDEX albums_genre ON albums(genre);

CREATE TABLE tracks (
  id             TEXT PRIMARY KEY,
  folder_id      INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
  rel_path       TEXT NOT NULL,                 -- components joined with '/'
  fingerprint    TEXT NOT NULL,
  title          TEXT NOT NULL,
  title_lc       TEXT NOT NULL,
  album_id       TEXT NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
  artist_id      TEXT NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
  artist_name    TEXT NOT NULL,                 -- track artist tag
  album_name     TEXT NOT NULL,
  track_no       INTEGER,
  disc_no        INTEGER,
  year           INTEGER,
  genre          TEXT,
  duration_ms    INTEGER NOT NULL,
  bitrate_kbps   INTEGER,
  sample_rate    INTEGER,
  bit_depth      INTEGER,
  channels       INTEGER,
  size           INTEGER NOT NULL,
  suffix         TEXT NOT NULL,
  content_type   TEXT NOT NULL,
  lossless       INTEGER NOT NULL,
  has_picture    INTEGER NOT NULL DEFAULT 0,
  has_lyrics     INTEGER NOT NULL DEFAULT 0,
  mtime          INTEGER NOT NULL,              -- unix seconds
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  UNIQUE(folder_id, rel_path)
);
CREATE INDEX tracks_album ON tracks(album_id, disc_no, track_no);
CREATE INDEX tracks_artist ON tracks(artist_id);
CREATE INDEX tracks_title_lc ON tracks(title_lc);
CREATE INDEX tracks_genre ON tracks(genre);
CREATE INDEX tracks_fingerprint ON tracks(fingerprint);

CREATE TABLE annotations (
  user_id        INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  item_id        TEXT NOT NULL,                 -- tr-, al- or ar- id; no FK
  item_type      TEXT NOT NULL,                 -- 'track' | 'album' | 'artist'
  starred_at     INTEGER,
  rating         INTEGER,                       -- 1..5 or NULL
  play_count     INTEGER NOT NULL DEFAULT 0,
  last_played_at INTEGER,
  PRIMARY KEY (user_id, item_id)
);
CREATE INDEX annotations_starred ON annotations(user_id, item_type, starred_at);

CREATE TABLE playlists (
  id          TEXT PRIMARY KEY,
  owner_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  comment     TEXT,
  public      INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);
CREATE TABLE playlist_tracks (
  playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  position    INTEGER NOT NULL,
  track_id    TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  PRIMARY KEY (playlist_id, position)
);
CREATE INDEX playlist_tracks_track ON playlist_tracks(track_id);

CREATE TABLE play_queue (
  user_id      INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  current_id   TEXT,
  position_ms  INTEGER NOT NULL DEFAULT 0,
  changed_at   INTEGER NOT NULL,
  changed_by   TEXT NOT NULL                    -- client name (c)
);
CREATE TABLE play_queue_tracks (
  user_id   INTEGER NOT NULL REFERENCES play_queue(user_id) ON DELETE CASCADE,
  position  INTEGER NOT NULL,
  track_id  TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  PRIMARY KEY (user_id, position)
);
CREATE INDEX play_queue_tracks_track ON play_queue_tracks(track_id);

CREATE TABLE scans (
  id             INTEGER PRIMARY KEY,
  started_at     INTEGER NOT NULL,
  finished_at    INTEGER,
  tracks_seen    INTEGER NOT NULL DEFAULT 0,
  tracks_added   INTEGER NOT NULL DEFAULT 0,
  tracks_changed INTEGER NOT NULL DEFAULT 0,
  tracks_moved   INTEGER NOT NULL DEFAULT 0,
  tracks_removed INTEGER NOT NULL DEFAULT 0,
  error          TEXT
);
