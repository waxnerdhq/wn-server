# wn-server

wn-server is the waxnerd music server. It runs [Navidrome](https://www.navidrome.org/) as a child process and adds three waxnerd features on top of it:

- `wn-server login`: a browser flow against a waxnerd account that registers the server and grants access.
- MusicBrainz matching: beets matches albums to MusicBrainz releases. Confident matches apply silently. Others wait for review in the web UI or the CLI.
- Rating sync: ratings and favorites are stored in wn-cloud on waxnerd's 10-point scale, not in Navidrome.

wn-server is licensed under GPL-3.0 (see `LICENSE`). It is powered by Navidrome, which is also GPL-3.0 and runs as a separate process.

**Status:** scaffold. The commands exist and return "not implemented". This file describes the agreed design and is updated as the code lands.

## Architecture

### Process model

One `wn-server` binary. It supervises a pinned `navidrome` binary, writes its configuration, and sits in front of it as a reverse proxy. Navidrome listens on `127.0.0.1` only. Every request from the network goes through wn-server.

beets runs as a subprocess. A pinned beets, with its Python interpreter, ships in the release bundle together with the pinned Navidrome binary, so a release works without any install step. ffmpeg is optional and only Navidrome uses it, for transcoding.

The first and only release target for now is Linux on x86_64. Other platforms come later.

```
clients (wn-mobile, wn-desktop, web UI)
        │  HTTPS + waxnerd JWT
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
| `internal/cli` | cobra command line: `serve`, `login`, `match`, `version` |
| `internal/config` | `config.toml`, data directory, rendered `navidrome.toml` |
| `internal/navidrome` | child process supervisor and wn-server's own Navidrome credential |
| `internal/proxy` | JWT verification, membership check, `Remote-User` forwarding, rating intercepts |
| `internal/wncloud` | login flow, server registration, membership lookup, rating writes |
| `internal/beets` | beets subprocess, match queue |
| `internal/ratings` | local store of ratings and favorites, rating API, reconcile loop |
| `internal/store` | wn-server's SQLite database: accounts, caches, match queue, ratings, outbox |
| `internal/web` | match review pages and server status |

### Authentication

The full flow is in [docs/auth-flow.svg](docs/auth-flow.svg).

wn-cloud is the v2 rewrite of waxnerd (`../v2`): an Express server with GoTrue for sign-in. GoTrue signs access tokens with a shared HS256 secret, so wn-server cannot verify a token on its own. wn-cloud also issues opaque, non-expiring CLI tokens (`wn_…`) through its CLI sign-in flow. wn-server therefore verifies every token by calling wn-cloud's `GET /me` with it, and caches the answer.

Only wn-server talks to Navidrome. Clients authenticate with wn-cloud and never hold a Navidrome credential.

Owner setup, once, on the server machine (`wn-server login`). This is wn-cloud's existing CLI sign-in flow:

1. wn-server listens on a free loopback port and opens the browser at `<wn-cloud>/cli/auth` with a state, a PKCE challenge, the port, and a label naming the server.
2. The owner signs in with their waxnerd account and approves.
3. wn-cloud redirects the browser to `http://127.0.0.1:<port>/callback` with a code.
4. wn-server posts the code and the PKCE verifier to `POST /cli/token` and receives a CLI token. That token is the server credential.
5. wn-server registers this server under the owner and receives a server id.
6. wn-server sends one request to Navidrome with `Remote-User: <owner id>`. Navidrome creates the user. The first user is admin.
7. Optional: wn-server mints a Navidrome API key for its own background jobs.

Every client request:

1. The client signs in with wn-cloud once and holds a wn-cloud token (a GoTrue access token or a CLI token).
2. The client sends the request to wn-server with `Authorization: Bearer <token>`.
3. wn-server resolves the token to a user with wn-cloud's `GET /me`. The result is cached until the token's expiry, or for a bounded time for a CLI token.
4. wn-server checks that the user is the owner or an invited listener of this server. The membership list comes from wn-cloud and is cached.
5. wn-server forwards the request to Navidrome on loopback with `Remote-User: <waxnerd user id>`. Navidrome's trusted proxy settings (`ExtAuth.UserHeader`, `ExtAuth.TrustedSources`) accept the header.
6. The response passes back through wn-server.

Navidrome's Subsonic endpoint looks the header user up but does not create it. wn-server creates an invited listener's Navidrome user, non-admin, the first time it sees them. Non-admin Navidrome users cannot change the library.

Navidrome's own web UI is blocked at the proxy. Only its Subsonic API passes through. wn-server's web UI is the only UI.

### Accounts, modes, and invites

Accounts belong to wn-server and live in its own SQLite store: id, display name, role (owner or listener), an app password, and an optional waxnerd user id. Navidrome's `Remote-User` header carries the wn-server account id. Linking to wn-cloud is a property of an account, not the identity.

One owner per server. The owner has up to 12 listeners. wn-server enforces the cap. Listeners stream and browse. They cannot write to or upload into the library.

wn-server runs in one of two modes:

- **Local-only.** The state before `wn-server login` has ever run, and the state after 30 days without reaching wn-cloud. `wn-server init` creates the owner account. The owner adds listeners by name from the web UI or the CLI. Clients authenticate with the Subsonic scheme (`u` plus `t`/`s`, or `u` plus `p`) against the account's app password. The web UI has a name and password form. Ratings are stored locally and mirrored into Navidrome. Nothing syncs with wn-cloud.
- **Cloud.** The state after `wn-server login`. Accounts gain waxnerd user ids. wn-cloud tokens are accepted alongside app passwords. Invites, membership, and rating sync go through wn-cloud.

Between an outage and day 30, the proxy serves from caches persisted in the store: token lookups and the member list. Expired GoTrue tokens are still refused. A revocation made during the outage takes effect at the next successful refresh. After day 30 the server behaves as local-only until wn-cloud answers again.

App passwords exist so that third-party Subsonic clients work in both modes. One is generated when an account is created: the owner's at `init` or `login`, a listener's when their membership first appears. The web UI shows it with copy and regenerate buttons. `wn-server password` prints the owner's and `--reset` regenerates it, for a headless machine. The Subsonic token scheme needs the password itself, so the store holds it as-is, in a file readable only by the server's user.

In cloud mode, invites are records in wn-cloud: server id, user id, role. Revoking an invite ends access at the membership check. Navidrome is not involved.

### Web UI

wn-server serves its own web UI. The owner sees status, invites, and match review. Invited listeners get a reduced view. The UI signs in through a variant of wn-cloud's CLI sign-in flow in which the redirect goes to wn-server's own URL instead of a loopback port. wn-server exchanges the code, receives a token for that browser, and keeps it in a cookie session. wn-cloud must accept the registered URLs of a server as redirect targets, on the LAN and on the relay. In local-only mode, and as an alternative in cloud mode, the UI accepts the account name and app password.

Later, waxnerd relay (a paid wn-cloud feature) lets authorized users reach a wn-server from the public internet. wn-cloud redirects the user to the server, and the server's own web UI takes over. The web UI is built so that this needs no change on the server beyond being reachable.

### Ratings and favorites

wn-cloud is the source of truth. Ratings use waxnerd's 10-point scale (5 stars with half stars). Releases and tracks also carry a favorite mark. Likes and comments may follow on the same model.

Navidrome's own rating is an integer from 0 to 5 and cannot hold half stars, so it is not the source of truth. Two write paths share one code path in wn-server:

- The proxy intercepts the Subsonic `setRating`, `star`, and `unstar` calls. It asks Navidrome for the item's MBID, writes the value to wn-cloud with the caller's token (or to a local outbox when wn-cloud is unreachable), updates the local store, and forwards the original call to Navidrome so its stars mirror the value. Whole stars only, because Subsonic carries integers.
- `PUT` and `DELETE` on `/api/v1/ratings/:subsonicId` take half stars and a like level from first-party clients. They write wn-cloud and the store, then send Navidrome a rounded `setRating`: half stars round up, love maps to starred. First-party clients never call wn-cloud's rating routes directly.

Navidrome's smart playlists and third-party clients see the rounded stars. First-party clients read the full values from `GET /api/v1/ratings`.

Later direction, not in v1: smart playlists defined in wn-server on the 10-point scale, evaluated against wn-server's rating store and Navidrome's library data, and written into Navidrome as ordinary playlists so every client can play them.

Sync only applies to MusicBrainz-matched music. wn-cloud's rating model (`ratings` in `../v2`) keys a rating by subject kind and MBID: `release_group`, `release`, `track`, or `artist`. A rating holds `half_stars` from 1 to 10 and a `like_level` of like or love. Navidrome albums map to `release` subjects through `mbz_album_id`. Navidrome songs map to `track` subjects through `mbz_release_track_id`, which is the MusicBrainz track id, not the recording id. A Navidrome star maps to `love`. wn-cloud has the tables but no JSON routes for ratings yet; those are a wn-cloud item.

### MusicBrainz matching

wn-server runs `beet import` with copy and move off and tag writing on. beets tags the files in place with MusicBrainz ids. Navidrome's scanner picks the ids up on the next scan. There is no coupling to Navidrome's database.

In quiet mode beets applies confident matches and skips the rest. A small beets plugin shipped in the bundle, `wn_candidates`, writes one JSON line per album it saw: folder, existing tags, and every candidate with release MBID, artist, album, year, label, and distance. wn-server reads that file into its match queue. There is no second beets pass and no parsing of the import log. The generated beets config sets `import.incremental` and `incremental_skip_later`, so repeated runs skip folders already imported and re-offer queued albums. The web UI shows the queue for review. The CLI drives the same queue.

The `chroma` plugin (AcoustID fingerprints) raises match confidence. wn-server ships with waxnerd's AcoustID application key compiled in. `acoustid_api_key` in `config.toml` overrides it.

### Decisions

- Navidrome runs as a separate process, not as an imported Go library and not as a Navidrome Wasm plugin. A library would tie wn-server to Navidrome's internal wiring. A plugin cannot hook auth or run beets.
- beets runs as a subprocess. It has no embedding story outside Python.
- wn-server is written in Go and ships as one static binary inside a release bundle that also carries Navidrome and beets.
- wn-server is GPL-3.0. It is the free part of waxnerd. wn-cloud and the client apps are separate projects. A managed hosting service running wn-server carries no GPL obligation, since GPL-3.0 has no network clause. A closed-source desktop app may ship wn-server as a separate executable and talk to it over HTTP or the CLI; it may not import its Go packages or link it statically. Outside contributions will need a contributor agreement to keep relicensing possible.
- Every route wn-server calls on wn-cloud is written down first in `docs/wn-cloud-api.md`, and a stub server under `internal/wncloud` serves the same responses for tests. wn-cloud is implemented against the document. (WAX-946)
- Accounts and a local-only mode are part of v1 so that the server is usable without wn-cloud and keeps working through an outage. (WAX-945, decisions 1 and 2)
- Third-party Subsonic clients authenticate with per-account app passwords. First-party clients use wn-cloud tokens. (WAX-945, decision 1)
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

## Repository layout

```
cmd/wn-server/   main package
internal/        one package per component (see Components)
docs/            design documents and diagrams
```

## Reference

- Navidrome source: a checkout at `../navidrome` tracks upstream `main`.
- The earlier from-scratch server (Rust) lives at `../wn-server-vibecoded` and is reference only.
