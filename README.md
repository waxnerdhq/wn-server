# wn-server

wn-server is the waxnerd music server. It runs [Navidrome](https://www.navidrome.org/) as a child process and adds three waxnerd features on top of it:

- `wn-server login`: the owner signs in at wn-cloud and pastes a sign-in code into the terminal. wn-server registers itself under the owner's waxnerd account. The owner's apps can then sign in with that account.
- MusicBrainz matching: beets matches albums to MusicBrainz releases. Confident matches apply silently. Others wait for review in the web UI or the CLI.
- Rating sync: ratings and favorites are stored in wn-cloud on waxnerd's 10-point scale, not in Navidrome.

wn-server is licensed under GPL-3.0 (see `LICENSE`). It is powered by Navidrome, which is also GPL-3.0 and runs as a separate process.

**Status:** scaffold, configuration, and the Navidrome supervisor. `wn-server init` writes `config.toml`. `wn-server serve` checks the Navidrome version, writes `navidrome.toml`, runs Navidrome as a child process, and adds the configured libraries. There is no proxy yet. `login` and `match` return "not implemented". The wn-cloud API contract and an in-memory wn-cloud stub for tests are in place. v1 has one account, the owner. This file describes the agreed design and is updated as the code lands.

## Architecture

### Process model

One `wn-server` binary. It supervises a pinned `navidrome` binary, writes its configuration, and sits in front of it as a reverse proxy. Navidrome listens on `127.0.0.1` only. Every request from the network goes through wn-server.

beets runs as a subprocess and only writes into the sorted directory. A pinned beets, with its Python interpreter, ships in the release bundle together with the pinned Navidrome binary, so a release works without any install step. ffmpeg is optional and only Navidrome uses it, for transcoding.

The first and only release target for now is Linux on x86_64. Other platforms come later.

```
clients (wn-mobile, wn-desktop, web UI)
        │  HTTPS + wn-server session or app password
        ▼
   wn-server ── proxy ──▶ navidrome (127.0.0.1, child process)
        │  ▲                   │
        │  └── beets ──▶ tags in music files ──▶ navidrome scanner
        ▼
   wn-cloud (Supabase auth, waxnerd API)
```

### Components

| Package | Responsibility |
|---|---|
| `internal/cli` | cobra command line: `init`, `serve`, `login`, `match`, `version` |
| `internal/config` | `config.toml` loading and defaults, data directory, rendered `navidrome.toml` |
| `internal/navidrome` | child process supervisor, version pin, library sync, and wn-server's own Navidrome identity |
| `internal/proxy` | session and app password checks, `Remote-User` forwarding, rating intercepts |
| `internal/wncloud` | wn-cloud client: login flow, server registration, `GET /me` lookup, rating routes; `stub` serves the same routes from memory for tests |
| `internal/beets` | beets subprocess, match queue |
| `internal/ratings` | local store of ratings and favorites, rating API, reconcile loop |
| `internal/store` | wn-server's SQLite database: the owner account and its sessions, match queue, ratings, outbox |
| `internal/web` | web UI for the owner: app password sign-in, status, match review, account page |

### Authentication

The full flow is in [docs/auth-flow.svg](docs/auth-flow.svg).

wn-cloud is the v2 rewrite of waxnerd (`../v2`): an Express server with GoTrue for sign-in. GoTrue signs access tokens with a shared HS256 secret, so wn-server cannot verify a token on its own. wn-cloud also issues opaque, non-expiring CLI tokens (`wn_…`) through its CLI sign-in flow. wn-server therefore resolves a wn-cloud token by calling wn-cloud's `GET /me` with it, and caches the answer.

wn-server has one account, the owner. It holds one wn-cloud credential, the server credential, which is the owner's CLI token. Only wn-server talks to Navidrome. No client holds a Navidrome credential.

Owner account. `wn-server init` creates the owner account with an app password (WAX-947). wn-server creates the owner's Navidrome user through Navidrome's native API, with admin set explicitly. This also lands with WAX-947. wn-server's own identity (`wn-server`) is already Navidrome's first user and its admin, created by the supervisor at startup (see Navidrome supervisor). The proxy must not forward any request to Navidrome before the supervisor reports ready, so that no client can become Navidrome's first user.

Owner setup for cloud mode, once, on the server machine (`wn-server login`). This is the code display variant of wn-cloud's CLI sign-in flow. Nothing redirects to wn-server, so it works on a headless machine:

1. wn-server prints a URL to `<wn-cloud>/cli/auth` with a state, a PKCE challenge, a label naming the server, and `display=code`.
2. The owner opens the URL in a browser on any device, signs in with their waxnerd account, and approves.
3. wn-cloud shows a sign-in code with a copy button. The owner pastes it at the `wn-server login` prompt.
4. wn-server checks the state, posts the code and the PKCE verifier to `POST /cli/token`, and receives a CLI token. That token is the server credential. It is the owner's token, and wn-server uses it for everything it does at wn-cloud.
5. wn-server registers this server with `POST /servers` and receives a server id.
6. wn-server links the owner account to the owner's waxnerd user id.

First-party clients (wn-mobile, wn-desktop), once per device, in cloud mode:

1. The client signs in at wn-cloud through wn-cloud's own sign-in flow and holds a non-expiring wn-cloud token (`wn_…`).
2. The client calls `POST /api/v1/session` on wn-server with that token.
3. wn-server resolves the token with wn-cloud's `GET /me` and checks that the user id equals the owner's waxnerd user id. Any other user is refused.
4. wn-server returns a wn-server session token. It is long-lived, revocable by the owner, and valid only at this server. It grants nothing at wn-cloud. wn-server does not store the client's wn-cloud token.

Every request after that:

1. The client sends `Authorization: Bearer <session>` (or the Subsonic `apiKey` parameter). Third-party clients send the owner's name and the app password instead.
2. wn-server checks the session or password against its own store. No call to wn-cloud.
3. wn-server forwards the request to Navidrome on loopback with `Remote-User: <account id>`. Navidrome's trusted proxy settings (`ExtAuth.UserHeader`, `ExtAuth.TrustedSources`) accept the header.
4. The response passes back through wn-server.

Navidrome's own web UI is blocked at the proxy. Only its Subsonic API passes through. wn-server's web UI is the only UI.

### Accounts and modes

wn-server has one account, the owner. It lives in wn-server's own SQLite store: id, display name, app password, and an optional waxnerd user id. Navidrome's `Remote-User` header carries the account id. Linking to wn-cloud is a property of the account, not its identity.

wn-server runs in one of two modes. The difference is whether `wn-server login` has run.

- **Local-only.** The state before `wn-server login` has run. The owner account has no waxnerd user id. Clients authenticate with the Subsonic scheme (`u` plus `t`/`s`, or `u` plus `p`) against the app password. Ratings are stored locally and mirrored into Navidrome. Nothing syncs with wn-cloud.
- **Cloud.** The state after `wn-server login`. The owner account carries the waxnerd user id, and wn-server holds the server credential. First-party clients exchange a wn-cloud token for a session. The app password keeps working. Ratings sync with wn-cloud.

Sessions are checked against wn-server's own store, so an outage of wn-cloud never refuses the owner. Sessions already issued and the app password keep working. New sessions through `POST /api/v1/session` fail until wn-cloud answers, because they need `GET /me`. Rating writes wait in the outbox. The web UI shows a banner "wn-cloud unreachable since <time>". There is no cut-off.

The app password exists so that third-party Subsonic clients and the web UI work in both modes. `wn-server init` generates it. The web UI's account page shows it with copy and regenerate buttons. `wn-server password` prints it and `--reset` regenerates it, for a headless machine. The Subsonic token scheme needs the password itself, so the store holds it as-is, in a file readable only by the server's user.

### Web UI

wn-server serves its own web UI. Only the owner signs in, with the account name and the app password, in both modes. The browser does not sign in at wn-cloud. The UI has a status page, match review, and an account page that shows the app password. In cloud mode the status page shows the outage banner while wn-cloud is unreachable, and tells the owner to run `wn-server login` again when the server credential stops working.

Later, waxnerd relay (a paid wn-cloud feature) lets authorized users reach a wn-server from the public internet. wn-cloud redirects the user to the server, and the server's own web UI takes over. The web UI is built so that this needs no change on the server beyond being reachable. Listeners, invites, and listener sign-in return after relay is specified.

### Ratings and favorites

wn-cloud is the source of truth for the owner's ratings. Ratings use waxnerd's 10-point scale (5 stars with half stars). Releases and tracks also carry a favorite mark. Likes and comments may follow on the same model.

Navidrome's own rating is an integer from 0 to 5 and cannot hold half stars, so it is not the source of truth. Two write paths share one code path in wn-server:

- The proxy intercepts the Subsonic `setRating`, `star`, and `unstar` calls. It asks Navidrome for the item's MBID, writes the value to wn-cloud with the server credential (or to a local outbox when wn-cloud is unreachable), updates the local store, and forwards the original call to Navidrome so its stars mirror the value. Whole stars only, because Subsonic carries integers.
- `PUT` and `DELETE` on `/api/v1/ratings/:subsonicId` take half stars and a like level from first-party clients. They write wn-cloud and the store, then send Navidrome a rounded `setRating`: half stars round up, love maps to starred. First-party clients never call wn-cloud's rating routes directly.

Navidrome's smart playlists and third-party clients see the rounded stars. First-party clients read the full values from `GET /api/v1/ratings`.

Later direction, not in v1: smart playlists defined in wn-server on the 10-point scale, evaluated against wn-server's rating store and Navidrome's library data, and written into Navidrome as ordinary playlists so every client can play them.

Sync only applies to MusicBrainz-matched music. wn-cloud's rating model (`ratings` in `../v2`) keys a rating by subject kind and MBID: `release_group`, `release`, `track`, or `artist`. A rating holds `half_stars` from 1 to 10 and a `like_level` of like or love. Navidrome albums map to `release` subjects through `mbz_album_id`. Navidrome songs map to `track` subjects through `mbz_release_track_id`, which is the MusicBrainz track id, not the recording id. A Navidrome star maps to `love`. wn-cloud has the tables but no JSON routes for ratings yet; those are a wn-cloud item, specified in [docs/wn-cloud-api.md](docs/wn-cloud-api.md).

### MusicBrainz matching

wn-server runs `beet import` in copy mode: `copy: yes`, `move: no`, `write: yes`. beets copies each matched album into the sorted directory (`sorted_dir` in the config) and writes the MusicBrainz ids into the copy. The original files are never modified or moved. Navidrome scans both the original folders and the sorted directory. After a confident copy, wn-server writes an empty `.ndignore` file into the original album folder, which Navidrome's scanner skips, so the album is not shown twice. Deleting the marker brings the original back. That marker is the only write to the originals tree.

Matched music therefore exists twice on disk. A move mode or a prune command can reclaim the space later, once the matching is trusted. Not in v1. There is no coupling to Navidrome's database.

In quiet mode beets applies confident matches and skips the rest. A small beets plugin shipped in the bundle, `wn_candidates`, writes one JSON line per album it saw: folder, existing tags, and every candidate with release MBID, artist, album, year, label, and distance. wn-server reads that file into its match queue. There is no second beets pass and no parsing of the import log. The generated beets config sets `import.incremental` and `incremental_skip_later`, so repeated runs skip folders already imported and re-offer queued albums. The web UI shows the queue for review. The CLI drives the same queue.

The `chroma` plugin (AcoustID fingerprints) raises match confidence. wn-server ships with waxnerd's AcoustID application key compiled in. `acoustid_api_key` in `config.toml` overrides it.

### Decisions

- Navidrome runs as a separate process, not as an imported Go library and not as a Navidrome Wasm plugin. A library would tie wn-server to Navidrome's internal wiring. A plugin cannot hook auth or run beets.
- beets runs as a subprocess. It has no embedding story outside Python.
- beets copies into a sorted directory and never modifies or moves the original files. Originals that were copied are hidden from Navidrome with a `.ndignore` marker. (2026-10-16)
- `wn-server login` uses a code that wn-cloud shows and the owner pastes into wn-server, not a redirect to wn-server. A redirect fails for a headless server, for a browser on another device, and behind the relay. (2026-10-16)
- wn-server is written in Go and ships as one static binary inside a release bundle that also carries Navidrome and beets.
- wn-server is GPL-3.0. It is the free part of waxnerd. wn-cloud and the client apps are separate projects. A managed hosting service running wn-server carries no GPL obligation, since GPL-3.0 has no network clause. A closed-source desktop app may ship wn-server as a separate executable and talk to it over HTTP or the CLI; it may not import its Go packages or link it statically. Outside contributions will need a contributor agreement to keep relicensing possible.
- Every route wn-server calls on wn-cloud is written down first in [docs/wn-cloud-api.md](docs/wn-cloud-api.md), and the stub server in `internal/wncloud/stub` serves the same routes from memory for tests. wn-cloud is implemented against the document. The document ends with open questions for the operator. (WAX-946)
- The owner account and a local-only mode are part of v1 so that the server is usable without wn-cloud and keeps working through an outage. (WAX-945, decisions 1 and 2)
- First-party clients exchange a wn-cloud token once for a wn-server session, and wn-server verifies sessions itself. Without this, every request would need wn-cloud, and an outage would stop the owner's clients. (WAX-945 decision 2, amended 2026-10-16)
- Third-party Subsonic clients authenticate with the owner's app password. First-party clients use wn-cloud tokens. (WAX-945, decision 1, amended 2026-10-16)
- Listeners, invites, and membership are deferred until waxnerd relay is specified, because listener sign-in depends on it. v1 has one account, the owner. (2026-10-16)
- Half stars are written through wn-server's own rating API, not through Subsonic. (WAX-945, decision 3)
- beets candidates come from a shipped beets plugin that writes JSON, not from the import log. (WAX-945, decision 4)
- One AcoustID application key is compiled in, with a config override. (WAX-945, decision 5)
- Work on wn-cloud is tracked in the same Linear project, labelled for wn-cloud, and implemented separately from this repository.
- Alternatives considered and not chosen: gonic, Jellyfin, Funkwhale, Koel, Airsonic-Advanced for the server; MusicBrainz Picard and a hand-written matcher for matching.

## Build

Requires Go. `go.mod` names the Go version and the toolchain downloads it when needed.

```
make build    # bin/wn-server
make test
make lint     # go vet, plus golangci-lint when installed
make check    # fmt, lint, test, build
```

## Configuration

The data directory is `$XDG_DATA_HOME/wn-server`, or `~/.local/share/wn-server` when `XDG_DATA_HOME` is unset. The config file is `<data dir>/config.toml`. The `--config` flag sets another path. In that case the data directory is the directory that contains the file.

`wn-server init --folder <path>` creates the data directory and a first `config.toml`. The flag repeats for more folders. `init` refuses to overwrite an existing config.

| Key | Default | Meaning |
|---|---|---|
| `music_folders` | none, required | Absolute paths of the music folders. At least one. Each must exist and be readable. |
| `listen` | `0.0.0.0:4533` | Address of the wn-server proxy. |
| `wn_cloud_url` | `https://waxnerd.com` | Base URL of wn-cloud. |
| `navidrome_bin` | `navidrome` next to the `wn-server` executable | Navidrome binary. |
| `beets_bin` | `beets` next to the `wn-server` executable | beets binary. |
| `fpcalc_bin` | `fpcalc` next to the `wn-server` executable | Chromaprint binary. |
| `navidrome_port` | `4534` | Loopback port of the Navidrome child process. |
| `sorted_dir` | `<data dir>/sorted` | Directory beets copies matched albums into. Created on load. |
| `acoustid_api_key` | empty | Overrides the compiled-in AcoustID key. |

`Config.WriteNavidrome` renders `<data dir>/navidrome.toml`. The file has `Address = '127.0.0.1'`, the Navidrome port, `sorted_dir` as `MusicFolder`, `DataFolder = <data dir>/navidrome`, and `ExtAuth.UserHeader` and `ExtAuth.TrustedSources` set for the proxy. Navidrome's config file takes one `MusicFolder` only. Navidrome stores it as the path of library 1 on every start, and library paths are unique, so `MusicFolder` must never change. `sorted_dir` is created on load and never moves, which makes it the one stable choice. The supervisor calls `WriteNavidrome` before it starts Navidrome, and adds every `music_folders` entry as a library through Navidrome's API.

## Navidrome supervisor

`internal/navidrome` runs the child process. `wn-server serve` loads the config, runs the supervisor, and stops it on SIGINT or SIGTERM.

- **Version pin.** `navidrome.PinnedVersion` is `0.64.2`. When `serve` starts, the supervisor runs `<navidrome_bin> --version`, which prints the version without the `v` followed by the commit sha, for example `0.64.2 (101145742)`. If the version differs, `serve` exits with an error that names both versions.
- **Start.** The supervisor calls `Config.WriteNavidrome`, then runs `<navidrome_bin> -c <data dir>/navidrome.toml`. Each stdout and stderr line of the child goes to wn-server's log through `log/slog`, prefixed `navidrome:`.
- **Ready.** The supervisor polls `http://127.0.0.1:<navidrome_port>/ping` every 250 ms until it answers 200. Navidrome runs database migrations before it answers, and a migration after a version bump can be slow. After 120 s without a 200 the supervisor stops the child and restarts it with backoff.
- **Restart.** When the child exits unexpectedly, the supervisor restarts it after 1 s. The delay doubles on each further exit, up to 30 s, and resets after the child stayed up for 60 s. `RestartPolicy.Next` computes the delay.
- **Stop.** The child runs in its own process group. On Linux the kernel also kills it when wn-server dies (`Pdeathsig`). On context cancellation the supervisor sends SIGTERM to the process group, waits up to 10 s, then sends SIGKILL.
- **wn-server identity.** `navidrome.Identity` is `wn-server`. The first request after ready is `GET /app/` with `Remote-User: wn-server`. Navidrome creates a header user only on that route, and the first user it creates is admin. The supervisor reads `auth.isAdmin` from the config that response injects into the page and logs an error if wn-server's user is not admin. Later requests use the same header on the native API, which accepts it from the trusted source `127.0.0.1/32`.
- **Library sync.** After ready, the supervisor lists `GET /api/library` and creates a library with `POST /api/library` (`{"name": ..., "path": ...}`) for each `music_folders` entry that has none. `sorted_dir` is expected to be library 1 already, because it is `MusicFolder`; the sync logs an error if it is not. A failed create does not stop the sync: every entry is attempted and the errors are reported together. Each library is named after the last path element, with a numeric suffix when the name is taken. Libraries that wn-server did not create are not changed or deleted.

## Repository layout

```
cmd/wn-server/   main package
internal/        one package per component (see Components)
docs/            design documents and diagrams
```

## Reference

- wn-cloud API contract: [docs/wn-cloud-api.md](docs/wn-cloud-api.md).
- Navidrome source: a checkout at `../navidrome` tracks upstream `main`.
- The earlier from-scratch server (Rust) lives at `../wn-server-vibecoded` and is reference only.
