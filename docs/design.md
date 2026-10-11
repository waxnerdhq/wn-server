# wn-server design

wn-server is a headless music server. It scans a local music collection into SQLite and serves it to OpenSubsonic clients on the local network. It is one Rust binary with CLI subcommands and a TOML config file. This document describes the present design. A change to designed behaviour updates this document in the same PR.

## 1. Scope

### 1.1 Features

- One binary, `wn-server`, with subcommands `init`, `run`, `scan`, `user`, `config`.
- TOML config file in a data directory. No web UI.
- SQLite library database built by a scanner over configured music folders. Scans are incremental; moved and renamed files keep their identity.
- Tag reading with `lofty` for FLAC, MP3, M4A (AAC and ALAC), Ogg Vorbis, Opus, WAV and AIFF.
- OpenSubsonic-compatible HTTP API under `/rest/`, JSON and XML, Subsonic protocol version `1.16.1`, `openSubsonic: true`, extensions `formPost`, `transcodeOffset`, `songLyrics` (version 1).
- Auth: username and password (plain `p`, `enc:` hex, and `t`+`s` MD5 token). One owner account. `[auth] disabled_unsafe = true` turns auth off.
- Streaming of the original file with HTTP Range support. ffmpeg transcoding to Opus or MP3 when the client sends `format` or `maxBitRate`. `timeOffset` seeks inside a transcoded stream.
- Cover art from album-folder image files or embedded pictures, resized on request, cached on disk.
- Stars, ratings, play counts, playlists, play queue, lyrics.
- mDNS/DNS-SD advertisement on the LAN.
- Logging to stderr. Graceful shutdown on SIGINT and SIGTERM (Unix) and console close (Windows).
- `download` is disabled: it returns Subsonic error 50, and `getUser` reports `downloadRole: false`.
- Builds and runs on Linux, Windows and macOS.

### 1.2 Deferred

Owner upload, multiple users, waxnerd-cloud registration, relay, guests, billing, web UI, API-key authentication (`apiKeyAuthentication`, `tokenInfo`), the `transcoding` extension (`getTranscodeDecision`, `getTranscodeStream`), HLS, video, podcasts, internet radio, shares, jukebox, chat, bookmarks, `getNowPlaying` content, `getAvatar` images, artist biographies and images, similar and top songs, external scrobbling, JSONP, TLS, transcode caching, full-text search (FTS5), MusicBrainz IDs, ReplayGain, multi-artist fields, filesystem watching.

### 1.3 Decisions

