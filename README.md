# wn-server

wn-server is the waxnerd music server. It runs [Navidrome](https://www.navidrome.org/) as a child process and adds three waxnerd features on top of it:

- `wn-server login`: the owner signs in at wn-cloud and pastes a sign-in code into the terminal. wn-server registers itself under the owner's waxnerd account. The owner's apps can then sign in with a short code that wn-cloud issues for this server.
- MusicBrainz matching: beets matches albums to MusicBrainz releases. Confident matches apply silently. Others wait for review in the web UI or the CLI.
- Rating sync: ratings and favorites are stored in wn-cloud on waxnerd's 10-point scale, not in Navidrome.

wn-server is licensed under GPL-3.0 (see `LICENSE`). It is powered by Navidrome, which is also GPL-3.0 and runs as a separate process.

**Status:** configuration, the Navidrome supervisor, the account store, and local-only mode. `wn-server init` writes `config.toml` and creates the owner account with an app password. `wn-server password` prints the app password and `--reset` replaces it. `wn-server serve` checks the Navidrome version, writes `navidrome.toml`, runs Navidrome as a child process, creates the owner's Navidrome user, adds the configured libraries, and serves the proxy on `listen`. The proxy checks a wn-server session or the owner's app password on `/rest/*`, passes `/share/*` without a check, serves wn-server's own `/api/v1/*` routes, answers 404 on every other path, and answers 503 on `/rest/*` and `/share/*` until the supervisor reports ready. First-party clients exchange a device code for a session at `POST /api/v1/session` and revoke it with `DELETE /api/v1/session`. `wn-server login` links the owner account to a waxnerd user and stores the server credential. `wn-server logout` revokes and removes it. `wn-server status` prints the owner account and the link without calling wn-cloud. `wn-server match` runs a quiet beets import over the music folders, copies confident matches into `sorted_dir`, marks their original folders with `.ndignore`, and asks Navidrome to scan. The match queue and the `match` subcommands are not built yet. The wn-cloud client covers sign-in, server registration, and device code verification. The wn-cloud API contract and an in-memory wn-cloud stub for tests are in place. v1 has one account, the owner. This file describes the agreed design and is updated as the code lands.

## Architecture

### Process model

One `wn-server` binary. It supervises a pinned `navidrome` binary, writes its configuration, and sits in front of it as a reverse proxy. Navidrome listens on `127.0.0.1` only. Every request from the network goes through wn-server.

beets runs as a subprocess and only writes into the sorted directory. A pinned beets, with its Python interpreter, ships in the release bundle together with the pinned Navidrome binary and Chromaprint's `fpcalc`, so a release works without any install step. ffmpeg is optional and only Navidrome uses it, for transcoding.

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
| `internal/cli` | cobra command line: `init`, `serve`, `password`, `login`, `logout`, `status`, `match`, `version` |
| `internal/config` | `config.toml` loading and defaults, data directory, rendered `navidrome.toml` |
| `internal/navidrome` | child process supervisor, version pin, library sync, wn-server's own Navidrome identity, and the owner's Navidrome user |
| `internal/proxy` | reverse proxy to Navidrome. It forwards `/rest/*` after checking a wn-server session (`apiKey` or `Authorization: Bearer`) or the Subsonic credentials against the app password, and `/share/*` without a check. It serves wn-server's own `/api/v1/*` routes: the device code exchange and session revocation at `/api/v1/session`, with a local rate limit on failed exchanges. Every other path answers 404. It answers 503 on `/rest/*` and `/share/*` until the supervisor is ready, strips incoming `Remote-User` and `Authorization`, sets `Remote-User` to the account id after a successful check, removes the credential parameters before forwarding, and streams responses unbuffered. Rating intercepts are not built yet |
| `internal/wncloud` | wn-cloud client: PKCE and state, the code display sign-in, `POST /cli/token`, `POST /cli/logout`, server registration, and device code verification, with typed errors per error code. The rating routes are not built yet. `stub` serves the same routes from memory for tests |
| `internal/beets` | generated beets config, `beet import` subprocess, candidates file parser, `.ndignore` markers, scan trigger. The match queue is not built yet |
| `internal/ratings` | local store of ratings and favorites, rating API, reconcile loop |
| `internal/store` | wn-server's SQLite database, `<data dir>/wn-server.db`, mode 0600, with embedded migrations. It holds the owner account, the app password, the server credential, and the sessions of first-party clients. The match queue, ratings, and the outbox are not built yet |
| `internal/web` | web UI for the owner: app password sign-in, status, match review, account page |

### Authentication

The full flow is in [docs/auth-flow.svg](docs/auth-flow.svg).

wn-cloud is the v2 rewrite of waxnerd (`../v2`): an Express server with GoTrue for sign-in. GoTrue signs access tokens with a shared HS256 secret, so wn-server cannot verify a token on its own. wn-cloud also issues opaque, non-expiring CLI tokens (`wn_…`) through its CLI sign-in flow. wn-server holds one of them, the owner's, and accepts no wn-cloud token from clients. First-party clients sign in with a device code that wn-server verifies at wn-cloud.

wn-server has one account, the owner. It holds one wn-cloud credential, the server credential, which is the owner's CLI token. Only wn-server talks to Navidrome. No client holds a Navidrome credential.

Owner account. `wn-server init` creates the owner account with an app password. When `serve` starts and Navidrome answers `/ping`, the supervisor creates the owner's Navidrome user through Navidrome's native API, with admin set, if the user does not exist yet. Navidrome does not create it from a header request. The supervisor reports ready only after the user exists. wn-server's own identity (`wn-server`) is already Navidrome's first user and its admin, created by the supervisor at startup (see Navidrome supervisor). The proxy must not forward any request to Navidrome before the supervisor reports ready, so that no client can become Navidrome's first user.

Owner setup for cloud mode, once, on the server machine (`wn-server login`). This is the code display variant of wn-cloud's CLI sign-in flow. Nothing redirects to wn-server, so it works on a headless machine:

1. wn-server prints a URL to `<wn-cloud>/cli/auth` with a state, a PKCE challenge, a label naming the server, and `display=code`.
2. The owner opens the URL in a browser on any device, signs in with their waxnerd account, and approves.
3. wn-cloud shows a sign-in code with a copy button. The code has the form `<code>.<state>`. The owner pastes it at the `code: ` prompt of `wn-server login`.
4. wn-server trims the pasted value and compares its state part with the state it generated. When they differ, it prints "the code does not belong to this login attempt" and exits without calling wn-cloud.
5. wn-server posts the code and the PKCE verifier to `POST /cli/token` once and receives a CLI token. That token is the server credential. It is the owner's token, and wn-server uses it for everything it does at wn-cloud. A rejected code ends the login, because a failed exchange spends the code.
6. wn-server registers this server with `POST /servers` and receives a server id. If the registration fails, wn-server revokes the new token with `POST /cli/logout` and exits. When the store holds a server id from an earlier login by the same waxnerd user, wn-server reuses it and skips `POST /servers`. `logout` keeps the server id for this purpose.
7. wn-server stores the server credential and links the owner account to the owner's waxnerd user id, in one transaction. If the store write fails, wn-server revokes the new token with `POST /cli/logout` and exits. It prints the waxnerd user id, the server id, and the app password.
8. On the first `login`, wn-server queues every rating in the local store in the outbox. The next drain sends them to wn-cloud with the server credential, so they become the owner's ratings at wn-cloud. This step is not built yet.

The server name is `login --name`, or the host name when the flag is absent. wn-server sends it as the label of `GET /cli/auth` and as the name of `POST /servers`. `login` refuses to run when a server credential is stored. `wn-server logout` revokes the server credential with `POST /cli/logout`, removes the token, and clears the owner's waxnerd user id. It keeps the server id. When wn-cloud answers with any error or is unreachable, `logout` prints a warning and removes the credential. `wn-server status` reads only the local store and the config.

First-party clients (wn-mobile, wn-desktop), once per device, in cloud mode:

1. The owner, signed in at wn-cloud in any browser, opens the device code page. It lists the owner's servers. The owner picks this server, and wn-cloud shows a short code. The code is bound to the server id, single use, and expires after 10 minutes. It carries no other rights.
2. The owner enters the code in the client. The client posts `{ "code": "K7QM-4XPA", "device": "<label>" }` to `POST /api/v1/session` on wn-server. `device` is optional, at most 100 characters, without control characters. wn-server removes `-` and whitespace from the code and upper-cases it, as wn-cloud does.
3. wn-server verifies the code with wn-cloud's `POST /servers/:id/device-codes/verify`, authenticated with the server credential. wn-cloud spends the code and answers with the id of the user who issued it. wn-server calls it once and does not retry.
4. wn-server checks that the user id equals the owner's waxnerd user id. Any other user is refused.
5. wn-server returns `{ "token": "wns_...", "account": { "id": "...", "name": "..." } }` with status 200. The token is `wns_` followed by 32 random bytes in base64url without padding. It does not expire, can be revoked, and is valid only at this server. It grants nothing at wn-cloud. The store keeps its SHA-256 hash and the device label, never the token. Nothing from wn-cloud is stored for the device. No client holds a wn-cloud CLI token.

`POST /api/v1/session` answers errors as `{ "error": "<message>", "code": "<code>" }`:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | The body is not JSON, the code does not have 8 characters of the device code alphabet, or the device label is too long or has a control character. |
| 401 | `invalid_code` | wn-cloud answered `invalid_grant`: the code is unknown, expired, used, or for another server. |
| 403 | `not_owner` | The code was issued by a waxnerd user other than the owner. |
| 409 | `not_linked` | The server has no server credential. `wn-server login` has not run, or `logout` has. |
| 429 | `rate_limited` | The local rate limit applies, or wn-cloud answered `rate_limited`. |
| 502 | `cloud_error` | wn-cloud refused the server credential (`401`, or `403 forbidden`) or gave another unexpected answer. wn-server logs an error that tells the owner to run `wn-server login` again. |
| 503 | `cloud_unreachable` | wn-cloud timed out, could not be reached, or answered 5xx. The code is not spent, and the client can retry with it. |

The local rate limit is in memory. It allows at most 5 failed exchanges per client address and at most 30 failed exchanges in total in any 10 minutes. The client address is the TCP peer address without the port; forwarded headers are not read. Failures with `invalid_request`, `invalid_code`, and `not_owner` count. An exchange counts as a failure while it waits for wn-cloud, so parallel attempts cannot pass the limit. A success does not count, and neither do `not_linked`, `rate_limited`, `cloud_error`, and `cloud_unreachable`, which the client did not cause. Beyond the limit wn-server answers 429 without calling wn-cloud. Every failed exchange is logged at warn level with the reason and the client address, never the code. While wn-cloud is unreachable, wn-server logs that at most once a minute.

`DELETE /api/v1/session` with `Authorization: Bearer <session>` revokes the caller's session and answers 204. Without a valid session it answers 401 with code `unauthorized`. Sessions are logged at info level when they are created, with the account id and the device label, and when they are revoked.

Every request after that:

1. The client sends `Authorization: Bearer <session>` or, on `/rest/*`, the OpenSubsonic `apiKey=<session>` parameter. Third-party clients send the owner's name and the app password instead.
2. wn-server checks the session or password against its own store. No call to wn-cloud. A revoked session is refused on the next request. wn-server records the time a session was last seen, at most once a minute per session.
3. wn-server forwards the request to Navidrome on loopback with `Remote-User: <account id>`. Navidrome's trusted proxy settings (`ExtAuth.UserHeader`, `ExtAuth.TrustedSources`) accept the header. The `apiKey` parameter and the `Authorization` header are removed before forwarding.
4. The response passes back through wn-server.

On `/rest/*` wn-server follows OpenSubsonic's rules for `apiKey`, and treats a Bearer header the same way. A session together with `u`, `p`, `t`, `s`, or `jwt`, or an `apiKey` together with a Bearer header, gets Subsonic error 43. An unknown or revoked session gets error 44. Both answer with HTTP 200, as Navidrome does. A request without a session goes through the app password check. wn-server's own `/api/v1/*` routes take only the Bearer header and answer 401 without a valid session. `POST /api/v1/session` needs no session. The `/api/v1/*` routes do not wait for the supervisor to report ready, because they do not reach Navidrome.

Navidrome's own web UI is blocked at the proxy. Only its Subsonic API passes through. wn-server's web UI is the only UI.

### Accounts and modes

wn-server has one account, the owner. It lives in wn-server's own SQLite store, in the `accounts` table: id (16 random hex characters), name, app password, an optional waxnerd user id, and the creation time. The name is the Subsonic user name. It defaults to `owner` and `init --name` sets it. Names compare without regard to case, as in Navidrome. Navidrome's `Remote-User` header carries the account id, and the owner's Navidrome user name is the account id. Linking to wn-cloud is a property of the account, not its identity.

wn-server runs in one of two modes. The difference is whether `wn-server login` has run.

- **Local-only.** The state before `wn-server login` has run. The owner account has no waxnerd user id. Clients authenticate with the Subsonic scheme (`u` plus `t`/`s`, or `u` plus `p`, plain or `enc:` hex) against the app password. A wrong credential gets Subsonic error 40 and a missing one gets error 10, both with HTTP 200, as from Navidrome. Ratings are stored locally and mirrored into Navidrome (not built yet). Nothing syncs with wn-cloud.
- **Cloud.** The state after `wn-server login`. The owner account carries the waxnerd user id, and wn-server holds the server credential. The credential is one row of the `server_credential` table: the CLI token, the server id, the waxnerd user id, and the creation time. The table holds at most one row. After `logout` the row keeps the server id and the waxnerd user id, and the token is empty, which means not linked. The token lives in the store file, which has mode 0600. There is no separate token file. First-party clients exchange a device code for a session. The app password keeps working. Ratings sync with wn-cloud.

Sessions are rows of the `sessions` table: an id (16 random hex characters), the account id, the SHA-256 hash of the token in hex, the device label, the creation time, and the time the session was last seen. Revoking a session deletes its row. `logout` does not revoke sessions; they keep working in local-only mode.

Sessions are checked against wn-server's own store, so an outage of wn-cloud never refuses the owner. Sessions already issued and the app password keep working. New sessions through `POST /api/v1/session` fail until wn-cloud answers, because wn-cloud verifies the device code. Rating writes wait in the outbox. The web UI shows a banner "wn-cloud unreachable since <time>". There is no cut-off.

Third-party Subsonic clients do not use the device code. They sign in with the owner's name and the app password through the Subsonic token scheme, and wn-server checks them locally. wn-cloud is not involved.

The app password exists so that third-party Subsonic clients and the web UI work in both modes. `wn-server init` generates it. The web UI's account page shows it with copy and regenerate buttons. `wn-server password` prints it and `--reset` regenerates it, for a headless machine. The Subsonic token scheme needs the password itself, so the store holds it as-is, in a file readable only by the server's user. An app password is 20 characters from `crypto/rand`, without the characters `0 O 1 l I`. A reset takes effect on the next request; `serve` does not need a restart.

### Web UI

wn-server serves its own web UI. Only the owner signs in, with the account name and the app password, in both modes. The browser does not sign in at wn-cloud. The UI has a status page, match review, and an account page that shows the app password. In cloud mode the status page shows the outage banner while wn-cloud is unreachable, and tells the owner to run `wn-server login` again when the server credential stops working.

Later, waxnerd relay (a paid wn-cloud feature) lets authorized users reach a wn-server from the public internet. wn-cloud redirects the user to the server, and the server's own web UI takes over. The web UI is built so that this needs no change on the server beyond being reachable. Listeners, invites, and listener sign-in return after relay is specified.

### Ratings and favorites

wn-cloud is the source of truth for the owner's ratings. Ratings use waxnerd's 10-point scale (5 stars with half stars). Releases and tracks also carry a favorite mark. Likes and comments may follow on the same model.

Navidrome's own rating is an integer from 0 to 5 and cannot hold half stars, so it is not the source of truth. Two write paths share one code path in wn-server:

- The proxy intercepts the Subsonic `setRating`, `star`, and `unstar` calls. It asks Navidrome for the item's MBID, writes the value to the local store, then to wn-cloud with the server credential (or to a local outbox when wn-cloud is unreachable), and forwards the original call to Navidrome so its stars mirror the value. Whole stars only, because Subsonic carries integers.
- `PUT` and `DELETE` on `/api/v1/ratings/:subsonicId` take half stars and a like level from first-party clients. They write the local store, then wn-cloud (or the outbox when wn-cloud is unreachable), then send Navidrome a rounded `setRating`: half stars round up, love maps to starred. First-party clients never call wn-cloud's rating routes directly.

Changes made at wn-cloud reach wn-server through the reconcile loop. On every background tick (every 5 minutes) wn-server calls `GET /ratings?since=` with its stored cursor, writes new values and deletions into the local store, and pushes rounded stars and the starred flag into Navidrome. A rating changed on waxnerd.com appears in every client after the next tick.

Navidrome's smart playlists and third-party clients see the rounded stars. First-party clients read the full values from `GET /api/v1/ratings`.

Later direction, not in v1: smart playlists defined in wn-server on the 10-point scale, evaluated against wn-server's rating store and Navidrome's library data, and written into Navidrome as ordinary playlists so every client can play them.

Sync only applies to MusicBrainz-matched music. wn-cloud's rating model (`ratings` in `../v2`) keys a rating by subject kind and MBID: `release_group`, `release`, `track`, or `artist`. A rating holds `half_stars` from 1 to 10 and a `like_level` of like or love. Navidrome albums map to `release` subjects through `mbz_album_id`. Navidrome songs map to `track` subjects through `mbz_release_track_id`, which is the MusicBrainz track id, not the recording id. A Navidrome star maps to `love`. wn-cloud has the tables but no JSON routes for ratings yet; those are a wn-cloud item, specified in [docs/wn-cloud-api.md](docs/wn-cloud-api.md).

### MusicBrainz matching

wn-server runs `beet import` in copy mode: `copy: yes`, `move: no`, `write: yes`. beets copies each matched album into the sorted directory (`sorted_dir` in the config) and writes the MusicBrainz ids into the copy. The original files are never modified or moved. Navidrome scans both the original folders and the sorted directory. After a confident copy, wn-server writes an empty `.ndignore` file into the original album folder, which Navidrome's scanner skips, so the album is not shown twice. Deleting the marker brings the original back. That marker is the only write to the originals tree. beets itself writes nothing there, because `beets.yaml` turns off every beets option that writes next to an original, and the `wn_candidates` plugin turns off beets' `fix_extension` (see [Configuration](#configuration)). `sorted_dir` must not be inside a music folder or contain one.

Matched music therefore exists twice on disk. A move mode or a prune command can reclaim the space later, once the matching is trusted. Not in v1. There is no coupling to Navidrome's database.

In quiet mode beets applies confident matches and skips the rest. A confident match is one below beets' default `match.strong_rec_thresh` of 0.04, which wn-server does not change. There is no second beets pass and no parsing of the import log. The generated beets config sets `import.incremental` and `incremental_skip_later`, so repeated runs skip folders already imported and re-offer skipped albums. `duplicate_action` is `skip`. An album whose match is already in the beets library is skipped, is not recorded in the incremental history, and is offered again on every run. Each run looks it up at MusicBrainz again.

`wn-server match` runs one import over every `music_folders` entry in the foreground:

1. It writes `<data dir>/beets.yaml` (mode 0600) and empties `<data dir>/beets-candidates.jsonl`.
2. It runs `<beets_bin> --config <data dir>/beets.yaml import -q <music folders>`. The directory of `fpcalc_bin` goes first on `PATH`, `FPCALC` names `fpcalc_bin`, and `BEETSDIR` is the data directory, so a personal beets config in `~/.config/beets` is not read. `PYTHONDONTWRITEBYTECODE=1` keeps Python from writing `__pycache__` into `beets-plugins`. `HOME` is unchanged. beets also reads `<data dir>/config.yaml` because of `BEETSDIR`. wn-server does not write that file. beets' stderr goes to wn-server's log at info level, and its stdout at debug level. Ctrl-C kills beets and its process group. wn-server then still reads the candidates file and writes the markers for the albums that beets applied before the kill, because beets has already recorded those albums in its incremental history. It starts no scan after a kill.
3. It reads the candidates file.
4. It writes an empty `.ndignore` into the original folder of every applied album, with mode 0644, when the file does not exist. It writes no marker into a folder that is itself a `music_folders` entry, because that would hide the whole library. An empty `.ndignore` hides everything below its folder. So it writes no marker, and logs a warning, when another album of the same run that was not applied has its folder below the applied folder. The folder and the music folders are compared after symlinks are resolved. The marker is the only write to the originals tree.
5. When at least one album was applied, it asks Navidrome to scan, as wn-server's identity, with `GET /rest/startScan`. Navidrome's native API has no scan route. When Navidrome is not running, wn-server logs a warning and the command still succeeds.
6. It prints `applied N, skipped N, duplicates N, errors N`, then one line per skipped album with its folder and best candidate.

The beets plugin `wn_candidates` ships in the bundle as `beets-plugins/wn_candidates.py`. It writes one JSON line per album task to `<data dir>/beets-candidates.jsonl`, with these keys:

| Key | Contents |
|---|---|
| `folder` | Absolute path of the album folder. For an album that beets collapsed from several folders, such as `CD1` and `CD2`, it is their common parent. |
| `tags` | The existing tags: `artist`, `album`, `year`, `track_count`. |
| `decision` | `applied`, `skipped`, `duplicate` (the match is already in the beets library, and beets skips it in quiet mode), or `error`. |
| `candidates` | Up to 10 candidates in beets' order, each with `release_id` (MusicBrainz release MBID), `artist`, `album`, `year`, `label`, and `distance`. |
| `copied_to` | The album folder in `sorted_dir`. Only on `applied` lines. |
| `error` | The message. Only on `error` lines. |

The plugin records the candidates on the `import_task_choice` event, writes `applied` lines on `album_imported`, and writes the remaining tasks when the `import` event ends the session. beets 2.14 decides duplicates after `import_task_choice` and sends no event for an album it skips as a duplicate, so those lines are written at the end of the run. The plugin never raises out of a hook. An internal error becomes an `error` line. wn-server skips lines that do not parse and counts them as errors. The match queue that will read this file (WAX-940) is not built yet.

The `chroma` plugin (AcoustID fingerprints) raises match confidence. The AcoustID application key is compiled in with `-ldflags "-X github.com/waxnerdhq/wn-server/internal/beets.DefaultAcoustIDKey=..."`. `make build`, `make bundle`, and `bundle/build.sh` take it from `WN_ACOUSTID_KEY` in the environment, which is empty by default. `acoustid_api_key` in `config.toml` overrides it. beets has no option for the lookup key: the chroma plugin looks up fingerprints with its module constant `beetsplug.chroma.API_KEY`, and `acoustid.apikey` is the user key for `beet submit`. wn-server therefore writes the key to `wn_candidates.acoustid_app_key` in `beets.yaml`, and the `wn_candidates` plugin sets `beetsplug.chroma.API_KEY` from it when it loads. When both keys are empty, `beets.yaml` has no `acoustid_app_key` and the chroma plugin uses its own key.

### Decisions

- Navidrome runs as a separate process, not as an imported Go library and not as a Navidrome Wasm plugin. A library would tie wn-server to Navidrome's internal wiring. A plugin cannot hook auth or run beets.
- beets runs as a subprocess. It has no embedding story outside Python.
- beets copies into a sorted directory and never modifies or moves the original files. Originals that were copied are hidden from Navidrome with a `.ndignore` marker. (2026-10-16)
- `wn-server login` uses a code that wn-cloud shows and the owner pastes into wn-server, not a redirect to wn-server. A redirect fails for a headless server, for a browser on another device, and behind the relay. (2026-10-16)
- wn-server is written in Go and ships as one static binary inside a release bundle that also carries Navidrome and beets.
- wn-server is GPL-3.0. It is the free part of waxnerd. wn-cloud and the client apps are separate projects. A managed hosting service running wn-server carries no GPL obligation, since GPL-3.0 has no network clause. A closed-source desktop app may ship wn-server as a separate executable and talk to it over HTTP or the CLI; it may not import its Go packages or link it statically. Outside contributions will need a contributor agreement to keep relicensing possible.
- Every route wn-server calls on wn-cloud is written down first in [docs/wn-cloud-api.md](docs/wn-cloud-api.md), and the stub server in `internal/wncloud/stub` serves the same routes from memory for tests. wn-cloud is implemented against the document. The document ends with open questions for the operator. (WAX-946)
- The owner account and a local-only mode are part of v1 so that the server is usable without wn-cloud and keeps working through an outage. (WAX-945, decisions 1 and 2)
- First-party clients exchange a device code once for a wn-server session, and wn-server verifies sessions itself. Without this, every request would need wn-cloud, and an outage would stop the owner's clients. (WAX-945 decision 2, amended 2026-10-16)
- Third-party Subsonic clients authenticate with the owner's app password. First-party clients use wn-server sessions. (WAX-945, decision 1, amended 2026-10-16)
- First-party clients obtain a wn-server session with a short-lived single-use code that wn-cloud issues bound to the server. No client holds a wn-cloud CLI token. (2026-10-16)
- Listeners, invites, and membership are deferred until waxnerd relay is specified, because listener sign-in depends on it. v1 has one account, the owner. (2026-10-16)
- Device-code exchange is rate limited at wn-server and at wn-cloud. (2026-10-16)
- wn-server allows at most 5 failed device code exchanges per client address and 30 in total per 10 minutes, in memory. A restart clears the counts. (WAX-935)
- A wn-server session token is `wns_` followed by 32 bytes from `crypto/rand` in base64url without padding. The prefix tells it apart from wn-cloud's `wn_` CLI tokens. The store keeps only its SHA-256 hash. (WAX-935)
- The server credential is stored in the SQLite store, not in a separate token file. `wn-server login` refuses to replace a stored credential. The owner runs `wn-server logout` first. Logout keeps the server id. (WAX-934)
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
make bundle   # Linux only: release tarball in dist/
```

`internal/beets` has an integration test that runs a real beets import against a temporary folder and needs network access to MusicBrainz. It is skipped unless `WN_BEETS_BIN` names a beets executable. `WN_FPCALC_BIN` names `fpcalc`. Without `WN_BEETS_ALBUM` the test generates one album with `ffmpeg`, which normally does not match, and checks the skip path. The album has a tagged MP3, an MP3 without an extension, and an MP3 stream in a WAV container, so the test also covers `fix_extension` and `remux_mp3_in_wav`. With `WN_BEETS_ALBUM` set to a folder that holds one real album, the test copies that folder into the temporary folder and expects it to be applied. In both cases it checks that the original files keep their bytes and mtimes, that no file other than `.ndignore` appears in the originals tree, that every applied copy carries a MusicBrainz album id (read with `ffprobe`), and that only applied originals get `.ndignore`.

```
WN_BEETS_BIN=/opt/wn-server/beets WN_FPCALC_BIN=/opt/wn-server/fpcalc \
WN_BEETS_ALBUM=/path/to/one/album go test ./internal/beets -run Integration -v
```

### Local-only quick start

1. `wn-server init --folder /path/to/music`. It prints the owner name (`owner` unless `--name` is given) and the app password. Keep the password.
2. `wn-server password` prints the password again. `wn-server password --reset` replaces it and prints the new one.
3. `wn-server serve`. It runs in the foreground. Stop it with Ctrl-C or SIGTERM.
4. In a Subsonic client, set the server to `http://<host>:4533`, the user name to the owner name, and the password to the app password.

Navidrome's web UI is not reachable through wn-server. Only the Subsonic API (`/rest/*`) and public shares (`/share/*`) are, next to wn-server's own `/api/v1/*` routes.

### Cloud mode

1. `wn-server login [--name <server name>]`. It prints a URL. Open the URL in a browser on any device, sign in with a waxnerd account, and approve. Paste the code that wn-cloud shows at the `code: ` prompt. `login` reads `wn_cloud_url` from `config.toml`.
2. `wn-server status` prints the owner name, the account id, the waxnerd user id, the server id, the time of the link, and the wn-cloud URL. It does not call wn-cloud.
3. `wn-server logout` revokes the server credential at wn-cloud and returns the server to local-only mode. Run it before running `login` again.

### Release bundle

A release is one tarball, `wn-server-<version>-linux-amd64.tar.gz`, for Linux on x86_64 with glibc. It holds one directory, `wn-server-<version>-linux-amd64/`, with these files:

| File | Contents |
|---|---|
| `wn-server` | Static Go build (`CGO_ENABLED=0`), version set by the ldflag from the Makefile. |
| `navidrome` | The Navidrome release binary, verified against the release's `navidrome_checksums.txt`. |
| `beets` | beets as one executable that carries its own Python interpreter. Includes `pyacoustid` for the `chroma` plugin and `requests`. |
| `beets-plugins/` | `wn_candidates.py`, wn-server's beets plugin. The generated `beets.yaml` lists this directory in `pluginpath`. |
| `fpcalc` | Chromaprint's static `fpcalc` release binary, verified against the sha256 in `bundle/checksums`. |
| `LICENSE`, `NOTICE` | wn-server's licence, and the bundled projects with their licences. |

The config defaults for `navidrome_bin`, `beets_bin`, and `fpcalc_bin` point at these files, because they sit next to the `wn-server` executable.

Pinned versions, all in `bundle/versions.env`:

| Component | Version |
|---|---|
| Navidrome | 0.64.2 |
| beets | 2.14.1 |
| Chromaprint (`fpcalc`) | 1.6.2 |
| Python inside `beets` | 3.13.16, python-build-standalone release 20261013 |
| pex (build tool) | 2.103.4 |
| uv (build tool) | 0.13.1 |

A test in `internal/navidrome` fails when `NAVIDROME_VERSION` in `bundle/versions.env` differs from `navidrome.PinnedVersion`.

beets is packaged with pex as a scie (`--scie eager`). The executable contains a python-build-standalone interpreter, so the target machine needs no Python. pex resolves the pinned packages once at build time, so the output does not depend on the build host's Python. PyInstaller was not chosen because it freezes the build host's interpreter and needs per-package hooks. On its first run the `beets` executable unpacks itself into `~/.cache/nce`, or into `$SCIE_BASE` when that is set.

`bundle/build.sh <output dir>` builds the tarball and a `.sha256` file next to it. It runs on Linux and needs `bash`, GNU `tar`, `curl`, `sha256sum`, `install`, `grep`, `mktemp`, Go, and `uv`. uv supplies the Python that pex runs under. On Linux x86_64 the script runs each binary once to check its version, and checks that beets loads the `chroma` plugin. `make bundle` calls the script with `dist` as the output directory.

On first run `beets` unpacks its interpreter into `~/.cache/nce`. On a machine where the service user has no writable home, set `SCIE_BASE` to a writable directory.

The `release` workflow in `.github/workflows/release.yml` runs `make check` and `bundle/build.sh` on every pull request to `main` and on every push of a `v*` tag. Each run uploads the tarball as a workflow artifact. A tag run also creates a GitHub release for the tag and attaches the tarball. Pull request runs publish nothing.

## Configuration

The data directory is `$XDG_DATA_HOME/wn-server`, or `~/.local/share/wn-server` when `XDG_DATA_HOME` is unset. The config file is `<data dir>/config.toml`. The `--config` flag sets another path. In that case the data directory is the directory that contains the file.

`wn-server init --folder <path> [--name <owner name>]` creates the data directory, a first `config.toml`, and the store `wn-server.db` with the owner account. The folder flag repeats for more folders. `init` prints the owner name and the app password once. `init` refuses to run when the config exists or the store already has an owner, and then changes nothing.

| Key | Default | Meaning |
|---|---|---|
| `music_folders` | none, required | Absolute paths of the music folders. At least one. Each must exist and be readable. |
| `listen` | `0.0.0.0:4533` | Address of the wn-server proxy. |
| `wn_cloud_url` | `https://waxnerd.com` | Base URL of wn-cloud. |
| `navidrome_bin` | `navidrome` next to the `wn-server` executable | Navidrome binary. |
| `beets_bin` | `beets` next to the `wn-server` executable | beets binary. |
| `fpcalc_bin` | `fpcalc` next to the `wn-server` executable | Chromaprint binary. |
| `navidrome_port` | `4534` | Loopback port of the Navidrome child process. |
| `sorted_dir` | `<data dir>/sorted` | Directory beets copies matched albums into. Created on load. Must not be inside a `music_folders` entry or contain one. Paths are compared after symlinks are resolved. |
| `acoustid_api_key` | empty | Overrides the compiled-in AcoustID key. |
| `navidrome_external_services` | `true` | Sets Navidrome's `EnableExternalServices`. `false` turns off Navidrome's artist and album lookups (Deezer, Last.fm, Spotify agents). `init` does not write this key. |

`wn-server match` writes these files into the data directory:

| File | Contents |
|---|---|
| `beets.yaml` | Generated beets config, mode 0600. Rewritten on every run. |
| `beets.db` | beets library database. |
| `beets-state.pickle` | beets state file, which holds the incremental import history. |
| `beets-import.log` | beets' import log. |
| `beets-candidates.jsonl` | Output of the `wn_candidates` plugin for the last run. |

beets also writes `beets.db-before-*.bak` files when it migrates its database, including on the first run.

`beets.yaml` sets `library`, `directory` (`sorted_dir`), `statefile`, the path format `$albumartist/$album%aunique{}/$track $title`, `import` (`copy`, `write`, `quiet`, `autotag`, `incremental`, and `incremental_skip_later` on; `move`, `link`, `hardlink`, `reflink`, `delete`, `resume`, `fix_ext_inplace`, and `remux_mp3_in_wav` off; `duplicate_action: skip`; `quiet_fallback: skip`; `log`), `plugins` (`musicbrainz`, `chroma`, `wn_candidates`), `pluginpath` (`beets-plugins` next to the beets executable), `chroma.auto`, `wn_candidates.output`, `wn_candidates.acoustid_app_key` when a key is set, `musicbrainz` (`host: musicbrainz.org`, `ratelimit: 1`), and `ui.color: no`. beets 2.14 loads MusicBrainz as a plugin, and a `plugins` key replaces its default list, so `musicbrainz` is listed explicitly.

The import options keep beets from writing to the original folders. Copy is the only file operation. `link`, `hardlink`, `reflink`, and `delete` are beets' defaults and are set explicitly. `remux_mp3_in_wav` defaults to on in beets 2.14. With it on, beets writes an `.mp3` next to a WAV file that holds an MP3 stream and deletes the WAV. `fix_ext_inplace` is off by default and is set explicitly. beets 2.14 also runs `fix_extension` on every file without an extension. When `ffprobe` is on `PATH`, it copies the file to `<name>.<ext>` in the original folder. beets has no option for it, so the `wn_candidates` plugin replaces `beets.util.extension.fix_extension` with a function that returns the path unchanged. beets then reads an extensionless file under its own name.

`Config.WriteNavidrome` renders `<data dir>/navidrome.toml`. The file has `Address = '127.0.0.1'`, the Navidrome port, `sorted_dir` as `MusicFolder`, `DataFolder = <data dir>/navidrome`, and `ExtAuth.UserHeader` and `ExtAuth.TrustedSources` set for the proxy. The file always has `EnableInsightsCollector = false`, so Navidrome Insights telemetry is off. It has `EnableExternalServices = true` unless `navidrome_external_services` is `false`, so external lookups are on by default. Navidrome's config file takes one `MusicFolder` only. Navidrome stores it as the path of library 1 on every start, and library paths are unique, so `MusicFolder` must never change. `sorted_dir` is created on load and never moves, which makes it the one stable choice. The supervisor calls `WriteNavidrome` before it starts Navidrome, and adds every `music_folders` entry as a library through Navidrome's API.

## Navidrome supervisor

`internal/navidrome` runs the child process. `wn-server serve` loads the config, opens the store, checks the Navidrome version, binds `listen`, runs the supervisor, and stops it on SIGINT or SIGTERM. It logs one line when the signal arrives and one when shutdown is complete.

- **Version pin.** `navidrome.PinnedVersion` is `0.64.2`. When `serve` starts, before it binds `listen`, it runs `<navidrome_bin> --version`, which prints the version without the `v` followed by the commit sha, for example `0.64.2 (101145742)`. If the version differs, `serve` exits with an error that names both versions.
- **Start.** The supervisor calls `Config.WriteNavidrome`, then runs `<navidrome_bin> -c <data dir>/navidrome.toml`. Each stdout and stderr line of the child goes to wn-server's log through `log/slog`, prefixed `navidrome:`.
- **Ready.** The supervisor polls `http://127.0.0.1:<navidrome_port>/ping` every 250 ms until it answers 200. Navidrome runs database migrations before it answers, and a migration after a version bump can be slow. After 120 s without a 200 the supervisor stops the child and restarts it with backoff.
- **Restart.** When the child exits unexpectedly, the supervisor restarts it after 1 s. The delay doubles on each further exit, up to 30 s, and resets after the child stayed up for 60 s. `RestartPolicy.Next` computes the delay.
- **Stop.** The child runs in its own process group. On Linux the kernel also kills it when wn-server dies (`Pdeathsig`). On context cancellation the supervisor sends SIGTERM to the process group, waits up to 10 s, then sends SIGKILL.
- **wn-server identity.** `navidrome.Identity` is `wn-server`. The first request after ready is `GET /app/` with `Remote-User: wn-server`. Navidrome creates a header user only on that route, and the first user it creates is admin. The supervisor reads `auth.isAdmin` from the config that response injects into the page and logs an error if wn-server's user is not admin. Later requests use the same header on the native API, which accepts it from the trusted source `127.0.0.1/32`.
- **Owner's user.** After wn-server's identity exists, the supervisor lists `GET /api/user` and looks for a user whose `userName` is the owner's account id. If there is none, it calls `POST /api/user` with `userName` set to the account id, `name` set to the owner name, `isAdmin` true, and a random `password` that is not stored. A failure is logged and retried every 5 s; the proxy answers 503 until it succeeds.
- **Library sync.** After ready, the supervisor lists `GET /api/library` and creates a library with `POST /api/library` (`{"name": ..., "path": ...}`) for each `music_folders` entry that has none. `sorted_dir` is expected to be library 1 already, because it is `MusicFolder`; the sync logs an error if it is not. A failed create does not stop the sync: every entry is attempted and the errors are reported together. Each library is named after the last path element, with a numeric suffix when the name is taken. Libraries that wn-server did not create are not changed or deleted.

## Repository layout

```
cmd/wn-server/   main package
internal/        one package per component (see Components)
docs/            design documents and diagrams
bundle/          release bundle script, pinned versions, checksums
.github/         release workflow
```

## Reference

- wn-cloud API contract: [docs/wn-cloud-api.md](docs/wn-cloud-api.md).
- Navidrome source: a checkout at `../navidrome` tracks upstream `main`.
- The earlier from-scratch server (Rust) lives at `../wn-server-vibecoded` and is reference only.
