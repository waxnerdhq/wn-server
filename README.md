# wn-server

wn-server is the waxnerd music server. It runs [Navidrome](https://www.navidrome.org/) as a child process and adds three waxnerd features on top of it:

- `wn-server login`: the owner signs in at wn-cloud and pastes a sign-in code into the terminal. wn-server registers itself under the owner's waxnerd account. The owner's apps can then sign in with a short code that wn-cloud issues for this server.
- MusicBrainz matching: beets matches albums to MusicBrainz releases. Confident matches apply silently. Others wait for review in the web UI or the CLI.
- Rating sync: ratings and favorites are stored in wn-cloud on waxnerd's 10-point scale, not in Navidrome.

wn-server is licensed under GPL-3.0 (see `LICENSE`). It is powered by Navidrome, which is also GPL-3.0 and runs as a separate process.

**Status:** configuration, the Navidrome supervisor, the account store, and local-only mode. `wn-server init` writes `config.toml` and creates the owner account with an app password. `wn-server password` prints the app password and `--reset` replaces it. `wn-server serve` checks the Navidrome version, writes `navidrome.toml`, runs Navidrome as a child process, creates the owner's Navidrome user, adds the configured libraries, and serves the proxy on `listen`. The proxy checks a wn-server session or the owner's app password on `/rest/*`, passes `/share/*` without a check, serves wn-server's own `/api/v1/*` routes and the web UI on `/ui/*`, answers 404 on every other path, and answers 503 on `/rest/*` and `/share/*` until the supervisor reports ready. First-party clients exchange a device code for a session at `POST /api/v1/session` and revoke it with `DELETE /api/v1/session`. The web UI signs the owner in with the account name and the app password. It has a status page and an account page with the app password and the list of sessions. `wn-server login` links the owner account to a waxnerd user and stores the server credential. `wn-server logout` revokes and removes it. `wn-server status` prints the owner account and the link without calling wn-cloud. `wn-server match` runs a quiet beets import over the music folders, copies confident matches into `sorted_dir`, marks their original folders with `.ndignore`, and asks Navidrome to scan. It records every album of the run in the match queue in the store. `wn-server match list` prints the queue. `wn-server match apply` tags and copies an album with a release that the owner chose. `wn-server match dismiss` sets an entry aside. `wn-server match revert` removes an applied copy from `sorted_dir` and the beets library. The proxy intercepts the Subsonic calls `setRating`, `star`, and `unstar`. After Navidrome accepts such a call on a matched album or song, wn-server records the rating in the local rating store and queues it in the outbox. The drain in `serve` sends it to wn-cloud with the server credential within a second, and retries at start and every minute. `wn-server login` to a new waxnerd user queues every local rating in the outbox. First-party clients set, clear, and read ratings at `/api/v1/ratings`, and read the rating rows that changed at `/api/v1/ratings/changes`. The reconcile loop in `serve` pulls the owner's rating changes from wn-cloud at start and every 15 minutes, writes them into the local rating store, and sets the rounded values in Navidrome. The wn-cloud client covers sign-in, server registration, device code verification, and the rating routes. The wn-cloud API contract and an in-memory wn-cloud stub for tests are in place. v1 has one account, the owner. This file describes the agreed design and is updated as the code lands.

## Architecture

### Process model

One `wn-server` binary. It supervises a pinned `navidrome` binary, writes its configuration, and sits in front of it as a reverse proxy. Navidrome listens on the Unix socket `<data dir>/navidrome.sock` with mode 0600 and has no TCP port. Every request from the network goes through wn-server.

Navidrome trusts the `Remote-User` header from its trusted source, and that header names any Navidrome user, admins included. On a loopback port any local process could send it. The socket file is owned by the service user and has mode 0600, so only processes of that user can connect. Navidrome reports the peer of a Unix socket as `@`, and `ExtAuth.TrustedSources = '@'` trusts exactly that peer. Navidrome creates the socket first and sets its mode after. `init` creates the data directory with mode 0700, which closes that moment to other users. A data directory that existed before `init` keeps its mode.

beets runs as a subprocess and only writes into the sorted directory. A pinned beets, with its Python interpreter, ships in the release bundle together with the pinned Navidrome binary and Chromaprint's `fpcalc`, so a release works without any install step. ffmpeg is optional and only Navidrome uses it, for transcoding.

The first and only release target for now is Linux on x86_64. Other platforms come later.

```
clients (wn-mobile, wn-desktop, web UI)
        │  HTTPS + wn-server session or app password
        ▼
   wn-server ── proxy ──▶ navidrome (Unix socket, child process)
        │  ▲                   │
        │  └── beets ──▶ tags in music files ──▶ navidrome scanner
        ▼
   wn-cloud (Supabase auth, waxnerd API)
```

### Components

| Package | Responsibility |
|---|---|
| `internal/cli` | cobra command line: `init`, `serve`, `password`, `login`, `logout`, `status`, `match` with `list`, `apply`, `dismiss`, and `revert`, `version`. The `match` commands call `internal/match` |
| `internal/config` | `config.toml` loading and defaults, data directory, rendered `navidrome.toml` |
| `internal/navidrome` | child process supervisor, version pin, library sync, wn-server's own Navidrome identity, and the owner's Navidrome user. `NewTransport` dials Navidrome's socket. `serve` builds one transport and shares it between the proxy, the supervisor, and the client. The client looks up songs and albums by id and by MusicBrainz id through the native API, and sets a rating or a star through the Subsonic API, as the owner's user |
| `internal/proxy` | reverse proxy to Navidrome over its Unix socket, with the `Host` header `localhost`. It forwards `/rest/*` after checking a wn-server session (`apiKey` or `Authorization: Bearer`) or the Subsonic credentials against the app password (at most 10 failed checks per client address per minute), and `/share/*` without a check. It serves wn-server's own `/api/v1/*` routes: the device code exchange and session revocation at `/api/v1/session`, with a local rate limit on failed exchanges, and the rating routes under `/api/v1/ratings`. It passes `/` and `/ui/*` to the web UI handler, without waiting for the supervisor. Every other path answers 404. The session exchange records in a shared status whether wn-cloud answered, for the web UI's outage banner. It answers 503 on `/rest/*` and `/share/*` until the supervisor is ready, strips incoming `Remote-User`, `Authorization`, and `Cookie`, sets `Remote-User` to the account id after a successful check, removes the credential parameters before forwarding, and streams responses unbuffered. It maps the account name to the account id in the `username` parameter of `getUser` and back in the `getUser` and `getUsers` responses. It intercepts `setRating`, `star`, and `unstar`: it resolves each item to a MusicBrainz subject before forwarding, and records the rating through `internal/ratings` when Navidrome answers `ok`. It buffers only the responses of these five calls |
| `internal/wncloud` | wn-cloud client: PKCE and state, the code display sign-in, `POST /cli/token`, `POST /cli/logout`, server registration, device code verification, and the rating routes (`PUT`, `DELETE`, and `GET /ratings/:kind/:mbid`, and `GET /ratings?since=`), with typed errors per error code. `stub` serves the same routes from memory for tests |
| `internal/beets` | generated beets config, `beet import` subprocess, candidates file parser, `.ndignore` markers, scan trigger, the apply run with a chosen release, and the revert (`beet remove` and removal of the empty marker) |
| `internal/match` | the match queue actions: a full import run, applying a chosen release to an entry, dismissing an entry, and reverting an applied entry. It records the results in the store and uses `internal/beets` for the subprocesses. `Jobs` runs one of these as a background job for the web UI, at most one at a time. The `match` commands and the web UI's match page call it |
| `internal/ratings` | the rating write path: the local rating store and the outbox. The outbox drain, which sends to wn-cloud with the server credential after each write, at `serve` start, and every minute. The lookup of a Navidrome song or album to a MusicBrainz subject. The rounding helper from half stars and like level to Navidrome's stars and starred flag. The writes behind the rating API. The reconcile loop, which pulls changes from wn-cloud into the local store and Navidrome at start and every 15 minutes |
| `internal/store` | wn-server's SQLite database, `<data dir>/wn-server.db`, mode 0600, with embedded migrations. It holds the owner account, the app password, the server credential, the sessions of first-party clients and the web UI, the match queue, the local rating store with its tombstones, the cursor of the rating sync, and the rating outbox |
| `internal/web` | web UI for the owner under `/ui/`, server-rendered with `html/template` and embedded in the binary: sign-in with the account name and the app password, the `wn_session` cookie, the status page, the account page with the app password and the sessions, and the match review page |

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
8. In the transaction of step 7, wn-server makes every outbox entry that waits for a retry due at once. When the waxnerd user differs from the one of the stored registration, or no registration is stored, it also queues every rating in the local store in the outbox. The next drain of a running `serve` sends them to wn-cloud with the server credential, so they become the owner's ratings at wn-cloud. A login of the same user does not queue the ratings again. Every change made in local-only mode, a cleared rating included, is already in the outbox.

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
| 502 | `cloud_error` | wn-cloud refused the server credential (`401`, or `403 forbidden`) or gave another unexpected answer. wn-server logs an error that tells the owner to run `wn-server logout`, then `wn-server login`. |
| 503 | `cloud_unreachable` | wn-cloud timed out, could not be reached, or answered 5xx. The code is not spent, unless wn-cloud answered and the answer was lost; then the owner gets a new code. Otherwise the client can retry with the same code. |

The local rate limit is in memory. It allows at most 5 failed exchanges per client address and at most 10 failed exchanges in total in any 10 minutes. The total matches wn-cloud's limit of 10 failed verify calls per server credential per 10 minutes, so local failures alone cannot make wn-cloud lock the server credential. The client address is the TCP peer address without the port; forwarded headers are not read. Failures with `invalid_request`, `invalid_code`, and `not_owner` count, and so does an exchange whose client went away before the answer. wn-server reads and checks the body, with a 10 second read deadline, before it takes a slot in the limit. An exchange counts as a failure while it waits for wn-cloud, so parallel attempts cannot pass the limit. A success does not count, and neither do `not_linked`, `rate_limited`, `cloud_error`, and `cloud_unreachable`, which the client did not cause. Beyond the limit wn-server answers 429 without calling wn-cloud. Every failed exchange is logged at warn level with the reason and the client address, never the code. `cloud_error` and `internal` are logged at error level with their cause. While wn-cloud is unreachable, wn-server logs that at most once a minute.

`DELETE /api/v1/session` with `Authorization: Bearer <session>` revokes the caller's session and answers 204. Without a valid session it answers 401 with code `unauthorized`. Sessions are logged at info level when they are created, with the account id and the device label, and when they are revoked.

Every request after that:

1. The client sends `Authorization: Bearer <session>` or, on `/rest/*`, the OpenSubsonic `apiKey=<session>` parameter. Third-party clients send the owner's name and the app password instead.
2. wn-server checks the session or password against its own store. No call to wn-cloud. A revoked session is refused on the next request. wn-server records the time a session was last seen, at most once a minute per session.
3. wn-server forwards the request to Navidrome on its Unix socket with `Remote-User: <account id>`. Navidrome's trusted proxy settings (`ExtAuth.UserHeader`, `ExtAuth.TrustedSources = '@'`) accept the header from a socket peer. The `apiKey` parameter and the `Authorization` header are removed before forwarding.
4. The response passes back through wn-server.

On `/rest/*` wn-server follows OpenSubsonic's rules for `apiKey`, and treats a Bearer header the same way. A session together with `u`, `p`, `t`, `s`, or `jwt`, or an `apiKey` together with a Bearer header, gets Subsonic error 43. An unknown or revoked session gets error 44. Both answer with HTTP 200, as Navidrome does. A request without a session goes through the app password check.

On `getUser` the proxy rewrites a `username` that equals the account name, without regard to case, to the account id, which is the owner's Navidrome user name. An account id passes unchanged, and any other value, such as `wn-server`, gets Subsonic error 50 without a forward. In the JSON and XML responses of `getUser` and `getUsers` the proxy replaces the account id in `username` with the account name. It buffers only those two responses.

wn-server's own Subsonic error bodies carry Navidrome's exact `serverVersion`, read from `navidrome --version` at the version check. A failed Subsonic sign-in is logged with the `u` parameter, cut to 64 bytes, and the reason `missing_user` or `bad_credentials`.

wn-server limits failed password checks on `/rest/*` itself, because Navidrome limits only `POST /login`, never `/rest/*`. It allows at most 10 failed checks per client address in any minute, in memory. The client address is the TCP peer address without the port, an IPv6 address counts by its /64 prefix, and forwarded headers are not read, so all clients behind one reverse proxy share one address. Only confirmed failures count, recorded after the check: a wrong password and an unknown user name. Checks in flight do not count, so parallel requests with the correct password never lock an address out. Parallel guessing can exceed the limit by the number of checks in flight, which is acceptable because the app password has about 116 bits. A missing credential (error 10), a valid session, and a check that failed through no fault of the client, such as a store error, do not count. A successful check resets the count of the address. Before a check, wn-server refuses the request when the address already has 10 failures in the last minute. It answers Subsonic error 40 in the requested format, with HTTP 200, without checking the password, so the correct password is refused too until the failures expire. Refused requests are not counted. The limit is logged at warn level as `subsonic sign-in rate limited` with the client address, at most once a minute per address. An entry is evicted when its last failure and its last warn line are both older than a minute. The sweep runs at most once a minute, so the map stays bounded. A restart clears the counts.

wn-server's own `/api/v1/*` routes take only the Bearer header and answer 401 without a valid session. `POST /api/v1/session` needs no session. The `/api/v1/*` routes do not wait for the supervisor to report ready, because they do not reach Navidrome. The rating routes are the exception: they look up Navidrome items, so they answer 503 with code `starting` until the supervisor reports ready.

Navidrome's own web UI is blocked at the proxy. Only its Subsonic API passes through. wn-server's web UI is the only UI.

### Accounts and modes

wn-server has one account, the owner. It lives in wn-server's own SQLite store, in the `accounts` table: id (16 random hex characters), name, app password, an optional waxnerd user id, and the creation time. The name is the Subsonic user name. It defaults to `owner` and `init --name` sets it. Names compare without regard to case, as in Navidrome. Navidrome's `Remote-User` header carries the account id, and the owner's Navidrome user name is the account id. Linking to wn-cloud is a property of the account, not its identity.

wn-server runs in one of two modes. The difference is whether `wn-server login` has run.

- **Local-only.** The state before `wn-server login` has run. The owner account has no waxnerd user id. Clients authenticate with the Subsonic scheme (`u` plus `t`/`s`, or `u` plus `p`, plain or `enc:` hex) against the app password. A wrong credential gets Subsonic error 40 and a missing one gets error 10, both with HTTP 200, as from Navidrome. Ratings are stored locally, and Navidrome keeps the whole stars and the starred flag of each Subsonic call. Nothing syncs with wn-cloud, and nothing is queued.
- **Cloud.** The state after `wn-server login`. The owner account carries the waxnerd user id, and wn-server holds the server credential. The credential is one row of the `server_credential` table: the CLI token, the server id, the waxnerd user id, and the creation time. The table holds at most one row. After `logout` the row keeps the server id and the waxnerd user id, and the token is empty, which means not linked. The token lives in the store file, which has mode 0600. There is no separate token file. First-party clients exchange a device code for a session. The app password keeps working. Ratings sync with wn-cloud.

Sessions are rows of the `sessions` table: an id (16 random hex characters), the account id, the SHA-256 hash of the token in hex, the device label, the creation time, and the time the session was last seen. Revoking a session deletes its row. `logout` does not revoke sessions; they keep working in local-only mode.

Sessions are checked against wn-server's own store, so an outage of wn-cloud never refuses the owner. Sessions already issued and the app password keep working. New sessions through `POST /api/v1/session` fail until wn-cloud answers, because wn-cloud verifies the device code. Rating writes wait in the outbox. The web UI shows a banner "wn-cloud unreachable since <time>". There is no cut-off.

Third-party Subsonic clients do not use the device code. They sign in with the owner's name and the app password through the Subsonic token scheme, and wn-server checks them locally. wn-cloud is not involved.

The app password exists so that third-party Subsonic clients and the web UI work in both modes. `wn-server init` generates it. The web UI's account page shows it with copy and regenerate buttons. `wn-server password` prints it and `--reset` regenerates it, for a headless machine. The Subsonic token scheme needs the password itself, so the store holds it as-is, in a file readable only by the server's user. An app password is 20 characters from `crypto/rand`, without the characters `0 O 1 l I`. A reset takes effect on the next request; `serve` does not need a restart.

### Web UI

wn-server serves its own web UI under `/ui/`. `GET /` redirects to `/ui/`. The pages are rendered on the server with Go's `html/template` and embedded in the binary, with one stylesheet and one small script for the copy button and the confirmation prompt. Only the owner signs in, with the account name and the app password, in both modes. The browser does not sign in at wn-cloud. The web UI works before Navidrome is ready.

Pages:

- `/ui/login`: the sign-in form. The account name compares without regard to case, and the app password is compared in constant time, as on `/rest/*`.
- `/ui/`: the status page. It shows the server's host name, the owner account name, the Navidrome version, Navidrome's scan state from `getScanStatus` ("Navidrome starting" until the supervisor reports ready), and whether the owner account is linked to a waxnerd user. When it is not linked, the page says to run `wn-server login`. wn-server does not store the server name that `login --name` sent to wn-cloud, so the page shows the host name, which is the default of `--name`.
- `/ui/account`: the app password with a copy button and a regenerate button, and the list of sessions with device label, creation time, last seen time, and a "this browser" marker. Each session has a revoke button. Regenerating has the effect of `wn-server password --reset`.
- `/ui/match`: the match review page, described below.

The status page also shows the number of pending match queue entries, with a link to the match page, and says when a match job is running.

#### Match review page

`GET /ui/match` lists the pending entries of the [match queue](#match-queue). Each entry shows its folder, its tags (artist, album, year, track count), and the time it was first seen, with its candidates below: artist, album, year, label, distance, a link to the release at `https://musicbrainz.org/release/<release id>` that opens in a new tab with `rel="noopener"`, and an Apply button. A Dismiss button sits on the entry. An entry whose last action failed shows the message from `last_error`. `?all=1` adds the other states: applied entries with the applied release, the folder in `sorted_dir`, and a Revert button (with a confirmation prompt); dismissed entries with their candidates and Apply buttons, because a dismissed entry can be applied; and duplicates, each with a link to the release it duplicates. A link switches between the two views, and the buttons keep the view.

The buttons post to `POST /ui/match/<id>/apply` (form field `release`, the candidate's release id), `POST /ui/match/<id>/dismiss`, `POST /ui/match/<id>/revert`, and `POST /ui/match/run` (the "Run import" button, a full `wn-server match` run). They redirect back to the match page. A run or an apply takes as long as the beets import, so they run as background jobs in the `serve` process, through the same `internal/match` code as the commands. Dismiss changes only the queue and runs in the request.

At most one job runs at a time. While one runs, the page shows which job and since when, disables every button, and reloads itself every 5 seconds with a `<meta http-equiv="refresh">` tag, so it needs no script. A POST that passes the state checks while a job runs answers 409. When the job has finished and no job runs, the next load of the page shows its result once: the summary line, or the error. A request that the entry's state refuses, such as applying an applied entry, answers 400 with the page and the reason, and an unknown entry id answers 404.

The commands and the web jobs share the SQLite store and the beets lock. A `wn-server match` command that runs while a web job holds the lock fails with `another beets run is in progress`. A web job that finds the lock held by a command fails at once with the same message, which the page shows as the job's result. A job gets the context of `serve`, so a shutdown stops beets as Ctrl-C does, and `serve` waits up to 30 seconds for the job to record its albums before it exits. The log has `web match <run|apply|revert> started` with `account` and `entry` (no entry on a run), and `web match dismissed`, at info level.

A sign-in creates a wn-server session in the `sessions` table, of the same kind that `POST /api/v1/session` issues, with the device label `web <user agent>`, cut to 60 characters of the user agent. The browser holds the session token in the cookie `wn_session` with `HttpOnly`, `SameSite=Lax`, and `Path=/ui/`, so the browser does not send it on `/rest/*` and `/share/*`. The proxy also removes every `Cookie` header before forwarding to Navidrome. The cookie expires after 400 days; the session row stays until it is revoked. `Secure` is set when the request came over TLS. wn-server reads no forwarded headers, so `Secure` is set only when TLS terminates at wn-server. A sign-in from a browser that already holds a session cookie revokes that session. Every page except the sign-in page needs the cookie of a valid session and redirects to `/ui/login` without one. wn-server records the session's last seen time at most once a minute. `POST /ui/logout` revokes the session and clears the cookie. Revoking the browser's own session on the account page does the same.

Routes that change state take POST only: `/ui/login`, `/ui/logout`, `/ui/account/password/reset`, `/ui/sessions/<id>/revoke`, and the match routes `/ui/match/run` and `/ui/match/<id>/apply`, `dismiss`, and `revert`. A POST whose `Origin` header names another host than the request's `Host` answers 403. A POST without `Origin` passes. A reverse proxy in front of wn-server must pass the original `Host` header through, because the Origin check compares against it. Every response carries a Content Security Policy that allows only the embedded stylesheet and script and forbids framing. Pages are sent with `Cache-Control: no-store`.

A failed sign-in answers 401 with the form and a message, and is logged at warn level as `web sign-in failed` with the client address and `reason=bad_credentials`. At most 10 failed sign-ins per client address and 100 in total count in any 10 minutes, in memory. Beyond that the form answers 429 without checking the password. The client address is the TCP peer address without the port. An IPv6 address counts by its /64 prefix. Sign-in, sign-out, password reset, and session revocation are logged at info level with the account id and the session id, never the token or the password.

While wn-cloud is unreachable, every signed-in page shows the banner "wn-cloud unreachable since <time>". The session exchange at `POST /api/v1/session` and the rating outbox drain set the banner when wn-cloud does not answer and clear it when wn-cloud answers. The status page does not yet tell the owner to run `wn-server login` again when wn-cloud refuses the server credential.

Later, waxnerd relay (a paid wn-cloud feature) lets authorized users reach a wn-server from the public internet. wn-cloud redirects the user to the server, and the server's own web UI takes over. The web UI is built so that this needs no change on the server beyond being reachable. Listeners, invites, and listener sign-in return after relay is specified.

### Ratings and favorites

wn-cloud is the source of truth for the owner's ratings. Ratings use waxnerd's 10-point scale (5 stars with half stars). Releases and tracks also carry a favorite mark. Likes and comments may follow on the same model.

Navidrome's own rating is an integer from 0 to 5 and cannot hold half stars, so it is not the source of truth. Navidrome holds a rounded mirror: half stars round up to whole stars, and `love` maps to starred. `ratings.Mirror` computes it. Two write paths share one code path in wn-server, `internal/ratings`:

- The proxy intercepts the Subsonic `setRating`, `star`, and `unstar` calls, with and without `.view`, after the credential check. Whole stars only, because Subsonic carries integers. The write order is:
  1. The proxy resolves each item to a MusicBrainz subject through Navidrome's native API on its socket, as the account: `GET /api/song/:id` first, then `GET /api/album/:id` when Navidrome answers 404. Subsonic's `getSong` carries only the recording id, not the release track id, so the native API is used. `setRating` takes `id`. `star` and `unstar` take `id` and `albumId`; `artistId` is forwarded and not synced. The results are kept for the one request only.
  2. The proxy forwards the original call to Navidrome, so its stars and starred flag hold the value.
  3. When Navidrome answers `status ok`, wn-server writes the local rating store and queues the value in the outbox. The drain sends it to wn-cloud with the server credential within a second. The client does not wait for wn-cloud. When Navidrome answers a Subsonic error, the client gets the error and nothing is written. The client always gets Navidrome's answer.
- `PUT` and `DELETE` on `/api/v1/ratings/{subsonicId}` take half stars and a like level from first-party clients (see Rating API below). They write the local store and the outbox, then set Navidrome's rounded rating and starred flag through the Subsonic API on Navidrome's socket, as the account. The drain sends the value to wn-cloud, as for the Subsonic calls. First-party clients never call wn-cloud's rating routes directly.

The Subsonic calls change one value and keep the other. `setRating` with 1 to 5 sets twice as many half stars and keeps the like level. `setRating` with 0 clears the stars and keeps the like level. A rating outside 0 to 5 is forwarded and not synced. `star` sets the like level to `love` and keeps the stars. `unstar` clears the like level and keeps the stars. A rating with neither value is cleared: the local row becomes a tombstone, and wn-cloud gets `DELETE /ratings/:kind/:mbid`. Every write to wn-cloud sends both values from the local store. Because `PUT` replaces both fields, a local change overwrites both fields at wn-cloud. A value set at waxnerd.com since the last reconcile run is overwritten by a local change of the same subject.

An item without a MusicBrainz id, or an id that is neither a song nor an album (an artist or a playlist), is forwarded unchanged and not synced. That is logged at debug level. When Navidrome cannot be asked, the call is forwarded unchanged and wn-server logs a warning.

Every rating write goes to the outbox, the `rating_outbox` table, due at once. The outbox holds one entry per subject, and a later change replaces the values of the earlier entry. In local-only mode the entries wait in the outbox, and nothing is sent. `serve` drains the outbox with the server credential, oldest entry first. It drains at start, every minute, and after each write while the shared status does not record wn-cloud as unreachable:

- A 2xx answer removes the entry.
- An unreachable wn-cloud (a timeout, a connection error, or a 5xx status) sets the entry's next attempt after 1, 2, 4, and so on minutes, at most one hour, and stops the drain. wn-server logs that at most once a minute, and the web UI shows the outage banner.
- A 401 keeps the entry with the same wait, stops the drain, and logs an error that tells the owner to run `wn-server logout`, then `wn-server login`. `login` makes the entry due at once.
- A 429 keeps the entry with the same wait and stops the drain. Any other unexpected answer keeps the entry with the same wait, and the drain goes on.
- A 400 `invalid_request` and any 403 keep the entry with `last_error` set, out of the retry loop, and log a warning. A later change of the subject replaces it.

Store writes run one at a time. The drain does not hold the lock while it calls wn-cloud. Each entry carries a version that grows with every change. The drain removes or delays an entry only when its version did not change during the send, so a newer write of the same subject is sent next.

#### Rating API

The routes under `/api/v1/ratings` take `Authorization: Bearer <session>` and answer errors as `{ "error": "<message>", "code": "<code>" }`. Without a valid session they answer 401 `unauthorized`. Until Navidrome is ready they answer 503 `starting`. A Subsonic id is a song or an album of the owner's Navidrome library. wn-server resolves it as the account: `GET /api/song/:id` first, then `GET /api/album/:id`, the same lookup as the Subsonic intercept. A song is a `track` subject and an album is a `release` subject.

`PUT /api/v1/ratings/{subsonicId}` with `{ "half_stars": 1..10 or null, "like_level": "like" or "love" or null }`. A field that is absent keeps its current local value. A field that is `null` clears it. At least one field must be present. The answer is 200 with the stored rating, `{ "kind", "mbid", "half_stars", "like_level", "updated_at" }`. A rating that ends with neither value is cleared. wn-server writes the local store and the outbox, then sets Navidrome's `setRating` to the rounded stars and `star` or `unstar` for `love`, on the item of the request. When Navidrome refuses that call, the rating stays stored and queued, wn-server logs a warning, and the client still gets 200.

`DELETE /api/v1/ratings/{subsonicId}` clears both values, sets Navidrome to rating 0 and unstarred, and answers 204.

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | The body is not a JSON object with `half_stars` and `like_level` only, a value is out of range, or no field is present. |
| 404 | `not_found` | The id is neither a song nor an album. |
| 422 | `no_mbid` | The item has no MusicBrainz id, so its rating is not synced. |
| 502 | `navidrome_error` | Navidrome could not be asked. |

`GET /api/v1/ratings?ids=<id>,<id>,...` answers `{ "ratings": { "<subsonicId>": { "kind", "mbid", "half_stars", "like_level", "updated_at" } } }` for the ids that resolve and have a stored rating. Other ids are left out. At most 200 ids per call; more, or none, answer 400 `invalid_request`.

`GET /api/v1/ratings/changes?since=<cursor>&limit=<n>` answers `{ "changes": [ { "kind", "mbid", "half_stars", "like_level", "deleted", "updated_at" } ], "next": "<cursor>" }` with the stored rating rows that changed after the cursor, oldest first. `limit` is 1 to 500 and defaults to 200. A client passes `next` as `since` on its next call and stops when `changes` is empty. `next` equals `since` when nothing changed. Without `since` the answer starts from the beginning and leaves out deletions. A cleared rating comes as an entry with `deleted: true` and both values `null`. A malformed `since` answers 400 `invalid_request`. The cursor is opaque to the client. It holds the time and id of the last row that the local store wrote.

#### Reconcile loop

Changes made at wn-cloud, for example on waxnerd.com, reach wn-server through the reconcile loop. `serve` runs it once Navidrome is ready and then every 15 minutes. Each run drains the outbox first, so that a pending local change reaches wn-cloud before the pull. The run is skipped when no server credential is stored. In that state `serve` logs once at start, at info level, that it is not linked to wn-cloud and that rating writes wait in the outbox until `wn-server login`, and logs nothing about the drain or the reconcile loop on later ticks. With the server credential it calls `GET /ratings?since=<stored cursor>`, follows `more` until it is `false`, and stores `next` after each page. The cursor is kept in the `rating_sync` table. A login of another waxnerd user removes it, and the next run reads every rating.

For each change:

- An outbox entry of the same subject that was created after the change's `updated_at` wins. The local values and the entry stay, and the change is skipped. An older entry is removed, because the change is newer than the local write.
- Otherwise the change overwrites the local values, and a deletion turns the local row into a tombstone. The local row takes `updated_at` from wn-cloud. A change from wn-cloud is never put into the outbox.
- When the change altered the local store, wn-server sets the rounded values in Navidrome: stars from the half stars rounded up, and starred for `love`, or rating 0 and unstarred for a deletion. It finds the Navidrome items through the native API as the account: albums by `mbz_album_id` for a `release`, and songs by `mbz_release_track_id` for a `track`. Every item found gets the values. A subject that the library does not hold is stored and skipped. A change that the store already held, such as the echo of a local write, is pushed again, so that a later run repairs a mirror that an earlier push or a library scan left out of date.
- A Navidrome failure is logged at warn level and counted. It does not stop the run. The mirror is repaired on the next run that sees the subject again.

Each run logs at info level the counts of pages, changes pulled, applied, unchanged, kept local, unreadable, Navidrome items set, subjects not in the library, and failed Navidrome calls. An unreachable wn-cloud updates the shared status as for the drain. A 401 or 403 for the server credential is logged at error level with the advice to run `wn-server logout`, then `wn-server login`, and the shared status records it. The web UI then shows the banner "server credential rejected by wn-cloud". The next successful run clears it.

Navidrome's smart playlists and third-party clients see the rounded stars. First-party clients read the full values from `GET /api/v1/ratings` and `GET /api/v1/ratings/changes`.

Later direction, not in v1: smart playlists defined in wn-server on the 10-point scale, evaluated against wn-server's rating store and Navidrome's library data, and written into Navidrome as ordinary playlists so every client can play them.

Sync only applies to MusicBrainz-matched music. wn-cloud's rating model (`ratings` in `../v2`) keys a rating by subject kind and MBID: `release_group`, `release`, `track`, or `artist`. A rating holds `half_stars` from 1 to 10 and a `like_level` of like or love. Navidrome albums map to `release` subjects through `mbzAlbumId`. Navidrome songs map to `track` subjects through `mbzReleaseTrackId`, which is the MusicBrainz track id, not the recording id. Artists are not synced. A Navidrome star maps to `love`. wn-cloud has the tables but no JSON routes for ratings yet; those are a wn-cloud item (WAX-942), specified in [docs/wn-cloud-api.md](docs/wn-cloud-api.md).

The local rating store is the `ratings` table: the account id, the kind, the MBID, `half_stars` (1 to 10 or null), `like_level` (`like`, `love`, or null), `updated_at`, `changed_at`, and a `deleted` flag. A row that is not deleted holds at least one value. Clearing a rating keeps the row as a tombstone with `deleted` set and both values null, so that the change feed can report the deletion. `updated_at` is the time of the change as its author made it: the local write, or the `updated_at` that wn-cloud reported. `changed_at` is the time this database wrote the row. The change feed orders by `changed_at` and the row id, so a change that came from wn-cloud with an old `updated_at` still comes after the rows that a client has already read. Tombstones are not purged. The outbox entry holds the kind, the MBID, both values, a deleted flag, the creation time, the number of attempts, the time of the next attempt (null for an entry that is not retried), the last error, and the version.

### MusicBrainz matching

wn-server runs `beet import` in copy mode: `copy: yes`, `move: no`, `write: yes`. beets copies each matched album into the sorted directory (`sorted_dir` in the config) and writes the MusicBrainz ids into the copy. The original files are never modified or moved. Navidrome scans both the original folders and the sorted directory. After a confident copy, wn-server writes an empty `.ndignore` file into the original album folder, which Navidrome's scanner skips, so the album is not shown twice. It writes the same marker into the folder of every other original copy of an applied release, which beets reports as a duplicate, so those copies are not shown either. Deleting a marker brings the original back. These markers are the only write to the originals tree. beets itself writes nothing there, because `beets.yaml` turns off every beets option that writes next to an original, and the `wn_candidates` plugin turns off beets' `fix_extension` (see [Configuration](#configuration)). `sorted_dir` must not be inside a music folder or contain one.

Matched music therefore exists twice on disk. A move mode or a prune command can reclaim the space later, once the matching is trusted. Not in v1. There is no coupling to Navidrome's database.

In quiet mode beets applies confident matches and skips the rest. A confident match is one below beets' default `match.strong_rec_thresh` of 0.04, which wn-server does not change. The `wn_candidates` plugin adds one rule. It applies the best candidate when every file in the album carries the same non-empty `MusicBrainz Album Id`, that id is the best candidate's release id, and the candidate's distance is at most beets' `match.medium_rec_thresh` of 0.25. An album whose files carry a different id, no id, or ids that differ between files goes to beets' own decision, and so does an album whose best candidate is farther than 0.25. Those albums are skipped and wait in the match queue. There is no second beets pass and no parsing of the import log. The generated beets config sets `import.incremental` and `incremental_skip_later`, so repeated runs skip folders already imported and re-offer skipped albums. `duplicate_action` is `skip`. An album whose match is already in the beets library is skipped, is not recorded in the incremental history, and is offered again on every run. Each run looks it up at MusicBrainz again.

`wn-server match` runs one import over every `music_folders` entry in the foreground:

1. It takes an exclusive lock on `<data dir>/beets.lock`, writes `<data dir>/beets.yaml` (mode 0600), and empties `<data dir>/beets-candidates.jsonl`. `match`, `match apply`, and `match revert` take the same lock, also when the web UI starts them, because they share `beets.yaml`, the candidates file, and `beets.db`. When another run holds the lock, the command fails with `another beets run is in progress`.
2. It runs `<beets_bin> --config <data dir>/beets.yaml import -q <music folders>`. The directory of `fpcalc_bin` goes first on `PATH`, `FPCALC` names `fpcalc_bin`, and `BEETSDIR` is the data directory, so a personal beets config in `~/.config/beets` is not read. `PYTHONDONTWRITEBYTECODE=1` keeps Python from writing `__pycache__` into `beets-plugins`. `HOME` is unchanged. beets also reads `<data dir>/config.yaml` because of `BEETSDIR`. wn-server does not write that file. beets' stderr goes to wn-server's log at info level, and its stdout at debug level. Ctrl-C kills beets and its process group. wn-server then still reads the candidates file and writes the markers for the albums that beets applied before the kill, because beets has already recorded those albums in its incremental history. It starts no scan after a kill.
3. It reads the candidates file.
4. It writes an empty `.ndignore` into the original folder of every applied album, with mode 0644, when the file does not exist. It writes no marker into a folder that is itself a `music_folders` entry, because that would hide the whole library. An empty `.ndignore` hides everything below its folder. So it writes no marker, and logs a warning, when another album of the same run that was not applied has its folder below the applied folder. The folder and the music folders are compared after symlinks are resolved. The markers are the only write to the originals tree. Step 7 writes the markers of duplicates.
5. When at least one album was applied, it asks Navidrome to scan, as wn-server's identity, with `GET /rest/startScan`. Navidrome's native API has no scan route. When Navidrome is not running, wn-server logs a warning and the command still succeeds.
6. It records every line of the candidates file in the match queue (see [Match queue](#match-queue)). After a Ctrl-C it records the lines that beets wrote before the kill.
7. It writes an empty `.ndignore` into the folder of every `duplicate` entry whose `duplicate_of_release_id` is the `applied_release_id` of an `applied` entry, with the rules of step 4: not into a music folder itself, not over an existing file, and not when another album of the same run that was not applied has its folder below the duplicate folder. It considers the duplicate lines of this run, and the duplicates of earlier runs whose release this run applied. It sets `marker_written` on the duplicate entry when it created the file. A marker that already existed leaves it at 0, so wn-server never removes it. It marks only duplicates whose lines were recorded, so it also runs after a Ctrl-C. It asks Navidrome to scan when it wrote a marker, except after a Ctrl-C and when step 5 already scanned. A duplicate of a release that no entry has applied gets no marker, and so does a duplicate whose library album is another release than the one on its line. The rule also holds back a marker when an entry from an earlier run that is not applied has its folder below the duplicate folder.
8. It prints `applied N, skipped N, duplicates N, errors N`, then one line per skipped album with its folder and best candidate, then the number of pending entries in the match queue. A run that the lock refused prints neither.

The beets plugin `wn_candidates` ships in the bundle as `beets-plugins/wn_candidates.py`. It writes one JSON line per album task to `<data dir>/beets-candidates.jsonl`, with these keys:

| Key | Contents |
|---|---|
| `folder` | Absolute path of the album folder. For an album that beets collapsed from several folders, such as `CD1` and `CD2`, it is their common parent. |
| `tags` | The existing tags: `artist`, `album`, `year`, `track_count`. |
| `decision` | `applied`, `skipped`, `duplicate` (the match is already in the beets library, and beets skips it in quiet mode), or `error`. |
| `candidates` | Up to 10 candidates in beets' order, each with `release_id` (MusicBrainz release MBID), `artist`, `album`, `year`, `label`, and `distance`. |
| `copied_to` | The album folder in `sorted_dir`. Only on `applied` lines. |
| `release_id` | The MusicBrainz release that beets applied. On a `duplicate` line, the release that beets chose for the folder being imported. beets decides duplicates by album artist and album, so the library album can be another release. Only on `applied` and `duplicate` lines. |
| `reason` | On an `applied` line whose match the plugin chose. `requested_release_id` for a `match apply` run, where the plugin chose the requested release. `existing_release_id` for a `match` run, where every file already carried the best candidate's release id and its distance was at most `match.medium_rec_thresh`. |
| `error` | The message. Only on `error` lines. |

A line with `decision` `forgotten` is not an album. It has only `decision` and `folder`, and confirms that the plugin removed a folder of `forget_folders` from the incremental history. wn-server does not count it as a queue entry or as a malformed line.

The plugin records the candidates on the `import_task_choice` event, writes `applied` lines on `album_imported`, and writes the remaining tasks when the `import` event ends the session. beets 2.14 decides duplicates after `import_task_choice` and sends no event for an album it skips as a duplicate, so those lines are written at the end of the run. The plugin never raises out of a hook. An internal error becomes an `error` line. wn-server skips lines that do not parse and counts them as errors.

The plugin registers an `import_begin` handler, which runs before beets' import pipeline reads the incremental history. For each folder in `wn_candidates.forget_folders` it removes the history entries whose album folder equals the folder, using beets' `ImportState` (the pickled state file: a set of tuples of byte paths), and saves the state file. The album folder of an entry is computed as for the `folder` key of the candidates file: the single directory, or the common parent of several, so a multi-disc album is removed by its parent folder. History entries for album folders below the listed folder stay. The handler then reads the state file again, because `ImportState` ignores a write error, and writes a `forgotten` line to the candidates file for each folder that is gone. It logs one line per folder, and a warning when a folder was not in the history. wn-server lists the folders of reverted entries there. See [Match queue](#match-queue).

The plugin also registers an `import_task_before_choice` handler. When `wn_candidates.apply_release_id` is set in `beets.yaml`, which wn-server does for a `match apply` run only, it applies the requested release. Otherwise it applies the best candidate under the existing release id rule above, and returns nothing for every other album. See [Match queue](#match-queue).

The `chroma` plugin (AcoustID fingerprints) raises match confidence. The AcoustID application key is compiled in with `-ldflags "-X github.com/waxnerdhq/wn-server/internal/beets.DefaultAcoustIDKey=..."`. `make build`, `make bundle`, and `bundle/build.sh` take it from `WN_ACOUSTID_KEY` in the environment, which is empty by default. `acoustid_api_key` in `config.toml` overrides it. beets has no option for the lookup key: the chroma plugin looks up fingerprints with its module constant `beetsplug.chroma.API_KEY`, and `acoustid.apikey` is the user key for `beet submit`. wn-server therefore writes the key to `wn_candidates.acoustid_app_key` in `beets.yaml`, and the `wn_candidates` plugin sets `beetsplug.chroma.API_KEY` from it when it loads. When both keys are empty, `beets.yaml` has no `acoustid_app_key` and the chroma plugin uses its own key.

#### Match queue

The match queue is two tables in the store. `match_queue` has one row per album folder: `id`, `folder` (unique), `first_seen`, `last_seen`, `state`, the existing tags (`artist`, `album`, `year`, `track_count`), `applied_release_id`, `copied_to`, `applied_at`, `marker_written`, `duplicate_of_release_id`, `forget_pending`, and `last_error`. `forget_pending` is 1 on a reverted entry until the plugin has confirmed that it forgot the folder, or the entry is applied again. `marker_written` is 1 when wn-server created the `.ndignore` in the folder, for an applied entry or for a duplicate entry of an applied release. A marker that already existed leaves it at 0. `duplicate_of_release_id` is the release id on the latest `duplicate` line of the entry. It is NULL on an entry that never had a duplicate line with a release id, and wn-server clears it when the entry becomes `applied`. `state` is `pending`, `applied`, `dismissed`, or `duplicate`. `match_candidates` holds the candidates of the latest line for the folder, in beets' order. Its rows are deleted with their queue row.

After every `wn-server match` run, wn-server records each line of the candidates file:

| Line | Effect on the queue |
|---|---|
| `skipped` | Creates a `pending` entry, or sets the entry to `pending`. A `dismissed` entry stays `dismissed`. An `applied` entry stays `applied`. The tags and candidates are replaced. |
| `applied` | Sets `applied` with the release id, `copied_to`, and `applied_at`. The tags and candidates are replaced. |
| `duplicate` | Sets `duplicate` and `duplicate_of_release_id`. An `applied` entry stays `applied`. The tags and candidates are replaced. |
| `error` | Creates a `pending` entry or keeps the state of the entry. The message goes to `last_error`. The tags and candidates stay. |

Every line sets `last_seen`. Every line except an `error` line clears `last_error`. A line without a folder is logged and not recorded. An `applied` entry changes state only through an `applied` line or `match revert`, so its copy can always be reverted.

`wn-server match list` prints the pending entries with id, state, folder, tags, and the top candidate with its distance. `--all` prints the entries in every state. A `duplicate` entry shows the release it duplicates in the state column. `--json` prints the entries with all their candidates as a JSON array.

`wn-server match apply <id> [--release <mbid>]` tags and copies the album of a `pending` or `dismissed` entry. The release is the entry's first candidate unless `--release` names another. wn-server refuses an entry in another state and a release id that is not a UUID. The run has these steps:

1. wn-server writes `beets.yaml` with `wn_candidates.apply_release_id` set to the release and `wn_candidates.apply_folder` set to the entry's folder, and empties the candidates file.
2. It runs `<beets_bin> --config <data dir>/beets.yaml import -q -I -S <mbid> <folder>` with the same environment as `match`. `-S` limits the MusicBrainz lookup to that release. `-I` (`--noincremental`) turns off the incremental history check, so beets imports a folder that its history already lists, for example after a revert.
3. beets' quiet mode applies only a strong match, also with `-S`. The plugin's `import_task_before_choice` handler therefore returns the candidate whose release id is `apply_release_id`. beets' `choose_match` takes a returned match as the decision before its own quiet-mode judgment. The handler returns SKIP when no candidate has that id. It also returns SKIP for every other album below the folder, whose folder is not `apply_folder`. The `applied` line gets `"reason": "requested_release_id"`.
4. `-I` also turns off the history writes. The plugin therefore adds the applied folder to the incremental history itself, as a quiet run does for an album it applies.
5. wn-server uses the entry's own line only when its decision is `applied`. It then sets the entry to `applied` with the release id, `copied_to`, `applied_at`, and `marker_written`, sets `last_seen`, and clears `last_error`, without the upsert that a `match` run uses. The tags and candidates stay, because with `-S` the line holds only the requested release. For any other decision, wn-server stores the reason in `last_error` and sets `last_seen`, and the state, tags, and candidates stay. Lines for other albums below the folder are not recorded.
6. It writes `.ndignore` into the original folder with the rules of `match`, and into the folders of earlier `duplicate` entries of the applied release that have no marker. It asks Navidrome to scan and writes `beets.yaml` again without the apply options.

The command fails when beets did not apply the release, and names the decision of the line.

`wn-server match dismiss <id>` sets an entry to `dismissed`. Nothing on disk changes. wn-server refuses an `applied` entry, which is reverted first. A later run that skips the folder again keeps the entry `dismissed` and refreshes its candidates and `last_seen`. A `dismissed` entry can still be applied.

`wn-server match revert <id>` undoes an `applied` entry:

1. wn-server checks that `copied_to` resolves to a path strictly inside `sorted_dir`, with symlinks resolved in both, and refuses otherwise.
2. It runs `<beets_bin> --config <data dir>/beets.yaml remove -a -d -f path:<copied_to>`. beets removes the album from its library, deletes the files of the copy, and removes the folders that become empty. When beets fails and `copied_to` no longer exists, wn-server logs a warning and continues.
3. It deletes `<folder>/.ndignore` only when wn-server wrote it (`marker_written` is 1), when it is an empty regular file, and when the folder is strictly inside a music folder, compared after symlinks are resolved. A marker with content stays.
4. It removes the `.ndignore` from every entry whose `duplicate_of_release_id` is the entry's release id, that is not applied, and whose `marker_written` is 1, with the rules of step 3, and clears their `marker_written`. Those entries keep the state `duplicate`. It logs each removal at info level. A marker that the owner made, which has `marker_written` 0, stays.
5. It sets the entry to `pending`, clears the applied fields, and sets `forget_pending` (see below).
6. It asks Navidrome to scan.

beets' incremental history still lists a reverted folder after step 5, so a full `match` run would skip it. Step 5 therefore also sets `forget_pending` on the entry. The next full `wn-server match` run (from the CLI or the web UI) reads the folders of all entries with `forget_pending` and writes them to `wn_candidates.forget_folders` in `beets.yaml`. At the start of the import, the `wn_candidates` plugin removes each of those folders from the incremental history and saves the state file, and logs one line per folder. beets then offers the folder again, and the existing rule for a skipped line replaces the entry's candidates with fresh ones. When beets applies the folder again, the entry becomes `applied`. Otherwise it stays `pending`. After the run, wn-server clears `forget_pending` only for the entries whose folder has a `forgotten` line in the candidates file, also when the run failed afterwards. beets can stop before the plugin runs (an import path that does not exist, a bad `beets.yaml`, a library or plugin error), and an unwritable state file also gives no `forgotten` line. The flag then stays and the next run tries again. A folder that was not in the history gets the line too, with a warning in the log. Applying the entry again (`match apply`, or a full run that applies it) clears the flag, and wn-server does not list an `applied` entry in `forget_folders`, so a reverted and re-applied folder stays in the history. `match apply` runs with `-I` and does not need the history to change.

`match apply` and `match revert` log the entry id, the folder, and the release id at info level. Neither changes, moves, or deletes anything in an originals folder except the empty `.ndignore` markers.

### Decisions

- Navidrome runs as a separate process, not as an imported Go library and not as a Navidrome Wasm plugin. A library would tie wn-server to Navidrome's internal wiring. A plugin cannot hook auth or run beets.
- Navidrome listens on a Unix socket with mode 0600, not on a loopback port. On a loopback port any local user could send `Remote-User` with an admin's name and act as that admin. `wn-server match` reaches Navidrome through the same socket, so it runs as the service user. (WAX-952)
- beets runs as a subprocess. It has no embedding story outside Python.
- beets copies into a sorted directory and never modifies or moves the original files. Originals that were copied are hidden from Navidrome with a `.ndignore` marker, and so are other original copies of an applied release, which beets reports as duplicates. A revert of the applied entry removes the markers of its duplicates. (2026-10-16)
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
- wn-server allows at most 5 failed device code exchanges per client address and 10 in total per 10 minutes, in memory. The total equals wn-cloud's limit per server credential, so wn-server refuses before wn-cloud locks the credential. A restart clears the counts. (WAX-935)
- A wn-server session token is `wns_` followed by 32 bytes from `crypto/rand` in base64url without padding. The prefix tells it apart from wn-cloud's `wn_` CLI tokens. The store keeps only its SHA-256 hash. (WAX-935)
- The web UI holds a wn-server session in the cookie `wn_session` (`HttpOnly`, `SameSite=Lax`, `Path=/ui/`, `Secure` over TLS). The cookie expires after 400 days; the session row stays until it is revoked. State changes are POST only, and a POST whose `Origin` names another host answers 403. There is no CSRF token. (WAX-937)
- The server credential is stored in the SQLite store, not in a separate token file. `wn-server login` refuses to replace a stored credential. The owner runs `wn-server logout` first. Logout keeps the server id. (WAX-934)
- Half stars are written through wn-server's own rating API, not through Subsonic. (WAX-945, decision 3)
- A Subsonic rating call is forwarded to Navidrome first. wn-server writes the local rating store and the outbox only after Navidrome answers `ok`. A call that Navidrome refuses changes nothing, so the local store and wn-cloud never hold a value that Navidrome rejected. (WAX-943)
- wn-server pulls rating changes from wn-cloud every 15 minutes, and at `serve` start. The earlier design said 5 minutes. (WAX-944)
- A cleared rating stays in the local store as a tombstone, and the change feed for first-party clients orders by the time this database wrote the row, not by the rating's `updated_at`. (WAX-944)
- A rating change from wn-cloud does not go into the outbox. An outbox entry that is newer than the change wins, and an older one is dropped. (WAX-944)
- A failed Navidrome call during the reconcile is logged and counted, and the mirror is repaired on the next run that sees the subject again. Navidrome holds a rounded mirror, and wn-cloud and the local store hold the full values. (WAX-944)
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

5. In a browser, open `http://<host>:4533/` and sign in with the owner name and the app password. The web UI shows the status page and the account page.

Navidrome's web UI is not reachable through wn-server. Only the Subsonic API (`/rest/*`) and public shares (`/share/*`) are, next to wn-server's own `/api/v1/*` routes and web UI.

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

beets is packaged with pex as a scie (`--scie eager`). The executable contains a python-build-standalone interpreter, so the target machine needs no Python. pex builds from the lock file `bundle/beets.lock`, so the output does not depend on the build host's Python or on what PyPI serves at build time. PyInstaller was not chosen because it freezes the build host's interpreter and needs per-package hooks. On its first run the `beets` executable unpacks itself into `~/.cache/nce`, or into `$SCIE_BASE` when that is set.

`bundle/build.sh <output dir>` builds the tarball and a `.sha256` file next to it. It runs on Linux and needs `bash`, GNU `tar`, `curl`, `sha256sum`, `install`, `grep`, `mktemp`, Go, and `uv`. uv supplies the Python that pex runs under. On Linux x86_64 the script runs each binary once to check its version, and checks that beets loads the `chroma` plugin. `make bundle` calls the script with `dist` as the output directory.

#### beets lock and interpreter check

`bundle/beets.lock` is a pex lock file. It pins beets and every transitive dependency to a version with the sha256 of each artifact, so a rebuild installs the same packages. It is a universal lock for CPython 3.13 on Linux (`--style universal --target-system linux`), so one lock made on any host works for the Linux x86_64 build. `build.sh` passes it to pex with `--lock`. pex fails the build when the requirements in `build.sh` or the Python version that pex runs under are not covered by the lock, which is how CI catches a lock that disagrees with `bundle/versions.env`.

Recreate the lock whenever `BEETS_VERSION`, `PYTHON_VERSION` (its major and minor version), or `PEX_VERSION` changes, or when the requirements in `build.sh` change. Run it from the repository root with the values from `bundle/versions.env`, and commit the result:

```
source bundle/versions.env
uvx --python "$PYTHON_VERSION" --python-preference only-managed \
    --from "pex==$PEX_VERSION" pex3 lock create \
    "beets[chroma]==$BEETS_VERSION" requests \
    --style universal --target-system linux \
    --interpreter-constraint "CPython==${PYTHON_VERSION%.*}.*" \
    --indent 2 -o bundle/beets.lock
```

Recreating the lock also moves transitive dependencies to the newest versions that PyPI serves at that time. Review the diff of the lock before committing it.

pex has no option to check the python-build-standalone archive that it downloads. `build.sh` therefore reads the scie's manifest after the build (`SCIE=inspect beets`), which records the sha256 of the embedded archive, and compares it with the pin in `bundle/checksums`. The scie launcher checks the archive against the same hash when it unpacks. A build fails when the two differ. The check runs on Linux x86_64 only, because reading the manifest runs the scie. The pinned file is `cpython-<PYTHON_VERSION>+<PBS_RELEASE>-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz`, and its sha256 is on the python-build-standalone release page, in the release's `SHA256SUMS` file. When `PYTHON_VERSION` or `PBS_RELEASE` changes, replace the line in `bundle/checksums` with the entry from `SHA256SUMS`. A build fails with a message if the line is missing.

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
| `navidrome_port` | none | Ignored. Earlier versions of `init` wrote it, and it is still accepted so those files load. Navidrome listens on `<data dir>/navidrome.sock`. |
| `sorted_dir` | `<data dir>/sorted` | Directory beets copies matched albums into. Created on load. Must not be inside a `music_folders` entry or contain one. Paths are compared after symlinks are resolved. |
| `acoustid_api_key` | empty | Overrides the compiled-in AcoustID key. |
| `navidrome_external_services` | `true` | Sets Navidrome's `EnableExternalServices`. `false` turns off Navidrome's artist and album lookups (Deezer, Last.fm, Spotify agents). `init` does not write this key. |

`wn-server match` writes these files into the data directory:

| File | Contents |
|---|---|
| `beets.yaml` | Generated beets config, mode 0600. Rewritten on every run. During a `match apply` run it also holds `wn_candidates.apply_release_id` and `wn_candidates.apply_folder`. wn-server writes it again without them after the run. `wn_candidates.forget_folders` is written when a full `match` run has reverted entries to forget. It stays in the file until the next write. `beet remove` (used by `match revert`) ignores it, because only `import` sends the event the plugin uses. |
| `beets.db` | beets library database. |
| `beets-state.pickle` | beets state file, which holds the incremental import history. The plugin removes reverted folders from it (see [Match queue](#match-queue)). |
| `beets-import.log` | beets' import log. |
| `beets-candidates.jsonl` | Output of the `wn_candidates` plugin for the last run. |
| `beets.lock` | Lock file. `match`, `match apply`, and `match revert` hold an exclusive `flock` on it while they run beets. |
| `wn-server.db` | The match queue, in the tables `match_queue` and `match_candidates`, next to the account, the server credential, the sessions, the local rating store (`ratings`), and the rating outbox (`rating_outbox`). |

beets also writes `beets.db-before-*.bak` files when it migrates its database, including on the first run.

`beets.yaml` sets `library`, `directory` (`sorted_dir`), `statefile`, the path format `$albumartist/$album%aunique{}/$track $title`, `import` (`copy`, `write`, `quiet`, `autotag`, `incremental`, and `incremental_skip_later` on; `move`, `link`, `hardlink`, `reflink`, `delete`, `resume`, `fix_ext_inplace`, and `remux_mp3_in_wav` off; `duplicate_action: skip`; `quiet_fallback: skip`; `log`), `plugins` (`musicbrainz`, `chroma`, `wn_candidates`), `pluginpath` (`beets-plugins` next to the beets executable), `chroma.auto`, `wn_candidates.output`, `wn_candidates.acoustid_app_key` when a key is set, `wn_candidates.apply_release_id` and `wn_candidates.apply_folder` during a `match apply` run, `wn_candidates.forget_folders` (a list of absolute folders, present when a full `match` run has reverted entries to forget), `musicbrainz` (`host: musicbrainz.org`, `ratelimit: 1`), and `ui.color: no`. beets 2.14 loads MusicBrainz as a plugin, and a `plugins` key replaces its default list, so `musicbrainz` is listed explicitly.

The import options keep beets from writing to the original folders. Copy is the only file operation. `link`, `hardlink`, `reflink`, and `delete` are beets' defaults and are set explicitly. `remux_mp3_in_wav` defaults to on in beets 2.14. With it on, beets writes an `.mp3` next to a WAV file that holds an MP3 stream and deletes the WAV. `fix_ext_inplace` is off by default and is set explicitly. beets 2.14 also runs `fix_extension` on every file without an extension. When `ffprobe` is on `PATH`, it copies the file to `<name>.<ext>` in the original folder. beets has no option for it, so the `wn_candidates` plugin replaces `beets.util.extension.fix_extension` with a function that returns the path unchanged. beets then reads an extensionless file under its own name.

`Config.WriteNavidrome` renders `<data dir>/navidrome.toml`. The file has `Address = 'unix:<data dir>/navidrome.sock'`, `UnixSocketPerm = '0600'`, `sorted_dir` as `MusicFolder`, `DataFolder = <data dir>/navidrome`, `ExtAuth.UserHeader = 'Remote-User'`, and `ExtAuth.TrustedSources = '@'`. It has no `Port`. Navidrome's trusted source check accepts a socket peer only when `Address` starts with `unix:` and `TrustedSources` has the entry `@`. The file always has `EnableInsightsCollector = false`, so Navidrome Insights telemetry is off. It has `EnableExternalServices = true` unless `navidrome_external_services` is `false`, so external lookups are on by default. Navidrome's config file takes one `MusicFolder` only. Navidrome stores it as the path of library 1 on every start, and library paths are unique, so `MusicFolder` must never change. `sorted_dir` is created on load and never moves, which makes it the one stable choice. The supervisor calls `WriteNavidrome` before it starts Navidrome, and adds every `music_folders` entry as a library through Navidrome's API.

## Navidrome supervisor

`internal/navidrome` runs the child process. `wn-server serve` loads the config, opens the store, checks the Navidrome version, binds `listen`, runs the supervisor, and stops it on SIGINT or SIGTERM. It logs one line when the signal arrives and one when shutdown is complete.

- **Version pin.** `navidrome.PinnedVersion` is `0.64.2`. When `serve` starts, before it binds `listen`, it runs `<navidrome_bin> --version`, which prints the version without the `v` followed by the commit sha, for example `0.64.2 (101145742)`. If the version differs, `serve` exits with an error that names both versions.
- **Start.** The supervisor refuses to start when `<data dir>/navidrome.sock` is longer than 107 bytes, the Linux limit for a socket path. It calls `Config.WriteNavidrome`, removes a stale socket file, then runs `<navidrome_bin> -c <data dir>/navidrome.toml` and logs `navidrome started` with the pid and the socket path. Navidrome creates the socket and sets its mode to 0600. Each stdout and stderr line of the child goes to wn-server's log through `log/slog`, prefixed `navidrome:`.
- **Ready.** The supervisor polls `/ping` on the socket every 250 ms until it answers 200. Until Navidrome listens, the dial fails and the poll retries. Navidrome runs database migrations before it answers, and a migration after a version bump can be slow. After 120 s without a 200 the supervisor stops the child and restarts it with backoff.
- **Restart.** When the child exits unexpectedly, the supervisor restarts it after 1 s. The delay doubles on each further exit, up to 30 s, and resets after the child stayed up for 60 s. `RestartPolicy.Next` computes the delay.
- **Stop.** The child runs in its own process group. On Linux the kernel also kills it when wn-server dies (`Pdeathsig`). On context cancellation the supervisor sends SIGTERM to the process group, waits up to 10 s, then sends SIGKILL.
- **wn-server identity.** `navidrome.Identity` is `wn-server`. The first request after ready is `GET /app/` with `Remote-User: wn-server`. Navidrome creates a header user only on that route, and the first user it creates is admin. The supervisor reads `auth.isAdmin` from the config that response injects into the page and logs an error if wn-server's user is not admin. Later requests use the same header on the native API, which accepts it from the trusted source `@`, the socket peer.
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