- Supported audio extensions: `flac mp3 m4a aac ogg opus wav aif aiff`, matched without regard to case. Other files are ignored. The extension in lower case is the track's suffix and selects the content type: `audio/flac`, `audio/mpeg` for `mp3`, `audio/mp4` for `m4a`, `audio/aac`, `audio/ogg` for `ogg` and `opus`, `audio/wav`, and `audio/aiff` for `aif` and `aiff`. A track is lossless when the suffix is `flac`, `wav`, `aif` or `aiff`, and when the suffix is `m4a` and the codec is ALAC.
- Tag reading (`library::tags`): lofty detects the format from the file content, and from the extension when the content does not decide it, so an Opus stream in a `.ogg` file is read. Fields come from the tag of the format's primary type, or from the first tag when the file has none. A missing title is the file stem, a missing artist is `[Unknown Artist]` and a missing album is `[Unknown Album]`. The year is the leading four digits of the recording date, or of the year tag when the date has none; `0000` is no year. A track or disc number such as `3/12` is 3. The values `1` and `true` set the compilation flag. A file that lofty cannot parse yields an error and no metadata.
- Library model: artists, albums, songs (the ID3 model). Folder-style browsing (`getIndexes`, `getMusicDirectory`) is simulated from the same data. One music folder per configured directory.
- Album identity: (album artist, album title), normalised (§3.3). Album artist falls back to the track artist. When the `compilation` flag is set and no album-artist tag exists, or the album artist is "Various Artists" (case-insensitive), the album artist is "Various Artists". A track whose album artist is "Various Artists" has the compilation flag. Disc numbers do not split albums.
- Artist identity: the album artist name, normalised. Track artists that differ from the album artist are stored as text on the track and do not create artist rows.
- Track identity is a surrogate ID assigned once. The scanner matches a vanished track to a newly seen file by fingerprint, so a rename, a move between directories, a move between music folders, and a change of a music folder's path keep the track's ID and everything attached to it. A retag that changes the file's first or last 64 KiB in the same scan interval as a move is treated as a deletion plus a new track (§3.3).
- Album and artist rows have surrogate IDs looked up by natural key. Retagging an album title or album artist makes a new album (and artist) row with a new ID; annotations attached to the old album or artist ID are deleted when the row disappears. Track annotations are unaffected.
- A case-only rename on a case-insensitive filesystem (macOS, Windows) is a rename and matches by fingerprint.
- Scans run at startup when `library.scan_on_start = true` (default), on `startScan`, on `wn-server scan`, and every `library.rescan_interval_minutes` when set.
- A music folder whose root cannot be read, or that previously held tracks and now yields no supported file, never causes deletions; the scan records an error naming the folder.
- Cover art sources, in order: a file in the album's directory named `cover`, `folder`, `front` or `album` with extension `jpg`, `jpeg`, `png` or `webp` (case-insensitive); then the embedded picture of the album's first track. Artists have no image.
- `getCoverArt?size=N` returns a JPEG whose longer side is at most N pixels (N clamped to 2000). Without `size`, the original bytes and content type are returned.
- A track has lyrics when its file holds a lyrics tag (`USLT` in ID3v2, `©lyr` in MP4, `UNSYNCEDLYRICS` or `LYRICS` in Vorbis comments) or the file `<stem>.lrc` exists next to it.
- Annotations (star, rating, play count) are per user and keyed by item ID in one table.
- Playlists belong to the owner. `public` is stored and has no effect with one user.
- One play queue per user; `savePlayQueue` replaces it.
- `scrobble` with `submission=true` (default) increments the play count and sets last played. `submission=false` is accepted and does nothing. Nothing is forwarded anywhere.
- Transcode targets: `opus` (libopus in Ogg, `audio/ogg`) and `mp3` (libmp3lame, `audio/mpeg`). `format=raw`, or no `format` and no `maxBitRate`, serves the original. `maxBitRate` alone selects Opus. Another `format` value returns error 0. Bitrates are clamped to 32–320 kbps.
- Transcoded streams are not cached and do not support Range (`Accept-Ranges: none`). Seeking uses `timeOffset`.
- Transcode concurrency: `transcoding.max_concurrent` (default 2). A request past the limit waits up to 5 s, then returns HTTP 503 with `Retry-After: 5`.
- The owner password is stored as plain text in the database. Token auth (`t = md5(password + s)`) requires a recoverable password. Anyone who can read the data directory can read the password, so it must not be reused anywhere else. On Unix, `init` creates the data directory with mode 0700 and the database with mode 0600; on Windows, the user-profile ACL protects the directory.
- `init` and `user set-password` generate the password by default and print it once: four groups of five characters separated by hyphens (`xxxxx-xxxxx-xxxxx-xxxxx`), drawn from the operating system's random generator (`rand::rngs::SysRng`) from the 31-character alphabet `abcdefghjkmnpqrstuvwxyz23456789` (no `i`, `l`, `o`, `0`, `1`), which gives 99 bits of entropy and types easily on a phone. Random bytes of 248 and above are discarded, so every character is equally likely. The owner may supply a password instead through `--password-stdin` (the first line of stdin, without its line ending; an empty line is an error) or the `WN_SERVER_PASSWORD` environment variable (an empty value counts as unset); the flag wins over the variable. A supplied password is not printed. There is no interactive prompt.
- A password is never logged and never part of an error message or of a `Debug` output. The types that hold one (`cli::PasswordSource`, `users::UserRow`) leave it out of their `Debug` output.
- Default port 4200, bind `0.0.0.0`.
- Search is case-insensitive substring match (SQL `LIKE` with `ESCAPE '\'`) over lower-cased name columns. An empty query returns everything, paged.
- API errors use HTTP 200 and the Subsonic error envelope. Unknown `/rest/*` methods return error 0 with message `unknown endpoint`. Paths outside `/rest/` return 404.
- `v` and `c` are required (error 10 when absent). The value of `v` is not compared; errors 20 and 30 are never produced.
- Music folder paths are absolute (`std::path::absolute`, no symlink resolution, no `\\?\` prefix). Symlinks inside folders are followed. Entries whose name starts with `.` are skipped on every OS; the Windows hidden attribute is not checked. A file name that is not valid Unicode is skipped with a warning.
- `jsonp` is not supported; `f=jsonp` returns error 0 in XML.

## 2. Platforms and what CI verifies

| OS | Build and run | Verified by |
|---|---|---|
| Linux | native | CI: `cargo xtask check` (fmt, clippy `-D warnings`, full test suite) in a `rust:1.98.1-bookworm` container on the Woodpecker Kubernetes agent |
| Windows | native (MSVC or GNU host toolchain; `rusqlite` `bundled` needs the Visual Studio Build Tools or mingw) | CI: `cargo clippy --locked --all-targets --target x86_64-pc-windows-gnu -- -D warnings` in the same container with `gcc-mingw-w64-x86-64`; compiles every crate, every test and every build script for Windows. Tests do not run in CI. Native test runs are a manual step recorded in the end-to-end verification until a Windows agent exists |
| macOS | native | Not verified by CI. Developers work on macOS and run `cargo xtask check` before every push; that run is the macOS verification |

Woodpecker has one Linux Kubernetes agent, container steps and no build cache. Packages installed in one step are gone in the next, so each pipeline step installs what it needs.

Portability rules in code:

- Signals: `tokio::signal::ctrl_c()` everywhere; `#[cfg(unix)]` SIGTERM; `#[cfg(windows)]` `ctrl_break`, `ctrl_close`, `ctrl_shutdown`.
- File modes: `#[cfg(unix)]` only (`std::os::unix::fs::PermissionsExt`). Tests of modes are `#[cfg(unix)]`.
- Relative paths are built from `Path::components()` joined with `/`, stored and compared in that form on every OS. The absolute path is rebuilt with `folder.join(components)`.
- Folder paths in the config are written as TOML literal strings (`'C:\Music'`) so backslashes survive. A literal string cannot hold a single quote or a control character other than tab, so a folder path with one of those is written in the string form the `toml` crate chooses for it: `"/music/Guns N' Roses"` is a basic string, and `'''C:\Music\Guns N' Roses'''` is a multi-line literal string because the path also has a backslash. Every form reads back as the same path. `transcoding.ffmpeg` is written in the form the `toml` crate chooses, which is a literal string when the value has a backslash and no single quote.
- `transcoding.ffmpeg` is a program name or a path; Rust resolves `ffmpeg` to `ffmpeg.exe` through `PATH` on Windows. Arguments are passed as argv, never through a shell, so paths with spaces work.
- `kill_on_drop` is `TerminateProcess` on Windows and SIGKILL on Unix.
- Atomic cache writes use temp file plus rename. On Windows the rename fails with a sharing violation while another request reads the destination; a failed rename whose destination exists is treated as success.
- `.gitattributes`: `* text=auto eol=lf`, `tests/fixtures/audio/* binary`.
- Checks run through `cargo xtask check` on every OS; `xtask` invokes the cargo named by `env!("CARGO")` so the toolchain that runs it runs the checks. `scripts/check.sh` is a one-line wrapper for Unix and CI.

## 3. Architecture

### 3.1 Crate layout

A Cargo workspace with a root package `wn-server` (library target plus one binary) and one helper crate `xtask`. The library target exists so tests exercise the router in-process. `xtask` exists so the checks have one definition that runs on every OS.

```
wn-server/
  Cargo.toml  Cargo.lock  rust-toolchain.toml  LICENSE  README.md
  AGENTS.md  CLAUDE.md  .linear.toml  .gitignore  .gitattributes
  .cargo/config.toml          alias xtask = "run -q --locked -p xtask --"
  .claude/agents/{planner,implementer,reviewer}.md
  .woodpecker/check.yaml      Linux check + Windows cross clippy
  .woodpecker/release.yaml    release binaries on a tag
  docs/design.md              this document
  xtask/                      `cargo xtask check`
  scripts/check.sh            `exec cargo xtask check`
  scripts/make-fixtures.sh    maintainer tool, regenerates tests/fixtures/audio/*
  migrations/0001_init.sql    embedded with include_str!
  tests/fixtures/audio/       five tiny audio files
  tests/common/mod.rs         LibraryBuilder, TestServer
  tests/*.rs                  router-level integration tests
  examples/e2e.rs             end-to-end session against a running server
  examples/make-library.rs    writes the standard test library to a directory
  src/
    main.rs                   clap entry
    lib.rs                    module tree, build_router, run_server
    cli.rs                    init, run, scan, user, config
    config.rs                 Config, defaults, validation, DataDir
    db/mod.rs                 Db handle, migrations, pragmas
    db/queries/*.rs           tracks, albums, artists, annotations, playlists, play_queue, users, scans, folders
    ids.rs                    ID prefixes, parse, new_id, natural keys, fingerprint
    library/tags.rs           lofty wrapper -> TrackMeta, pictures, lyrics
    library/scanner.rs        walk, diff, move detection, upsert, delete, aggregate
    library/covers.rs         cover resolution, resize, disk cache
    auth/mod.rs               verify password and token
    auth/extract.rs           Authed(User) extractor
    api/mod.rs                Router, /rest/{method} table, AppState
    api/params.rs             Params multimap and typed getters
    api/envelope.rs           SubsonicResponse, Format, ApiError
    api/xml.rs                serde_json::Value -> Subsonic XML
    api/model.rs              Child, AlbumID3, ArtistID3, constructors
    api/system.rs             ping, getLicense, getOpenSubsonicExtensions
    api/browse.rs             getArtists, getArtist, getAlbum, getSong, getMusicFolders, getGenres
    api/folders.rs            getIndexes, getMusicDirectory
    api/lists.rs              getAlbumList(2), getRandomSongs, getSongsByGenre, getStarred(2)
    api/search.rs             search2, search3
    api/annotate.rs           star, unstar, setRating, scrobble
    api/playlists.rs
    api/queue.rs              getPlayQueue, savePlayQueue
    api/lyrics.rs             getLyrics, getLyricsBySongId
    api/users.rs              getUser, getUsers
    api/stubs.rs              empty-result stubs for deferred endpoints
    api/media.rs              stream, download (disabled)
    api/covers.rs             getCoverArt
    api/scan.rs               startScan, getScanStatus
    library/lyrics.rs         LRC parsing
    media/stream.rs           StreamRequest, StreamPlan, decide(), raw file response
    media/transcode.rs        Transcoder trait, FfmpegTranscoder, FakeTranscoder
    discovery.rs              mdns-sd registration
    server.rs                 state, router, listener, shutdown
```

### 3.2 Dependencies

All dependencies are declared in the root `Cargo.toml` from the start and `Cargo.lock` is committed; a feature issue adds no dependency. Every crate compiles on Rust 1.98.1. The project is Apache-2.0; every dependency has a compatible licence. The resolved tree has no GPL, LGPL or AGPL crate. `option-ext` (MPL-2.0, through `directories`) is used unmodified as a dependency, which the MPL permits.

| Crate | Version | Licence | Role |
|---|---|---|---|
| axum | 0.8 | MIT | HTTP router, extractors, streaming bodies |
| tokio | 1.53 (`full`) | MIT | runtime, process, signals |
| tokio-util | 0.7 (`io`) | MIT | `ReaderStream`, `CancellationToken` |
| tower (`util`), tower-http | 0.5 / 0.7 (`fs`, `trace`) | MIT | `ServiceExt::oneshot`; `ServeFile` with Range and ETag; request tracing |
| http-body-util, bytes, futures-util | 0.1 / 1 / 0.3 | MIT, MIT/Apache-2.0 | body plumbing |
| rusqlite | 0.40 (`bundled`) | MIT | SQLite 3.53 on every platform |
| lofty | 0.25 | MIT/Apache-2.0 | tags, properties, pictures, lyrics; pure Rust |
| quick-xml | 0.42 | MIT | XML writer with escaping |
| mdns-sd | 0.21 | MIT/Apache-2.0 | DNS-SD responder; macOS, Linux, Windows |
| toml, serde (`derive`), serde_json | 1 | MIT/Apache-2.0 | config and models |
| clap | 4.6 (`derive`, `env`) | MIT/Apache-2.0 | CLI |
| gethostname | 1 | Apache-2.0 | default instance name |
| md-5 | 0.11 | MIT/Apache-2.0 | Subsonic token check |
| sha2, hex | 0.11 / 0.4 | MIT/Apache-2.0 | fingerprints |
| rand | 0.10 | MIT/Apache-2.0 | surrogate IDs, generated passwords, the instance UUID |
| unicode-normalization | 0.1 | MIT/Apache-2.0 | natural keys |
| image | 0.25 (`jpeg`, `png`, `webp` only) | MIT/Apache-2.0 | cover resize |
| walkdir | 2 | MIT/Unlicense | scanner |
| directories | 6 | MIT/Apache-2.0 | default data directory |
| mime_guess | 2 | MIT | content types |
| tracing, tracing-subscriber (`env-filter`) | 0.1 / 0.3 | MIT | logging |
| thiserror | 2 | MIT/Apache-2.0 | errors |
| dev: tempfile 3 | | MIT/Apache-2.0 | temp directories in tests |

Two small helpers are written in the crate rather than added as dependencies: the instance UUID is formatted from 16 random bytes with the version-4 and variant bits set, and the `filename*=UTF-8''` percent-encoding for `Content-Disposition` is a short function over the unreserved set.

`sqlx` is not used: an async API adds nothing for a single-writer embedded database, its query macros need a database at compile time, and its tree is larger. `symphonia` is not used: ffmpeg decodes; `lofty` reads tags. `argon2` is not used: the password must be recoverable.

ffmpeg is an external program found on `PATH` or named by `transcoding.ffmpeg`. The project does not link, vendor or bundle ffmpeg or its libraries, so the licence of a user's ffmpeg build does not reach this project's licence boundary.

### 3.3 Identity

IDs are opaque strings of a two-letter prefix, a hyphen and 32 lower-case hex characters: `tr-`, `al-`, `ar-`, `pl-`. `ids::parse` accepts exactly that shape and returns the kind and the 32 hex characters; anything else is rejected before it reaches SQL or the filesystem. Music folder IDs are the integer `folders.id`, which Subsonic clients expect as a number. Cover-art IDs are an album ID (album art) or a track ID (embedded art).

- Track: `tr-` + 16 random bytes, assigned on first sight. `(folder_id, rel_path)` is the locator; it is unique and it changes when the file moves.
- Fingerprint: `size` in bytes, a colon, and the first 16 bytes hex of SHA-256 over the first 64 KiB of the file followed by the last 64 KiB (the whole file when it is 128 KiB or smaller). A pure move, rename or copy keeps the fingerprint. A tag edit changes it, because tags live in the head (ID3v2, Vorbis comments, MP4 atoms) or the tail (ID3v1, APE). Two identical files share a fingerprint.
- Artist key: album artist name, Unicode NFC, trimmed, lower-cased. Artist: `ar-` + random, `UNIQUE(key)`.
- Album key: album title, NFC, trimmed, lower-cased. Album: `al-` + random, `UNIQUE(artist_id, key)`.
- Playlist: `pl-` + random.

### 3.4 SQLite schema

Pragmas: `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000`. Migrations are numbered SQL files applied in one transaction each, tracked with `PRAGMA user_version`. `Db::open` returns an error for a database whose `user_version` is negative or greater than the number of migrations in the build.

`Db` holds one writer `Connection` behind a `tokio::sync::Mutex` and four reader connections, each opened read-only behind its own `tokio::sync::Mutex`. Async callers use `db.read(|conn| ..)` and `db.write(|conn| ..)`, which run the closure on `spawn_blocking`. Blocking-thread callers (the scanner) use `db.with_writer(|conn| ..)` and `db.with_reader(..)`, which take the lock with `blocking_lock`. The closures return `Result<T, DbError>`. `db.close().await` waits for the running reads and writes of the handle and runs `PRAGMA wal_checkpoint(TRUNCATE)`; it returns an error when a connection outside the handle blocks the checkpoint. The connections close when the last clone of the handle is dropped. Tests use `Db::open_memory()` (one shared connection) or a temp file.

```sql
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
```

Genres are derived with `GROUP BY genre`. Cascades from `tracks` to `playlist_tracks` and `play_queue_tracks` are correct because a track row is deleted only when its file is gone and no new file carries its fingerprint, and never for a folder that is unreadable or newly empty. Both child tables index `track_id`, so the deletion of a track does not scan them.

Multi-user readiness: every per-user table carries `user_id`; `users.role` separates the owner from future guests and has no default, so every insert names the role; the CLI finds the owner with `WHERE role = 'owner'`.

### 3.5 Data directory and config

Data directory, in order: `--data-dir`, `WN_SERVER_DATA_DIR`, then `directories::ProjectDirs::from("", "", "wn-server").data_dir()`: `~/Library/Application Support/wn-server` on macOS, `~/.local/share/wn-server` on Linux, `C:\Users\<user>\AppData\Roaming\wn-server\data` on Windows. An empty `WN_SERVER_DATA_DIR` counts as unset. The chosen path is made absolute with `std::path::absolute`.

```
<data-dir>/
  config.toml
  library.db  (+ -wal, -shm)
  instance.id          UUID string created by init; the mDNS TXT id
  cache/covers/        <coverId>-<size>-<coverMtime>.jpg
```

`config.toml` with defaults (`init` writes it with comments):

```toml
[server]
bind = "0.0.0.0"
port = 4200
name = "<hostname>"             # instance name shown by discovery
log_level = "info"              # RUST_LOG overrides when set

[auth]
disabled_unsafe = false         # true turns authentication off; unsafe

[library]
folders = ['/Users/adnan/Music'] # TOML literal strings; 'C:\Music' on Windows
scan_on_start = true
rescan_interval_minutes = 0     # 0 disables periodic rescans

[transcoding]
ffmpeg = "ffmpeg"               # program name or path
max_concurrent = 2
opus_bitrate_kbps = 192         # used when format=opus and maxBitRate is absent
mp3_bitrate_kbps = 320          # used when format=mp3 and maxBitRate is absent

[discovery]
mdns = true
```

Validation at load: at least one folder; each folder exists and is a directory; `port != 0`; `max_concurrent >= 1`; bitrates 32–320. Unknown keys are an error (`deny_unknown_fields`). Internal timing values (transcode permit wait, scan poll) are fields of the config structs with defaults but are not config keys; tests set them.

### 3.6 CLI

```
wn-server init  [--data-dir D] [--folder PATH]... [--username NAME] [--port N] [--password-stdin]
                creates the data directory (0700 on Unix), config.toml, instance.id,
                the database (0600 on Unix) and the owner account; generates the
                password and prints it once, unless --password-stdin or
                WN_SERVER_PASSWORD supplies one
wn-server run   [--data-dir D]                 runs the server in the foreground
wn-server scan  [--data-dir D] [--full]        runs one scan and exits; --full re-reads every tag
wn-server user show                            prints the owner's name, role and creation date (UTC)
wn-server user set-password [--username NAME] [--password-stdin]
                generates a new password and prints it once, or stores the supplied one
wn-server config path                          prints the resolved config path
wn-server config show                          prints the effective config
wn-server --version
```

`--data-dir` is accepted before and after the subcommand. `init` checks that each `--folder` is an existing directory and writes it as an absolute path. Without `--folder` it writes `folders = []`, and the commands that load the config fail until the owner adds a folder. `init` does not overwrite an existing `config.toml`. On Unix it creates the directories that are missing with mode 0700 and leaves the mode of an existing data directory unchanged. `--port 0` is rejected.

`init` works in this order: it checks the folders, refuses an existing `config.toml`, obtains the password, creates the data directory, writes `config.toml`, writes `instance.id` when the file is missing, opens the database and inserts the owner. `--username` names the owner; the default is `owner`. The insert does nothing when an owner row exists, and `init` then fails. When a step after the config write fails, `init` removes the `config.toml` it wrote, so the command can run again. An existing `instance.id` is kept.

`init` and `user set-password` print `Password for <name> (shown once): <password>` when they generated the password, and then `The password is stored in plain text in the data directory. Do not reuse it elsewhere.` Both checkpoint the write-ahead log with `Db::close` before they print, so the stored password is on disk; a checkpoint that another process blocks is not an error.

`user set-password --username NAME` selects the account by name; without the option the account is the owner. The `user` commands fail when `library.db` does not exist and do not create it.

A command that fails prints `wn-server: <error>` to stderr and exits with 1. A usage error exits with 2.

`scan` while `run` is active works through WAL and `busy_timeout`; `getScanStatus` reports only scans the server process started.

### 3.7 Authentication

`auth::verify(user, params) -> Result<(), ApiError>`:

- `p` present, `t` and `s` absent: compare with the stored password (constant time); `enc:` prefix means hex-decoded. A value with the prefix and invalid hex matches nothing (`auth::verify_password`).
- `t` and `s` present, `p` absent: `md5(password + s)` as lower-case hex equals `t`, compared in constant time; upper-case hex does not match (`auth::verify_token`).
- `apiKey` present: error 42. `apiKey` with `u`: error 43. `p` with `t`: error 43. Nothing: error 10. Unknown user or wrong credential: error 40.

`Authed(User)` is an axum extractor over `Params`. With `auth.disabled_unsafe = true` it returns the owner without checks and `run` logs a warning at startup. `getOpenSubsonicExtensions` is public. Failed attempts are logged with username and client, never the credential.

Remote-mode seam: `users.role` and `auth::verify` are the only places that turn parameters into a `User`. Guests and API keys extend that function and add a table.

### 3.8 Request pipeline and envelope

- Routes `/rest/{method}` and `/rest/{method}.view` for GET and POST, one route with a path parameter. A static method table maps every name in §4 to a handler; names not yet implemented map to `not_implemented`, which returns error 0 with message `not supported`. Unknown names return error 0 with `unknown endpoint`. Paths outside `/rest/` return 404.
- `AppState { config, db, covers, transcoder, scan, cancel, started_at }` is shared through `State`.
- `Params` is a multimap of query-string and form-body fields (`application/x-www-form-urlencoded` on POST; body values append to query values). `req_str`, `opt_i64`, `opt_bool`, `all(name) -> Vec<&str>`. A missing required parameter maps to error 10 naming the parameter.
- Handlers return `Result<Payload, ApiError>`; `Payload` is a key plus a serialisable value, or empty. `Format` is parsed from `f` before dispatch so errors use the requested format: `json` selects JSON, and `xml` or an absent `f` selects XML. Any other value, `jsonp` included, returns error 0 with message `unsupported format`, rendered in XML.
- JSON envelope: `{"subsonic-response": {"status":"ok","version":"1.16.1","type":"wn-server","serverVersion":"<crate version>","openSubsonic":true, "<key>": ...}}`, with content type `application/json`. XML: the declaration `<?xml version="1.0" encoding="UTF-8"?>` followed by `<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="wn-server" serverVersion="..." openSubsonic="true"><key .../></subsonic-response>`, with content type `application/xml`. The envelope fields are written in this order in both formats.
- XML rendering: the payload is serialised to `serde_json::Value` and walked: scalars become attributes, objects become child elements, arrays become repeated child elements named after the key, and a scalar field named `value` becomes text content. A scalar array item becomes an element with the item as text content (`"versions": [1]` renders `<versions>1</versions>`). Attribute values and text are escaped. Characters outside the XML 1.0 `Char` production (U+0000 to U+001F other than tab, line feed and carriage return; U+FFFE; U+FFFF) are dropped from attribute values and text, because a strict parser rejects a document that holds one, escaped or not. One set of `#[derive(Serialize)] #[serde(rename_all = "camelCase")]` models serves both formats. `None` fields are skipped; empty arrays are `[]` in JSON and absent in XML.
- Payload key order: `serde_json` is built without `preserve_order`, so the keys of every payload object are sorted by their bytes in both formats (upper-case letters sort before lower-case letters). In XML that is the order of the attributes and of the child elements. Array items keep their order.
- Errors: HTTP 200, `status: "failed"`, `error: {code, message}`. `ApiError` variants: `MissingParam` 10, `BadCredentials` 40, `AuthUnsupported` 42, `AuthConflict` 43, `Forbidden` 50, `NotFound` 70, `Other(msg)` 0.
- `EXTENSIONS: &[(&str, &[u32])]` holds the advertised extensions, one entry per line.
- Request log: method name, `c`, `u`, status, duration; never the query string. `stream` and `getCoverArt` log at `debug`.
- Binary endpoints (`stream`, `getCoverArt`) bypass the envelope on success and use it on error.
- Model constructors `model::child(&Track, Option<&Annotation>)`, `model::album(..)`, `model::artist(..)` are the only way a handler builds a response object; the caller passes the user's annotations from `annotations::get_many(user, ids)`.

### 3.9 Streaming and transcoding

`media::stream::decide(&Track, &StreamRequest, &config::Transcoding) -> Result<StreamPlan, ApiError>` with `StreamPlan::Raw` or `StreamPlan::Transcode { target, bitrate_kbps, offset_s }`:

- `format=raw` → Raw. No `format` and `maxBitRate` absent or 0 → Raw.
- `format=opus|mp3` → Transcode at `maxBitRate` when given, else the config bitrate for that target.
- `maxBitRate > 0` and no `format` → Transcode to Opus at `maxBitRate` when the source is lossless or its bitrate exceeds `maxBitRate`; else Raw.
- Bitrates are clamped to 32–320. Any other `format` → error 0.

Remote-mode seam: remote mode adds a connection-kind argument to `decide` (forcing Opus 192 unless the connection is direct) and a second listener in `server::run` that tags its connections. In local mode there is one `TcpListener` and no connection kind.

Raw: the handler builds a fresh GET request carrying only `Range`, `If-Range`, `If-Modified-Since` and `If-None-Match` from the client request and calls `tower_http::services::ServeFile::new_with_mime(path, content_type)`, which serves with `Accept-Ranges: bytes`, `Content-Length`, `Last-Modified`, `ETag` and 206 for ranges. A 404 from `ServeFile` (file removed after the lookup) becomes error 70. `Content-Disposition: inline; filename="<ascii file name>"; filename*=UTF-8''<encoded file name>` uses the file name from `rel_path`.

Transcode: `Transcoder` is a trait with `async fn spawn(&self, job: TranscodeJob) -> Result<TranscodeStream, TranscodeError>`. `FfmpegTranscoder` runs

```
ffmpeg -nostdin -v error [-ss <offset>] -i <path> -map 0:a:0 -vn -c:a libopus -b:a <N>k -f ogg -
ffmpeg -nostdin -v error [-ss <offset>] -i <path> -map 0:a:0 -vn -c:a libmp3lame -b:a <N>k -f mp3 -
```

through `tokio::process::Command` with `stdout` piped, `stderr` to null (piped to a `debug` log task when the log level is `debug`), `kill_on_drop(true)`. The body is `Body::from_stream(ReaderStream::new(stdout))` in a wrapper that owns the `Child` and the semaphore permit; its `Drop` calls `start_kill()` and spawns `wait()` so the process is reaped promptly. Headers: the target `Content-Type`, `Accept-Ranges: none`, `Cache-Control: no-store`, and `Content-Length` only with `estimateContentLength=true` (bitrate × remaining duration ÷ 8; the wrapper truncates at that length, and a shorter stream ends the connection early). A `Range` header is ignored. The permit wait is 5 s, then HTTP 503 with `Retry-After: 5`. At startup `ffmpeg -version` is run once; failure logs a warning and transcode requests return error 0 `ffmpeg not available`. `Child.transcodedContentType` and `transcodedSuffix` are not set because the server does not transcode by default.

No transcode cache: local streams serve originals by default and transcodes happen only on request.

### 3.10 Scanner

`library::scanner::scan(db, folders, cancel, full, options) -> Result<ScanSummary>` runs on one blocking thread. `ScanOptions { batch_size: usize, on_batch: Option<Box<dyn Fn() + Send>> }` defaults to 500 and `None`; tests set a small batch and a hook that runs after each batch, which makes cancellation deterministic.

1. Insert a `scans` row.
2. Load every existing track of every folder into a map keyed by `(folder_id, rel_path)` with `(id, mtime, size, fingerprint)`.
3. For each folder, `walkdir` with `follow_links(true)`; skip entries whose name starts with `.`, names that are not valid Unicode (warning), and unsupported extensions. If the root cannot be read, mark the folder `failed` and continue with the next folder. For each file, build `rel_path` from components. Existing and unchanged (`mtime`, `size`) → mark seen in memory. Existing and changed → queue as a changed path. New path → queue as a candidate.
4. Fingerprints and tags are read in batches of `batch_size` files, one file open per file, before each write transaction, so the writer is held for the upserts only. A changed path whose fingerprint equals the stored one keeps its row and has its tags re-read in place. A changed path whose fingerprint differs makes its stored row missing and the file a candidate. Missing = existing − seen − rows of `failed` folders, plus the rows displaced by changed paths. Matching is missing-driven: for each missing track, among candidates with the same fingerprint choose the one with the same file name, then the shortest path distance, then lexical order; a match relinks the row (`folder_id`, `rel_path`, `mtime`, `size`; tags re-read only when `mtime` or `size` differ) and counts as moved. Candidates without a match become new rows with new IDs. Two files that exchange paths in one interval are two changed paths with foreign fingerprints, so they become two missing rows and two candidates and keep their identities.
5. Deletion: remaining missing tracks are deleted, except in a folder that `failed` or that previously had tracks and yielded no supported file this scan; such a folder is named in `scans.error` and nothing in it is deleted.

First-scan cost: the fingerprint reads up to 128 KiB per new file (12.8 GB for 100 000 files), which is acceptable on a local disk and slow on a network share; later scans fingerprint only new and changed files.
6. Aggregate: delete albums without tracks, then artists without albums; recompute `song_count`, `duration_ms`, `year` (minimum), `genre` (most common), `artists.album_count`; for albums touched by this scan set `cover_source` and `cover_mtime` and remove stale `cache/covers/<album id>-*` files when `cover_mtime` changed. Delete `annotations` whose `item_id` no longer exists.
7. Close the `scans` row with counts, or with `error` when cancelled or failed.

Cancellation is checked between batches. `ScanController` in `AppState` runs at most one scan at a time; `startScan` during a scan returns the running status. A periodic task starts a scan every `rescan_interval_minutes` when the value is non-zero.

Folder sync at `run` and `scan`: folders from config are upserted by path. The stored path is rebuilt from `Path::components()`, so a trailing separator, a `.` component or a repeated separator does not make a second row for one directory; `..` components are kept, as `std::path::absolute` keeps them. The name of a folder is the last component of its path (the whole path for a root such as `/` or `C:\`). A folder removed from config has its tracks deleted (tracks of a folder whose path changed are relinked by fingerprint into the new folder row before the old row is removed, because deletion happens after matching).

### 3.11 Cover art

`library::covers::Covers { cache_dir, db }` with `get(id, size: Option<u32>) -> Result<Option<Cover>>`:

- `al-` → `albums.cover_source`; `tr-` → the track's embedded picture when `has_picture`. Any other ID shape → `None`.
- The embedded picture of a track is its first picture of the type front cover, or its first picture when none has that type; M4A stores no picture type. Its MIME type is the one the tag names when that is JPEG, PNG, TIFF, BMP or GIF. Otherwise the type is detected from the first bytes of the picture, and it is `application/octet-stream` when they are not a known image format.
- No `size`: file bytes with the type from the extension, or the embedded picture with its MIME.
- `size`: look up `cache/covers/<id>-<size>-<cover_mtime>.jpg`; on miss decode with `image`, `thumbnail(size, size)`, encode JPEG quality 85, write temp file plus rename, return. Decode and encode run on `spawn_blocking`. A corrupt image logs a warning and returns `None` for sized requests and raw bytes for unsized ones.

`getCoverArt` sends `Cache-Control: public, max-age=86400` and `ETag: "<id>-<size>-<cover_mtime>"`, and answers `If-None-Match` with 304.

### 3.12 mDNS

No DNS-SD type exists for Subsonic servers; the server registers its own and the mobile app browses for it.

- Type `_waxnerd._tcp.local.`; instance name `server.name`; host `<hostname>.local.`; port `server.port`.
- TXT: `txtvers=1`, `api=opensubsonic`, `path=/rest`, `id=<instance.id>`, `name=<server.name>`, `auth=password|none`, `tls=0`, `ver=<server version>`.
- `mdns-sd` `ServiceDaemon` with `enable_addr_auto()`. When `server.bind` is an IPv4 address the daemon disables IPv6 interfaces (`disable_interface(IfKind::IPv6)`), so no AAAA record points at an address the server does not listen on; when `bind` is `::` or an IPv6 address both families are advertised. `unregister` and `shutdown` on graceful stop send a goodbye packet. Windows Firewall prompts on first start for TCP 4200 and UDP 5353.

### 3.13 Logging and shutdown

`tracing_subscriber::fmt` to stderr with an env filter from `RUST_LOG`, else `server.log_level`. No log files.

One `CancellationToken` in `AppState`. `ctrl_c`, SIGTERM (Unix) and `ctrl_close`/`ctrl_shutdown` (Windows) cancel it. `axum::serve(..).with_graceful_shutdown(token.cancelled())` with a 10 s grace, after which the process exits. On cancel: the scanner stops between batches, the mDNS daemon unregisters, transcode children die with their bodies, and the database runs `PRAGMA wal_checkpoint(TRUNCATE)`.

### 3.14 Remote-mode seams

1. `server::run` owns the single `TcpListener`; remote mode adds a TLS-terminating listener fed by the relay and tags its connections.
2. `media::stream::decide` gains a connection-kind argument; it is the only function that decides lossy versus lossless.
3. `users.role` and `auth::verify` extend to guests and API keys.
4. The cloud client attaches as a new module with its own background task started from `server::run`.
5. `annotations`, `playlists` and `play_queue` already carry `user_id`.

## 4. OpenSubsonic coverage

Sources: OpenSubsonic API (https://opensubsonic.netlify.app/docs/opensubsonic-api/), API reference (https://opensubsonic.netlify.app/docs/api-reference/), error codes (https://opensubsonic.netlify.app/docs/responses/error/), Subsonic 1.16.1 (https://www.subsonic.org/pages/api.jsp), Navidrome coverage (https://www.navidrome.org/docs/developers/subsonic-api/), client sources: Supersonic (https://github.com/dweymouth/supersonic/blob/main/backend/mediaprovider/subsonic/subsonicmediaprovider.go), Amperfy (https://github.com/BLeeEZ/amperfy/blob/develop/AmperfyKit/Api/Subsonic/SubsonicServerApi.swift), Feishin (https://github.com/jeffvli/feishin/blob/development/src/renderer/api/subsonic/subsonic-controller.ts), Symfonium requirements (https://docs.symfonium.app/wiki/providers/subsonic-opensubsonic-media-provider-configuration/).

Amperfy calls `ping, getOpenSubsonicExtensions, getMusicFolders, getIndexes, getMusicDirectory, getGenres, getArtists, getArtist, getAlbum, getSong, getStarred2, getAlbumList2, getRandomSongs, search3, getPlaylists, getPlaylist, createPlaylist, updatePlaylist, deletePlaylist, scrobble, setRating, star, unstar, getLyricsBySongId, getCoverArt, stream, download, getPodcasts, getNewestPodcasts`. Supersonic adds `getPlayQueue, savePlayQueue, getArtistInfo2, getAlbumInfo, getTopSongs, getSimilarSongs, getSimilarSongs2, getInternetRadioStations, startScan, getLyrics, createShare`. Feishin adds `getStarred, getSimilarSongs`. Symfonium needs API 1.13 and `search3` with an empty query. Stubs that return empty results keep these clients working.

| Endpoint | Status |
|---|---|
| ping, getLicense | required |
| getOpenSubsonicExtensions | required, public |
| tokenInfo | deferred: `not supported` |
| getMusicFolders, getArtists, getArtist, getAlbum, getSong, getGenres | required |
| getIndexes, getMusicDirectory | required, simulated from ID3 data |
| getVideos, getVideoInfo | deferred: `not supported` |
| getArtistInfo, getArtistInfo2, getAlbumInfo, getAlbumInfo2, getSimilarSongs, getSimilarSongs2, getTopSongs | empty result stubs |
| getAlbumList, getAlbumList2, getRandomSongs, getSongsByGenre | required |
| getNowPlaying | empty result stub |
| getStarred, getStarred2 | required |
| search | deprecated: error 0 |
| search2, search3 | required, empty query supported |
| getPlaylists, getPlaylist, createPlaylist, updatePlaylist, deletePlaylist | required |
| stream | required |
| download | disabled: error 50 |
| hls, getCaptions | deferred: `not supported` |
| getCoverArt | required |
| getLyrics | unsynced lyrics from the tag |
| getLyricsBySongId | `songLyrics` v1 |
| getAvatar | error 70 |
| star, unstar, setRating | required |
| scrobble | required |
| getShares, getPodcasts, getNewestPodcasts, getInternetRadioStations, getBookmarks | empty list stubs |
| createShare, updateShare, deleteShare, refreshPodcasts, createPodcastChannel, deletePodcastChannel, deletePodcastEpisode, downloadPodcastEpisode, jukeboxControl, createInternetRadioStation, updateInternetRadioStation, deleteInternetRadioStation, getChatMessages, addChatMessage, createBookmark, deleteBookmark | deferred: `not supported` |
| getUser, getUsers | required; owner only; `downloadRole=false` |
| createUser, updateUser, deleteUser, changePassword | deferred: error 50 |
| getPlayQueue, savePlayQueue | required |
| getScanStatus, startScan | required |
| getTranscodeDecision, getTranscodeStream | deferred: `not supported` |
| Extensions advertised | `formPost` [1], `transcodeOffset` [1], `songLyrics` [1] |
## 5. Tests

- `cargo xtask check` runs `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings` and `cargo test --locked --workspace`. The default suite finishes in under 30 s after compiling, in a debug build, on every OS. No network, no audio hardware, no ffmpeg, real SQLite (memory or temp file), no mocks of the database. No test sleeps or polls for more than 1 s; timing values are fields of config structs that tests set to tens of milliseconds.
- Fixtures: `tests/fixtures/audio/` holds `a.flac`, `a.mp3`, `a.m4a`, `a.ogg`, `a.opus`, generated by `scripts/make-fixtures.sh` from a 0.5 s mono 440 Hz sine. The sample rate is 22.05 kHz, except for `a.opus`, which is 24 kHz because libopus does not accept 22.05 kHz. The files total about 24 KB and each is under 16 KB; a test enforces a 64 KB budget. They carry no tag fields.
- `tests/common/mod.rs`: `LibraryBuilder` copies fixtures into a temp directory as `<Artist>/<Album>/<nn> <Title>.<ext>` and writes tags with lofty; `LibraryBuilder::standard()` builds 3 artists, 4 albums, 10 tracks (one compilation, one album with `cover.jpg`, one with embedded art, one track with a lyrics tag, one album with tracks on two discs). Names are valid on Windows and differ by more than case. `TestServer` builds the router over a scanned standard library and offers `get_json(method, params)`, `get_xml`, `get_raw`, `post_form`.
- Unit tests sit next to the code: IDs and fingerprints, tag mapping, `decide`, envelope and XML rendering, config, auth verification, LRC parsing, cover resize, argv builders.
- Router tests in `tests/` call the router in-process with `tower::ServiceExt::oneshot`. Every endpoint has a JSON test; the envelope has XML tests.
- `tests/cli.rs` runs the binary with `std::process::Command` for `init`, `user show`, `user set-password`, `config path` and `config show`. Every run that writes names a temp data directory. The password tests read the stored password from the database and check it with `auth::verify_password`.
- `tests/tags.rs` reads tracks that `LibraryBuilder` tagged, in the five fixture formats, through `library::tags`. The unit tests in `src/library/tags.rs` cover the mapping rules, unreadable files, and WAV and AIFF files that the tests build in memory.
- Transcoding: the default suite uses `FakeTranscoder`, which returns known bytes and records the job. One `#[ignore]` test runs real ffmpeg.
- One test (`tests/shutdown.rs`) binds `127.0.0.1:0` to exercise graceful shutdown; it is the only socket in the suite.
- Unix-only tests (file modes) are `#[cfg(unix)]`.
- Heavy tests are `#[ignore]`, read `WN_SERVER_FIXTURES`, and fail naming the variable when it is unset.
- mDNS registration and real clients are checked by hand and recorded in the PR.
- `cargo run --example e2e -- <url> <user> <password>` runs a scripted client session against a running server in Rust, on every OS.
