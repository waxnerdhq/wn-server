# wn-cloud API used by wn-server

This document lists every wn-cloud route that wn-server calls, and the wn-cloud page that issues device codes so that first-party clients can sign in to a wn-server. wn-cloud is the v2 server in `../v2`. wn-cloud is implemented against this document. The package `internal/wncloud/stub` serves the same routes from memory for tests.

**Scope.** v1 of wn-server has one account, the owner. wn-server holds one wn-cloud credential: the server credential, which is the owner's CLI token. Listeners, invites, and server membership are not in v1. Listener sign-in depends on waxnerd relay, which is not specified yet. The routes for listeners return after relay is specified.

Third-party Subsonic clients do not use the device code. They sign in with the owner's name and app password through the Subsonic token scheme, checked by wn-server locally. wn-cloud is not involved.

First-party clients (wn-mobile, wn-desktop) never hold or send a wn-cloud CLI token to wn-server. The owner gets a short device code from a page at wn-cloud, and the client posts it to wn-server. wn-server verifies the code with the server credential. See [Device codes](#device-codes-wax-932).

Each route is marked **existing** (it is in v2 today and is described as it behaves now) or **new (WAX-nnn)** (wn-cloud does not have it yet).

| Route | Status | Called by |
|---|---|---|
| `GET /cli/auth` | existing | the browser, opened by `wn`. wn-server uses the variant below. |
| `POST /cli/token` | existing | wn-server |
| `POST /cli/logout` | existing | wn-server |
| `GET /cli/auth` with `display=code` | new (WAX-933) | the browser, opened by the owner at a URL that `wn-server login` prints |
| `POST /cli/token` without `redirect_port` | new (WAX-933) | wn-server |
| `POST /servers` | new (WAX-932) | wn-server |
| `POST /servers/:id/device-codes/verify` | new (WAX-932) | wn-server |
| `PUT /ratings/:kind/:mbid` | new (WAX-942) | wn-server |
| `DELETE /ratings/:kind/:mbid` | new (WAX-942) | wn-server |
| `GET /ratings/:kind/:mbid` | new (WAX-942) | wn-server |
| `GET /ratings?since=` | new (WAX-942) | wn-server |

wn-cloud also serves the device code page (WAX-932) in the browser. wn-server does not call it. `GET /me` exists in v2, and wn-server does not call it.

wn-server calls the rating routes with the server credential, so the ratings are the owner's. Clients send ratings to wn-server, through the Subsonic `setRating`, `star`, and `unstar` calls or through wn-server's own `/api/v1/ratings` route. wn-server relays each write to wn-cloud. Clients never call wn-cloud's rating routes directly, so the local store and the Navidrome mirror stay on one code path (README, Ratings and favorites).

## Conventions

- The base URL is wn-cloud's `SITE_URL`, for example `https://waxnerd.com`. wn-server reads it from the `wn_cloud_url` key in `config.toml`.
- Request and response bodies are JSON. Requests with a body send `Content-Type: application/json`.
- Times are RFC 3339 strings in UTC.
- IDs of users, servers, and ratings are UUIDs. MBIDs are UUIDs.
- An error body on the existing routes is `{ "error": "<message>" }` or `{ "error": "<message>", "code": "<code>" }`, as listed per route. Every new route answers errors with `{ "error": "<message>", "code": "<code>" }`. wn-server branches on `code` and the status, never on `error`.
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
- Errors: a malformed query gets status 400 and an HTML page. No redirect happens. The approve form posts to `POST /cli/auth`, which v2's `sameOrigin` middleware (`web/src/session.ts`, near lines 73-86) guards. A POST whose `Origin` header names another site gets 403 with `{ "error": "Cross-origin request rejected" }`.
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
| 400 | `{ "error", "code": "invalid_request" }` | A field is missing or malformed. |
| 400 | `{ "error", "code": "invalid_grant" }` | The code is unknown, expired, or used; the verifier does not hash to the challenge; or the port differs. A live code is consumed even when the exchange fails. |
| 400 | Express's default error page | The body is not valid JSON. |
| 403 | `{ "error": "Cross-origin request rejected" }` | The `Origin` header of the POST names another site. v2's `sameOrigin` middleware (`web/src/session.ts`, near lines 73-86) answers it. This can reach programs that send an `Origin` header. |

wn-server sends this route a code from the code display, without `redirect_port`. See [`POST /cli/token` without `redirect_port`](#post-clitoken-without-redirect_port-new-wax-933).

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

### `GET /cli/auth` with `display=code` (new, WAX-933)

- Authentication: a browser session at wn-cloud, as in the existing flow.
- Query parameters:

| Parameter | Value |
|---|---|
| `state` | As in the existing flow. Returned unchanged on the code page. |
| `challenge` | As in the existing flow. |
| `display` | `code`. Exactly one of `port` and `display` is present. No other value is accepted. |
| `label` | As in the existing flow. wn-server sends the server's name. |

- Response: the same approval page, without the port. It shows the label and tells the user to paste the code only into the program that showed them the link. Approve answers with status 200, `Cache-Control: no-store`, and a page that shows the sign-in code and a copy button. Cancel answers with a page that says the sign-in was cancelled and shows no code. Neither redirects.
- The sign-in code on the page is `<code>.<state>`: the 43 base64url characters of the code, a `.`, and `state` unchanged. base64url has no `.`, so the first `.` separates the two parts.
- Errors: a malformed query, both or neither of `port` and `display`, or a `display` other than `code` gets status 400 and an HTML page.
- The code is valid for 120 seconds and for one exchange, as in the existing flow. It is bound to the user, the challenge, and the label. It has no port.

wn-server rule: wn-server trims whitespace from the pasted value and splits it at the first `.`. When the state part differs from the `state` it generated, it stops and tells the owner to start again. It does not retry. Otherwise it calls `POST /cli/token` once.

### `POST /cli/token` without `redirect_port` (new, WAX-933)

The route, the response, and the errors are those of the existing exchange. The body of a code from the code display has no `redirect_port`:

```json
{
  "code": "Jx3...43 base64url characters",
  "code_verifier": "43 to 128 unreserved characters"
}
```

- A body with `redirect_port` for a code-display code, or without `redirect_port` for a port code, is `invalid_grant`.

wn-server rule: one attempt. On any error, `wn-server login` stops and prints the error. A code cannot be retried, because a failed exchange consumes it.

### wn-cloud changes for WAX-933

- `returnPath` (`web/src/pages.ts`, near lines 107-112) and `authPath` (`web/src/cliAuth.ts`, near lines 75-79) rebuild the query from `state`, `challenge`, `port`, and `label` only. `display` would be lost across `/login`. Both must carry `display` and omit `port` when `display=code`.
- `parseAuthRequest` (`web/src/cliAuth.ts`, near line 63) requires `port`. It must accept a request without `port` when `display=code`, and reject a request with both or neither.
- The approve form handler `POST /cli/auth` (`web/src/pages.ts`, near line 687) redirects to `callbackUrl` after the decision. With `display=code` it must render the code page instead. The form in `web/views/cli-auth.eta` posts `port`. With `display=code` it posts `display` instead and shows no port.
- `cli_auth_codes` needs no `redirect_uri` column. Its `redirect_port` is `not null` today (`web/migrations/1791212912000_cli-auth.sql`). It must become nullable, and a code-display code stores `null`.
- `exchangeToken` (`web/src/cliAuth.ts`, near line 175) requires `redirect_port`, and `EXCHANGE_SQL` (near line 119) compares `redirect_port = $3`. It must accept a body without `redirect_port` and compare with `redirect_port is not distinct from $3`. A code-display code then matches only a body without a port, and a port code only its own port.

## Servers (WAX-932)

A server is a record in wn-cloud that names its owner.

The error body carries a `code`. The route also answers the 401 and 403 answers of the bearer check.

Server object:

```json
{
  "id": "0193a0b2-77c1-7f00-8e21-3d9c5b6a4f10",
  "owner_id": "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70",
  "name": "living-room",
  "created_at": "2026-10-16T12:00:00Z"
}
```

### `POST /servers` (new, WAX-932)

Registers a server under the caller, who becomes its owner.

- Authentication: the server credential (the CLI token that `wn-server login` just received).
- Query parameters: none.
- Request body:

```json
{ "name": "living-room" }
```

`name` is 1 to 100 characters after trimming, with no control characters.

- Response 201: the server object.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `name` is missing or malformed. |

wn-server rule: `wn-server login` calls it once, when the data directory has no server id. It stores the returned `id`. When a server id is already stored, `login` only replaces the server credential, after checking that `user.id` in the `POST /cli/token` answer equals the stored owner's waxnerd user id. On an unreachable wn-cloud, `login` stops and the owner runs it again. It does not retry on its own, so it does not register twice. `wn-server logout` removes the token and keeps the server id, so a later `login` by the same owner reuses it. When the waxnerd user differs from the stored one, `login` calls `POST /servers` and replaces the stored server id.

## Device codes (WAX-932)

First-party clients (wn-mobile, wn-desktop) obtain a wn-server session with a device code. wn-cloud issues the code for one server. The client never holds a wn-cloud token.

1. The owner, signed in at wn-cloud in any browser, opens the device code page and picks a server.
2. wn-cloud shows a code. The owner enters it in the client.
3. The client posts the code to wn-server's `POST /api/v1/session`.
4. wn-server calls `POST /servers/:id/device-codes/verify` with the server credential. wn-cloud spends the code and answers with the user who issued it.
5. wn-server checks that the user id is the owner's and issues its own session. Nothing from wn-cloud is stored for the device.

### The device code page (wn-cloud UI)

The page is part of wn-cloud's web UI. wn-server does not call it. The suggested path is `/devices`.

- Authentication: a browser session at wn-cloud. A visitor without one is redirected to `/login` and returns to the page afterwards.
- The page lists the servers that the signed-in user owns, by name. The user picks one and presses a button. wn-cloud creates a code and shows it with a copy button and the time it expires.
- The code is bound to the server id and to the user who issued it. It is valid for 10 minutes and for one verification. It carries no other rights.
- The code is 8 characters from `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`, shown as two groups of 4 joined by `-`, for example `K7QM-4XPA`. wn-cloud ignores case, `-`, and whitespace when it reads a code.
- Issuing a code does not revoke earlier unused codes. Each expires on its own.
- wn-cloud stores the SHA-256 hash of the code without `-` and in upper case, never the code itself.

### `POST /servers/:id/device-codes/verify` (new, WAX-932)

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

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `:id` is not a UUID, the body is not valid JSON, or `code` is missing or does not have 8 characters of the alphabet after removing `-` and whitespace. |
| 400 | `invalid_grant` | The code is unknown, expired, already used, or issued for another server. |
| 403 | `forbidden` | The bearer's user does not own `:id`, or no server has that id. |
| 429 | `rate_limited` | The server credential made more than 10 failed verify calls in 10 minutes. |

The route also answers the 401 and 403 answers of the bearer check. Only a 200 answer spends a code.

wn-server rule: see [Rules](#rules-wn-server-applies), device code exchange.

### wn-cloud changes for WAX-932

- A `device_codes` table: `id`, `server_id` (references `servers`, deleted with the server), `user_id` (references `users`), `code_hash` (unique), `created_at`, `expires_at` (`created_at` plus 10 minutes), and `used_at` (null until the code is spent).
- The verify route allows at most 10 failed calls per server credential per 10 minutes, answers 429 beyond that, and logs the failures.
- The verify route spends the code in one statement: `update device_codes set used_at = now() where code_hash = $1 and server_id = $2 and used_at is null and expires_at > now() returning user_id`. No row means `invalid_grant`.
- The device code page described above, with a link to it from wn-cloud's account pages.

## Ratings (WAX-942)

A rating is the caller's rating of one MusicBrainz subject. The routes read and write v2's `subjects` and `ratings` tables. The caller is always the bearer's user. wn-server calls them with the server credential, so the caller is the owner.

Path parameters:

| Parameter | Value |
|---|---|
| `:kind` | `release_group`, `release`, `track`, or `artist`. The values of `subjects.kind`. |
| `:mbid` | The MBID of the subject, a UUID. |

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

### `PUT /ratings/:kind/:mbid` (new, WAX-942)

Sets the caller's rating of a subject. The body replaces the whole value: a field that is `null` or absent is cleared. This matches `saveMark` in v2 (`web/src/marks.ts`, near lines 320-321), except that both fields `null` is a 400. `saveMark` deletes the rating in that case.

- Authentication: bearer (the server credential).
- Query parameters: none.
- Request body:

```json
{ "half_stars": 9, "like_level": "love" }
```

- Response 200: the rating object. wn-cloud creates the `subjects` row if needed, inserts or updates the `ratings` row, and sets `updated_at`.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `:kind` or `:mbid` is malformed, `half_stars` is out of range, `like_level` is not `like` or `love`, or both are `null` (use `DELETE`). |
| 429 | `rate_limited` | wn-cloud rate-limits the rating routes. |

wn-server rule: wn-server sends both fields from its local store, so a Subsonic `setRating` keeps the like level and a `star` keeps the stars. Every write goes through the outbox. See [Rules](#rules-wn-server-applies).

### `DELETE /ratings/:kind/:mbid` (new, WAX-942)

Clears the caller's rating of a subject. The `ratings` row is deleted. The `subjects` row stays.

- Authentication: bearer (the server credential).
- Query parameters: none.
- Request body: none.
- Response: 204, also when the caller had no rating of the subject.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `:kind` or `:mbid` is malformed. |
| 429 | `rate_limited` | wn-cloud rate-limits the rating routes. |

wn-server rule: as for `PUT`.

### `GET /ratings/:kind/:mbid` (new, WAX-942)

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

### `GET /ratings?since=` (new, WAX-942)

Returns the caller's rating changes after a cursor, oldest first. wn-server uses it to bring its local store up to date.

- Authentication: bearer (the server credential).
- Query parameters:

| Parameter | Value |
|---|---|
| `since` | Optional. The `next` cursor of an earlier answer. Absent means from the start, which returns every current rating. |
| `limit` | Optional. 1 to 1000. Default 500. |

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
      "created_at": "2026-10-16T11:00:00Z",
      "updated_at": "2026-10-16T12:20:00.654321Z",
      "deleted": true
    }
  ],
  "next": "2026-10-16T12:20:00.654321Z,0193a0c4-aa10-7c00-8d00-112233445566",
  "more": false
}
```

- `changes` is ordered by `updated_at`, then `id`. Each subject appears at most once, with its latest state.
- A cleared rating appears with `deleted: true` and both values `null`. `updated_at` is the time it was cleared. A request without `since` returns no deleted entries.
- `next` is the cursor after the last entry. It is `updated_at` with microseconds and `id`, joined by a comma, as v2's `/reviews` cursor is built. wn-server treats it as opaque. When `changes` is empty, `next` equals the `since` that was sent, or `null` when none was sent.
- `more` is `true` when the page is full and further changes may follow.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `since` or `limit` is malformed. |

v2 deletes a `ratings` row when a rating is cleared, so it cannot report deletions today. This route needs wn-cloud to record cleared ratings, for example in a table of deletions keyed by user and subject with the time of the clear. See [Open questions](#open-questions-for-the-operator).

A deleted entry carries the `id` and `created_at` of the rating that was cleared, so the table of deletions stores both. Rating the subject again removes its deletion row, so the feed shows each subject once, either as a rating or as a deletion.

wn-server rule: see [Rules](#rules-wn-server-applies), rating sync.

## Rules wn-server applies

These restate decisions in `README.md` in terms of the routes above.

### Device code exchange

- wn-server calls `POST /servers/:id/device-codes/verify` once for each `POST /api/v1/session`, with the stored server id and the server credential. It does not retry. Nothing is cached.
- On 200, wn-server compares `user_id` with the owner's waxnerd user id. Any other user is refused. wn-server stores nothing from the answer.
- On 400, the session request is refused. The client tells the owner to get a new code.
- On 401, the session request is refused, and the web UI tells the owner to run `wn-server login` again.
- On 403, the session request is refused and wn-server logs an error. The stored server id does not belong to the server credential's user.
- When wn-cloud is unreachable, the session request fails with an error that tells the client to try again later or to use the app password. The code is not spent unless wn-cloud answered 200, so the owner can retry with the same code while it is valid. If wn-cloud spent the code but the answer was lost, the owner gets a new code.
- In local-only mode `POST /api/v1/session` is refused. There is no server credential to verify with.
- Requests that carry a wn-server session or an app password never call wn-cloud.
- Rate limits. wn-server allows at most 5 failed `POST /api/v1/session` attempts per client address per 10 minutes and at most 10 failed attempts in total per 10 minutes. It answers 429 beyond either limit, without calling wn-cloud. wn-cloud allows at most 10 failed verify calls per server credential per 10 minutes, answers 429, and logs them.
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

## Open questions for the operator

1. **Deletions in `GET /ratings?since=`.** v2 deletes a rating row on clear, so a delta cannot show clears. Recommendation: wn-cloud records clears in a `rating_deletions` table (user, subject, `deleted_at`, and the cleared rating's `id` and `created_at`) and merges it into the change feed. Rating the subject again removes its deletion row, so the feed shows each subject once. The alternative, a full re-pull on each tick, costs more as libraries grow.
2. **Ordering of outbox writes against changes made elsewhere.** A queued `PUT` replayed after an outage overwrites a newer value set in wn-cloud's web pages meanwhile. Recommendation: accept this for v1. Later, `PUT` can take an optional `updated_at` from the client, and wn-cloud ignores a write older than the stored value.
3. **Intervals.** The README and this document both use 15 minutes for the background tick that runs the outbox drain and the rating sync. Recommendation: 15 minutes.
