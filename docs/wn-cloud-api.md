# wn-cloud API used by wn-server

This document lists every wn-cloud route that wn-server calls, and the wn-cloud page that issues device codes so that first-party clients can sign in to a wn-server. wn-cloud is the v2 server in `../v2`. wn-cloud implements every route listed here, and this document states what it does on v2's `main` after v2 #91 (merge `527ff35`). Where wn-cloud made a choice that an earlier version of this document left open, the choice is recorded at the route. The package `internal/wncloud/stub` serves the same routes from memory for tests.

**Scope.** v1 of wn-server has one account, the owner. wn-server holds one wn-cloud credential: the server credential, which is the owner's CLI token. Listeners, invites, and server membership are not in v1. Listener sign-in depends on Waxnerd Relay, which is specified in [relay.md](relay.md). The routes for members, invites, and listeners are designed there and are added to this document when they are built.

Third-party Subsonic clients do not use the device code. They sign in with the owner's name and app password through the Subsonic token scheme, checked by wn-server locally. wn-cloud is not involved.

First-party clients (wn-mobile, wn-desktop) never hold or send a wn-cloud CLI token to wn-server. The owner gets a short device code from a page at wn-cloud, and the client posts it to wn-server. wn-server verifies the code with the server credential. See [Device codes](#device-codes-wax-932).

Every route below is in v2's `main`, except the relay routes, which name their open issue. The column "Added" names the change that added it. "existing" means the route was in v2 before wn-server's work.

| Route | Added | Called by |
|---|---|---|
| `GET /cli/auth` | existing | the browser, opened by `wn`. wn-server uses the variant below. |
| `POST /cli/token` | existing | wn-server |
| `POST /cli/logout` | existing | wn-server |
| `GET /cli/auth` with `display=code` | WAX-933 (v2 #89) | the browser, opened by the owner at a URL that `wn-server login` prints |
| `POST /cli/token` without `redirect_port` | WAX-933 (v2 #89) | wn-server |
| `POST /servers` | WAX-932 (v2 #91) | wn-server |
| `POST /servers/:id/device-codes/verify` | WAX-932 (v2 #91) | wn-server |
| `GET /servers/:id` | WAX-981 (not built yet) | wn-server, with `relay = true` |
| `GET /v1/tunnel` on the edge | WAX-986 (not built yet) | wn-server's relay package |
| `PUT /ratings/:kind/:mbid` | WAX-942 (v2 #90) | wn-server |
| `DELETE /ratings/:kind/:mbid` | WAX-942 (v2 #90) | wn-server |
| `GET /ratings/:kind/:mbid` | WAX-942 (v2 #90) | wn-server |
| `GET /ratings?since=` | WAX-942 (v2 #90) | wn-server |

wn-cloud also serves the device code page `/devices` (WAX-932, v2 #91) in the browser. wn-server does not call it. `GET /me` exists in v2, and wn-server does not call it.

wn-server calls the rating routes with the server credential, so the ratings are the owner's. Clients send ratings to wn-server, through the Subsonic `setRating`, `star`, and `unstar` calls or through wn-server's own `/api/v1/ratings` route. wn-server relays each write to wn-cloud. Clients never call wn-cloud's rating routes directly, so the local store and the Navidrome mirror stay on one code path (README, Ratings and favorites).

## Conventions

- The base URL is wn-cloud's `SITE_URL`, for example `https://waxnerd.com`. wn-server reads it from the `wn_cloud_url` key in `config.toml`.
- Request and response bodies are JSON. Requests with a body send `Content-Type: application/json`.
- Times are RFC 3339 strings in UTC. The routes of WAX-932 and WAX-942 write them with six digits of microseconds, for example `2026-10-16T12:10:00.123456Z` (`utcText` in `web/src/marks.ts`).
- IDs of users, servers, and ratings are UUIDs. MBIDs are UUIDs.
- An error body on the existing routes is `{ "error": "<message>" }` or `{ "error": "<message>", "code": "<code>" }`, as listed per route. The routes of WAX-932 and WAX-942 answer their own errors with `{ "error": "<message>", "code": "<code>" }`. The 401 answers of the bearer check on them carry no `code`. wn-server branches on `code` and the status, never on `error`.
- `express.json()` reads every request body before the route and before authentication. On the server and rating routes, a body it refuses gets `{ "error", "code": "invalid_request" }` with the status it gives: 400 for a body that is not valid JSON, 413 for a body over its size limit, 415 for a charset it does not read (`jsonBodyErrorHandler` in `web/src/jsonBody.ts`). Such a request gets this answer also without a bearer. On `POST /cli/token` a body that is not valid JSON gets Express's default HTML error page with status 400.
- wn-server sends every request with a 10 second timeout. A timeout, a connection error, or a 5xx status counts as "wn-cloud unreachable" in the rules below.

### Authentication

Every authenticated request sends `Authorization: Bearer <token>`. wn-cloud accepts three kinds of bearer. wn-server never sends cookies.

| Kind | Form | Where wn-server gets it |
|---|---|---|
| CLI token | `wn_` followed by 43 base64url characters. Opaque. Does not expire. Ends when revoked with `POST /cli/logout`. | `POST /cli/token`, in `wn-server login`. |
| GoTrue access token | An HS256 JWT signed by GoTrue. Expires after the GoTrue lifetime (one hour by default). | wn-server does not obtain these. The existing routes accept them, so they are listed for completeness. |
| Server credential | The owner's CLI token. It is the only credential wn-server holds at wn-cloud. `wn-server login` obtains it once, on the server machine, from `POST /cli/token`, with the server's name as its label. wn-server sends it on `POST /servers`, on `POST /servers/:id/device-codes/verify`, and on the rating routes. | `wn-server login`. wn-server stores it in its data directory, readable only by the server's user. |

No client sends a wn-cloud token to wn-server. A first-party client sends a device code instead.

The server credential is an ordinary CLI token. wn-cloud does not tell it apart from other CLI tokens of the owner. Each route authenticates the bearer as a user. `POST /servers` makes that user the owner of the new server. The verify route checks that that user owns the server. The rating routes read and write that user's ratings.

A bearer on the routes marked "bearer" below goes through v2's `requireUser` check:

| Status | Body | When |
|---|---|---|
| 401 | `{ "error": "Missing bearer token" }` | No `Authorization` header and no session cookies. |
| 401 | `{ "error": "Invalid or expired token" }` | The token is malformed, unknown, revoked, or expired. |
| 403 | `{ "error": "User not created", "code": "user_not_created" }` | A GoTrue account without a row in `users`. A CLI token always has a row, because the approval page creates it. |

## Existing routes

### `GET /me` (existing, not called by wn-server)

Returns the user that the bearer belongs to. wn-server does not call it. `POST /cli/token` returns the user together with the token, and device codes are checked with `POST /servers/:id/device-codes/verify`. The stub does not serve it.

### `GET /cli/auth` (existing)

The approval page of the CLI sign-in. wn-server does not call it and does not use this form of it. `wn` uses it. wn-server uses the code display variant in [Sign-in with a displayed code](#sign-in-with-a-displayed-code-wax-933), which builds on this page.

- Authentication: a browser session at wn-cloud. A visitor without one is redirected to `/login` and returns to the page afterwards with the query intact.
- Query parameters:

| Parameter | Value |
|---|---|
| `state` | 16 to 128 base64url characters. Returned unchanged in the redirect. |
| `challenge` | 43 base64url characters: the S256 PKCE challenge, `base64url(sha256(verifier))`. |
| `port` | 1024 to 65535. The loopback port wn-server listens on. |
| `label` | Optional. At most 100 characters, no control characters. Shown on the page and stored with the token. wn-server sends the server's name. |

- Response: an HTML page with Approve and Cancel. Approve redirects the browser with 303 to `http://127.0.0.1:<port>/callback?code=<code>&state=<state>`. Cancel redirects to `http://127.0.0.1:<port>/callback?error=access_denied&state=<state>`. The host and the path are fixed. Only the port comes from the query.
- Errors: a malformed query gets status 400 and an HTML page. No redirect happens. The approve form posts to `POST /cli/auth`, which v2's `sameOrigin` middleware (`web/src/session.ts`, near lines 73-87) guards. A POST whose `Origin` header names another site gets 403 with `{ "error": "Cross-origin request rejected" }`. `POST /cli/auth` refuses a request with a bearer with 403 and `{ "error": "This route accepts a browser session only" }`, so a CLI token cannot approve a sign-in.
- Both `GET` and `POST /cli/auth` send `Cache-Control: no-store` and `Content-Security-Policy: frame-ancestors 'none'`.
- The code is 43 base64url characters, valid for 120 seconds and for one exchange. It is bound to the user, the challenge, the port, and the label.

### `POST /cli/token` (existing)

Exchanges a one-time code for a CLI token.

- Authentication: none. The code and the verifier are the proof.
- Query parameters: none.
- Request body:

```json
{
  "code": "Jx3...43 base64url characters",
  "code_verifier": "43 to 128 unreserved characters",
  "redirect_port": 53122
}
```

`redirect_port` is a JSON number.

Response 200, with `Cache-Control: no-store`:

```json
{
  "token": "wn_9mQ...43 base64url characters",
  "token_type": "Bearer",
  "user": { "id": "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70", "email": "owner@example.com" }
}
```

Errors:

| Status | Body | When |
|---|---|---|
| 400 | `{ "error", "code": "invalid_request" }` | A field is missing or malformed. `redirect_port`, when present, must be a JSON integer from 1024 to 65535; `"redirect_port": null` is malformed, not absent. |
| 400 | `{ "error", "code": "invalid_grant" }` | The code is unknown, expired, or used; the verifier does not hash to the challenge; or the port differs. A live code is consumed even when the exchange fails. |
| 400 | Express's default error page | The body is not valid JSON. |
| 403 | `{ "error": "Cross-origin request rejected" }` | The `Origin` header of the POST names another site. v2's `sameOrigin` middleware (`web/src/session.ts`, near lines 73-87) answers it. This can reach programs that send an `Origin` header. wn-server sends none. |

wn-server sends this route a code from the code display, without `redirect_port`. See [`POST /cli/token` without `redirect_port`](#post-clitoken-without-redirect_port-wax-933).

### `POST /cli/logout` (existing)

Revokes the CLI token in the `Authorization` header.

- Authentication: the CLI token to revoke.
- Query parameters: none.
- Request body: none.
- Response: 204, no body.

Errors:

| Status | Body | When |
|---|---|---|
| 401 | `{ "error": "Missing bearer token" }` | No bearer. |
| 401 | `{ "error": "Invalid or expired token" }` | The token is unknown or already revoked. |
| 400 | `{ "error": "Only a CLI token can be revoked here", "code": "invalid_request" }` | The bearer is a GoTrue access token. |

wn-server rule: wn-server calls it in three cases:

- `wn-server logout` revokes the stored server credential.
- `wn-server login` revokes the new token when `POST /servers` fails.
- `wn-server login` revokes the new token when the local store write fails.

A 401 or any other error, including an unreachable wn-cloud, is a warning on stderr. The command continues, and the credential is removed locally. wn-server does not retry.

## Sign-in with a displayed code (WAX-933)

The existing flow redirects the browser to `127.0.0.1` on the machine that runs the browser. A redirect only works when the browser can reach the program that started the sign-in. It fails for a headless server, for a browser on another device, and behind the relay. In this variant wn-cloud shows the code on the page, and the owner pastes it into wn-server. Nothing redirects to wn-server. `wn-server login` uses it. It is the owner's sign-in.

1. `wn-server login` generates `state` and a PKCE verifier. It prints the URL `<wn-cloud>/cli/auth?state=…&challenge=…&label=…&display=code` in the terminal.
2. The owner opens the URL in any browser, signs in, and approves.
3. wn-cloud shows the sign-in code on the page, with a copy button.
4. The owner pastes the code at the terminal prompt of `wn-server login`.
5. wn-server checks `state` and posts the code and the verifier to `POST /cli/token`. It receives a CLI token, which becomes the server credential.

### `GET /cli/auth` with `display=code` (WAX-933)

- Authentication: a browser session at wn-cloud, as in the existing flow.
- Query parameters:

| Parameter | Value |
|---|---|
| `state` | As in the existing flow. Returned unchanged on the code page. |
| `challenge` | As in the existing flow. |
| `display` | `code`. Exactly one of `port` and `display` is present. No other value is accepted. |
| `label` | As in the existing flow. wn-server sends the server's name. |

- Response: the same approval page, without the port. It shows the label and tells the user to paste the code only into the program that showed them the link. The form posts `display=code` in place of `port`. Approve answers with status 200, `Cache-Control: no-store`, and a page that shows the sign-in code and a copy button (`web/views/cli-auth-code.eta`). Cancel answers with a page that says the sign-in was cancelled and shows no code. Neither redirects.
- A visitor without a session is sent to `/login` and returns to the page with `display=code` in the query (`authPath` in `web/src/cliAuth.ts`, `returnPath` in `web/src/pages.ts`).
- The sign-in code on the page is `<code>.<state>`: the 43 base64url characters of the code, a `.`, and `state` unchanged. base64url has no `.`, so the first `.` separates the two parts.
- Errors: a malformed query, both or neither of `port` and `display`, or a `display` other than `code` gets status 400 and an HTML page.
- The code is valid for 120 seconds and for one exchange, as in the existing flow. It is bound to the user, the challenge, and the label. It has no port: `cli_auth_codes.redirect_port` is nullable since the migration `web/migrations/1792239144771_cli-auth-code-display.sql`, and a code of the code display stores `null`.

wn-server rule: wn-server trims whitespace from the pasted value and splits it at the first `.`. When the state part differs from the `state` it generated, it stops and tells the owner to start again. It does not retry. Otherwise it calls `POST /cli/token` once.

### `POST /cli/token` without `redirect_port` (WAX-933)

The route, the response, and the errors are those of the existing exchange. The body of a code from the code display has no `redirect_port`:

```json
{
  "code": "Jx3...43 base64url characters",
  "code_verifier": "43 to 128 unreserved characters"
}
```

- A body with `redirect_port` for a code-display code, or without `redirect_port` for a port code, is `invalid_grant`. `EXCHANGE_SQL` (`web/src/cliAuth.ts`, near line 171) compares `redirect_port is not distinct from $3`, with `$3` null for a body without `redirect_port`. The code is consumed in either case.

wn-server rule: one attempt. On any error, `wn-server login` stops and prints the error. A code cannot be retried, because a failed exchange consumes it.

### In wn-cloud (WAX-933)

v2 #89 implemented this variant in `parseAuthRequest`, `authPath`, `createAuthCode`, and `exchangeToken` (`web/src/cliAuth.ts`), the `POST /cli/auth` handler and `returnPath` (`web/src/pages.ts`), the views `web/views/cli-auth.eta` and `web/views/cli-auth-code.eta`, and the migration that makes `redirect_port` nullable.

## Servers (WAX-932)

A server is a record in wn-cloud that names its owner.

The error body carries a `code`. The route also answers the 401 and 403 answers of the bearer check.

Server object:

```json
{
  "id": "0193a0b2-77c1-7f00-8e21-3d9c5b6a4f10",
  "owner_id": "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70",
  "name": "living-room",
  "created_at": "2026-10-16T12:00:00.123456Z",
  "relay": {
    "enabled": true,
    "until": null,
    "connect_url": "https://connect.relay.waxnerd.com/v1/tunnel",
    "host": "0193a0b2-77c1-7f00-8e21-3d9c5b6a4f10.relay.waxnerd.com",
    "connected": true,
    "last_seen_at": "2026-10-18T12:00:00.123456Z"
  },
  "addresses": [
    { "kind": "relay", "url": "https://0193a0b2-77c1-7f00-8e21-3d9c5b6a4f10.relay.waxnerd.com" }
  ]
}
```

`created_at` carries microseconds, as the rating objects do. The fields `relay` and `addresses` come with WAX-981 and are described in [Relay](#relay-wax-981-wax-986). The table is `servers` (`web/migrations/1792327731628_servers-and-device-codes.sql`).

### `POST /servers` (WAX-932)

Registers a server under the caller, who becomes its owner.

- Authentication: the server credential (the CLI token that `wn-server login` just received).
- Query parameters: none.
- Request body:

```json
{ "name": "living-room" }
```

The body is a JSON object. `name` is a string of 1 to 100 characters after trimming, with no control characters (U+0000 to U+001F and U+007F to U+009F). wn-cloud counts the length in UTF-16 code units, as JavaScript does. It stores the trimmed name.

- Response 201: the server object, with `relay.enabled` false.
- wn-cloud inserts the caller's owner row into `server_members` with the server (WAX-981).
- Each call creates a new server. Names need not be unique. The route has no rate limit.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | The body is not a JSON object, or `name` is missing, not a string, or malformed. |

wn-server rule: `wn-server login` calls it once, when the data directory has no server id. It stores the returned `id`. When a server id is already stored, `login` only replaces the server credential, after checking that `user.id` in the `POST /cli/token` answer equals the stored owner's waxnerd user id. On an unreachable wn-cloud, `login` stops and the owner runs it again. It does not retry on its own, so it does not register twice. `wn-server logout` removes the token and keeps the server id, so a later `login` by the same owner reuses it. When the waxnerd user differs from the stored one, `login` calls `POST /servers` and replaces the stored server id.

## Device codes (WAX-932)

First-party clients (wn-mobile, wn-desktop) obtain a wn-server session with a device code. wn-cloud issues the code for one server. The client never holds a wn-cloud token.

1. The owner, signed in at wn-cloud in any browser, opens the device code page and picks a server.
2. wn-cloud shows a code. The owner enters it in the client.
3. The client posts the code to wn-server's `POST /api/v1/session`.
4. wn-server calls `POST /servers/:id/device-codes/verify` with the server credential. wn-cloud spends the code and answers with the user who issued it.
5. wn-server checks that the user id is the owner's and issues its own session. Nothing from wn-cloud is stored for the device.

### The device code page (wn-cloud UI)

The page is part of wn-cloud's web UI. wn-server does not call it. Its path is `/devices`, linked from wn-cloud's nav bar (`web/views/components/nav.eta`). The handlers are in `web/src/pages.ts` (near lines 302 and 588-610), the page is `web/views/devices.eta`, and the code is made by `createDeviceCode` in `web/src/servers.ts`.

- Authentication: a browser session at wn-cloud only. A request with a bearer gets 403 and `{ "error": "This route accepts a browser session only" }` on `GET` and `POST /devices`, before the session is checked. wn-server, which holds the owner's CLI token, therefore cannot make a code without the owner at a browser. A visitor without a session is redirected to `/login` and returns to the page afterwards.
- `GET /devices` lists the servers that the signed-in user owns, by name, in the order of their registration. Each has a button that posts the form field `server` (the server id) to `POST /devices`. The answer shows the code with a copy button and the time it expires in UTC, with `Cache-Control: no-store`. A `server` that the user does not own, or that is not a UUID, gets the page with an error line and status 400.
- The code is bound to the server id and to the user who issued it. It is valid for 10 minutes and for one verification. It carries no other rights.
- The code is 8 characters from `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`, shown as two groups of 4 joined by `-`, for example `K7QM-4XPA`. wn-cloud ignores case, `-`, and whitespace when it reads a code.
- Issuing a code does not revoke earlier unused codes. Each expires on its own. The same statement deletes the user's used and expired codes.
- wn-cloud stores the SHA-256 hash of the code without `-` and in upper case, never the code itself.

### `POST /servers/:id/device-codes/verify` (WAX-932)

Checks a device code for a server and spends it.

- Authentication: bearer (the server credential). The bearer's user must own `:id`.
- Path parameters: `:id`, the server id from `POST /servers`, a UUID.
- Query parameters: none.
- Request body:

```json
{ "code": "K7QM-4XPA" }
```

- Response 200. The code is spent.

```json
{
  "user_id": "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70",
  "email": "owner@example.com"
}
```

`user_id` and `email` belong to the user who issued the code.

The checks run in this order, and the first that fails answers (`verifyDeviceCode` in `web/src/servers.ts`, near line 221):

1. The bearer check (401, or 403 `user_not_created`). It records no failure.
2. The rate limit: 10 or more failures of this credential in the last 10 minutes answer 429 `rate_limited`. A 429 records no failure.
3. `:id` is a UUID, else 400 `invalid_request`.
4. The caller owns the server, else 403 `forbidden`. A server id that no server has gets the same answer.
5. The body is a JSON object with a string `code` that has 8 characters of the alphabet after removing `-` and whitespace and upper-casing, else 400 `invalid_request`.
6. The code is spent in one statement: `update device_codes set used_at = now() where code_hash = $1 and server_id = $2 and used_at is null and expires_at > now() returning user_id`. No row answers 400 `invalid_grant`.

Each 400 and 403 of the steps 3 to 6 records one failure before the answer goes out. A body that is not valid JSON is answered by `express.json()` before the bearer check, with 400 `invalid_request`, and records no failure. Only a 200 answer spends a code.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `:id` is not a UUID, the body is not valid JSON or not a JSON object, or `code` is missing, not a string, or does not have 8 characters of the alphabet after normalization. |
| 400 | `invalid_grant` | The code is unknown, expired, already used, or issued for another server. |
| 403 | `forbidden` | The bearer's user does not own `:id`, or no server has that id. |
| 429 | `rate_limited` | The credential has 10 or more failed verify calls in the last 10 minutes. The limit is checked before the code, so a valid code is refused too. |

The route also answers the 401 and 403 answers of the bearer check.

Rate limit. The failures are rows of the table `device_code_failures` (`token_hash`, `failed_at`), so the count holds across wn-cloud's web replicas. The key is the SHA-256 hash of the bearer in hex, or the user id for a call with a browser session. Recording a failure deletes every failure row older than 10 minutes. The count and the insert are two statements, so two concurrent failing calls at a count of 9 both pass: at most one failure over the limit. Each failure writes one log line with the error code, the server id, and the user id, never the code, the bearer, or the email.

wn-server rule: see [Rules](#rules-wn-server-applies), device code exchange.

### In wn-cloud (WAX-932)

v2 #91 added the tables `servers`, `device_codes` (`id`, `server_id`, `user_id`, `code_hash` unique, `created_at`, `expires_at`, `used_at`), and `device_code_failures` in `web/migrations/1792327731628_servers-and-device-codes.sql`. A `device_codes` row is deleted with its server or its user. The routes are in `web/src/servers.ts`, the shared error answers in `web/src/jsonBody.ts`, and the page in `web/src/pages.ts` and `web/views/devices.eta`.

## Relay (WAX-981, WAX-986)

Waxnerd Relay gives a server a public address, `https://<server id>.relay.waxnerd.com`, through a tunnel that wn-server opens to the edge, a service in wn-cloud. The design is in [relay.md](relay.md). WAX-981 adds `GET /servers/:id` and the relay columns to wn-cloud. WAX-986 adds the edge with `GET /v1/tunnel`. Neither is in v2's `main` yet.

The server object of [Servers](#servers-wax-932) carries the fields `relay` and `addresses`. `relay.enabled` is true when `relay_enabled` is true and `relay_until` is null, in the future, or within the 7-day grace period after it. `relay.until` is `relay_until` or null. `relay.connect_url` is the edge's tunnel URL. `relay.host` is the relay host. `relay.connected` is true when `relay_connected_at` is not null. `relay.last_seen_at` is `relay_last_seen_at` or null. `addresses` lists the connection candidates of the server. v1 has one entry, `{ "kind": "relay", "url": "https://<id>.relay.waxnerd.com" }`. Later kinds, such as `lan` and `direct`, come before `relay`, and a client tries the entries in order.

### `GET /servers/:id` (WAX-981)

Answers one server.

- Authentication: bearer (the server credential). The bearer's user must own `:id`. A browser session of the owner is also accepted.
- Path parameters: `:id`, a UUID.
- Query parameters: none.
- Response 200: the server object with the `relay` and `addresses` fields.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `:id` is not a UUID. |
| 403 | `forbidden` | The caller does not own `:id`, or no server has that id. |

The route also answers the 401 and 403 answers of the bearer check.

The checks run in this order: the bearer check (401), `:id` is a UUID (400), the caller owns the server (403). The route has no rate limit, as `POST /servers` has none.

wn-server rule: with `relay = true`, `serve` calls it at start, every hour, and within 10 seconds of a server credential appearing in the store. It stores `relay.connect_url`, `relay.host`, and `addresses` in the table `relay_settings`, and dials the stored `connect_url` when wn-cloud is unreachable. The config key `relay_edge_url` replaces the stored `connect_url`, for a test edge. A 401 marks the server credential as refused for the web UI, as on the other routes, and the edge's own 401 stops the relay package. A failed call is retried after 5 minutes.

### `GET /v1/tunnel` on the edge (WAX-986)

The tunnel handshake. The host is the one in `relay.connect_url` from `GET /servers/:id`, `connect.relay.waxnerd.com`, not the API host.

- Authentication: bearer (the server credential). The bearer's user must own the server in `Wn-Server-Id`.
- Request headers: `Upgrade: wn-relay/1`, `Connection: Upgrade`, `Wn-Server-Id` (a UUID), `Wn-Server-Version`.
- Transport: TLS with ALPN `http/1.1`, verified against the system roots, and one HTTP/1.1 request on the connection.
- Response 101 with `Wn-Relay-Host` (`<id>.relay.waxnerd.com`) and `Wn-Relay-Session` (the session id). After the response both sides speak yamux on the connection. The edge opens streams, and wn-server accepts them and serves HTTP/1.1 on each.

The checks run in this order, and the first that fails answers. The steps 2 to 5 count as a failed handshake for the rate limit.

1. The headers: `Upgrade` names a protocol the edge speaks (else 409), and `Wn-Server-Id` is a UUID (else 400).
2. The rate limit: 10 or more failed handshakes from this client address in the last 10 minutes answer 429. The count is in memory in the edge.
3. The bearer check, as for any CLI token (401).
4. The token's user owns the server (403).
5. The entitlement: `relay.enabled` as above (402).

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | A header is missing or malformed, or `Wn-Server-Id` is not a UUID. |
| 402 | `relay_not_enabled` | The server is not entitled, or the grace period has ended. |
| 403 | `forbidden` | The token's user does not own the server, or no server has that id. |
| 409 | `unsupported_protocol` | The edge does not speak the `Upgrade` protocol version. |
| 429 | `rate_limited` | 10 or more failed handshakes from the client address in 10 minutes. |
| 503 | `cloud_unavailable` | Postgres does not answer. |

The route also answers the 401 and 403 answers of the bearer check.

A second tunnel for the same server id replaces the first, and the edge closes the earlier session. Relayed requests carry `X-Relay-Client` with the client's IP address and no other header that the client did not send. The edge removes incoming `X-Relay-Client`, `Forwarded`, and `X-Forwarded-*` headers.

wn-server rule: the relay package dials while `relay = true`, a server credential is stored, and an edge address is known. On 400, 401, 403, and 409 it stops dialing until the server credential changes. On 402 it dials again every 10 minutes. On 429, 503, a dial failure, and a dropped tunnel it backs off from 1 second to 5 minutes. `serve` writes each state to the table `relay_state`, and `wn-server status` prints it (README, Web UI).

### In wn-cloud (WAX-981, WAX-986)

WAX-981 adds `servers.relay_enabled`, `servers.relay_until`, `servers.relay_connected_at`, `servers.relay_session_id`, `servers.relay_last_seen_at`, `servers.relay_streams`, the table `relay_usage (server_id, month date, bytes bigint, primary key (server_id, month))`, and the table `server_members` with the owner rows. `POST /servers` inserts the owner row and answers the server object with the relay fields. The route is in `web/src/servers.ts`. WAX-986 adds the edge in `relay/`. It reads Postgres with the web service's `DATABASE_URL`, and writes `servers.relay_connected_at` and `servers.relay_last_seen_at`.

## Ratings (WAX-942)

A rating is the caller's rating of one MusicBrainz subject. The routes read and write v2's `subjects`, `ratings`, and `rating_deletions` tables (`web/src/ratings.ts`). The caller is always the bearer's user. wn-server calls them with the server credential, so the caller is the owner.

The rating routes have no rate limit. Their errors use the codes `invalid_request` and `not_found` only. wn-server still treats a 429 `rate_limited` as described in [Rules](#rules-wn-server-applies), in case wn-cloud adds a limit.

Path parameters:

| Parameter | Value |
|---|---|
| `:kind` | `release_group`, `release`, `track`, or `artist`. The values of `subjects.kind`. |
| `:mbid` | The MBID of the subject, a UUID in either case. Answers carry it in lower case. |

Rating object. The field names are the column names of `subjects` and `ratings`:

```json
{
  "id": "0193a0c0-1d2e-7a3b-9c4d-5e6f7a8b9c0d",
  "kind": "release",
  "mbid": "6e12a11e-0ef9-49f3-b646-66929507d915",
  "half_stars": 9,
  "like_level": "love",
  "created_at": "2026-10-16T12:10:00.123456Z",
  "updated_at": "2026-10-16T12:10:00.123456Z"
}
```

- `half_stars`: an integer from 1 to 10, or `null`. 1 is half a star, 10 is five stars.
- `like_level`: `"like"`, `"love"`, or `null`.
- At least one of the two is not `null`.

### `PUT /ratings/:kind/:mbid` (WAX-942)

Sets the caller's rating of a subject. The body replaces the whole value: a field that is `null` or absent is cleared. This matches `saveMark` in v2 (`web/src/marks.ts`, near line 321), the mark form of wn-cloud's pages, except that both fields `null` is a 400. `saveMark` clears the rating in that case and writes a `rating_deletions` row, as `DELETE` does.

- Authentication: bearer (the server credential).
- Query parameters: none.
- Request body:

```json
{ "half_stars": 9, "like_level": "love" }
```

The body is a JSON object. `half_stars` is a JSON number with an integer value.

- Response 200: the rating object. In one transaction wn-cloud creates the `subjects` row if needed, inserts the `ratings` row or updates it with `updated_at = now()`, and then deletes the caller's `rating_deletions` row of the subject in a statement of its own. The separate statement matters for a concurrent `DELETE` of the same subject: the upsert waits for it, and the following delete sees its deletion row. A rating row and a deletion row of one subject never coexist.
- The last write wins. wn-cloud does not compare `updated_at` or take one from the client.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `:kind` or `:mbid` is malformed, the body is not a JSON object, `half_stars` is not an integer from 1 to 10, `like_level` is not `like` or `love`, or both are `null` or absent (use `DELETE`). |

wn-server rule: wn-server sends both fields from its local store, so a Subsonic `setRating` keeps the like level and a `star` keeps the stars. Every write goes through the outbox. See [Rules](#rules-wn-server-applies).

### `DELETE /ratings/:kind/:mbid` (WAX-942)

Clears the caller's rating of a subject. One statement (`CLEAR_SQL` in `web/src/ratings.ts`, near line 61) deletes the `ratings` row and writes a `rating_deletions` row with the `id` and `created_at` of the deleted rating and `deleted_at = now()`. When the caller had no rating of the subject, nothing is written, and no `subjects` row is created. The `subjects` row stays.

- Authentication: bearer (the server credential).
- Query parameters: none.
- Request body: none.
- Response: 204, also when the caller had no rating of the subject.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `:kind` or `:mbid` is malformed. |

wn-server rule: as for `PUT`.

### `GET /ratings/:kind/:mbid` (WAX-942)

Returns the caller's rating of one subject.

- Authentication: bearer (the server credential).
- Query parameters: none.
- Request body: none.
- Response 200: the rating object.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `:kind` or `:mbid` is malformed. |
| 404 | `not_found` | The caller has no rating of the subject. |

wn-server rule: not cached by the client of this route. wn-server uses it to check one value after an outbox conflict and in tests. The rating store serves reads.

### `GET /ratings?since=` (WAX-942)

Returns the caller's rating changes after a cursor, oldest first. wn-server uses it to bring its local store up to date.

- Authentication: bearer (the server credential).
- Query parameters:

| Parameter | Value |
|---|---|
| `since` | Optional. The `next` cursor of an earlier answer. Absent means from the start, which returns every current rating. A present but empty `since` is malformed. |
| `limit` | Optional. Decimal digits, 1 to 1000. Default 500. |

- Request body: none.
- Response 200:

```json
{
  "changes": [
    {
      "id": "0193a0c0-1d2e-7a3b-9c4d-5e6f7a8b9c0d",
      "kind": "release",
      "mbid": "6e12a11e-0ef9-49f3-b646-66929507d915",
      "half_stars": 9,
      "like_level": "love",
      "created_at": "2026-10-16T12:10:00.123456Z",
      "updated_at": "2026-10-16T12:10:00.123456Z",
      "deleted": false
    },
    {
      "id": "0193a0c4-aa10-7c00-8d00-112233445566",
      "kind": "track",
      "mbid": "1f0d2b5e-3c4a-4b6d-8e9f-0a1b2c3d4e5f",
      "half_stars": null,
      "like_level": null,
      "created_at": "2026-10-16T11:00:00.000000Z",
      "updated_at": "2026-10-16T12:20:00.654321Z",
      "deleted": true
    }
  ],
  "next": "2026-10-16T12:20:00.654321Z,0193a0c4-aa10-7c00-8d00-112233445566",
  "more": false
}
```

- `changes` holds the entries with `(updated_at, id)` greater than the cursor, ordered by `updated_at`, then `id`. Each subject appears at most once, with its latest state, because a subject has either a rating row or a deletion row.
- A cleared rating appears with `deleted: true` and both values `null`. Its `id` and `created_at` are those of the rating that was cleared. `updated_at` is the time it was cleared (`deleted_at`). A request without `since` returns no deleted entries.
- `next` is the cursor of the last entry: `updated_at` with six digits of microseconds and `id`, joined by a comma, in the format of the cursor of v2's recent ratings grid (`cursorText` and `parseCursor` in `web/src/marks.ts`). wn-server treats it as opaque. When `changes` is empty, `next` equals the `since` that was sent, or `null` when none was sent.
- `more` is `true` when more than `limit` entries follow the cursor. wn-cloud reads one row past the page to decide it.
- `updated_at` and `deleted_at` are `now()` of the writing transaction, which is its start time. A write whose transaction starts before a feed read and commits after it can carry a time below the `next` of that read. The feed does not return such a write later. The window is the length of one rating transaction.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `since` is not `<YYYY-MM-DDTHH:MM:SS.ffffffZ>,<UUID>` with a date that exists, or `limit` is not an integer from 1 to 1000. |

wn-cloud records cleared ratings in `rating_deletions` (`user_id`, `subject_id`, `rating_id`, `rating_created_at`, `deleted_at`, unique on `user_id` and `subject_id`; `web/migrations/1792239187679_rating-deletions.sql`). The feed merges those rows with `ratings` as `deleted: true` entries. Rating the subject again, through `PUT` or the mark form, removes the deletion row, so the feed shows each subject once, either as a rating or as a deletion. A clear made before v2 #90 was deployed wrote no deletion row and is not in the feed.

wn-server rule: see [Rules](#rules-wn-server-applies), rating sync.

## Rules wn-server applies

These restate decisions in `README.md` in terms of the routes above.

### Device code exchange

- wn-server calls `POST /servers/:id/device-codes/verify` once for each `POST /api/v1/session`, with the stored server id and the server credential. It does not retry. Nothing is cached.
- On 200, wn-server compares `user_id` with the owner's waxnerd user id. Any other user is refused. wn-server stores nothing from the answer.
- On 400, the session request is refused. The client tells the owner to get a new code.
- On 401, the session request is refused, and the web UI tells the owner to run `wn-server login` again.
- On 403, the session request is refused and wn-server logs an error. The stored server id does not belong to the server credential's user.
- On 429 `rate_limited`, the session request is refused with 429 `rate_limited`.
- When wn-cloud is unreachable, the session request fails with an error that tells the client to try again later or to use the app password. The code is not spent unless wn-cloud answered 200, so the owner can retry with the same code while it is valid. If wn-cloud spent the code but the answer was lost, the owner gets a new code.
- In local-only mode `POST /api/v1/session` is refused. There is no server credential to verify with.
- Requests that carry a wn-server session or an app password never call wn-cloud.
- Rate limits. wn-server allows at most 5 failed `POST /api/v1/session` attempts per client address per 10 minutes and at most 10 failed attempts in total per 10 minutes. It answers 429 beyond either limit, without calling wn-cloud. wn-cloud answers every verify call of the server credential with 429 once 10 failed calls are recorded in the last 10 minutes, and logs each failure. wn-cloud also counts its 403 `forbidden` answers, which wn-server does not count, and its count survives a restart of wn-server.
- Code space. A code has 8 characters from a 32-symbol alphabet without ambiguous glyphs, about 40 bits, and lives 10 minutes. Guessing is infeasible under these limits.

### Background tick

- In cloud mode, wn-server runs a background tick at start and every 15 minutes. Each tick drains the outbox and then runs the rating sync.
- Apart from the tick, `serve` drains the outbox after each rating write, at start, and every minute.
- A 401 answer to the server credential, on any route, means the credential no longer works. wn-server stops the tick's work and the web UI tells the owner to run `wn-server login` again.

### Outage

- Sessions already issued keep working. wn-server checks them against its own store and does not call wn-cloud.
- The owner is never refused because wn-cloud is unreachable. There is no cut-off.
- `POST /api/v1/session` and `wn-server login` fail while wn-cloud is unreachable, because they need `POST /servers/:id/device-codes/verify` or `POST /cli/token`. The app password keeps working.
- Rating writes go to the outbox.
- wn-server records the time of the first unreachable answer after a successful call. The web UI shows a banner "wn-cloud unreachable since <time>" until a call succeeds.

### Rating writes and the outbox

- For a Subsonic `setRating`, `star`, or `unstar`, wn-server forwards the call to Navidrome first. When Navidrome answers `ok`, wn-server writes the rating to its local store and queues it in a local outbox. The client does not wait for wn-cloud.
- The outbox holds one entry per subject. A later change replaces the earlier entry.
- The drain sends each entry with `PUT` or `DELETE /ratings/:kind/:mbid` and the server credential, oldest entry first. It runs within a second after each write, unless wn-server records wn-cloud as unreachable, and at `serve` start and every minute.
- A 2xx answer removes the entry. After a failed attempt an entry waits 1, 2, 4, and so on minutes, at most one hour. An unreachable answer, a 401, or a 429 `rate_limited` stops the drain until the next run.
- A 400 `invalid_request` or any 403 keeps the entry with its error, out of the retry loop, and logs it. A 401 answer keeps the entry and stops the drain. `wn-server login` makes every waiting entry due at once, so the entry is sent once a working server credential is stored.
- In local-only mode wn-server writes the local store and the outbox, and sends nothing. A cleared rating waits in the outbox as a `DELETE`.
- When `wn-server login` links to a waxnerd user other than the one of the stored registration, or no registration is stored, wn-server queues every rating in the local store in the outbox. The next drain sends them to wn-cloud with the server credential, so they become the owner's ratings. A login of the same user only makes the waiting entries due at once.

### Rating sync

- On each tick, after the outbox is empty, wn-server calls `GET /ratings?since=<stored cursor>` with the server credential, and follows `more` until it is `false`. It stores `next` after each page.
- A change from wn-cloud overwrites the local value, and wn-server sends Navidrome the rounded value (half stars round up, `love` maps to starred). A `deleted` change clears the local value and unstars and unrates in Navidrome.
- Draining the outbox before pulling keeps a pending local change from being overwritten by an older value from wn-cloud.

## Settled questions

These were open in earlier versions of this document. wn-cloud implemented them as follows.

1. **Deletions in `GET /ratings?since=`.** wn-cloud records clears in the `rating_deletions` table and merges it into the change feed as `deleted: true` entries. Rating the subject again removes its deletion row. See [`GET /ratings?since=`](#get-ratingssince-wax-942).
2. **Ordering of outbox writes against changes made elsewhere.** `PUT` is last write wins. A queued `PUT` replayed after an outage overwrites a newer value set in wn-cloud's web pages meanwhile. `PUT` takes no `updated_at` from the client.
3. **Intervals.** The 15-minute background tick is wn-server's. wn-cloud has no interval of its own for these routes.
