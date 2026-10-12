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
- Track identity is a surrogate ID assigned once. The scanner matches a vanished track to a newly seen file by fingerprint, so a rename, a move between directories, a move between music folders, and a change of a music folder's path keep the track's ID and everything attached to it. A file that changes at its path, by a retag or by other content, keeps its ID too, unless its old bytes are found at another path in the same scan. A retag that changes the file's first or last 64 KiB in the same scan interval as a move is treated as a deletion plus a new track (§3.3).
- Album and artist rows have surrogate IDs looked up by natural key. A row keeps its ID while a track row refers to it, so a rescan, a full scan, a new track of the album, a move of its files and a change of case, of surrounding white space or of Unicode form in its name keep the ID and the annotations attached to it. Such a change of the name reaches the row when no track carries the spelling that the row has (§3.3). An album row is deleted when no track row refers to it, and an artist row when no album row and no track row refers to it, both at the end of a scan that was not cancelled (§3.10 step 6); nothing else deletes these rows. Retagging an album title or album artist makes a new album (and artist) row with a new ID; the old row stays while one track still has the old tags. Annotations attached to the old album or artist ID are deleted when the row disappears. Track annotations are unaffected.
- A case-only rename on a case-insensitive filesystem (macOS, Windows) is a rename and matches by fingerprint.
- Scans run at startup when `library.scan_on_start = true` (default), on `startScan`, on `wn-server scan`, and every `library.rescan_interval_minutes` when set. The interval is 0 by default, which turns the periodic scans off. One scan of a library runs at a time, in the server process and in the `wn-server scan` command together. A trigger that finds a scan running starts none and queues none (§3.10).
- A track is deleted only when the scan knows that its file is gone. That needs two things. The scan read the directory of the file and found no file there, or the folder was removed from config. And the scan read every file that could be the file of the track. A path that cannot be read (a folder root, a directory, a symbolic link whose target is gone, a file whose state cannot be read) may hold any file, so a scan with such a path deletes no track. A file whose content cannot be read may be any file of its size, so a scan with such a file deletes no track of that size. A music folder that previously held tracks and now yields no supported file keeps its tracks. While a configured folder yields no supported file, the tracks of a folder removed from config are kept, because their files may be in it. Each of these cases is an error of the scan that names the folder, the path or the file. A track that cannot be deleted for one of these reasons while another track takes its path is kept without a file: it gets a placeholder path, so its ID does not name the file of the other track (§3.10).
- Cover art sources, in order: a file in the album's directory named `cover`, `folder`, `front` or `album` with extension `jpg`, `jpeg`, `png` or `webp` (case-insensitive); then the embedded picture of the album's first track that has one. The tracks of an album are ordered by disc number, where a track without one counts as disc 1, then by track number, where a track without one follows the numbered tracks, then by path. The album's directory is the directory of its first track, and no other directory is searched. An album whose discs are in the directories `CD1` and `CD2` therefore has no cover file when the image is in their parent directory; its cover is the embedded picture, or none. Among several cover files the name decides first, in the order `cover`, `folder`, `front`, `album`, then the extension, in the order `jpg`, `jpeg`, `png`, `webp`. A file with such a name that is a symbolic link is not a cover, because a link can name any file that the server can read (§3.11); the directory of the file can be a link. A track without a file (§3.4) is not a first track and gives no picture. Artists have no image.
- `getCoverArt?size=N` returns a JPEG whose longer side is at most N pixels. An N over 2000 counts as 2000. An image whose sides are within N keeps its size: no image is enlarged. Without `size`, and with `size=0`, the original bytes are returned when they are a JPEG, PNG or WebP image, with the content type of that format (§3.11).
- A track has lyrics when its file holds a lyrics tag (`USLT` in ID3v2, `©lyr` in MP4, `UNSYNCEDLYRICS` or `LYRICS` in Vorbis comments) or the file `<stem>.lrc` exists next to it.
- Annotations (star, rating, plays) are per user and keyed by item ID in one table. A play is counted for a track. The plays of an album are those of its tracks and are not stored (§3.4).
- Playlists belong to the owner. `public` is stored and has no effect with one user.
- One play queue per user; `savePlayQueue` replaces it.
- `scrobble` with `submission=true` (default) increments the play count of the track and sets its last play to the later of the stored time and the time of the play. `submission=false` is accepted and writes nothing. Nothing is forwarded anywhere.
- Transcode targets: `opus` (libopus in Ogg, `audio/ogg`) and `mp3` (libmp3lame, `audio/mpeg`). `format=raw`, or no `format` and no `maxBitRate`, serves the original. `maxBitRate` alone selects Opus. Another `format` value returns error 0. Bitrates are clamped to 32–320 kbps.
- Transcoded streams are not cached and do not support Range (`Accept-Ranges: none`). Seeking uses `timeOffset`.
- Transcode concurrency: `transcoding.max_concurrent` (default 2). A request past the limit waits up to 5 s, then returns HTTP 503 with `Retry-After: 5`.
- The owner password is stored as plain text in the database. Token auth (`t = md5(password + s)`) requires a recoverable password. Anyone who can read the data directory can read the password, so it must not be reused anywhere else. On Unix, `init` creates the data directory with mode 0700 and the database with mode 0600; on Windows, the user-profile ACL protects the directory.
- `init` and `user set-password` generate the password by default and print it once: four groups of five characters separated by hyphens (`xxxxx-xxxxx-xxxxx-xxxxx`), drawn from the operating system's random generator (`rand::rngs::SysRng`) from the 31-character alphabet `abcdefghjkmnpqrstuvwxyz23456789` (no `i`, `l`, `o`, `0`, `1`), which gives 99 bits of entropy and types easily on a phone. Random bytes of 248 and above are discarded, so every character is equally likely. The owner may supply a password instead through `--password-stdin` (the first line of stdin, without its line ending; an empty line is an error) or the `WN_SERVER_PASSWORD` environment variable (an empty value counts as unset); the flag wins over the variable. A supplied password is not printed. There is no interactive prompt.
- A password is never logged and never part of an error message or of a `Debug` output. The types that hold one (`cli::PasswordSource`, `users::UserRow`, `api::params::Params` and the credential types of `auth::extract`) leave it out of their `Debug` output. `Params` holds every parameter of a request, so its `Debug` output writes the name of every field and writes `[redacted]` in place of the value of `p`, `t`, `s`, `apiKey` and `password`, whatever the letter case of the name.
- Default port 4200, bind `0.0.0.0`.
- Search is a substring match with SQL `LIKE` and `ESCAPE '\'`. A query is split at white space into terms, and every term must be part of a name of the item. The names are compared through the lower-cased columns `artists.key`, `albums.key` and `tracks.title_lc`, so the letter case has no meaning. The track artist has no lower-cased column: it is compared as the tag spells it, and `LIKE` ignores the case of ASCII letters only. A letter with a diacritic is not its base letter. An empty query returns everything, paged (§4).
- API errors use HTTP 200 and the Subsonic error envelope. Unknown `/rest/*` methods return error 0 with message `unknown endpoint`. Paths outside `/rest/` return 404.
- `v` and `c` are required by every method except the public `getOpenSubsonicExtensions` (error 10 when absent), also when authentication is disabled. The value of `v` is not compared; errors 20 and 30 are never produced.
- Music folder paths are absolute (`std::path::absolute`, no symlink resolution, no `\\?\` prefix). Symlinks inside folders are followed, with one exception: a cover image file that is a symbolic link is not a cover (see the cover art sources above). The scanner gives a linked audio file a track row wherever the link points, inside the configured folders or outside them. `stream` serves a linked file only when its target is inside a configured folder (§3.9). A track whose link points outside the configured folders is therefore listed like every other track and answers error 70 to `stream`. The owner's rule is that nothing outside the configured folders is indexed or served. `stream` applies the rule. The scanner does not apply it: such a track has its row, and `getCoverArt` reads its embedded picture (§3.11). The scanner half of the rule is tracked separately as WAX-887. Entries whose name starts with `.` are skipped on every OS; the Windows hidden attribute is not checked. Directories with the names that an operating system gives to the directories it keeps at the root of a volume are skipped too: `System Volume Information` and `$RECYCLE.BIN`, matched without regard to case, and `lost+found`, matched in that spelling. They are skipped at every depth, because a volume can be mounted below a music folder. They hold no music, and the user of the server cannot list them. A skipped entry is not walked; when it cannot be read, that is no error and holds no deletion back. A file name that is not valid Unicode is skipped with a warning.
- `jsonp` is not supported; `f=jsonp` returns error 0 in XML.

## 2. Platforms and what CI verifies

| OS | Build and run | Verified by |
|---|---|---|
| Linux | native | CI: `cargo xtask check` (fmt, clippy `-D warnings`, full test suite) in a `rust:1.98.1-bookworm` container on the Woodpecker Kubernetes agent |
| Windows | native (MSVC or GNU host toolchain; `rusqlite` `bundled` needs the Visual Studio Build Tools or mingw) | CI: `cargo clippy --locked --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings` in the same container with `gcc-mingw-w64-x86-64`; compiles every crate, every test and every build script for Windows. Tests do not run in CI. Native test runs are a manual step recorded in the end-to-end verification until a Windows agent exists |
| macOS | native | Not verified by CI. Developers work on macOS and run `cargo xtask check` before every push; that run is the macOS verification |

Woodpecker has one Linux Kubernetes agent, container steps and no build cache. Packages installed in one step are gone in the next, so each pipeline step installs what it needs.

Portability rules in code:

- Signals: `tokio::signal::ctrl_c()` everywhere; `#[cfg(unix)]` SIGTERM; `#[cfg(windows)]` `ctrl_break`, `ctrl_close`, `ctrl_shutdown`. `cli::OtherStopSignals` is the type that differs (§3.13).
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
  tests/common/mod.rs         LibraryBuilder, TestServer, the servers over a scanned library
  tests/*.rs                  router-level integration tests
  examples/e2e.rs             end-to-end session against a running server
  examples/make-library.rs    writes the standard test library to a directory
  src/
    main.rs                   clap entry
    lib.rs                    module tree
    cli.rs                    init, run, scan, user, config
    config.rs                 Config, defaults, validation, DataDir
    db/mod.rs                 Db handle, migrations, pragmas
    db/queries/mod.rs         the list of the query modules and nothing else
    db/queries/*.rs           tracks, albums, artists, annotations, browse, lists, search, playlists, play_queue, lyrics, users, scans, folders
    ids.rs                    ID prefixes, parse, new_id, natural keys, fingerprint
    library/tags.rs           lofty wrapper -> TrackMeta, pictures, lyrics
    library/scanner.rs        walk, diff, move detection, upsert, delete, aggregate
    library/scan_controller.rs  ScanController, ScanLock, the periodic scans
    library/covers.rs         cover resolution, resize, disk cache
    auth/mod.rs               verify password and token
    auth/extract.rs           authenticate, Authed(User) extractor
    api/mod.rs                Router, /rest/{method} table, AppState
    api/cors.rs               CORS headers of the API route
    api/params.rs             Params multimap and typed getters
    api/envelope.rs           SubsonicResponse, Format, ApiError
    api/xml.rs                serde_json::Value -> Subsonic XML
    api/model.rs              Child, AlbumID3, ArtistID3, Artist, Directory, Index, Genre, MusicFolder, constructors
    api/system.rs             ping, getLicense, getOpenSubsonicExtensions
    api/browse.rs             getArtists, getArtist, getAlbum, getSong, getMusicFolders, getGenres
    api/folders.rs            getIndexes, getMusicDirectory
    api/lists.rs              getAlbumList(2), getRandomSongs, getSongsByGenre
    api/search.rs             search2, search3
    api/annotate.rs           star, unstar, setRating, getStarred(2), scrobble
    api/playlists.rs          getPlaylists, getPlaylist, createPlaylist, updatePlaylist, deletePlaylist
    api/queue.rs              getPlayQueue, savePlayQueue
    api/lyrics.rs             getLyrics, getLyricsBySongId
    api/users.rs              getUser, getUsers
    api/stubs.rs              download (disabled), empty-result and error stubs for deferred endpoints
    api/media.rs              stream
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
| gethostname | 1 | Apache-2.0 | none; the crate is declared and no code uses it |
| md-5 | 0.11 | MIT/Apache-2.0 | Subsonic token check |
| sha2, hex | 0.11 / 0.4 | MIT/Apache-2.0 | fingerprints; the digests that a credential comparison compares |
| rand | 0.10 | MIT/Apache-2.0 | surrogate IDs, generated passwords, the instance UUID, the names of temp files in the cover cache, the random lists |
| unicode-normalization | 0.1 | MIT/Apache-2.0 | natural keys |
| image | 0.25 (`jpeg`, `png`, `webp` only) | MIT/Apache-2.0 | cover resize |
| walkdir | 2 | MIT/Unlicense | scanner |
| directories | 6 | MIT/Apache-2.0 | default data directory |
| mime_guess | 2 | MIT | content types |
| tracing, tracing-subscriber (`env-filter`) | 0.1 / 0.3 | MIT | logging |
| thiserror | 2 | MIT/Apache-2.0 | errors |
| dev: tempfile 3 | | MIT/Apache-2.0 | temp directories in tests |

Three small helpers are written in the crate rather than added as dependencies: the instance UUID is formatted from 16 random bytes with the version-4 and variant bits set, the `filename*=UTF-8''` percent-encoding for `Content-Disposition` is a short function over the unreserved set, and `Params` parses `application/x-www-form-urlencoded` data by the rules of the URL standard. axum's `Query` and `Form` extractors parse the same data and are not used: one function in `Params` parses the query string and the form body, the router applies its own content-type rule and the 8 MiB limit, and every failure is an `ApiError` in the requested format.

`sqlx` is not used: an async API adds nothing for a single-writer embedded database, its query macros need a database at compile time, and its tree is larger. `symphonia` is not used: ffmpeg decodes; `lofty` reads tags. `argon2` is not used: the password must be recoverable.

ffmpeg is an external program found on `PATH` or named by `transcoding.ffmpeg`. The project does not link, vendor or bundle ffmpeg or its libraries, so the licence of a user's ffmpeg build does not reach this project's licence boundary.

### 3.3 Identity

IDs are opaque strings of a two-letter prefix, a hyphen and 32 lower-case hex characters: `tr-`, `al-`, `ar-`, `pl-`. `ids::parse` accepts exactly that shape and returns the kind and the 32 hex characters; anything else is rejected before it reaches SQL or the filesystem. Music folder IDs are the integer `folders.id`, which Subsonic clients expect as a number. Cover-art IDs are an album ID (album art) or a track ID (embedded art).

- Track: `tr-` + 16 random bytes, assigned on first sight. `(folder_id, rel_path)` is the locator; it is unique and it changes when the file moves. A track that is kept without a file has the placeholder path `/<id>` as its `rel_path`; a relative path never starts with `/`, so no file has that path (§3.10). `db::queries::tracks::placeholder_path` builds the value and `db::queries::tracks::has_file` tests for it (§3.4).
- Fingerprint: `size` in bytes, a colon, and the first 16 bytes hex of SHA-256 over the first 64 KiB of the file followed by the last 64 KiB (the whole file when it is 128 KiB or smaller). A pure move, rename or copy keeps the fingerprint. A tag edit changes it, because tags live in the head (ID3v2, Vorbis comments, MP4 atoms) or the tail (ID3v1, APE). Two identical files share a fingerprint.
- Artist key: album artist name, Unicode NFC, trimmed, lower-cased. Artist: `ar-` + random, `UNIQUE(key)`.
- Album key: album title, NFC, trimmed, lower-cased. Album: `al-` + random, `UNIQUE(artist_id, key)`.
- Playlist: `pl-` + random.

Display names of album rows and artist rows: names that differ in case, in surrounding white space or in Unicode form have one key and are spellings of one name. A row is made with the spelling in the tags of the track that makes it (§3.10). At the end of every scan (§3.10 step 6) the stored name of a row is compared with the spellings that the track rows of the row carry:

- A row keeps its name while one track carries it, however many tracks carry another spelling.
- A row whose name no track carries takes the spelling that the most tracks carry. Among spellings that equally many tracks carry it takes the first in byte order of the UTF-8 text, as the album genre does (§3.10). Upper-case ASCII letters precede lower-case ones in that order, so `The Beatles` precedes `the beatles`.
- A row of which no track carries a spelling keeps its name.

The spelling that a track carries for its album is its album title, `tracks.album_name`. A track row holds the track artist (`tracks.artist_name`) and not the album artist of its file (§1.3). The spelling that a track carries for its artist is therefore its track artist, when the track artist has the key of the artist row. That is the album artist of a track without an album-artist tag and without the compilation flag. A track whose track artist has another key carries no spelling of the artist. When the files of an artist spell its name in one way in the album-artist tag and in another way in the track-artist tag, the track-artist tags decide the name of the row. The row of `Various Artists` always has that name: §1.3 gives every track of it that album artist in that spelling. A track without a file (§3.4) carries its spellings as every other track does.

The step sets `name` of an album row, and `name` and `sort_name` of an artist row, which have one value. `artist_name` of an album row is the `name` of its artist row, and the same step sets it. The ID and the key of a row do not change, no row is inserted or deleted for a name, and the annotations of a row stay attached to it. A row that has its names is not written.

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

Readers of `tracks`: a row whose `rel_path` starts with `/` has no file. The value is the placeholder path of §3.3. `db::queries::tracks::has_file(rel_path)` is the one test for it, and `Track::has_file` calls it; no reader tests the value itself. Code never joins such a `rel_path` to a folder path, because `Path::join` replaces the folder with a root path. `stream`, `getCoverArt` for the ID of the track, and the lyrics endpoints answer error 70 for such a track. `getIndexes` and `getMusicDirectory` leave it out of the folder tree. `getSong`, `getAlbum`, search and playlist results may list it; it keeps its tags, its ID and its annotations. Aggregation (§3.10 step 6) treats it as a track of its album: the album row and the artist row stay while it refers to them, and `song_count`, `duration_ms`, `year` and `genre` count it, so the counts of an album agree with the rows that `getAlbum` may list. The names of the two rows count the spellings it carries (§3.3). Aggregation never takes such a row as the track that gives the album its directory or as the source of embedded cover art; an album whose tracks all have no file has no cover. SQL that needs files filters with `db::queries::tracks::HAS_FILE_SQL`, which is `rel_path NOT LIKE '/%'`. The constant is the one place that spells the test in SQL: every query module uses it, and a unit test checks that it agrees with `has_file`.

The library queries and the response models follow the rule (§3.8). `browse::tracks_of_album`, `browse::track` and `browse::tracks_by_ids` return such a row as every other row, and `browse::genres` counts it. `browse::list_artists` with a music folder counts only the tracks that have a file, because a track without a file is in no folder. `browse::file_path` returns no path for it. `model::child` leaves `path` out, and never gives the ID of the track as `coverArt`, because `getCoverArt` has no picture for it. The other fields of the element are those of the file the track had.

Genres are derived with `GROUP BY genre`. Cascades from `tracks` to `playlist_tracks` and `play_queue_tracks` are correct because a track row is deleted only when its file is gone and no new file carries its fingerprint, and never while a path or a file that the scan could not read may hold it, nor for a folder that is newly empty. Both child tables index `track_id`, so the deletion of a track does not scan them. The cascade deletes the rows of the track and writes no other row, so the `position` values that a playlist keeps can have a gap. The playlist queries order by `position` and depend on no value of it, and their next write of the entries stores the positions 0 to n − 1 (§4).

Plays: `play_count` and `last_played_at` of the `annotations` table hold the plays of a track, and only the row of a track holds them. `scrobble` writes the row of the track and no other row (§4). The play count of an album is the sum of the play counts of its tracks, and the last play of an album is the latest play of one of its tracks. Both are computed from the rows of the tracks when they are read: by `annotations::get_many` for the response models (§3.8) and by the `frequent` and `recent` album lists (§4). In the row of an album and of an artist the two columns keep their defaults, and no reader reads them. The rows of the tracks are therefore the one place that holds a play. The plays of an album follow its tracks: they move with a track that a retag puts into another album, and they go with a track row that a scan deletes.

Multi-user readiness: every per-user table carries `user_id`; `users.role` separates the owner from future guests and has no default, so every insert names the role; the CLI finds the owner with `WHERE role = 'owner'`.

### 3.5 Data directory and config

Data directory, in order: `--data-dir`, `WN_SERVER_DATA_DIR`, then `directories::ProjectDirs::from("", "", "wn-server").data_dir()`: `~/Library/Application Support/wn-server` on macOS, `~/.local/share/wn-server` on Linux, `C:\Users\<user>\AppData\Roaming\wn-server\data` on Windows. An empty `WN_SERVER_DATA_DIR` counts as unset. The chosen path is made absolute with `std::path::absolute`.

```
<data-dir>/
  config.toml
  library.db  (+ -wal, -shm)
  instance.id          UUID string created by init, and by run when it is missing; the mDNS TXT id and the source of the SRV target
  scan.lock            empty file created by the first scan; the process that scans the library holds a lock on it (§3.10)
  cache/covers/        <coverId>-<size>-<coverMtime>.jpg
```

`config.toml` with defaults (`init` writes it with comments):

```toml
[server]
bind = "0.0.0.0"
port = 4200
name = "wn-server"              # instance name shown by discovery
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

The default `server.name` is the fixed string `wn-server` (`config::DEFAULT_NAME`). `init` writes it to `config.toml`, and a config that omits the key gets it at load. An owner changes the name in `config.toml`. A name that `config.toml` holds is kept as written. No default and no other value of the configuration is taken from the host name of the machine, and no code reads that name.

Validation at load: at least one folder; each folder exists and is a directory; `port != 0`; `server.name` is 1 to 63 bytes long; `max_concurrent >= 1`; bitrates 32–320. `server.name` is the DNS-SD instance name and the TXT `name` value (§3.12), and each holds at most 63 bytes. The error for `server.name` states the length of the name and names the key, the table and the file to edit. Unknown keys are an error (`deny_unknown_fields`). Internal timing values (the grace of a stop, transcode permit wait, scan poll) are fields of the config structs with defaults but are not config keys; tests set them.

### 3.6 CLI

```
wn-server init  [--data-dir D] [--folder PATH]... [--username NAME] [--port N] [--password-stdin]
                creates the data directory (0700 on Unix), config.toml, instance.id,
                the database (0600 on Unix) and the owner account; generates the
                password and prints it once, unless --password-stdin or
                WN_SERVER_PASSWORD supplies one; with a config.toml and no
                owner account, keeps config.toml and creates what is missing
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

`init` works in this order: it checks the folders, refuses a data directory that has `config.toml` and an owner row in `library.db`, obtains the password, creates the data directory, writes `config.toml`, writes `instance.id` when the file is missing, opens the database and inserts the owner. `--username` names the owner; the default is `owner`. The insert does nothing when an owner row exists, and `init` then fails. When a step after the config write fails, `init` removes the `config.toml` and the `instance.id` it wrote in that run, so the command can run again. An existing `instance.id` is kept.

A data directory that has `config.toml` and no owner row has lost its database or its owner: `library.db` is missing, or it exists without an owner row. `init` opens an existing `library.db` before it reads the password to tell this case from the refused one. It then keeps `config.toml` as it is and creates what is missing. `--folder` and `--port` do not change an existing `config.toml`, so `init` fails when either is given, before it reads the password. It skips the config write. The other steps are those of a first `init`: the password rules and `--username` are the same, a missing `instance.id` is written, a missing `library.db` is created (0600 on Unix) and the owner is inserted. The other rows of an existing database stay as they are. `init` prints `kept <config path>` and then `created <database path>` or `kept <database path>` in place of `wrote <config path>`. When a later step fails, this run removes no file: the next run keeps the `instance.id` and uses the database file that the failed run left. SQLite deletes a write-ahead log that it finds next to an empty database file, so a log that a lost database left behind does not reach the new one. Nothing of a lost database is recovered.

`init` and `user set-password` print `Password for <name> (shown once): <password>` when they generated the password, and then `The password is stored in plain text in the data directory. Do not reuse it elsewhere.` Both checkpoint the write-ahead log with `Db::close` before they print, so the stored password is on disk; a checkpoint that another process blocks is not an error.

`user set-password --username NAME` selects the account by name; without the option the account is the owner. The `user` commands fail when `library.db` does not exist or has no owner row, and do not create either. Only `init` creates them, so the error names `wn-server init`, which creates what is missing and keeps an existing `config.toml` and an existing database.

A command that fails prints `wn-server: <error>` to stderr and exits with 1. A usage error exits with 2.

`scan` loads the config, opens `library.db` and runs one scan in its own process. It fails when `library.db` does not exist and does not create it. It logs to stderr as `run` does. The scan holds the scan lock of the data directory (§3.10), so the command does one of three things:

- No server runs: the command scans.
- A server runs and does not scan: the command scans. WAL lets the server read while the scan writes, and a write of the server waits for one transaction of the scan at most (`busy_timeout`). The server starts no scan of its own while the command holds the lock.
- A server scans, or another `wn-server scan` runs: the command scans nothing, prints an error that says so and exits with 1. It does not wait for the other scan.

After the scan the command checkpoints the write-ahead log with `Db::close`; a checkpoint that a running server blocks is not an error. It then prints the summary to stdout: one `name: number` line for each of `seen`, `added`, `changed`, `moved`, `removed`, `albums`, `artists` and `errors`, and one `error: <message>` line for each error of the scan. It exits with 0, or with 1 when the scan recorded an error. A folder of the config that does not exist fails the config load (§3.5), so the command exits with 1 and names the folder before a scan starts. A process that is ended during its scan keeps the batches that the scan committed. Its `scans` row has no end time, and the next scan finishes the work.

`getScanStatus` reports `scanning` and `count` for the scans of the server process only. Its `lastScan` is read from the `scans` table and names the scans of the command as well (§4).

`run` opens `library.db` and needs its owner account, because every request acts as a user. It creates neither. When the file does not exist or has no owner row, `run` fails before it binds the address, and the error names `wn-server init`, which creates what is missing and keeps `config.toml`.

### 3.7 Authentication

Every method needs a user, except `getOpenSubsonicExtensions`, which is public. The router authenticates a request before it calls the handler of the method (§3.8). The check does not depend on the handler, so a method that a change adds to the method table is protected without any step in that change. `api::PUBLIC_METHODS` lists the public methods, and a test compares it with the rows that §4 marks `public`.

`auth::extract::authenticate(auth, db, params) -> Result<User, ApiError>` applies these rules in this order:

1. `v` and `c` are required: error 10 naming the parameter. The value of `v` is not compared.
2. With `auth.disabled_unsafe = true` the user is the owner, and no other parameter is read.
3. `apiKey` alone: error 42. `apiKey` with `u`, `p`, `t` or `s`: error 43. `p` with `t` or `s`: error 43.
4. `u` absent: error 10 naming `u`. None of `p`, `t` and `s`: error 10 naming `p, or t and s`. `t` without `s`, or `s` without `t`: error 10 naming the absent one.
5. The account is looked up by `u` without regard to ASCII case. `p` is compared with the stored password in constant time; an `enc:` prefix means hex-encoded, and a value with the prefix and invalid hex matches nothing (`auth::verify_password`). `t` must equal `md5(password + s)` as lower-case hex, compared in constant time; upper-case hex does not match (`auth::verify_token`). An unknown name and a wrong credential are both error 40 with the same message. The credential of an unknown name is checked against an empty password, so both cases run the same steps.

Both comparisons hash the two values with SHA-256 and compare the two 32-byte digests in a loop that does not stop at the first difference. No step of the comparison depends on the length of the stored password, which is empty for an unknown name. The time depends on a length only through the number of 64-byte blocks that SHA-256 reads.

Rules 1, 3 and 4 read no account, so their answers do not depend on the accounts of the server. An empty value is a value: `p=` is a password and `apiKey=` is an API key.

`Authed(User)` is the axum extractor through which a handler receives the user. It returns the user that the router stored in the request. For a request that the router did not authenticate it applies the rules itself, so a handler that takes `Authed` never runs without a user.

A request that breaks a rule is logged at `info` with the values of `u` and `c`, each cut to 64 characters, and the error code. The values of `p`, `t`, `s` and `apiKey` are never logged and are never part of an error message: an error names a parameter and holds no value of the request.

With `auth.disabled_unsafe = true`, `run` logs a warning at startup that names the key and says that the setting is unsafe. Every request that has `v` and `c` then acts as the owner.

Remote-mode seam: `users.role` and `auth::extract::authenticate` are the only places that turn parameters into a `User`. Guests and API keys extend that function and add a table.

### 3.8 Request pipeline and envelope

- Routes `/rest/{method}` and `/rest/{method}.view` for GET and POST, one route with a path parameter. A HEAD request is answered by the route of GET: the handler runs as for a GET request, and the router removes the body of the answer and keeps `Content-Length`. A static method table maps every name in §4 to a handler; names not yet implemented map to `not_implemented`, which returns error 0 with message `not supported`. Unknown names return error 0 with `unknown endpoint`. Paths outside `/rest/` return 404. An `OPTIONS` request to the route is answered as a CORS preflight request (see below). Any other HTTP method on the route returns 405.
- `AppState { config, db, covers, transcoder, scan, cancel, started_at }` is shared through `State`.
- The router handles a request in this order: it reads the parameters, selects the format, looks up the method, authenticates the request unless the method is public (§3.7), and calls the handler. An unsupported format, a form body that cannot be read and an unknown method name are therefore answered without a user. Every other response of a method that is not public needs one, including the `not supported` error of a method without a handler.
- CORS: a browser-based client such as Feishin calls the server from a page of another origin, and the browser lets the page read a response only when the response allows that origin. The route answers `OPTIONS` with the handler `api::cors::preflight`, and the middleware `api::cors::add_headers` wraps the route.
  - An `OPTIONS` request is answered by `preflight` with HTTP 204, an empty body, `Access-Control-Allow-Methods: GET, POST, OPTIONS`, `Access-Control-Allow-Headers: Content-Type, Range` and `Access-Control-Max-Age: 86400`. The request does not reach the steps above, so a preflight request needs no user and no parameter. `Content-Type` and `Range` are the request headers that the target clients set and that a browser may name in a preflight request: Feishin sets `Content-Type: application/json`, and a player sets `Range`.
  - The middleware adds `Access-Control-Expose-Headers: Content-Range, Accept-Ranges, Content-Length` to every other response of the route. A player reads these headers from a media response.
  - The middleware adds `Access-Control-Allow-Origin: *` to every response of the route: an envelope, an error envelope, an authentication error, a body of `stream` or `getCoverArt`, the answer to `OPTIONS` and HTTP 405. The headers do not depend on the `Origin` header of the request.
  - No response carries `Access-Control-Allow-Credentials`. The credentials are request parameters and not cookies, so the wildcard origin is enough. The server has no list of allowed origins.
  - A path outside the route gets HTTP 404 without these headers.
  - With `auth.disabled_unsafe = true` (§3.7), every request that has `v` and `c` acts as the owner, and `Access-Control-Allow-Origin: *` lets a page of any origin read the responses. A web page that is open in a browser on the network can then use the server as the owner. It can read and stream the library, and it can call every method, including the methods that change state: `star`, `unstar`, `setRating`, `scrobble`, `createPlaylist`, `updatePlaylist`, `deletePlaylist`, `savePlayQueue` and `startScan`.
  - The handler and the middleware are functions in the crate. The `cors` feature of tower-http is not enabled, because its layer answers a preflight request with HTTP 200.
- `Params` is a multimap of query-string and form-body fields (`application/x-www-form-urlencoded` on POST; body values append to query values). A form body larger than 8 MiB returns error 0, which bounds the memory a request takes before it is authenticated; a POST body of another content type is not read. `req_str`, `opt_str`, `opt_i64`, `opt_u32` and `opt_bool` read the first value of a name; `all(name) -> Vec<&str>` returns every value. A missing required parameter maps to error 10 naming the parameter. For the typed getters an empty value counts as absent and a value of the wrong type returns error 0 naming the parameter; `opt_bool` accepts `true` and `false` in any letter case.
- A handler is an axum handler function. Its arguments are extractors (`Params`, `State<AppState>`, `Authed`), and the method table holds it through the `handler!` macro. Handlers return `Result<Payload, ApiError>`; `Payload` is a key plus a serialisable value, or empty. Neither knows the format, so the router renders the envelope after the handler returns. `Format` is parsed from `f` before dispatch so errors use the requested format: `json` selects JSON, and `xml` or an absent `f` selects XML. Any other value, `jsonp` included, returns error 0 with message `unsupported format`, rendered in XML.
- JSON envelope: `{"subsonic-response": {"status":"ok","version":"1.16.1","type":"wn-server","serverVersion":"<crate version>","openSubsonic":true, "<key>": ...}}`, with content type `application/json`. XML: the declaration `<?xml version="1.0" encoding="UTF-8"?>` followed by `<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="wn-server" serverVersion="..." openSubsonic="true"><key .../></subsonic-response>`, with content type `application/xml`. The envelope fields are written in this order in both formats.
- XML rendering: the payload is serialised to `serde_json::Value` and walked: scalars become attributes, objects become child elements, arrays become repeated child elements named after the key, and a scalar field named `value` becomes text content. A scalar array item becomes an element with the item as text content (`"versions": [1]` renders `<versions>1</versions>`). Attribute values and text are escaped. Characters outside the XML 1.0 `Char` production (U+0000 to U+001F other than tab, line feed and carriage return; U+FFFE; U+FFFF) are dropped from attribute values and text, because a strict parser rejects a document that holds one, escaped or not. One set of `#[derive(Serialize)] #[serde(rename_all = "camelCase")]` models serves both formats. `None` fields are skipped; empty arrays are `[]` in JSON and absent in XML.
- Payload key order: `serde_json` is built without `preserve_order`, so the keys of every payload object are sorted by their bytes in both formats (upper-case letters sort before lower-case letters). In XML that is the order of the attributes and of the child elements. Array items keep their order.
- Errors: HTTP 200, `status: "failed"`, `error: {code, message}`. `ApiError` variants: `MissingParam` 10, `BadCredentials` 40, `AuthUnsupported` 42, `AuthConflict` 43, `Forbidden` 50, `NotFound` 70, `Other(msg)` 0.
- `EXTENSIONS: &[(&str, &[u32])]` holds the advertised extensions, one entry per line.
- The method table, `EXTENSIONS`, the fields of `AppState` (in its definition and where it is built) and the list of handler modules in `src/api/mod.rs` hold one line for each entry that this document names, and a blank line follows every entry. An entry that is not built is a comment line with its name, or a method table line that maps to `not_implemented`. A change that builds an entry replaces that line and keeps the blank lines, so two changes never touch neighbouring lines and git merges them without a conflict. The arrays, the struct and the statements that build the struct carry `#[rustfmt::skip]`, because rustfmt removes the blank lines inside an array and the blank line between a comment and the struct field after it.
- The list of the query modules is `src/db/queries/mod.rs`, which holds that list and nothing else, and the list of the library modules is in `src/library/mod.rs`. Both follow the same rule: one line for each module that §3.1 names, in that order, a blank line after every entry, and a comment line with the name for a module that is not built. Their `mod` lines carry no `#[rustfmt::skip]`, because rustfmt orders only `mod` lines that follow each other without a blank line. A query module of a group of methods adds no function to another query module (see Library queries below). The module list of the crate, `src/lib.rs`, has the union merge driver instead (`.gitattributes`).
- The texts that the change of one handler module adds to this document and to `README.md` follow the rule as well. A comment line `<!-- api/<module>.rs -->` holds the place of the module in §4 and in the API section of `README.md`, and a comment line `<!-- tests/<file>.rs -->` holds the place of its test file in §5. A blank line follows every comment line. A change replaces its comment line with its text and keeps the blank lines.
- Request log: one line for each request, written when the response headers are ready. Its fields are `method` (the method name), `c`, `u`, `status` (the HTTP status) and `duration_ms` (the milliseconds from the arrival of the request to the response headers). The line never holds the query string, the form body or a value of `p`, `t`, `s` or `apiKey`. The method name, `c` and `u` are cut to 64 characters, because the client chooses them. `u` is the name the request sends, whether the request is authenticated or not. A request for a path outside `/rest/` has a line with the status and no method.
  - The level is `info`. It is `debug` for `stream` and `getCoverArt`, with and without the `.view` suffix and whatever the answer is, because a client that plays music or draws a list of albums sends many of them. `api::DEBUG_LOG_METHODS` names the two.
  - The tower-http trace layer makes the span of the request and calls `api::record_response`, which writes the line. The lines that the layer writes by default at the start of a request and at the end of a body are turned off, so the request log is this one line at every log level. A request that fails authentication has the line of §3.7 as well.
  - A body that is streamed after the headers adds no time to the line and no line of its own. When the client closes the connection during a stream, the server drops the body and logs nothing: an aborted stream has the one line of its request. The trace layer adds one line at `error` for a body that fails on the server, and for a response with a 5xx status.
- Binary endpoints (`stream`, `getCoverArt`) bypass the envelope on success and use it on error.
- Model constructors `model::child(&Song, Option<&Annotation>)`, `model::album(..)`, `model::artist(..)` and the others of `api/model.rs` are the only way a handler builds an element that several methods share; the caller passes the user's annotations from `annotations::get_many(conn, user_id, ids)`. An element that one method alone returns, such as the `searchResult3` that holds three lists of these elements, is a private type in the module of its handler.

#### Library queries

`db::queries::browse` reads the library for the API. Its rows are what the response models are built from: `Artist` and `Album` (rows of their tables), `Song` (a `tracks::Track` row and whether its album has a cover), `Genre`, and the `Folder` of `db::queries::folders`.

| Function | Result |
|---|---|
| `list_artists(folder_id)` | Every artist, in the order of the natural keys. With a music folder: the artists that have a track with a file in it. |
| `artist(id)`, `album(id)`, `track(id)` | One row, or none for an ID that no row has. |
| `albums_of_artist(id)` | The albums of one artist by year, where an album without a year follows the others, then by the natural key of the title. |
| `tracks_of_album(id)` | Every track row of one album in the track order of §1.3: disc, track, path. |
| `genres()` | Each genre tag of the tracks with the number of tracks that have it and the number of albums whose `genre` it is, in the order of the names without regard to ASCII case. |
| `folders()` | The music folder rows in ID order. |
| `artists_by_ids(ids)`, `albums_by_ids(ids)`, `tracks_by_ids(ids)` | The rows of the IDs, in the order of the IDs. An ID that no row has adds nothing. An ID that the list holds twice adds its row twice. |
| `file_path(track)` | The absolute path of the file of a track, or none for a track without a file (§3.4). |
| `last_scan_finished_at()` | The end time of the scan that finished last, in Unix seconds, or none. `getIndexes` reports it as `lastModified`. `scans::last_finished` is the same time as text for `getScanStatus`. |

- A query module of a group of methods (`lists`, `search`, `playlists`, `play_queue`, `lyrics`) selects the IDs of its result with its own SQL, in the order of the result, and reads the rows with the `_by_ids` functions. It needs no column list and no row reader, and it adds no function to `browse` or to the module of a table.
- A list of IDs is one parameter of a statement: a JSON array that the statement reads with `json_each`. A list of any length is one statement, and no ID is part of the SQL text. `annotations::get_many` takes its IDs the same way.
- `album_count` of an artist and `song_count` and `duration_ms` of an album are the values that the last scan stored (§3.10 step 6). A music folder filter does not change them.
- The tracks are grouped by the genre tag as it is stored, so two spellings that differ by case are two genres. The album count of a genre is the number of albums whose `genre` column holds it. That column is the genre of most tracks of the album (§3.10 step 6), so an album counts for one genre.

`annotations::get_many(conn, user_id, item_ids)` returns a map from item ID to `Annotation { starred_at, rating, play_count, last_played_at }`. The IDs can be of tracks, albums and artists at once. The two times are Unix seconds. The star and the rating of an item are those of its row. The plays of a track are those of its row. The plays of an album are the sum of the play counts of its tracks and the latest of their plays (§3.4): a second statement reads them from the rows of the tracks, which it finds through the index `tracks_album`, so it reads neither table as a whole. It runs only when the list holds the ID of an album. The plays of an artist are none, because no element reports them. The map holds an entry for each ID that has a row of the user, and for each album of which the user played a track. An item without an entry has the annotation `Annotation::default()`: no star, no rating, a play count of 0 and no play. A handler calls the function in the read transaction of its answer, so the two statements read one state of the database.

#### Response models

`api/model.rs` holds these elements. The names are those of the OpenSubsonic API. Every constructor of an item that a user can star, rate or play takes `Option<&Annotation>`, the entry of the item in the map of `get_many`.

| Element | Constructor | Fields |
|---|---|---|
| `Child`, a song | `child(&Song, ..)` | `id`, `parent` (the album ID), `isDir: false`, `title`, `album`, `artist`, `track`, `year`, `genre`, `coverArt`, `size`, `contentType`, `suffix`, `duration`, `bitRate`, `bitDepth`, `samplingRate`, `channelCount`, `path`, `discNumber`, `created`, `albumId`, `artistId`, `type: music`, `mediaType: song`, `starred`, `userRating`, `playCount`, `played` |
| `Child`, an album as a directory | `child_dir_for_album(&Album, ..)` | `id`, `parent` (the artist ID), `isDir: true`, `title`, `album`, `artist`, `year`, `genre`, `coverArt`, `duration`, `created`, `artistId`, `mediaType: album`, `starred`, `userRating`, `playCount`, `played` |
| `AlbumID3` | `album(&Album, ..)`, `album_with_songs(&Album, .., songs)` | `id`, `name`, `artist`, `artistId`, `coverArt`, `songCount`, `duration`, `created`, `year`, `genre`, `starred`, `userRating`, `playCount`, `played`; with songs also `song` |
| `ArtistID3` | `artist(&Artist, ..)`, `artist_with_albums(&Artist, .., albums)` | `id`, `name`, `albumCount`, `starred`; with albums also `album`. No `coverArt`: artists have no image (§1.3) |
| `Artist` of the folder methods | `artist_dir(&Artist, ..)` | `id`, `name`, `starred`, `userRating` |
| `Directory` | `directory_for_artist(&Artist, .., children)`, `directory_for_album(&Album, .., children)` | `id`, `name`, `starred`, `userRating`, `child`; for an album also `parent` (the artist ID) and `playCount` |
| `Index` | `index(artists, constructor)` | `name`, `artist` |
| `Genre` | `genre(&Genre)` | `value`, `songCount`, `albumCount` |
| `MusicFolder` | `music_folder(&Folder)` | `id`, `name` |

- `getAlbumList`, `search2`, `getStarred` and `getMusicDirectory` list an album as a directory. `getIndexes`, `search2` and `getStarred` list an artist as the `Artist` of the folder methods.
- A time (`created`, `starred`, `played`) is ISO 8601 in UTC with whole seconds, as in `2023-03-14T17:51:22Z` (`model::iso8601`). `created` is the time of the first scan that saw the track or the album.
- `duration` is whole seconds, rounded to the nearest second, and half a second is rounded up (`model::seconds`). The duration of an album is its stored sum in milliseconds, rounded once, so it can differ from the sum of the rounded durations of its songs.
- `path` is `tracks.rel_path`: the path below the music folder, with `/` between the components.
- `artist` of a song is the track artist tag. `artistId` is the ID of the album artist, because a track artist has no row (§1.3).
- `discNumber` is 1 for a track without a disc number, as in the track order (§1.3).
- `bitDepth`, `samplingRate` and `channelCount` are 0 for a song whose file gave none. The OpenSubsonic API asks a server to send an added field that it supports with a default value. `track`, `year`, `genre` and `bitRate` are absent when the track has no such value.
- `playCount` is 0 for an item without a play. `starred`, `userRating` and `played` are absent for an item without a star, a rating or a play; the Subsonic schema allows no `userRating` below 1. The two forms of an album, `AlbumID3` and the directory, report the same four values of its annotation. `playCount` and `played` of an album are the sum of the play counts of its tracks and the latest play of one of them (§3.4).
- `coverArt` of an album is its own ID when `cover_source` is set, and absent otherwise. `coverArt` of a song is the ID of its album when the album has a cover, else the ID of the track when `has_picture` is set and the track has a file, else absent (`model::album_cover_art`, `model::song_cover_art`).
- `model::index_letter(name)` is the index of an artist name: its first letter in upper case when that is a letter from A to Z, and `#` otherwise. A leading article is not part of the name for this: `The`, `El`, `La`, `Los`, `Las`, `Le` or `Les`, without regard to ASCII case, as a whole word that another word follows. "The Beatles" is in `B`, and "Them" and "The" are in `T`. A letter with a diacritic counts as its base letter, so "Édith Piaf" is in `E`. `model::ignored_articles()` is the value of `ignoredArticles`: `The El La Los Las Le Les`.
- `model::index` groups artists by that letter. The indexes are in the order of their names, so `#` is the first. The artists of an index are in the order of their names without the leading article, without diacritics and in lower case.
- `AlbumID3` has no `isCompilation`, and no element has a multi-artist field, a sort name or a MusicBrainz ID (§1.2).

### 3.9 Streaming and transcoding

`StreamRequest { format, max_bitrate_kbps, time_offset_s, estimate_content_length }` holds what a `stream` request asks for. `StreamRequest::from_params` reads `format`, `maxBitRate`, `timeOffset` and `estimateContentLength`: an empty value is an absent one, and a value of the wrong type is error 0 that names the parameter (§3.8).

`media::stream::decide(&Track, &StreamRequest, &config::Transcoding) -> Result<StreamPlan, ApiError>` with `StreamPlan::Raw` or `StreamPlan::Transcode { target, bitrate_kbps, offset_s }`, where `target` is `Target::Opus` or `Target::Mp3`:

- `format=raw` → Raw. No `format` and `maxBitRate` absent or 0 → Raw.
- `format=opus|mp3` → Transcode at `maxBitRate` when it is above 0, else at the config bitrate for that target.
- `maxBitRate > 0` and no `format` → Transcode to Opus at `maxBitRate` when the source is lossless or its bitrate exceeds `maxBitRate`; else Raw. A lossy source whose bitrate is not known is Raw.
- `maxBitRate` and the config bitrates are clamped to 32–320 before these rules read them, so a source of 32 kbps is Raw for `maxBitRate=8`.
- Any other `format` → error 0 with the message `parameter format must be raw, opus or mp3`. The value is compared as it is, so `RAW` is another format.
- `timeOffset` is the `offset_s` of a transcode. It has no effect on Raw, and `estimateContentLength` has none either: a client seeks in the original file with a range, and the response has the length of the file.

The server has no transcoder, so the `stream` handler answers a `Transcode` plan as it answers `Raw`: with the original file, the content type of the file, its length and its ranges (see Raw below). A request with `format=opus`, with `format=mp3` or with a `maxBitRate` below the bitrate of the file gets the bytes of the file, and its `timeOffset` has no effect. `decide` returns the plan that the request selects, and the one `match` arm of the handler for `Transcode` is the place of this rule. WAX-869 builds the transcoder and replaces the rule. A `format` that names no target stays error 0.

Remote-mode seam: remote mode adds a connection-kind argument to `decide` (forcing Opus 192 unless the connection is direct) and a second listener in `server::run` that tags its connections. In local mode there is one `TcpListener` and no connection kind.

Raw (`media::stream::raw`) serves the original file unchanged. The path of the file is the path of the folder row of the track joined with the components of `rel_path` (`browse::file_path`); no value of the request is part of it. Two checks precede the response, and each failure is error 70:

- The file is inside a configured music folder. `fs::canonicalize` resolves every symbolic link of the path, and the result must be below the resolved path of a folder of `library.folders`. A link inside a folder, a link from one folder into another one and a folder that is itself reached through a link pass. A link that points outside the configured folders does not pass. The scanner follows such a link and stores a row for the file behind it (§1.3), so the track is listed, and `stream` answers error 70 for it. A row whose folder left the config is not served either, until a scan removes it (§3.10 step 5).
- The file is the file that the scan saw: a regular file whose size and whose modification time in whole seconds equal `size` and `mtime` of the row. That is the test by which the scanner decides that a file did not change (§3.10 step 3). A file that was replaced, exchanged with another file or retagged since the scan is not served until the next scan stores its state, so the bytes of another file are not served under the ID. A replacement with the same size in the same second is not detected, as the scanner does not detect it.

Both checks read the path, and `ServeFile` opens the path after them. A file that is exchanged between the checks and the open is served for that one request. A check through the open file needs a file service that takes an open file, which `ServeFile` is not.

The file is served by `tower_http::services::ServeFile::new_with_mime(resolved path, content type of the row)`. It answers GET and HEAD with `Accept-Ranges: bytes`, `Content-Length`, `Last-Modified` and `ETag`, a range with HTTP 206 and `Content-Range`, a range that no byte satisfies with HTTP 416 and `Content-Range: bytes */<size>`, and `If-None-Match` and `If-Modified-Since` with HTTP 304. It reads the file in chunks of 64 KiB as the client takes the body, so the memory of a response does not grow with the file. No task outlives the body, and the file is closed when the body is dropped, which is what a client that disconnects causes. The handler builds the request that `ServeFile` gets:

- A HEAD request is passed on as a HEAD request, for which `ServeFile` opens no file. Every other request is passed on as a new GET request: the router has read the form body of a POST (§3.8), and the method has no other meaning for `stream`.
- The request carries `If-None-Match` and `If-Modified-Since` of the client request, and `Range` unless the request is a HEAD, for which a range is not defined (RFC 9110 section 14.2). No other header and nothing of the URI is passed on.
- `If-Range`: `ServeFile` does not read the header, so the handler evaluates it. When the answer to a range request is HTTP 206 or 416 and the `If-Range` value is neither the `ETag` nor the `Last-Modified` value of that answer, the handler drops the answer and asks `ServeFile` again without the range, so the client gets the whole file. A weak tag equals no `ETag`.
- Several ranges: `ServeFile` builds no multipart response. With `ignore_multi_range_requests(true)` it answers such a request with the whole file.
- A suffix range that is longer than the file selects the whole file (RFC 9110 section 14.1.2). `ServeFile` answers it with HTTP 416, so the handler passes `bytes=0-` in its place.

A 404 from `ServeFile` (file removed after the checks) becomes error 70, and another failure status becomes error 0. `ServeFile` writes the modification time as an HTTP date and panics for a time before 1970 or after the year 9999. A row with such an `mtime` is answered with error 0 before the file is read. HTTP 200 and 206 get `Content-Disposition: inline; filename="<plain>"; filename*=UTF-8''<encoded>` with the file name from `rel_path`. `<plain>` is the name with `_` in place of every character outside ASCII, every control character, `"` and `\`. `<encoded>` is the exact name, percent-encoded outside the unreserved set of RFC 3986.

Transcode: `Transcoder` is a trait with `async fn spawn(&self, job: TranscodeJob) -> Result<TranscodeStream, TranscodeError>`. `FfmpegTranscoder` runs

```
ffmpeg -nostdin -v error [-ss <offset>] -i <path> -map 0:a:0 -vn -c:a libopus -b:a <N>k -f ogg -
ffmpeg -nostdin -v error [-ss <offset>] -i <path> -map 0:a:0 -vn -c:a libmp3lame -b:a <N>k -f mp3 -
```

through `tokio::process::Command` with `stdout` piped, `stderr` to null (piped to a `debug` log task when the log level is `debug`), `kill_on_drop(true)`. The body is `Body::from_stream(ReaderStream::new(stdout))` in a wrapper that owns the `Child` and the semaphore permit; its `Drop` calls `start_kill()` and spawns `wait()` so the process is reaped promptly. Headers: the target `Content-Type`, `Accept-Ranges: none`, `Cache-Control: no-store`, and `Content-Length` only with `estimateContentLength=true` (bitrate × remaining duration ÷ 8; the wrapper truncates at that length, and a shorter stream ends the connection early). A `Range` header is ignored. The permit wait is 5 s, then HTTP 503 with `Retry-After: 5`. At startup `ffmpeg -version` is run once; failure logs a warning and transcode requests return error 0 `ffmpeg not available`. `Child.transcodedContentType` and `transcodedSuffix` are not set because the server does not transcode by default.

No transcode cache: local streams serve originals by default and transcodes happen only on request.

### 3.10 Scanner

`library::scanner::scan(db, folders, cancel, full, options) -> Result<ScanSummary>` runs on one blocking thread; an async caller runs it with `spawn_blocking`. `folders` holds the configured folder paths. `ScanOptions { batch_size: usize, on_batch: Option<Box<dyn Fn() + Send>>, progress: Arc<AtomicU64>, covers_cache_dir: Option<PathBuf> }` defaults to 500, `None`, a new counter and `None`; tests set a small batch and a hook that runs after each batch, which makes cancellation deterministic. The scan adds 1 to `progress` for each supported file it has finished with. `covers_cache_dir` is `cache/covers` of the data directory (§3.5). Every caller that scans the library of a data directory sets it to `DataDir::covers_cache_dir()`: the scan at startup, `startScan`, the periodic scan and `wn-server scan`. Without it the scan removes no cached cover, and a resized cover of an old image can be served for a new image with the same modification time. `ScanSummary { seen, added, changed, moved, removed, tags_read, albums, artists, errors }` holds the supported files seen, the track rows inserted, updated in place, relinked and deleted, the calls of the tag reader, the album rows and the artist rows in the database when the scan ended, and one message per problem that kept the scan from deleting tracks.

1. Insert a `scans` row and sync the folder rows with the configured paths (see the end of this section).
2. Load every existing track of every folder into a map keyed by `(folder_id, rel_path)` with `(id, mtime, size, fingerprint)`. Steps 1 and 2 are one transaction.
3. For each configured folder, `walkdir` with `follow_links(true)` and the entries of a directory in name order; skip entries whose name starts with `.` and the system directories of §1.3 (the name of the folder itself is exempt), names that are not valid Unicode (warning), entries that are not regular files, and unsupported extensions. A skipped directory is not walked, and a skipped entry that cannot be read is no unread path. The walk reads directory entries and file states, not file content. For each file, build `rel_path` from components. Existing and unchanged (`mtime`, `size`) → the row is present and the file needs no work. Existing and changed, or existing while `full` is set → a changed path. No row → a new path. For step 6 the walk also records each directory in which it saw a supported file, and the cover image file of each directory (§1.3) with its modification time. A cover image file that is a symbolic link is skipped, so that the scanner stores no cover that `library::covers` does not read (§3.11). A cover image file whose state cannot be read is skipped with a warning; it is no unread path, because it is the file of no track.

   What the walk cannot read is an unread path: the whole folder when its root cannot be read or is not a directory, and otherwise the path of the entry that failed (a directory that cannot be listed, a symbolic link whose target is gone, a link loop, a file whose state cannot be read). The scan does not know whether the files at and below an unread path exist, so their rows are neither matched nor deleted. It does not know which other files are there either, so no track is deleted in a scan with an unread path (step 5). The scan records an error that names the folder and the path. The walk continues with the next entry or the next folder.
4. Fingerprints, matching and writes:
   - Fingerprints: the fingerprint of every changed and every new file is read. The pass is skipped when no row can be matched, which is when no path changed and no file of a row is gone; every new file is then a new track. A changed path whose fingerprint equals the stored one keeps its row and has its tags re-read in place. A changed path whose fingerprint differs makes its stored row displaced and the file a candidate. A new file is a candidate. A file whose fingerprint cannot be read is an unread file: it is no candidate, its row, when it has one, stays as it is, and the scan records an error that names the file and keeps its size for step 5.
   - Missing = the rows whose file is gone, plus the displaced rows. The file of a row is gone when the walk read the path and found no file, and when the folder of the row is no longer configured. Rows at and below unread paths are not missing.
   - Matching is missing-driven, in the order of `(folder_id, rel_path)`: for each missing track, among the candidates with the same fingerprint that no earlier track took, choose the one with the same file name, then the shortest path distance (the directory steps between the two relative paths), then the order of the relative paths; a match relinks the row (`folder_id`, `rel_path`, `mtime`, `size`; tags re-read only when `mtime`, `size` or the extension differ) and counts as moved. A displaced row without a match stays at its path and takes the fingerprint and the tags of the file there, which counts as changed; when another track took that file, the displaced row leaves its path (the exchange batch below). A file that is retagged in place is the first case: the retag changes the fingerprint, no file carries the old one, and the row keeps its ID. Candidates that no track took and that update no row become new rows with new IDs. Two files that exchange paths in one interval are two displaced rows and two candidates, so both rows are relinked and keep their identities.
   - Writes: the changes are stored in batches of `batch_size` files. For each batch the tags are read first, and the fingerprints when the fingerprint pass was skipped; then one write transaction stores the batch; then `on_batch` runs. The writer is held for the statements only. A file is opened once for its fingerprint and once for its tags.
   - The exchange batch: the changes that vacate or take the path of a displaced row are the first batch and one transaction whatever their number, because one row takes the path that another row leaves. They are the relink of a displaced row, the relink of a row to the path of a displaced row, the new row of a file at the path of a displaced row that moved, and the removal of a displaced row whose path another track takes. In the transaction the deletions run first, then every displaced row that leaves its path is given the placeholder path `/<id>`, which no file can have, and then the rows that move are given their new paths, because SQLite checks `UNIQUE(folder_id, rel_path)` per statement.
   - A track without a file: a displaced row leaves its path in the exchange batch when its own file is found at another path, and when another track takes the file at its path. It is relinked in the first case and deleted in the second. In two cases the scan can do neither, and the row keeps the placeholder path `/<id>` as its `rel_path`: another track takes its path and an unread path or an unread file may hold its file (step 5), or its file was found and the tags of that file cannot be read. The scan records an error that names the track, its old path and the reason. The row keeps its ID and everything attached to it. It has no file to serve, and its ID does not name the file that is now at its old path. A later scan treats it as a missing row: it relinks the row when a file with its fingerprint is found and its tags can be read, and it deletes the row under the rules of step 5.
   - A file whose tags cannot be read is left for the next scan with a warning: a new file gets no row, a changed file leaves its row as it is, and a moved file does not get its row. The row of the moved file stays at its old path when no file is there, and it becomes a track without a file when it is displaced. The next scan matches the row again and reads the tags again.
5. Deletion: a missing track without a match is deleted when the scan knows that its file is gone. The scan does not know that in these cases, and the track stays:
   - The scan has an unread path. Any file may be at or below it, so no track is deleted in that scan.
   - The scan has an unread file whose size is the size at the start of the track's fingerprint (`size:hash`). The file may be the file of the track. Tracks of other sizes are not affected.
   - The track is in a configured folder that previously had tracks and yielded no supported file this scan.
   - The track is in a folder that is no longer configured, and a configured folder yielded no supported file, or no folder is configured. The files of the track may be in a folder whose volume is not mounted.

   Each case is named in `scans.error`, with the number of tracks that were kept: one message counts the tracks that an unread path or an unread file kept, and the message of a folder that is no longer configured counts the tracks kept in it. The row of a folder that is no longer configured is deleted when no track is left in it. These rules hold the deletions back on every scan, with an error each time, for as long as their cause stays: a path that cannot be read (a link loop, a directory without permission, a link whose target is gone) until it is fixed or removed, and a configured folder without a supported file (an import folder that is still empty) until it holds a file.
6. Aggregate, in one write transaction that reads no file:
   - Delete the albums that no track row refers to, then the artists that no album row and no track row refers to. These are the only deletions of album rows and artist rows. A track that step 5 kept, and a track without a file, keep their album and their artist.
   - Recompute for every album `song_count`, `duration_ms`, `year` (the smallest year of a track) and `genre` (the genre of the most tracks; the first in byte order among genres of equally many tracks), and for every artist `album_count`. A row that has its values is not written, so a scan that finds nothing changed writes no album row and no artist row.
   - Set the display names (§3.3): `name` and `sort_name` of every artist row whose name no track carries, then `name` of every album row whose name no track carries, then `artist_name` of every album row that differs from the `name` of its artist row. Each of the first two reads the tracks of one row through the index on `artist_id` or `album_id`, and stops at the first track that carries the name. A row that has its names is not written.
   - Set `cover_source` and `cover_mtime` of every album whose directory the walk saw: the absolute path and the modification time of the cover image file of the directory, else `embedded:<track id>` and the modification time of the first track that has a file and a picture, else NULL for both (§1.3). The walk saw a directory when it listed the directory and saw a supported file in it. It did not see the directory of an album in a folder that is not configured or yields no supported file, a directory at or below an unread path, and the directory of a row that step 5 kept although its file is gone; the cover of such an album stays as it is stored. A cover file that was replaced, added or removed is therefore found by a scan in which no track changed.
   - Delete the `annotations` whose `item_id` is the ID of no track, album or artist.

   After the commit, with the write connection released, the scan removes the files `<album id>-*` from `covers_cache_dir` for every album that was deleted and for every album whose `cover_source` or `cover_mtime` changed. The name of a cached file holds the modification time and not the source (§3.5), so a new source with the same modification time needs the removal too. An album whose stored cover was NULL is included: the cache holds files of such an album when a scan without `covers_cache_dir` cleared its cover, and when a process ended between the commit and the removal. A file that cannot be removed stays, with a warning.

   A cancelled scan runs the recomputation and sets the display names only: it deletes no album, no artist and no annotation and sets no cover. It has not stored every change it found, so an album without a track may get a track from a file whose change waits for the next scan, and a row may name a path at which the walk saw no file. Until a scan completes, an album that lost its tracks is listed with `song_count` 0 and a new album has no cover. Step 6 keeps no state between scans: a scan that completes sets the counts and the covers of every album, also of albums whose tracks an earlier scan stored without reaching step 6.
7. Close the `scans` row with the counts and with `error`: the messages of the scan joined with `; `, `cancelled` for a cancelled scan, or the database error that ended the scan.

First-scan cost: the fingerprint reads up to 128 KiB per new file (12.8 GB for 100 000 files), which is acceptable on a local disk and slow on a network share; later scans fingerprint only new and changed files.

Cancellation is checked during the walk, during the fingerprint pass and between batches. A cancelled scan keeps the batches it committed and deletes nothing; of step 6 it runs the recomputation and the display names only.

Scan triggers (`library::scan_controller`): one scan of a library runs at a time. The scan decides its changes from the rows it read at its start (step 2), and a second scan that writes rows in the meantime would make those decisions stale. Two rules keep it to one scan:

- `ScanController` in `AppState` runs the scans of the server process: the scan at startup, the periodic scans and `startScan`. `start(full) -> Started | AlreadyRunning | RunningElsewhere` starts a scan with `spawn_blocking` and returns without waiting for it, so a scan delays no request and not the start of the server. `status() -> { scanning, count }` reports whether a scan of the controller runs, and the `progress` counter of the scan that started last. The counter is 0 when a scan starts and keeps its value when the scan has ended.
- `ScanLock` is an exclusive lock of the operating system on the file `scan.lock` in the data directory (`File::try_lock`: `flock` on Unix, `LockFileEx` on Windows). Every scan holds it from its start to its end: a scan of the controller and a scan of `wn-server scan`. A process asks for the lock without waiting. The operating system ends the lock when the process ends, so a process that was killed leaves no lock behind.

| Trigger | No scan runs | A scan of the server process runs | Another process holds the scan lock |
|---|---|---|---|
| startup, with `library.scan_on_start` | starts a scan that is not full | does not occur: the controller is new | starts none and logs a warning |
| periodic, with `library.rescan_interval_minutes > 0` | starts a scan that is not full | starts none; the next one is due one interval later | starts none and logs a warning |
| `startScan` | starts a scan, a full one with `fullScan=true`, and answers `scanning: true` | starts none and answers `scanning: true` with the count of the running scan | starts none and answers error 0 `another process is scanning the library` |
| `wn-server scan` | scans in its own process (§3.6) | scans nothing and exits with 1 | scans nothing and exits with 1 |

No trigger queues a scan. A `fullScan=true` or a `--full` that finds a scan running is not carried out, and the caller asks again. When the lock file cannot be opened or locked, no scan starts: `startScan` answers error 0, the startup and the periodic trigger log a warning, and `wn-server scan` fails.

`run` (`server::start_scans`) works in this order after it has bound the listener and before it serves the first request. A second server on the same data directory fails at the bind, so it stores no folder and starts no scan.

1. It stores the configured folders in the `folders` table (see the folder sync below), so folder listings are right without a scan.
2. It builds the controller with `DataDir::covers_cache_dir()` and `DataDir::scan_lock_path()`.
3. With `library.scan_on_start` it calls `start(false)`.
4. With `library.rescan_interval_minutes > 0` it starts a task that calls `start(false)` every interval, the first time one interval after the start.

`run` waits for no scan. The token of the controller is a child of the token that asks the server to stop, so a stop cancels the running scan and ends the periodic task at once. Step 3 of the stop waits for both, for a limited time, and the scan ends as a cancelled scan (§3.13).

The controller keeps no state of a scan that has ended. After a scan that recorded errors, after a scan that a database error ended and after a scan whose thread panicked, `scanning` is `false`, the lock is free and the next trigger starts a scan. The errors of a scan are in `scans.error` and in the log.

The walk sees each directory once. A file that moves during a scan from a directory the walk has not reached into one it has passed is not seen in that scan: its row is deleted, and the next scan inserts the file with a new ID. A walk without a filesystem watch cannot see such a move.

Album and artist rows are upserted by natural key when a track row is written, so a track that is written finds the row of its key with the ID the row has. A new album row takes the compilation flag of the track; a later track with the flag sets it. Nothing clears the flag while the album row exists. A new album row and a new artist row take the spelling of the names in the tags of that track. A later track with the key of a row does not change `name`, `sort_name` or `artist_name` of the row when it is written; step 6 sets them (§3.3). A correction of the tags that keeps the key, such as the capitalisation of an album title, therefore shows on the album row when no track carries the old spelling. A change of the key makes a new row with a new ID.

Folder sync at `run` and at the start of every scan: folders from config are upserted by path. The stored path is rebuilt from `Path::components()`, so a trailing separator, a `.` component or a repeated separator does not make a second row for one directory; `..` components are kept, as `std::path::absolute` keeps them. The name of a folder is the last component of its path (the whole path for a root such as `/` or `C:\`). A folder removed from config has its tracks deleted and its row removed in step 5 (tracks of a folder whose path changed are relinked by fingerprint into the new folder row first, because deletion happens after matching). The sync at `run` only upserts, in one transaction. It deletes no folder row and no track row, so a folder that left the config keeps its row and its tracks until a scan applies the rules of step 5.

### 3.11 Cover art

`library::covers::Covers` is the cover service. `Covers::new(cache_dir, db)` takes `cache/covers` of the data directory (§3.5), and `AppState` holds the value; its clones share the cache and the work in progress. `get(id, size: Option<u32>) -> Result<Option<Cover>, CoverError>` returns `Cover { bytes: Bytes, content_type: &'static str, mtime: i64 }`. `mtime` is the modification time that the last scan stored for the source of the cover. `CoverError` is a database error, or `Unfinished` when the work on the cover ended without a result, which happens when the runtime shuts down. Every other failure is `None`.

Resolution:

- `al-` → the cover that the last scan stored for the album: `cover_source` and `cover_mtime` of the row that `browse::album` returns (§3.10 step 6). The source is the absolute path of an image file, or `embedded:<track id>`, which is the embedded picture of that track. An album without a stored cover → `None`.
- `tr-` → the embedded picture of the track when `has_picture`, with the modification time of the track.
- A track without a file (§3.4) gives no picture, for its own ID and as the source of the cover of an album. `browse::file_path` returns no path for it.
- An ID that `ids::parse` rejects, an ID of another kind and an ID that no row has → `None`.
- The embedded picture of a track is its first picture of the type front cover, or its first picture when none has that type; M4A stores no picture type.

Result:

- No `size`, and `size` 0: the bytes of the image file or of the embedded picture, when they start with the header of a JPEG, PNG or WebP image (see Files). The content type is the type of that format: `image/jpeg`, `image/png` or `image/webp`. The extension of the file and the MIME type that a tag names do not decide it. The image is not decoded, so an image with a whole header and damaged data is returned as it is.
- `size`: a JPEG of quality 85 whose longer side is at most `size` pixels. A `size` over 2000 counts as 2000. The image is decoded with `image`. An image with a side over `size` is scaled down with `thumbnail(size, size)`, which keeps the proportions. An image whose sides are within `size` keeps its size and is encoded as a JPEG; no image is enlarged. The pixels are converted to 8-bit RGB, which drops an alpha channel. The cover is read from `cache/covers/<id>-<size>-<cover_mtime>.jpg`. On a miss it is made, written there and returned.
- An image that cannot be decoded logs a warning and gives `None` for a request with `size`: a damaged image, an image over the limits below, and a WebP image that is lossless or in the extended file form.
- A source that is gone or that cannot be read gives `None` with and without `size`.

Values from the client: `id` and `size` come from the client, and neither is part of a file path. The ID selects a row, and the service opens the path that the scanner stored for that row. A stored cover source that is not an absolute path with the extension `jpg`, `jpeg`, `png` or `webp` is not opened. The name of a cache file is built from the ID that the row holds, the size after the limit of 2000 is applied, and the stored modification time.

Files: the files come from the music folders. A file there can be damaged, and it can be put there to make the server return another file or to exhaust its memory. Two rules hold for every request. The first keeps the service from reading a file through a link, and the second keeps it from returning a file that is no image. A link to a file that is no image is refused under both.

- No symbolic link is followed to an image file. The service reads an image file when its path names a regular file. It reads the state of the path without following a link (`fs::symlink_metadata`), and the file that it then opens must be the file of that state. On Unix it compares the device and the inode. The standard library gives no file identity on Windows, where the test of the path is the only one and creating a link needs a privilege. A path that names a link, a directory or a device gives `None`, with and without `size`. A directory in the path can be a link. The scanner follows the same rule: it stores no link as the cover of an album (§1.3, §3.10 step 3), so such an album has its embedded picture as its cover, or none. The rule also holds for a file of the cache.
- Only an image is returned. Without `size` the service returns the bytes of a source when the decoder of JPEG, PNG or WebP accepts their header. It reads the header and no pixel. With `size` the service returns the JPEG that it encoded. Other bytes give `None` and a warning: text, a file of another kind under the name of a cover, and a picture in GIF, BMP or TIFF, for which `image` is built without a decoder.

An audio file is read through a symbolic link, as the scanner reads it (§1.3). The service takes from an audio file the picture that lofty finds in its tag, under the second rule, and never the bytes of the file.

Limits:

- A source image of more than 16 MiB has no cover, with and without `size`. It is the limit of lofty for one item of a tag, so an image file has the limit of an embedded picture.
- An image is decoded when its width and its height are at most 4096 pixels and its decoded pixels take at most 64 MiB, which is an image of 4096 × 4096 pixels with four bytes for each pixel. Both are set as `image::Limits`. The decoder reads the size from the header of the image and returns an error before it allocates the pixels, so a small file that declares a huge image takes no memory.
- A WebP image is decoded when it is lossy and in the simple file form, which is a file whose first chunk is `VP8 `. For a lossless image and for the extended file form, which holds an alpha channel, an animation or metadata, the WebP decoder builds tables and planes whose size the two limits above do not bound.
- The service holds the source of one image at a time. One permit of a semaphore covers the read of a source and then its verification, or its decoding, scaling and encoding. A job and a request without the permit wait without a thread.
- Reading, verifying, decoding and encoding run on `spawn_blocking`. A tag reader or a decoder that panics gives `None` and a warning.

Memory: with these limits the work of the service on images takes 242 MiB at most. It is the largest sum in the table. Each row counts the buffers that exist at one time: the copies of the source, the working memory of the decoder, and the pixels. An image of 4096 × 4096 pixels has 16 Mi pixels, so one byte for each pixel is 16 MiB.

| Case | Copies of the source | Decoder and pixels | Sum |
|---|---|---|---|
| JPEG with four components that is progressive or has a scan for each component | 64 MiB: the source (16), the copy that `image` hands to zune-jpeg (16), and the metadata segments that zune-jpeg copies and joins (32, for a file that holds nothing else) | 178 MiB: two bytes for each coefficient of each component (8 × 16 = 128), the RGB pixels (3 × 16 = 48), and the buffers for one row of blocks (2) | 242 MiB |
| JPEG with three components, of the same kind | 64 MiB | 146 MiB: coefficients (6 × 16 = 96), pixels (48), row buffers (2) | 210 MiB |
| JPEG, baseline with one scan | 64 MiB | 50 MiB: pixels (48), row buffers (2) | 114 MiB |
| PNG | 32 MiB: the source (16), and its text chunks as strings, which can take twice their bytes (16 more) | 128 MiB: the pixels (64), and the buffers of the `png` crate, which holds them under the same limit of 64 MiB: one chunk, the ICC profile and the text chunks | 160 MiB |
| WebP, lossy, simple form | 64 MiB: the source (16) and the copies of its partitions (48 while the last one is read, 16 after) | 76 MiB: the RGB pixels (48), the three planes of the frame (1.5 × 16 = 24), the records of the macroblocks (4) | 140 MiB |
| Scaling and encoding, when the decoder and the source are freed | none | 95 MiB: the pixels (64) and the scaled pixels (31, for 2000 × 2000 pixels of 8 bytes); after that the scaled pixels, their RGB form (12) and the JPEG | 95 MiB |
| Verifying a cover without `size` | as in the rows above | no pixels; for PNG the buffers of the `png` crate (64) | 96 MiB |

The figure leaves out two things. lofty reads the whole tag of an audio file to find a picture, with at most 16 MiB for one item and no limit on their number, so a tag with several large pictures takes more; the scanner reads the same tags. A cover that `get` returned is held by its request until it is sent: at most 16 MiB for each request in progress.

Requests at the same time: the first request for a resized cover that the cache does not hold starts a job in a task of its own, under the name of the cache file. Every request for the same file while the job runs waits for that job and gets its result, a failure included, so the image is decoded once. The job looks in the cache again before it decodes, because another job can have written the file after the request looked. Jobs for different cache files wait for the one permit in the order in which they asked. A job that waits for the permit is dropped when no request waits for it any more, so a client that ends its requests leaves no work in the queue. A job that has the permit runs to its end and fills the cache, also when every request that waited for it is gone. A request that comes while a job is dropped starts a new job. A request without `size` waits for the permit itself and leaves the queue when it ends.

Cache writes: the JPEG is written to the new file `tmp.<32 hex characters>` in the cache directory, flushed to the disk and renamed (§2), so a reader opens the whole file or none. The file is flushed before the rename, so that a power failure leaves no file with the final name and without its bytes. The directory is not flushed: a power failure can undo the rename, and the cover is then made again. The service creates the cache directory at the first write. A cover that cannot be written to the cache is returned without being cached, with a warning. The scanner removes every file of the cache whose name starts with the ID of an album and a hyphen when the cover of that album changes or the album is deleted (§3.10 step 6), so the name of the temp file has no hyphen and starts with no cover ID.

Present limits:

- The cache has no bound. A cover has one file for each size from 1 to 2000 that a client asked for, also when the files hold the same JPEG because the image is smaller than the sizes. Nothing removes the cached covers of a track ID, and nothing removes a temp file that a crash left behind.
- The EXIF orientation of an image is not applied to a resized cover. A client applies it to a cover without `size`, so the two can differ by a rotation.
- An embedded picture in GIF, BMP or TIFF is not served, with or without `size`. `has_picture` is set for such a track, so `coverArt` can name a cover that `get` does not return.
- A WebP image that is lossless or in the extended file form is served without `size` and is not resized.
- An image with a side over 4096 pixels, or with more than 64 MiB of pixels, is served without `size` and is not resized.
- A cover without `size` is held in memory: at most 16 MiB for each request in progress. `get` returns bytes and no stream.
- An image that cannot be decoded is read and tried again for each request with `size`, with a warning each time. Nothing remembers the failure.
- A scan that stores a new cover with the modification time of the old one removes the cache files after its commit. A job that resolved the old source before the commit can write its file after that removal, and the file then holds the old image under the name of the new cover until the cover changes again.
- A hard link to a file outside the music folders is read as the regular file that it is. Its bytes are returned only when they are an image.

`getCoverArt` sends `Cache-Control: public, max-age=86400` and `ETag: "<id>-<size>-<cover_mtime>"`, and answers `If-None-Match` with 304.

### 3.12 mDNS

No DNS-SD type exists for Subsonic servers; the server registers its own and the mobile app browses for it. This section is the contract between the server and a client that discovers it, and a client can be written from it alone. Unit tests in `discovery` pin the service type, the SRV target, the TXT record and the address choice.

#### Records

A client browses the service type `_waxnerd._tcp` in the domain `local.`. The server registers no subtype. A running server is one service instance with these records:

| Record | Content |
|---|---|
| PTR `_waxnerd._tcp.local.` | `<instance name>._waxnerd._tcp.local.` |
| SRV | priority 0, weight 0, port `server.port`, target `wn-server-<hex>.local.` |
| TXT | the keys of the next table |
| A and AAAA of the target | the addresses under Addresses |

- The instance name is `server.name`: 1 to 63 bytes of UTF-8, which the validation at load enforces (§3.5). When another responder on the link holds that name with other records, the daemon renames the instance by appending ` (2)`, then ` (3)` and so on. Two servers on one link that keep the default name `wn-server` are such a case. TXT `name` keeps `server.name`. A client does not identify a server by its instance name.
- The target is the server's own host name, `wn-server-<hex>.local.`. `<hex>` is the first 8 hexadecimal digits of the UUID in `instance.id`: the target for an `instance.id` of `1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d` is `wn-server-1a2b3c4d.local.`. `discovery::host_name` builds the name from the instance ID and from no other input. The A and AAAA records that the server publishes are records of this name only.
- The server never claims the host name of the machine. It does not read that name, and it publishes no record for it and answers no query for it. The responder of the operating system owns the machine's host name, and a second responder that publishes records for that name makes the operating system rename the machine: macOS changes the local host name `studio` to `studio-2`.
- The target says nothing about the machine. It is not the machine's host name, and a client derives no name and no property of the machine from it. A client takes the target from the SRV record and builds it from no other value. It resolves the target to the A and AAAA records and connects to one of their addresses, on the SRV port.
- The A and AAAA records that the server publishes for the target hold the addresses under Addresses and no other. An address can still fail for a client: a link-local IPv6 address needs a zone index, and a server that binds `::` on Windows does not accept its IPv4 addresses. A client therefore follows three rules.
  1. It skips every link-local IPv6 address (`fe80::/10`). A connection to such an address needs a zone index, and the URL of this contract carries none.
  2. It tries the other addresses one after the other, in an order of its own choice. It bounds each attempt with a timeout of its own choice.
  3. It goes on to the next address when an attempt fails for any reason: the connection is refused, the attempt times out, the address is unreachable, or any other error occurs. The server is unreachable for the client when no address is left.

#### TXT record

| Key | Value | What a client does with it |
|---|---|---|
| `txtvers` | `1`. A decimal number: the version of this table. | Ignores an instance whose `txtvers` it does not know. |
| `api` | `opensubsonic`. | Ignores an instance with another value. |
| `path` | `/rest`. A URL path that starts with `/` and does not end with `/`. | Builds the URL of an API method as `http://<address>:<port><path>/<method>`, for example `http://192.168.1.5:4200/rest/ping`. The scheme is `http` because `tls` is `0`. An IPv6 address is in brackets. |
| `id` | The UUID in `instance.id`: 36 characters, lower-case hex digits in groups of 8, 4, 4, 4 and 12, separated by hyphens. | Stores the server under this key. Two instances with one `id` are one server: the client keeps the instance whose SRV and TXT records it received last, with the port and the addresses of that instance, and drops the other. |
| `name` | `server.name`: 1 to 63 bytes of UTF-8. | Shows it as the name of the server. |
| `auth` | `password`, or `none` when `auth.disabled_unsafe` is set. | `password`: sends `u` with `p`, or with `t` and `s` (§3.7). `none`: sends none of them. Ignores an instance with another value. |
| `tls` | `0`: the server speaks plain HTTP. No other value is defined. | Ignores an instance with another value. |
| `ver` | The Cargo version of the server, unchanged: `MAJOR.MINOR.PATCH`, for example `0.1.0`, with a suffix for a pre-release, as in `0.2.0-beta.1`. | Shows the text as it is. A client does not parse it and derives no behaviour from it. |

- The record holds no other key today. Every device on the network can read the record, so no key carries a credential, an account name or a folder path.
- `txtvers` is the first key. The order of the other keys is not part of the contract. Each key appears once. The keys are lower-case ASCII, and a client compares keys without regard to case (RFC 6763 section 6.4). The values are UTF-8; only `name` can hold characters outside ASCII.
- A new key does not change `txtvers`, and a client ignores a key it does not know. `txtvers` increases when a key is removed or when the format or the meaning of a key changes.
- `id` stays the same across restarts and across changes of `server.name`, the machine's host name, the port and the addresses. It is new after `init` on a new data directory, and after `run` wrote a missing `instance.id`. The SRV target is built from `id` and changes only when `id` does.

#### Addresses

`discovery::addresses` and `discovery::interface_kinds` choose the addresses from `server.bind`:

| `server.bind` | A and AAAA records |
|---|---|
| `0.0.0.0` | Every IPv4 address of the interfaces that are up, the loopback interface included. The daemon follows address changes. No AAAA record. |
| `::` | Every IPv4 and IPv6 address of those interfaces, link-local IPv6 addresses included. The daemon follows address changes. |
| another IPv4 address | That address. No AAAA record. |
| another IPv6 address | That address. No A record. |

- The daemon announces an address on the interface whose subnet holds it and on no other. A client therefore receives the addresses of the link it is on. A server that binds one address is announced on the link of that address only: a server on `127.0.0.1` is visible to clients on the same machine and not on the LAN.
- The daemon does not use point-to-point interfaces, such as VPN tunnels, or the Apple peer-to-peer interfaces `awdl` and `llw`.
- The daemon publishes the link-local IPv6 addresses of a server that binds `::`. A client skips them (Records, rule 1).
- Windows: a listener on `::` accepts IPv6 connections only, because Windows sets `IPV6_V6ONLY` on a new socket and the listener does not clear it. The A records of a server that binds `::` on Windows therefore name addresses that the listener does not accept. Linux and macOS accept both families on that listener.

#### Start, failure and stop

- `discovery::TxtRecord::build`, `discovery::host_name`, `discovery::addresses` and `discovery::interface_kinds` are pure. `discovery::Advertiser` owns the `mdns-sd` `ServiceDaemon`, which runs on its own thread. For `0.0.0.0` and `::` the registration names no address and uses `enable_addr_auto()`. For another address the registration names that address. For an IPv4 bind the daemon disables IPv6 interfaces (`disable_interface(IfKind::IPv6)`).
- The `run` command starts the advertisement before `run_server` when `discovery.mdns` is on. Step 1 of the stop withdraws it, at the moment the server is asked to stop (§3.13). `run` withdraws it itself when `run_server` fails before it serves, and it waits for a withdrawal that is running when a second signal ends the stop. The step runs once, and the process does not exit during it. Before the start it writes a new `instance.id` when the file is missing and `library.db` exists, and logs one warning. `init` refuses a data directory that has a config file and an owner account, so no other command restores the file. The ID must be a UUID in the form `init` writes.
- The daemon probes for its names for 750 ms to one second and then announces the instance (RFC 6762 section 8.1).
- The server runs without the advertisement when the advertisement cannot start. `run` logs one warning when `instance.id` cannot be read or is not a UUID, or when `mdns-sd` returns an error. No host name is built from a text that is not a UUID. `discovery` checks the length of `server.name` as well and reports it the same way. `run` does not reach that check with a name outside 1 to 63 bytes, because the validation at load fails first (§3.5). `mdns-sd` opens its multicast sockets on the daemon thread and reports a socket it cannot open in its debug log only, so a second thread waits for the first announcement: it logs the announced name at `info`, or one warning when nothing was announced within 10 s.
- Withdrawal: `unregister` sends a goodbye packet on every interface, the daemon repeats it after 120 ms, and `shutdown` ends the daemon thread. A client then sees the instance removed. `Advertiser::stop` returns when these steps are done: it waits for the reply to `unregister`, which the daemon sends after the first packet, then 200 ms for the repeat, then for the reply to `shutdown`. It waits 1 s at most for each reply, so it returns after 200 ms when the daemon answers and after 2.2 s at most. A stop before the first announcement sends no goodbye packet, because nothing was announced. The thread that waits for the first announcement records it; a stop after the 10 s of that thread always sends the packet. A goodbye packet for an instance that was not announced would remove the records of another running server with the same instance name from the clients, for example when `run` is started twice and the second server cannot bind its port.
- A server that ends without the goodbye packet stays in the caches of clients until its records expire. `mdns-sd` sets 120 s for the SRV and address records and 4500 s for the PTR and TXT records. These times are not part of the contract, and a client does not depend on them.
- Windows Firewall prompts on first start for TCP 4200 and UDP 5353.

### 3.13 Logging and shutdown

`tracing_subscriber::fmt` to stderr with an env filter from `RUST_LOG`, else `server.log_level`. An empty `RUST_LOG` counts as unset. Colours are written only when stderr is a terminal. No log files.

Startup log: `run_server` logs one line at `info` when the listener is bound and the scans are started. The message is `listening on <address>` with the address of the listener, and the fields are `version` (the Cargo version), `data_dir`, `folders` (the number of configured music folders) and `auth` (`password`, or `none` with `auth.disabled_unsafe`, as the TXT key of §3.12). §3.8 describes the request log.

#### Stop signals

One `CancellationToken` asks the server to stop. `run_server` receives it, and `AppState.cancel` holds it. The `run` command cancels it at the first stop signal:

| OS | Stop signals |
|---|---|
| Linux, macOS | Ctrl-C (SIGINT) and SIGTERM |
| Windows | Ctrl-C, Ctrl-Break, and the console events close and shutdown |

`cli::forward_stop_signals` listens with `tokio::signal::ctrl_c()` on every OS and with `cli::OtherStopSignals` for the signals that differ, which is behind `#[cfg(unix)]` and `#[cfg(windows)]`. It sends the name of each signal to `cli::run_until_stopped`, which cancels the token at the first one and logs it. The two listeners do not depend on each other. When the process cannot listen for Ctrl-C, or cannot listen for the other signals, `run` logs a warning and listens for the rest. A signal without a listener keeps the action of the operating system, which ends the process without the stop.

#### The stop

`run_server` stops the server in four steps and returns after the last one. Steps 1 and 2 start when the token is cancelled and run at the same time. `server::withdraw_at_stop` is step 1, `server::serve` is step 2 and `server::stop` is steps 3 and 4.

| Step | Starts | What it does | Time limit |
|---|---|---|---|
| 1. Advertisement | when the token is cancelled | Calls `ServerOptions::withdraw` on a blocking thread. The `run` command passes `Advertiser::stop`, which returns when the goodbye packets are sent (§3.12). | 2.2 s inside `Advertiser::stop`; 200 ms when the mDNS daemon answers. |
| 2. Requests | when the token is cancelled | The listener is closed, so the port refuses a new connection. A connection without a request in progress is closed. A request in progress is answered, and its connection is closed after the response. | 10 s, the grace (`server.shutdown_grace_ms`). The connections that are open after it are closed, with one warning. |
| 3. Scan | when step 2 has ended | Waits for the task of the periodic scans and for the running scan, which the token cancelled. | 10 s, the length of the grace again. A scan that runs after it is left behind, with one warning. |
| 4. Database | when steps 1 and 3 have ended | `Db::close` waits for the running reads and writes of the handle and runs `PRAGMA wal_checkpoint(TRUNCATE)`. | 5 s, the `busy_timeout`, when a connection of another process holds the log. |

`run` then ends its runtime and exits with status 0. A stop without a request in progress and without a scan takes the time of step 1. The longest stop takes the two limits of 10 s and the limit of step 4. The log of a stop has these lines at `info`: the signal, `the mDNS advertisement is withdrawn`, `waiting for the running scan to stop` when a scan runs, the end of that scan, and `the server has stopped`.

- Step 1 comes first, because the listener is closed at the same moment: from then on the advertisement would name a server that refuses every connection. The advertisement serves new connections only, so the requests in progress do not need it. A process that is ended from outside during a later step, by `docker stop` after its 10 s or by a service manager after its limit, has sent its goodbye packet. Step 4 waits for the end of step 1, so the process does not exit while the packet is sent.
- Step 2 is `axum::serve(..).with_graceful_shutdown(token.cancelled())`. The grace starts when the token is cancelled. The server does not wait the whole grace: the step ends when the last connection is closed. `axum::serve` runs each connection in a task and gives no handle of it, so the listener hands out streams that the stop can end (`server::ClosableListener`). After the grace every read and write of such a stream fails, the task of the connection ends, and the handler and the body of its request are dropped. The client reads the end of the connection in place of the response, or after the part of the body that was sent. A dropped body ends what it owns: a transcode child dies with its body (§3.9). When step 2 has ended, no handler runs.
- Step 3: the token of the scans is a child of the token of the stop (§3.10), so the scan was cancelled when the stop began and stops during step 2 in most cases. The scanner checks the token during the walk, during the fingerprint pass and between batches. A cancelled scan reads at most the files of one batch of 500, then recomputes the album and artist rows and closes its `scans` row with `cancelled` (§3.10). The server waits for that for 10 s. A scan that has not ended then reads a file that does not answer, or a batch that takes longer. The server goes on without it: the thread of the scan ends with the process, its `scans` row keeps no end time, and the next scan finishes the work. That is the state that a process leaves that is killed during a scan (§3.6).
- Step 4: a scan that was left behind holds a connection for the statements of a batch only, so the checkpoint runs. A checkpoint that a connection of another process blocks, such as a running `wn-server scan`, is logged at `info` and is no error. The log then holds the changes, and that process or the next start moves them. Another error of the checkpoint is the error of `run`, which exits with 1. After the step the file `library.db` holds every change that was committed before it, and `library.db-wal` is empty unless a scan that was left behind commits another batch.

A second stop signal during the stop ends the process without the steps that have not run. `run_until_stopped` drops `run_server` where it is. `run` then calls the step that withdraws the advertisement. The step runs once: the call does nothing when step 1 has ended, which is the case 2.2 s after the first signal at the latest, and it waits when step 1 is running. `run` prints `wn-server: <signal> ended the server before its stop had finished` and exits with 1. A scan that still runs ends with the process, as the scan of a process that is killed does (§3.6).

The database is consistent after a stop in order, after a stop that left a scan behind, after a second signal and after the operating system has ended the process at any moment. Every change of the server and of the scanner is one SQLite transaction, which is applied in full or not at all, and the write-ahead log that a process leaves behind is read at the next start. What a stop that was cut short leaves undone is the work of the scan: its `scans` row has no end time, and the next scan finishes the work.

`run` ends its runtime with `shutdown_background`, which waits for no blocking thread. A scan that was left behind and a blocking call of a request that step 2 dropped, such as a file read on a mount that does not answer, therefore do not keep the process from exiting.

When `run_server` returns an error before it serves (§3.6), `run` withdraws the advertisement and exits with 1.

Windows ends a console process that has not exited 5 s after the close event or the shutdown event; the time is a system parameter with that default (https://learn.microsoft.com/en-us/windows/console/handlerroutine, Timeouts). Ctrl-C and Ctrl-Break have no such limit. Within the 5 s the server sends the goodbye packet, which takes 200 ms and 2.2 s at most, and it completes the stop when its requests and its scan end in that time. A server whose requests or whose scan outlast the 5 s is ended by the system during step 2 or 3, without the checkpoint of step 4. The database is consistent as described above, and the next start reads the log.

### 3.14 Remote-mode seams

1. `server::run` owns the single `TcpListener`; remote mode adds a TLS-terminating listener fed by the relay and tags its connections.
2. `media::stream::decide` gains a connection-kind argument; it is the only function that decides lossy versus lossless.
3. `users.role` and `auth::extract::authenticate` extend to guests and API keys.
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
| getNowPlaying | empty list stub |
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

<!-- The comment lines below hold the places of the handler modules that have no text in this section yet. A change replaces the line of its module with its text and keeps the blank lines (section 3.8). -->

Browse methods (`api/browse.rs`): the six methods of the ID3 model read the library with `db::queries::browse` and build their elements with the constructors of `api/model.rs` (§3.8). A method that returns artists, albums or songs reads the annotations of the user of the request for the IDs of its answer with `annotations::get_many`, so the elements carry `starred`, `userRating`, `playCount` and `played` as §3.8 describes them.

- `getMusicFolders` returns `musicFolders`, whose `musicFolder` list holds the rows of the `folders` table in ID order, each with its integer `id` and its `name`. `run` stores the folders of the config before it serves (§3.10), so the list holds them before the first scan. A folder that left the config is listed while its row exists, which is until a scan removes the row (§3.10 step 5); its tracks are in the library for the same time. The IDs are the ones that `getUser` reports as `folder` and that `musicFolderId` names.
- `getArtists` returns `artists` with `ignoredArticles` and the `index` list of `model::index` (§3.8). `musicFolderId` limits the list to the artists that have a track with a file in that folder, and `albumCount` stays the stored count. An ID that no folder has gives `artists` without an index: no artist is in such a folder. An empty value counts as absent, and a value that is no integer is error 0 that names the parameter (§3.8).
- `getArtist` returns `artist` with its `album` list in the order of `albums_of_artist`. `getAlbum` returns `album` with its `song` list in the order of `tracks_of_album`, which is the track order of §1.3. `getSong` returns `song`. A track without a file is listed by `getAlbum` and returned by `getSong`, without `path` (§3.4).
- `getArtist`, `getAlbum` and `getSong` require `id`, and an absent one is error 10. Error 70 answers an ID that no row of the kind of the method has. That includes a value without the shape of an ID (§3.3), an empty value and the ID of an item of another kind: `ids::parse` rejects them before a statement runs.
- `getGenres` returns `genres`, whose `genre` list holds the rows of `browse::genres`. In XML the name of a genre is the text of its element (§3.8).
- A list without an item is `[]` in JSON and absent in XML (§3.8). A server without a library answers `getArtists` with `ignoredArticles` and no index, and `getMusicFolders` and `getGenres` with an element that holds nothing.
- Each handler runs its statements in one read transaction (`read_library` in `api/browse.rs`), so the rows of one answer are of one state of the database. A scan that commits between two statements of a request does not change the answer. A database that cannot be read is error 0 with the message `the server cannot read its library`.
- The order of every list is the order of its query, which ends with a unique column (§3.8, Library queries), and of `model::index`, which ends with the ID. Two requests on a library that did not change return the same document.

<!-- api/folders.rs -->

List methods (`api/lists.rs`, queries in `db/queries/lists.rs`): `getAlbumList2` returns `albumList2`, whose `album` list holds `AlbumID3` elements. `getAlbumList` returns `albumList` with the same albums in the same order, each as a directory (`isDir: true`, `parent` is the ID of the album artist). `getRandomSongs` returns `randomSongs` and `getSongsByGenre` returns `songsByGenre`, each with a `song` list of `Child` elements. A list without an entry is `[]` in JSON and absent in XML. The parameters, their defaults and their bounds are those of the OpenSubsonic API, which takes them from Subsonic 1.16.1. Subsonic 1.16.1 does not name `highest` for `getAlbumList2` and OpenSubsonic does; both album methods serve the ten types.

- Parameters: `type` is required by the two album methods. An absent `type` is error 10, and a value that is none of the ten names in their letter case is error 0. `byYear` requires `fromYear` and `toYear`, and `byGenre` requires `genre` (error 10); the other types ignore these three. `getSongsByGenre` requires `genre` (error 10). `size`, which `getSongsByGenre` calls `count`, is 10 without a value, and a value above 500 is lowered to 500. `offset` is 0 without a value, and `getRandomSongs` has none. `musicFolderId` restricts each of the four methods to one music folder, and an ID that no folder has gives an empty list. `size`, `count`, `offset`, `fromYear` and `toYear` are integers from 0 to 4294967295 and `musicFolderId` is an integer; another value is error 0 (§3.8). An empty value counts as absent, also for the optional `genre` of `getRandomSongs`. An empty `genre` of `byGenre` and of `getSongsByGenre` is a value, as the value of every required text parameter is (§3.8), and no genre matches it.
- A value of a request is a parameter of a statement, or it is compared in Rust. The SQL text of the lists is built from constants of the query module.
- Each handler reads its list in one read transaction (`read_lists` in `api/lists.rs`), as the browse methods do. The IDs of a page, the rows of these IDs and their annotations are of one state of the database, so a scan that commits during a request takes no row out of the answer. A page of a list that has an order is shorter than its size only where the list ends. A database that cannot be read is error 0 with the message `the server cannot read its library`.
- The albums and the order of each `type`:
  - `random`: every album, drawn at random for every request.
  - `newest`: every album, `created` descending.
  - `highest`: the albums that the user rated, rating descending.
  - `frequent`: the albums of which the user played a track, play count descending.
  - `recent`: the albums of which the user played a track, last play descending.
  - `alphabeticalByName`: every album, by title.
  - `alphabeticalByArtist`: every album, by the name of the album artist and then by title.
  - `starred`: the albums that the user starred, time of the star descending.
  - `byYear`: the albums of the years from `fromYear` to `toYear`, both included, year ascending. When `fromYear` is greater than `toYear`, the range is the same and the year descends.
  - `byGenre`: the albums whose genre is `genre`, by title.
- Order: the order of every type but `random` ends with the natural key of the title, the natural key of the artist and the album ID (§3.3), so it is total. Two requests return the albums in one order, and the pages that `offset` selects neither repeat nor skip an album while the library and the annotations stay as they are. Titles and names are compared by their natural keys, as `getArtist` orders albums (§3.8). `getSongsByGenre` orders the songs by the title in lower case, then by the track artist, the album title and the track ID.
- Annotations: `highest` and `starred` read the rating and the star of the album. `frequent` and `recent` read the plays of the tracks of the album, because `scrobble` records the play of a track: the play count of an album is the sum of the play counts of its tracks, and its last play is the latest play of one of them. Each of the four lists holds only the albums that have such a value, so it is empty for a user without a rating, a star or a play. They are the lists of the highest rated, the most played, the recently played and the starred albums, and an album without a rating or a play is none of these. The code of the Subsonic server, as Airsonic carries it, and Navidrome answer the same way: both select the albums with a play count above 0, with a last play and with a rating.
- Years: an album or a track without a year counts as year 0 in a year range, which is `byYear` and the `fromYear` and `toYear` of `getRandomSongs`. No item has the year 0 for another reason, because the tag reader stores no year for `0000` (§1.3). `byYear` from 0 lists the albums without a year first, and `byYear` down to 0 lists them last. Feishin asks for all albums by year with the range from 0 to the present year. Navidrome stores the year 0 for an album without one, so it answers that range with these albums. In `getRandomSongs`, a range whose first year is greater than its last year holds no song.
- Genres: `genre` matches the stored genres that equal it without regard to case, as whole texts. `LIKE` is not used, so `%` and `_` are plain characters. Two texts are equal when their lower-case forms are (`str::to_lowercase`). `byGenre` compares with the genre of the album (§3.10 step 6), and the two song methods compare with the genre tag of the track. `getGenres` lists spellings that differ by case as separate genres (§3.8). A list for `Rock` holds the items of `rock` as well, so it can hold more items than `getGenres` counts for one of the two.
- Random lists: `random` and `getRandomSongs` draw their rows anew for every request. No row is in an answer twice, and every row that the request allows is equally likely to be in it. No request sorts a table, and a request that many rows match reads only the rows that it looks up. `tracks` and `albums` are rowid tables, and the query depends on that. It reads the smallest and the largest rowid, draws distinct rowids of that range in random order with the `rand` crate, and looks each one up by its key. A rowid without a row and a row that the request does not allow add nothing. For `n` rows the query looks up `n + n/8 + 8` rowids first and twice as many each further time, up to `16 n + 64` in all. When that limit is at least the number of rowids, every rowid is looked up and the answer is exact. Otherwise lookups that find fewer than `n` rows mean that few rows count: the request allows few rows, or the table has lost most of its rows. The query then reads the IDs of the rows that count and keeps a random sample of `n` of them in one pass, without a sort. With a `genre` it reads the tracks of that genre through the index `tracks_genre`, and without one it reads the table. The stored genres that a `genre` is compared with are read through the genre index, one lookup for each distinct genre.
- `offset` selects no rows of `random`, because every request draws anew, and two pages can hold the same album. It lowers the number of albums instead: a request returns at most the number of albums of the list less the offset. A client that reads pages until one is short therefore comes to an end. A request with an offset counts the albums, which reads the album table once.
- Tracks without a file (§3.4): such a track is in no music folder. A list with `musicFolderId` leaves it out, and an album is in a music folder when one of its tracks has a file there. `getRandomSongs` never returns it, because a client plays the songs it gets and `stream` answers error 70 for it. `getSongsByGenre` without `musicFolderId` lists it without a `path`, as `getGenres` counts it. The album lists without `musicFolderId` list every album row, also one whose tracks all have no file.

Search methods (`api/search.rs`, `db/queries/search.rs`): `search3` returns `searchResult3` with the lists `artist` (`ArtistID3`), `album` (`AlbumID3`) and `song` (`Child`). `search2` finds the same rows and returns `searchResult2` with `artist` (the `Artist` of the folder methods), `album` (a `Child` that is a directory) and `song` (`Child`). The parameters and their defaults are those of the Subsonic API 1.16.1 and of the OpenSubsonic API (https://opensubsonic.netlify.app/docs/endpoints/search3/). A list without an item is `[]` in JSON and absent in XML.

- `query` is required, and an absent one is error 10. It may be empty. Every double quote is removed from it, and so is one `*` at its end: the search of the Subsonic API 1.x took `*` as a wildcard, and some clients still append it. The rest is split at white space into terms. A `*` at another place is a character of its term. A query without a term, such as `query=` and `query=""`, matches every artist, album and song. The OpenSubsonic API requires that, and Symfonium reads the whole library that way.
- A term is brought into Unicode form NFC and into lower case, as a natural key is (§3.3). An item is a match when every term is part of one of its names. The terms may be parts of different names, their order has no meaning, and a term may be a part of a word: `beat` matches "The Beatles". A term that the query holds twice counts once.
- The name of an artist is its name. The names of an album are its title and the name of its artist. The names of a song are its title, the title of its album, its track artist and the name of its album artist.
- `artists.key`, `albums.key` and `tracks.title_lc` hold a name in lower case, so the letter case of a name and of a term has no meaning. The title of the album of a song and the name of its album artist are compared through the keys of its album row and of its artist row. `tracks.artist_name`, the track artist, is stored as the tag spells it and in no other form. `LIKE` ignores the case of the letters A to Z only, so a letter outside ASCII in a track artist matches a term only when the tag has it in lower case. `tracks.title_lc` and `tracks.artist_name` are in the Unicode form of the tag. A term is in NFC, so a composed letter of a term does not match the same letter in decomposed form in a title or in a track artist. A letter with a diacritic is not its base letter: `edith` does not match "Édith".
- The terms are one parameter of a statement: a JSON array of `LIKE` patterns that the statement reads with `json_each`. A pattern is a term between two `%`, and a `%`, a `_` and a `\` of the term follow the escape character `\`, so each matches itself. No text of a request is part of a statement, and a query with any number of terms is the same statement.
- A query of more than 1024 characters is error 0, and so is a query of more than 32 different terms. Both are refused before a statement runs. The work of a search grows with the number of terms: the statement of the tracks tests the key of every album and of every artist against every term, and then every track against the terms until one does not match. A query that a person types has fewer than 32 terms. The limit of the characters bounds the length of a pattern. A term that holds the character U+0000 matches nothing, because SQLite reads a pattern and a name up to that character.
- `artistCount`, `albumCount` and `songCount` are 20 by default, and a value above 500 counts as 500. `artistOffset`, `albumOffset` and `songOffset` are 0 by default. Each is an integer from 0 to 4294967295, and another value is error 0. A negative limit is no limit in SQLite, and none reaches a statement. An answer holds 500 items of each kind at most.
- Each list is in the order of the lower-cased names: `artists.key`, `albums.key` and `tracks.title_lc`, in the byte order of the UTF-8 text. Albums with one title and tracks with one title are in the order of their rowids, which is the order in which the scanner stored them. A rowid changes only when the database is rebuilt with `VACUUM`, which the server never runs. The order is total and the same for every page, so a client that reads a list with offsets gets every item once while the library does not change.
- The statements read the unique index of `artists.key`, the index `albums_key` and the index `tracks_title_lc` in their order, stop when the page is full and sort nothing. The rows before the offset of a query without a term and without a music folder are read from the index alone. A pattern that starts with `%` cannot be looked up in an index, so a query with a term tests every row up to the end of its page. For the tracks, the statement first collects the IDs of the albums and of the artists that each pattern matches. The test of a track then reads no album row and no artist row.
- The three lists and the annotations of the user for their items are read in one read transaction. A list is shorter than its count only when it holds the last match, also while a scan deletes rows. A client that reads page after page takes a short page for the last one.
- `musicFolderId` limits the lists to one music folder: the tracks that have a file in it, and the albums and the artists that have such a track. The ID of no folder gives three empty lists. A value that is no integer is error 0.
- A track without a file (§3.4) is a match as every other track, and its element has no `path`. It is in no music folder, so a request with `musicFolderId` leaves it out.
- `search` returns error 0 with the message `deprecated; use search3`. Its handler reads no parameter.

Annotation methods (`api/annotate.rs`): `star`, `unstar`, `setRating`, `getStarred`, `getStarred2` and `scrobble`. A star and a rating are values of the row that the `annotations` table holds for the user of the request and the item (§3.4). A play is counted in the row of a track.

- `id` of `star`, `unstar` and `setRating` takes the ID of a track, an album or an artist; the prefix of the ID names the kind (§3.3). `albumId` takes the ID of an album and `artistId` the ID of an artist. The folder methods list an album and an artist under the same IDs, so an item has one star, whichever parameter set it. `star` and `unstar` take each parameter any number of times and require none: a request without an ID changes nothing and answers `ok`. `setRating` requires `id` and reads its first value.
- A value that is not the ID of an existing item of the kind its parameter takes is error 70: a text that `ids::parse` rejects, an empty value, the ID of a playlist, an ID that no row has, and the ID of a track in `albumId`.
- A request changes every item it names or none. `annotations::apply` checks that every item has a row and writes the items in one `IMMEDIATE` transaction. After error 70 and after a write that failed, no item of the request is changed. The check is in the transaction, so a scan in another process deletes no item between the check and the write.
- `star` sets `starred_at` to the time of the request in Unix seconds. The time is read once the write connection is held, and the items of one request get one time. `star` for an item that has a star replaces the time with the time of the new request. The API does not define that case. Navidrome and Airsonic replace the time, and the server does the same. `unstar` sets `starred_at` to NULL. `unstar` for an item that exists and has no star answers `ok`.
- `setRating` requires `rating`. 1 to 5 is stored, and 0 sets the rating to NULL. An absent or empty `rating` is error 10. A value that is no integer, and an integer outside 0 to 5, is error 0 and changes nothing. The rating is checked before the item.
- Each write sets the star, the rating or the plays of the row and leaves the other two. A star, a rating and the plays of an item therefore do not depend on the order of the requests that set them. No method deletes a row: `unstar` and `setRating` with 0 keep the other values of the row. A row without a star, a rating and a play reads as the annotation of an item without a row.
- `getStarred2` returns `starred2` with the lists `artist` (`ArtistID3`), `album` (`AlbumID3`) and `song` (`Child`). `getStarred` returns `starred` with `artist` (the `Artist` of the folder methods), `album` (directories) and `song`. The elements are those of the model constructors (§3.8), so `starred` and `userRating` have the form they have in every other method. `ArtistID3` has no `userRating`: the rating of an artist is part of `getStarred` and not of `getStarred2`. A list without an item is `[]` in JSON and absent in XML.
- Each list has the item that was starred last as its first (`annotations::starred`). Items with the same time are in the order of their IDs.
- `musicFolderId` limits the three lists to one music folder: a track whose file is in the folder, and an album or an artist that has such a track. A track without a file is in no folder (§3.4); without `musicFolderId` it is listed, and it takes a star and a rating as every other track does. An ID that no folder has gives three empty lists. A value that is no integer is error 0.
- A star whose item has no row is not listed. Such a row exists from the deletion of a track row until a scan completes and deletes the annotation (§3.10 step 6).
- A scan keeps the ID of a track through a rename and a move of its file, and the ID of an album and of an artist through a new spelling of the name (§1.3, §3.3). The scan does not write the `annotations` rows of these items, so their stars, their ratings, their plays and the times stay.
- `scrobble` counts plays of tracks for the user of the request and returns the empty response (https://opensubsonic.netlify.app/docs/endpoints/scrobble/, and the same parameters in Subsonic 1.16.1). `id` is required and takes the ID of a track. It may occur any number of times, and a track that a request names twice gets two plays. `time` is the time of a play in milliseconds since 1970. A request has no `time` or one for each `id`: the first `time` is the time of the first `id`, wherever the parameters are in the request. An empty `time` counts as absent (§3.8). A play without a `time` has the time of the request in Unix seconds. That time is read once the write connection is held, and the plays of one request get one time. `submission` is `true` or `false` in any letter case (§3.8), and `true` without a value.
- A play is stored in the row of its track: `annotations::record_play` adds 1 to `play_count` and sets `last_played_at` (§3.4). The time is stored in whole seconds, so the part of `time` below a second is dropped. The last play of a track is the latest of the times of its plays. A play whose time is earlier than the stored time is counted and leaves the stored time. A client can report a play long after it happened and the plays of one request in any order, and the plays of a track then do not depend on the order of the reports. Navidrome stores the later time as well. A `time` is stored as the request gives it, also when it is far in the past or later than the time of the request. The server does not compare it with its clock: a server without a real-time clock can have the wrong time while the client has the right one.
- Every request that passes the checks counts its plays. The API has no value that tells a repeated report from a second play. A request that is sent twice therefore counts twice, and so does a play that one request names twice with one time. Navidrome and Airsonic count every report as well.
- `submission=false` is the "now playing" notification of the API. The server checks the request as it checks a submission, answers `ok` and writes nothing: no play count, no last play and no list of the songs that play, because `getNowPlaying` is a stub (§1.2). Navidrome counts no play for the notification either. Airsonic counts a play for both forms.
- Errors of `scrobble`, in the order of the checks: an absent `id` is error 10. Error 0 answers a `submission` that is no boolean, a `time` that is no integer from 0 to 253402300799999, and a number of `time` values that is neither 0 nor the number of `id` values. The largest `time` is the last millisecond of the year 9999, the last time that `model::iso8601` writes with a year of four digits. Error 70 answers a value of `id` that is not the ID of an existing track: a text that `ids::parse` rejects, an empty value, the ID of an album, an artist or a playlist, and an ID that no track row has. Navidrome and Airsonic skip such an ID and answer `ok`. The server answers error 70, as `star` does, so a client learns that the plays were not counted.
- A `scrobble` request counts every play it names or none. `annotations::record_plays` checks that every track has a row and writes the plays in one `IMMEDIATE` transaction, as `annotations::apply` does for the other writes. After an error, and after a write that failed, no play of the request is counted. The number of plays of a request has no limit of its own; the form body limit bounds it (§3.8).
- The plays of an album are not stored (§3.4). `scrobble` writes no row of the album and none of the artist. `playCount` and `played` of an album element are the sum of the play counts of its tracks and the latest play of one of them (§3.8), and the `frequent` and `recent` lists order the albums by these two values (see the list methods). No element reports the plays of an artist.
- A track without a file (§3.4) takes a play as every other track does: a client can report the play of a copy that it holds after the file is gone. Its plays count for its album.
- Plays and the scan: a track keeps its play count and its last play through a rescan, a rename and a move of its file, and an album keeps the sums through a new spelling of its name, because the IDs stay (§1.3). A retag that gives a track another album title moves the plays of the track to the album row that the scan makes for it. When a scan deletes a track row, it deletes the annotation of the track (§3.10 step 6), and the plays of the track leave its album.

Playlist methods (`api/playlists.rs`): `getPlaylists`, `getPlaylist`, `createPlaylist`, `updatePlaylist` and `deletePlaylist`. The handlers read and write the `playlists` and `playlist_tracks` tables with `db::queries::playlists`. They read the songs with `browse::tracks_by_ids` and the annotations of the user with `annotations::get_many`, and they build each song with `model::child` (§3.8). The `playlist` element is a type of the handler module, because no other module returns it. Parameters, responses and versions follow the OpenSubsonic pages of the five methods and of the `playlist` and `playlistWithSongs` responses, and the Subsonic 1.16.1 schema.

- A playlist belongs to the user that created it (`playlists.owner_id`). Every query takes the ID of the user of the request and reads or changes the playlists of that user only. Every method answers the ID of a playlist of another user as the ID of no playlist, with error 70, whatever the `public` value of that playlist is. `public` is stored and returned and has no other effect (§1.3). `owner` is the name of the account that owns the playlist.
- The `playlist` element holds `id`, `name`, `comment`, `owner`, `public`, `songCount`, `duration`, `created`, `changed` and `coverArt`. The API requires `id`, `name`, `songCount`, `duration`, `created` and `changed`. `comment` is absent for a playlist without one. The element has no `allowedUser`, `readonly` and `validUntil`.
- `songCount` is the number of entries. `duration` is the sum of the durations of the songs in milliseconds, rounded once to seconds as the duration of an album is (§3.8). A song with two entries counts twice in both. `created` is the time of the request that created the playlist, and `changed` the time of the last request that changed it, both in the form of §3.8. A scan that deletes a track of the playlist does not set `changed`.
- `coverArt` is the cover art ID of the first song (`model::song_cover_art`, §3.8). It is absent for a playlist without a song and for one whose first song has no cover, also when a later song has one. A playlist has no image of its own.
- `getPlaylists` returns `playlists`, whose `playlist` list holds the playlists of the user without `entry`, in the order of their names without regard to ASCII case. Names that differ by case only are in the order of their bytes, and playlists with one name in the order of their IDs. `username` is optional, and an empty value counts as absent. It must be the name of the user of the request, compared without regard to ASCII case. Another name is error 50, whether an account has it or not, as in `getUser`. With `auth.disabled_unsafe = true` every name returns the playlists of the owner.
- `getPlaylist` returns `playlist` with the `entry` list: the `Child` element of the song of each entry, with the annotation of the user, in the order of the entries. A song with two entries is listed twice. The list is `[]` in JSON and absent in XML for a playlist without a song (§3.8).
- `createPlaylist` without `playlistId` requires `name` and creates a playlist of the user with one entry for each `songId`, in the order of the values. A song that is named twice gets two entries, and a request without `songId` creates a playlist without a song. The new playlist has no comment and `public: false`. An empty `playlistId` and an empty `name` count as absent, so a playlist always has a name; an absent `name` is error 10.
- `createPlaylist` with `playlistId` replaces the entries of that playlist with one entry for each `songId` and keeps the name, the comment, `public` and `created`. `name` is not read in this form, as in Airsonic and Navidrome; `updatePlaylist` renames a playlist.
- `createPlaylist` returns `playlist` with `entry` in both forms, as `getPlaylist` does. That is the answer of API version 1.14.0 and later. Earlier versions answer with the empty response; the server does not compare `v` (§1.3) and sends the playlist to every client. The handler reads the playlist while it holds the write connection, so the answer is the playlist as the request left it.
- `updatePlaylist` requires `playlistId` and returns the empty response. `name` sets the name, and an empty value counts as absent. `comment` sets the comment, and an empty value removes it. `public` is `true` or `false` in any letter case (§3.8). `songIndexToRemove` and `songIdToAdd` may each occur any number of times.
- The API defines no order for a request with several `songIndexToRemove` values and with `songIdToAdd` values. The server follows Airsonic (`SubsonicRESTController.updatePlaylist`), Navidrome (`core/playlists`) and gonic. Every `songIndexToRemove` is the position of a song in the playlist as it is before the request, counted from 0. The entries at these positions are removed, which gives the list that is left when they are removed one by one from the highest position to the lowest. The order of the values changes nothing, and a position that is named twice removes one entry, as in Airsonic and Navidrome. Then one entry for each `songIdToAdd` is appended, in the order of the values. A position therefore never names a song that the same request adds.
- Errors of the parameters: a required `id` or `playlistId` that is absent is error 10. A value that is the ID of no playlist of the user is error 70. A `songId` or a `songIdToAdd` that is the ID of no track row is error 70. That includes a value without the shape of an ID (§3.3), an empty value and the ID of an item of another kind: the handler checks each value with `ids::parse` before a statement runs. A `songIndexToRemove` that is not the position of an entry is error 0 with a message that names the parameter: a value that is no integer, a negative one, and a position at or after the end of the playlist. The reference servers do not agree on that case: Navidrome deletes the rows that have the named positions, so such a position removes nothing, and Airsonic fails after it stored the name. An empty `songIndexToRemove` counts as absent, as an empty value of every parameter that holds a number does (§3.8).
- A playlist holds at most 100 000 entries (`playlists::MAX_ENTRIES`). A list of that many track IDs is a form body of about 4.4 MB, which the form body limit of 8 MiB admits (§3.8). A request with more `songId` or `songIdToAdd` values, and an `updatePlaylist` request after which the playlist would be longer, is error 0 with the message `a playlist holds at most 100000 songs`. The values are counted before they are read. The limit bounds the rows that one request writes and the rows that one request reads.
- A request changes everything it names or nothing. `playlists::create`, `playlists::replace_tracks` and `playlists::update` each run in one `IMMEDIATE` transaction and check the playlist, the positions, the length and the tracks, in that order, before the first write. After an error, and after a write that failed, the name, the comment, `public`, `changed` and every entry are those from before the request. `deletePlaylist` is one statement, and the entries go with the playlist row (`ON DELETE CASCADE`). The time of a write is read once the write connection is held. An `updatePlaylist` request that names nothing to change writes nothing and keeps `changed`.
- An entry is a row of `playlist_tracks`, and the entries of a playlist are its rows in the order of `position`. The position of an entry in the API is its place in that order. A write of the entries stores them at the positions 0 to n − 1. A request that only adds songs to rows without a gap inserts the new rows and leaves the others; every other change of the entries writes the kept rows again. Every value of a request is a parameter of a statement, and a list of track IDs is one JSON array (§3.8).
- Entries and the scan: a scan keeps the ID of a track through a rename and a move of its file (§1.3) and writes no row of `playlist_tracks`, so a playlist keeps its entries and their order. A track without a file keeps its entries and is listed without `path` (§3.4); it is added and removed as every other track. When a scan deletes the track row (§3.10 step 5), the entries of the track are deleted with it (§3.4). The other entries keep their order, and `songCount`, `duration` and the positions of the API follow the entries that are left. The `position` column has a gap from that scan until the next write of the entries of the playlist.
- Each handler that reads runs its statements in one read transaction, so `songCount` and `duration` agree with the `entry` list. A database that cannot be read is error 0 with the message `the server cannot read its playlists`, and one that cannot be written is error 0 with the message `the server cannot store the playlist`.

<!-- api/queue.rs -->

<!-- api/lyrics.rs -->

Media methods (`api/media.rs`): `stream` returns the original file of the track `id`, unchanged, with the content type of the track row (§3.9). It answers GET, HEAD and POST. A success is the bytes of the file and no envelope; an error is the envelope (§3.8).

- `id` is required, and an absent one is error 10. Error 70 answers a value that is no track ID (§3.3), an ID that no track has, a track without a file (§3.4), a file that is gone, a file outside the configured music folders and a file that changed since the scan.
- The server has no transcoder. A request for which `decide` returns `Transcode` gets the original file with its own content type, as a request for which it returns `Raw` does, until WAX-869 replaces that rule (§3.9). `timeOffset` and `estimateContentLength` have no effect on the response. A `format` that names no target is error 0.
- `stream` counts no play. `scrobble` does (§1.3).

Cover method (`api/covers.rs`): `getCoverArt` answers with the bytes of an image and not with the envelope (§3.8). The handler checks the form of `id` and `size` and calls `Covers::get` (§3.11). The service resolves the ID through the database, and it owns the limits on the size, on the bytes of a source and on the memory of a decoded image, and the rule for symbolic links. The handler builds no path and opens no file.

- `id` is required, and an absent one is error 10. It is the ID of an album or of a track, which is what the `coverArt` of an album and of a song holds (§3.8). The `coverArt` of a playlist is the one of its first song, so it is such an ID as well. `size` is an integer from 0 to 4294967295. An empty value counts as absent, and any other value is error 0 that names the parameter (§3.8): a negative number, a number with a fraction, a larger number and text. The parameters are checked in this order: `id` is present, `size` is an integer, `id` has the form of an ID.
- Error 70 answers every request for which the server has no cover: a value that `ids::parse` rejects, an ID that no row has, the ID of an artist or of a playlist, an album without a cover, a track without an embedded picture or without a file (§3.4), and a source that the service does not serve (§3.11). The server has no placeholder image and no artist image (§1.3). The OpenSubsonic API names no answer for an item without a cover. Navidrome sends a placeholder image for such an item.
- A cover is HTTP 200 with the bytes that the service returned, the `Content-Type` that the service reported, `Content-Length`, `Cache-Control: public, max-age=86400` and `ETag: "<id>-<size>-<cover_mtime>"`. `<size>` is the size that the cover is served at: 0 for the image as it is stored, and 2000 for every size over 2000. The three parts are the ones in the name of the cache file (§3.11), so two requests that get the same bytes have the same entity tag. The response has no `Last-Modified`.
- A request whose `If-None-Match` names the entity tag of the cover gets HTTP 304 with `Cache-Control` and `ETag`, without a body and without `Content-Type`. The header holds a list of entity tags or `*`, and a weak tag (`W/`) names the cover as well (RFC 9110 section 13.1.2). The handler reads the cover before it compares the header, because `Covers::get` returns the modification time with the cover. A conditional request therefore reads the cache file, or the source and its header, and a cover that is gone is error 70 for it as well. `Range`, `If-Range`, `If-Match` and `If-Modified-Since` are not read: the answer is the whole cover.
- The request is a GET, a HEAD or a POST with a form body, and the method changes nothing in the answer. The router removes the body of the answer to a HEAD request (§3.8).
- An error is the envelope in the format of the request, with HTTP 200 and without the cache headers. A database that cannot be read is error 0 with the message `the server cannot read its library`. Work on a cover that ended without a result (`CoverError::Unfinished`) is error 0 with the message `the server did not finish the cover`.
- The handler awaits `Covers::get` in the future of the request and starts no task. A client that disconnects drops that future. A request for a stored image then leaves the queue of the service, and a resized cover that waits for its turn is not made (§3.11).
- Present limit: the entity tag holds the modification time that the last scan stored, and a cover without `size` is read from the file as it is now. An image file that is replaced after the last scan is served under the old entity tag until the next scan, and a client that holds the old image gets HTTP 304 for that time. A cover that is replaced by an image with the same modification time keeps its entity tag after the scan as well.

User methods (`api/users.rs`): the server has one account, so `getUser` and `getUsers` return the user of the request. `getUser` requires `username`, and an absent one is error 10. The value is compared with the name of the user of the request without regard to ASCII case, as the lookup of an account is (§3.7). Another name is error 50 with the message `username must be the name of the user of the request`, whether an account has that name or not. With `auth.disabled_unsafe = true` every name returns the owner. `getUsers` returns `users`, whose `user` list holds that one user. The `user` element holds `username` (the stored name); `scrobblingEnabled`, `adminRole`, `playlistRole` and `streamRole` as `true`; `settingsRole`, `downloadRole`, `uploadRole`, `coverArtRole`, `commentRole`, `podcastRole`, `jukeboxRole`, `shareRole` and `videoConversionRole` as `false`; and `folder`, the IDs of the rows of the `folders` table in ID order. It has no `email`.

Scan methods (`api/scan.rs`): `startScan` and `getScanStatus` return the `scanStatus` element with `scanning` and `count` (https://opensubsonic.netlify.app/docs/responses/scanstatus/). Both describe the scans of the server process (§3.10).

- `count` is the number of supported files that the running scan has finished with. When no scan runs, it is the number of the scan of the server process that ended last, and 0 before the first one.
- `startScan` takes `fullScan`, which is `true` or `false` in any letter case; another value is error 0 and starts no scan. The method follows the trigger table of §3.10 and returns without waiting for the scan. Its answer has `scanning: true` when it started a scan and when it found a scan of the server running.
- `getScanStatus` adds `lastScan`: the end time of the scan that ended last, as ISO-8601 in UTC with seconds (`2026-10-11T08:15:30Z`). `lastScan` is not part of the OpenSubsonic response. Navidrome sends it, and it is here for the clients that read it. It is the latest `finished_at` of the `scans` table, formatted by SQLite, so it counts a scan that ended with errors, a cancelled scan, a scan of `wn-server scan` and a scan from before the server started. It is absent until a scan has ended.
- A scan that recorded errors is no API error. `getScanStatus` answers `ok` with `scanning: false`, the count of that scan and its end time as `lastScan`.

`download` and the stubs (`api/stubs.rs`): a handler of that module takes no argument. It reads no parameter, no state and no file, so its answer is the same for every request that has a user. A request without a user gets the authentication error (§3.7).

- `download` returns error 50 with the message `download is disabled on this server`. The response is the error envelope and never holds a byte of a file.
- An empty result is an object without fields under the key of the method: `getArtistInfo` → `artistInfo`, `getArtistInfo2` → `artistInfo2`, `getAlbumInfo` and `getAlbumInfo2` → `albumInfo`, `getSimilarSongs` → `similarSongs`, `getSimilarSongs2` → `similarSongs2`, `getTopSongs` → `topSongs`.
- An empty list is an object whose one field is an empty list, which is `[]` in JSON and absent in XML: `getNowPlaying` → `nowPlaying` with `entry`, `getShares` → `shares` with `share`, `getPodcasts` → `podcasts` with `channel`, `getNewestPodcasts` → `newestPodcasts` with `episode`, `getInternetRadioStations` → `internetRadioStations` with `internetRadioStation`, `getBookmarks` → `bookmarks` with `bookmark`. The OpenSubsonic API marks `entry` as required and the other lists as optional.
- `getAvatar` returns error 70.
- `createUser`, `updateUser`, `deleteUser` and `changePassword` return error 50 with the message `user management is not available on this server`. The `wn-server user` commands manage the account.

## 5. Tests

- `cargo xtask check` runs `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings` and `cargo test --locked --workspace`. The default suite finishes in under 30 s after compiling, in a debug build, on every OS. No network, no audio hardware, no ffmpeg, real SQLite (memory or temp file), no mocks of the database. No test sleeps or polls for more than 1 s; timing values are fields of config structs that tests set to tens of milliseconds.
- Fixtures: `tests/fixtures/audio/` holds `a.flac`, `a.mp3`, `a.m4a`, `a.ogg`, `a.opus`, generated by `scripts/make-fixtures.sh` from a 0.5 s mono 440 Hz sine. The sample rate is 22.05 kHz, except for `a.opus`, which is 24 kHz because libopus does not accept 22.05 kHz. The files total about 24 KB and each is under 16 KB; a test enforces a 64 KB budget. They carry no tag fields.
- `tests/common/mod.rs`: `LibraryBuilder` copies fixtures into a temp directory as `<Artist>/<Album>/<nn> <Title>.<ext>` and writes tags with lofty; `LibraryBuilder::standard()` builds 3 artists, 4 albums, 10 tracks (one compilation, one album with `cover.jpg`, one with embedded art, one track with a lyrics tag, one album with tracks on two discs). Names are valid on Windows and differ by more than case. `TestServer` builds the router and offers `get_json(method, params)`, `get_xml`, `get_raw`, `post_form` and `get_with_headers(method, params, headers)`; `body_string` and `body_bytes` read the body of a response. Its database is in memory and holds the owner `owner` with the password `sesame`; `db()` returns it, so a test writes the rows that its requests read. The five helpers add the auth defaults to the parameters of the caller: `u`, a token `t` with a new salt `s` for each request, `v` and `c`; a default whose name the caller passes is left out. `get_json_noauth` and `get_raw_noauth` send the parameters of the caller and no other.
- Every `TestServer` has a database of its own. `TestServer::bare(config)` has the owner account and no library; it reads no music folder until a scan starts. Three constructors return a server whose library is scanned. `TestServer::standard().await` scans the standard library and returns a `StandardServer`, which exposes the ID of each artist, album and track of the library as a field (`server.artist.aurora_field`, `server.album.glass_harbour`, `server.track.low_tide`) and the ID of the music folder as `folder_id`. `TestServer::standard_with_own_files().await` returns the same over files that no other test reads; a test that renames, changes or deletes a file uses it and calls `rescan()`, which runs the scanner without the scan controller and returns the summary of the scan. `TestServer::scanned(library).await` scans a library that the test built with `LibraryBuilder` and returns a `ScannedServer`; `catalogue()` reads its rows, and a test finds a row by its name. The config of a scanned server names the directory of the library as its one music folder, and `root()` and `path(rel_path)` return paths in it. The servers of `standard()` that exist at the same time in one test binary share the files of one library: it is written when no server holds it and deleted when the last one is dropped. The tests of a binary run on several threads and overlap, so most runs write the library once; a run with one test thread writes it for every test. No directory stays behind when the process ends. A test does not change these files. A test checks that a standard server is ready in under 1 s.
- Unit tests sit next to the code: IDs and fingerprints, tag mapping, `decide`, envelope and XML rendering, the response models, the library queries and the annotation lookup, config, auth verification, LRC parsing, cover resize, argv builders. The tests of the response models compare the whole JSON document and the whole XML document of each element with the expected text, for a row that has every value and for a row that has few.
- `tests/model.rs` builds the response models from the rows of a scanned standard library: the properties of a file of each of the five formats, the cover art IDs that follow the covers the scan found, the order of the library queries, the annotation lookup, and a track that is given a placeholder path. It also tests the constructors of `TestServer` over a scanned library.
- Router tests in `tests/` call the router in-process with `tower::ServiceExt::oneshot`. Every endpoint has a JSON test; the envelope has XML tests.

<!-- The comment lines below hold the places of the test files that have no text in this section yet. A change replaces the line of its test file with its text and keeps the blank lines (section 3.8). -->

- `tests/browse.rs` covers the six browse methods over the router. For each method it compares the whole response with the document that the standard library gives, in JSON and in XML: the music folder, the three artists in their indexes, each artist with its albums, each album with its songs, each of the ten songs, and the genres. Tables in the test file hold what `LibraryBuilder::standard` writes and what the five fixture files are; the IDs, the creation times and the file sizes are read from the rows and the files. Other tests build a library for one rule: the index of a name with an article, a digit or a diacritic; the albums of an artist by year and title; genres in the order of their names, with a name that XML escapes and a genre of no album. A server with two music folders, scanned with `scan_and_wait`, covers `musicFolderId`: the artists of each folder, the stored `albumCount`, an ID that no folder has, an empty value and a value that is no integer. The remaining tests cover the folder rows of a server that has not scanned, a server without a library, an `id` that is absent (error 10), unknown, malformed or of another kind (error 70) in both formats, a form body, the four annotation values on an artist, an album and a song, a track that is given a placeholder path, and two calls that return the same document. The unit tests in `src/api/browse.rs` cover the check of `id` and the read transaction: a write that commits between two statements of one read, on a temp-file database, is not seen by the second statement.

<!-- tests/folders.rs -->

- `tests/lists.rs` covers `getAlbumList`, `getAlbumList2`, `getRandomSongs` and `getSongsByGenre` over the router, in JSON and XML. On the standard library it checks each of the ten list types: the albums and the order of each type, with the annotations and the creation times that the test writes; each album and each song against the response model of its row; the required parameters and the error of each wrong value; the pages that `offset` selects in every type; `musicFolderId`; and a track without a file. A test of a random list checks what holds for every draw: the number of rows, that no row is there twice, and that every row is one that the request allows. Libraries that a test writes into the database without files cover the limit of 500 with 510 albums, two music folders, names in which the two alphabetical orders differ, and an album without a year. The unit tests in `src/db/queries/lists.rs` cover the random pick on tables of 1500 rows: with a request that many rows match, with one that few rows match, and after most rows are deleted. They draw with a generator of a fixed seed, and no assertion depends on the values that the seed gives. One test reads the query plan of each statement of the pick and checks that none scans `tracks` or `albums`. The unit tests in `src/api/lists.rs` cover the size and the page of a request, and the read transaction of a handler with a write that commits between two of its statements.

- `tests/search.rs` covers `search2`, `search3` and `search` over the router, in JSON and XML. It checks the empty query and `""`; the counts, the offsets and their defaults; a count above 500; counts, offsets and folder IDs that are no numbers; terms that are parts of names, several terms, letter case and names outside ASCII; queries of `%`, `_`, `\`, quotes, SQL text, a NUL character and control characters; queries of 1024 characters and of more; queries of 32 terms and of more; `musicFolderId` with two folders; tracks without a file; the annotations of the user; and the error of `search`. One test reads the standard library in pages of several sizes until a page is short, and checks that every ID was returned once and in the order of one request. The unit tests in `src/db/queries/search.rs` cover the patterns of a query, the same matching rules on names that are no file names on Windows, the order of rows with one name, and the query plans: each statement reads the index of its name column and sorts nothing. One `#[ignore]` test there needs no fixture: it fills a memory database with 100 000 tracks, reads all of them page by page, and prints the time of the searches. The unit tests in `src/api/search.rs` cover the parameters and a search in a file database.

- `tests/annotations.rs` covers `star`, `unstar`, `setRating`, `getStarred` and `getStarred2` over the router, in JSON and XML. It reads the rows of the `annotations` table after the requests: the IDs that each parameter takes, a repeated `star`, `unstar` for an item without a star, every rating from 0 to 5 and the values that are refused, a request with several IDs, a form body, and a request that names an unknown ID as its first, a middle or its last value, after which the rows are those from before the request. It reads a star and a rating back through `getSong` and `getAlbum`, compares the lists with the elements of the model constructors and checks their order, `musicFolderId`, a track without a file, a star whose item is gone, and the rows of a second user. Two tests change the files of a library of their own and rescan: after a renamed file, a moved file and a new spelling of an album and of an artist, the rows are the rows they were and the lists show the new path and the new names. The unit tests in `src/db/queries/annotations.rs` cover the write queries with rows that the tests write, among them a request whose write fails after it wrote an earlier item.

- `tests/scrobble.rs` covers `scrobble` over the router, in JSON and XML, and the plays that the other methods report. It reads the rows of the `annotations` table after the requests: one play in the row of the track and no row of the album or the artist, `time` in milliseconds and the first and the last time that a request can name, a play that is reported late, several `id` values with their times, a request that is sent twice, a form body, and the rows of a second user. For each kind of refused request (an unknown or malformed ID, the ID of an album, an artist or a playlist as the first, a middle or the last value, a `time` or a `submission` of the wrong form, a number of `time` values that is not the number of `id` values, a request without a user) it checks the error and that the rows are those from before the request. `submission=false` is checked the same way and leaves the rows and `getNowPlaying` as they were. On the standard library the tests read the plays back through `getSong`, `getAlbum`, `getArtist`, `search2` and `search3`, and they check the albums and the order of `type=frequent` and `type=recent` of `getAlbumList2` and `getAlbumList`, with the play count and the last play of each album. Four tests change the files of a library of their own or the path of a row: the plays stay through a rescan, a renamed file, a moved file and a new spelling of the names; a track without a file keeps its plays and takes a play; and the plays of an album follow a track that is retagged into another album and leave with a track whose row the scan deletes. The unit tests in `src/db/queries/annotations.rs` cover `record_play`, `record_plays` and the plays that `get_many` computes for an album, among them a request whose write fails after it counted an earlier play, a row of an album that holds a play count, and the query plan of the statement of the album plays. The unit tests in `src/api/annotate.rs` cover the checks of the parameters of `scrobble` and their order.

- `tests/playlists.rs` covers the five playlist methods over the router, in JSON and XML. It compares the whole `playlist` document of a playlist of three songs, of a playlist without a song and of the list of `getPlaylists`, with the songs taken from `getSong`, and it reads the rows of `playlist_tracks` after every write and checks that their positions are 0 to n − 1. The tests cover a song that a request names several times, the order of `getPlaylists`, `username`, the cover of the first song, removals and additions in one request in every order of their values, a position that is named twice, the three values `name`, `comment` and `public` and their empty forms, `created` and `changed`, the replace form of `createPlaylist`, `deletePlaylist`, a form body, and the annotation values on an entry. For each kind of refused request (a position outside the playlist, an unknown or malformed song ID as the first, a middle or the last value, a `public` value that is no boolean, an unknown playlist, a request with 100 001 songs) they check the error and that the documents and the rows are those from before the request. A second account, whose row the test inserts, gets error 70 from every method for the playlists of the owner. Three tests change the files of a library of their own and rescan: a playlist keeps its entries through a rescan and a moved file, and loses the entries of a track whose row the scan deletes, after which the positions of the API are the places in the list that is left. A track that is given a placeholder path stays in its playlist. The unit tests in `src/db/queries/playlists.rs` cover the queries with rows that the tests write: the removal rule, the rows of a second user, the order of the refusals, a write that fails after its first statement, the rows after the deletion of a track row, the append that leaves the other rows, and the limit of 100 000 entries. The unit tests in `src/api/playlists.rs` cover the checks of the parameters and the read transaction.

<!-- tests/queue.rs -->

<!-- tests/lyrics.rs -->

- `tests/stream.rs` covers `stream` over the router. It compares the body with the file for each of the five fixture formats, by GET, by POST and with the `.view` suffix, and checks the headers of a HEAD request. It sends single, open-ended and suffix ranges, a range that no byte satisfies and several ranges; `If-Range`, `If-None-Match` and `If-Modified-Since` with the validators of the file and with others; the parameters that select the original file; requests for which `decide` selects a transcode (`format=opus`, `format=mp3` and a `maxBitRate` below the bitrate of the file), which get the original file with its content type and its ranges; and values of `id` that name no track. It checks the CORS headers of a media response. The files that are not served: the file of a track without a file, a file that is deleted, replaced or exchanged with another one after the scan (and served again after the next scan), a file that a row places outside the music folder, and a file with a modification time before 1970. `Content-Disposition` is checked for a file name outside ASCII and, on Unix, for a name with a quote and a backslash. A WAV file of 1 MiB that the test writes is read in chunks, and on Unix a response that the test drops after its first chunk leaves no open file, counted through `/dev/fd`, and no task. The tests of symbolic links are `#[cfg(unix)]`: a link out of the music folder that exists before the scan, one that takes the path of a file after the scan, links inside the folder and a music folder behind a link. The unit tests in `src/media/stream.rs` cover the `decide` table, the two checks of the file with and without links, `If-Range`, the request that `ServeFile` gets, a 404 of `ServeFile` and the header values.

- `tests/coverart.rs` covers `getCoverArt` over the router. A library with a cover file of 128 × 64 pixels gives the bytes of the file without `size` and a JPEG of 64 × 32 pixels with `size=64`, in both path forms, with the content type, the length and the cache headers. The standard library gives the cover file, the embedded PNG picture of an album and of a track, and the round trip: every `coverArt` that `getAlbum`, `getSong` and `getArtist` return is accepted, an album and a song without `coverArt` are error 70, and a song whose album lost its stored cover names its own track ID. A second round trip collects every `coverArt` of the answers of `getAlbumList`, `getAlbumList2`, `getRandomSongs`, `getSongsByGenre`, `search2`, `search3`, `getStarred`, `getStarred2`, `getPlaylists` and `getPlaylist`, after the user starred items and made a playlist, and checks that each one is accepted and that the ID of the playlist itself is error 70. The conditional request is tested with the entity tag as it was sent, in the weak form, in a list and as `*`, with tags of another size and another time, after a scan that stored a new cover, by HEAD and by POST, and for an ID without a cover, which stays error 70. A cover file of 2048 × 2 pixels covers `size`: absent, empty, 0, sizes up to 2000, sizes over 2000 with the entity tag of the size 2000, and the values that are error 0. The remaining tests cover the IDs without a cover in JSON and XML (unknown IDs of the four kinds, artists, albums and tracks without a picture, values that are no ID), a track without a file, an absent `id`, a HEAD request, a form body, request headers that are not read, a request without a user, eight requests at the same time, and requests that are dropped at each point at which they wait. The unit tests in `src/api/covers.rs` cover the text of the entity tag, the parsing of `If-None-Match`, the order of the parameter checks, a database that cannot be read, and two cases over a cover service with a cache directory of the test: a value that is no ID creates no file and no cache directory, and the cover of a request that is dropped while it waits is not made. The test server gives no access to its cache directory, so these two cases are not router tests. The last test relies on the one thread of the test runtime: the task that makes a cover first runs after the request is dropped.

- `tests/users.rs` covers `getUser` and `getUsers` in JSON and XML, with authentication on and off. `tests/stubs.rs` holds a table of every stub with its key or its error, and a test compares the table with the rows of §4. It sends `download` in both path forms by GET and by POST, with a user, without one and with authentication off, for the ID of a track whose file is on disk, and checks that each body equals the error document byte for byte.
- `tests/auth.rs` covers every rule of §3.7. For every method of the table that is not public it checks that a request without a user gets the authentication error and does not reach the handler. A unit test in `api/mod.rs` adds a method whose handler takes no `Authed` to a table and checks the same. The log of the failed attempts is captured and holds no password, token or salt.
- `tests/cli.rs` runs the binary with `std::process::Command` for `init`, `scan`, `user show`, `user set-password`, `config path` and `config show`. Every run that writes names a temp data directory. The password tests read the stored password from the database and check it with `auth::verify_password`. The `scan` tests run the command on the standard library, with a missing folder, after the files of the folder are gone, while the test holds the scan lock as a scan of the server does, and while the test holds the database open as a server does.
- Scan triggers: a test that needs a running scan sets the batch size of the scanner to 1 and a batch hook that waits, so the scan stops after its first file until the test lets it go on. Such a test does not depend on the time a scan takes. The unit tests in `src/library/scan_controller.rs` cover the controller, the scan lock and the periodic task. The unit tests in `src/server.rs` cover what `run` does before it serves: the folder rows, the scan at startup and the periodic scans. The tests of the periodic scans are the ones that wait for a time: the interval is 20 ms, and they poll for the second `scans` row for one second at most. `tests/scan.rs` covers `startScan` and `getScanStatus` over the router. `TestServer::scan_and_wait` starts a scan and polls the status of the controller every `library.scan_poll_ms`, which the tests set to 10, for one second at most.
- `tests/scanner.rs` scans libraries that `LibraryBuilder` wrote into a temp-file database and checks the rows, the IDs and the scan counts after files are added, changed, moved, exchanged, copied and deleted, after a folder path changes, and after a folder, a path or a file becomes unreadable or a folder becomes empty. Its second part checks the album rows, the artist rows, the annotations and the covers of step 6: the IDs that a rescan, a new track, a move and a respelling keep, the names that a respelling changes and those it leaves, the rows that a retag and a deletion remove, compilations, discs, missing tags, names outside ASCII, cover files that are replaced, added and removed, the files of the cover cache, and cancelled scans. A test that changes a file in place sets its modification time, because the scanner compares whole seconds. The file larger than 128 KiB is a WAV file that the test writes. The tests that take the read permission from a file check nothing when the process runs as root; the unit tests in `src/library/scanner.rs` cover the same cases on every OS and as every user with a reader whose reads the test makes fail.
- `tests/covers.rs` scans the standard library into the database of a temp data directory and reads covers through `library::covers`: the cover file and the embedded picture with and without a size, the cache file of a resized cover and a second request that does not write it again, a track ID, an album and a track without art, a track without a file, the cached cover that a scan removes after the cover file changed, a cover file and an embedded picture that are no image, and a cover file that is a symbolic link, before a scan and after it. The cover file and the picture of the standard library are 2 × 2 pixels, so a test of scaling writes a larger image first. The unit tests in `src/library/covers.rs` write image files and rows themselves. They cover the sizes of resized images and images that are not enlarged, the pixel formats, the header verification, the forms of WebP, the limits on the size, on the bytes of a source and on the sides and the memory of a decoded image, IDs that are not IDs, stored sources that are not image paths, symbolic links and a hard link in the place of an image and of a cache file, and the cache write. An image over a limit is a PNG of 4097 × 1 pixels, a JPEG whose header is changed to declare 65535 × 65535 pixels, and a PNG without pixel data whose header declares 5 KiB more than 64 MiB; the file over 16 MiB is a file whose length is set without writing its bytes. The lossy WebP is a constant of 72 bytes, because `image` writes lossless WebP only. A counter of decoded images shows that a cached cover is not decoded again. The tests of requests at the same time take the decode permit, start the requests, wait until every request waits for the one job, and release the permit or end the requests. The tests with symbolic links are `#[cfg(unix)]`, because creating a link on Windows needs a privilege.
- `tests/tags.rs` reads tracks that `LibraryBuilder` tagged, in the five fixture formats, through `library::tags`. The unit tests in `src/library/tags.rs` cover the mapping rules, unreadable files, and WAV and AIFF files that the tests build in memory.
- Transcoding: the default suite uses `FakeTranscoder`, which returns known bytes and records the job. One `#[ignore]` test runs real ffmpeg.
- `tests/shutdown.rs` runs `run_server` on `127.0.0.1` with port 0 and speaks HTTP/1.1 over a `TcpStream`. Its tests are the only ones of the suite that open a socket, and they run one at a time, because a test checks that the port of its server refuses a connection. `ServerOptions` gives a test the address of the listener, routes next to the API (`api::build_router_with`), the hooks of the scans and the step that withdraws the advertisement, so no test starts the mDNS daemon. The tests cover the steps of the stop (§3.13):
  - A request to a route that answers after 200 ms is answered after the token is cancelled, and the server then ends. The grace of the server is one hour, so the server did not wait for it.
  - The port refuses a connection and an idle connection is closed while a request that the test holds is in progress.
  - With a grace of 100 ms, a handler that never returns and a body that never ends are dropped with one warning.
  - The step that withdraws starts when the token is cancelled, while a held request is in progress. The server does not checkpoint and does not end while the step runs.
  - A scan with a batch size of 2 whose hook cancels the token of the server ends as `cancelled`, and the log is empty when the server has ended.
  - A scan that its hook holds is left behind after a limit of 100 ms, with one warning: the server ends, the log is empty, `PRAGMA integrity_check` answers `ok`, and the `scans` row has no end time.
  - Two tests read the captured log: the startup line, and the one line of a streamed response whose client closes the connection.
- The tests of the stop assert the order of events and require no operation to finish within a time. A wait for an event has a limit of 10 s, which ends a test whose server hangs; a passing run waits milliseconds. The fixed waits are the 200 ms of the slow route, the two limits of 100 ms, which are asserted as lower bounds, and waits of 50 ms in which a server that must wait does not end.
- The unit tests in `src/server.rs` cover steps 1, 3 and 4 without a socket, with the batch hook of the scanner: the token of the server that the hook cancels after the first batch; the step that withdraws, which starts with the stop while the scan is held, and whose end the checkpoint waits for; a scan that does not end within its limit, after which the database passes `PRAGMA integrity_check` and `PRAGMA foreign_key_check`, holds the rows of the committed batch only, and is completed by the next scan, which keeps those rows; and a checkpoint that another connection blocks. The unit tests in `src/cli.rs` cover the first and the second stop signal through a channel in place of the signals of the system, and the step that runs once. `tests/system.rs` covers the request log: the one line, its level for `stream` and `getCoverArt`, and a request with each form of a credential (`p`, `p` with `enc:`, `t` with `s`, `apiKey`) in the query string and in a form body, whose line holds the fields of §3.8 and no credential.
- Unix-only tests (file modes) are `#[cfg(unix)]`.
- Heavy tests are `#[ignore]`. A heavy test that reads fixture files reads `WN_SERVER_FIXTURES` and fails naming the variable when it is unset.
- mDNS registration and real clients are checked by hand and recorded in the PR.
- `cargo run --example e2e -- <url> <user> <password>` runs a scripted client session against a running server in Rust, on every OS.
