# wn-server

wn-server is the waxnerd music server. It runs [Navidrome](https://www.navidrome.org/) as a child process and adds three waxnerd features on top of it:

- `wn-server login`: a browser flow against a waxnerd account that registers the server and grants access.
- MusicBrainz matching: beets matches albums to MusicBrainz releases. Confident matches apply silently. Others wait for review in the web UI or the CLI.
- Rating sync: ratings and favorites are stored in wn-cloud on waxnerd's 10-point scale, not in Navidrome.

wn-server is powered by Navidrome, which is licensed under GPL-3.0. Navidrome runs as a separate process and is not linked into this program.

**Status:** scaffold. The commands exist and return "not implemented". This file describes the agreed design and is updated as the code lands.

## Architecture

### Process model

One `wn-server` binary. It supervises a pinned `navidrome` binary, writes its configuration, and sits in front of it as a reverse proxy. Navidrome listens on `127.0.0.1` only. Every request from the network goes through wn-server.

beets runs as a subprocess (Python). It is the one external runtime requirement besides the Navidrome binary. ffmpeg is optional and only Navidrome uses it, for transcoding.

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
| `internal/ratings` | local cache of ratings and favorites, reconcile loop |
| `internal/web` | match review pages and server status |

### Authentication

The full flow is in [docs/auth-flow.svg](docs/auth-flow.svg).

Only wn-server talks to Navidrome. Clients authenticate with wn-cloud and never hold a Navidrome credential.

Owner setup, once, on the server machine (`wn-server login`):

1. wn-server opens the browser at wn-cloud's login page with a PKCE challenge and a redirect to `http://127.0.0.1:<port>/callback`.
2. The owner signs in with their waxnerd account.
3. wn-cloud redirects the browser to the local callback with a code.
4. wn-server exchanges the code for the owner's access and refresh tokens.
5. wn-server registers this server under the owner and receives a server id and server credential.
6. wn-server sends one request to Navidrome with `Remote-User: <owner id>`. Navidrome creates the user. The first user is admin.
7. Optional: wn-server mints a Navidrome API key for its own background jobs.

Every client request:

1. The client signs in with wn-cloud once and holds a waxnerd JWT.
2. The client sends the request to wn-server with `Authorization: Bearer <JWT>`.
3. wn-server verifies the signature against the Supabase JWKS, cached locally.
4. wn-server checks with wn-cloud that the user is the owner or an invited listener of this server. The membership list is cached.
5. wn-server forwards the request to Navidrome on loopback with `Remote-User: <waxnerd user id>`. Navidrome's trusted proxy settings (`ExtAuth.UserHeader`, `ExtAuth.TrustedSources`) accept the header.
6. The response passes back through wn-server.

Navidrome's Subsonic endpoint looks the header user up but does not create it. wn-server creates an invited listener's Navidrome user, non-admin, the first time it sees them. Non-admin Navidrome users cannot change the library.

### Users and invites

One owner per server. The owner invites a capped number of listeners. Listeners stream and browse. They cannot write to or upload into the library.

Invites are records in wn-cloud: server id, user id, role. Revoking an invite ends access at the membership check. Navidrome is not involved.

### Ratings and favorites

wn-cloud is the source of truth. Ratings use waxnerd's 10-point scale (5 stars with half stars). Releases and tracks also carry a favorite mark. Likes and comments may follow on the same model.

Navidrome's own rating is an integer from 0 to 5 and cannot hold half stars, so it is not used as storage. wn-server intercepts the Subsonic `setRating`, `star`, and `unstar` calls at the proxy and writes them to wn-cloud. First-party clients read ratings from wn-server's own API. A rounded star value may be mirrored into Navidrome so its smart playlists can use it; that is a later decision.

Sync only applies to MusicBrainz-matched music. Items are keyed by MusicBrainz release and recording ids, which Navidrome reads from file tags into its `mbz_*` columns.

### MusicBrainz matching

wn-server runs `beet import` with copy and move off and tag writing on. beets tags the files in place with MusicBrainz ids. Navidrome's scanner picks the ids up on the next scan. There is no coupling to Navidrome's database.

In quiet mode beets applies confident matches and skips the rest. wn-server records the skipped albums and shows candidates for review in its web UI. The CLI can drive the same queue. The `chroma` plugin (AcoustID fingerprints) raises match confidence and is planned.

### Decisions

- Navidrome runs as a separate process, not as an imported Go library and not as a Navidrome Wasm plugin. A library would tie wn-server to Navidrome's internal wiring. A plugin cannot hook auth or run beets.
- beets runs as a subprocess. It has no embedding story outside Python.
- wn-server is written in Go and ships as one static binary.
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
