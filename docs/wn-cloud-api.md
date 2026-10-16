# wn-cloud API used by wn-server

This document lists every wn-cloud route that wn-server calls, and the routes wn-cloud must offer so that first-party clients can sign in to a wn-server. wn-cloud is the v2 server in `../v2`. wn-cloud is implemented against this document. The package `internal/wncloud/stub` serves the same routes from memory for tests.

Each route is marked **existing** (it is in v2 today and is described as it behaves now) or **new (WAX-nnn)** (wn-cloud does not have it yet).

| Route | Status | Called by |
|---|---|---|
| `GET /me` | existing | wn-server |
| `GET /cli/auth` | existing | the browser, opened by wn-server |
| `POST /cli/token` | existing | wn-server |
| `POST /cli/logout` | existing | wn-server |
| `GET /cli/auth` with `redirect_uri` | new (WAX-933) | the browser, opened by wn-server's web UI or a first-party app |
| `POST /cli/token` with `redirect_uri` | new (WAX-933) | wn-server, first-party apps |
| `POST /servers` | new (WAX-932) | wn-server |
| `PUT /servers/:id/redirect-uris` | new (WAX-932, needed by WAX-933) | wn-server |
| `GET /servers/:id/members` | new (WAX-932) | wn-server |
| `POST /servers/:id/invites` | new (WAX-932) | wn-server |
| `DELETE /servers/:id/members/:userId` | new (WAX-932) | wn-server |
| `PUT /ratings/:kind/:mbid` | new (WAX-942) | wn-server |
| `DELETE /ratings/:kind/:mbid` | new (WAX-942) | wn-server |
| `GET /ratings/:kind/:mbid` | new (WAX-942) | wn-server |
| `GET /ratings?since=` | new (WAX-942) | wn-server |

## Conventions

- The base URL is wn-cloud's `SITE_URL`, for example `https://waxnerd.com`. wn-server reads it from `config.toml`.
- Request and response bodies are JSON. Requests with a body send `Content-Type: application/json`.
- Times are RFC 3339 strings in UTC.
- IDs of users, servers, and ratings are UUIDs. MBIDs are UUIDs.
- An error body on the existing routes is `{ "error": "<message>" }` or `{ "error": "<message>", "code": "<code>" }`, as listed per route. Every new route answers errors with `{ "error": "<message>", "code": "<code>" }`. wn-server branches on `code` and the status, never on `error`.
- wn-server sends every request with a 10 second timeout. A timeout, a connection error, or a 5xx status counts as "wn-cloud unreachable" in the rules below.

### Authentication

Every authenticated request sends `Authorization: Bearer <token>`. wn-cloud accepts three kinds of bearer. wn-server never sends cookies.

| Kind | Form | Where wn-server gets it |
|---|---|---|
| CLI token | `wn_` followed by 43 base64url characters. Opaque. Does not expire. Ends when revoked with `POST /cli/logout`. | `POST /cli/token`. A first-party client hands its own CLI token to wn-server in `POST /api/v1/session`. wn-server keeps it with the account. |
| GoTrue access token | An HS256 JWT signed by GoTrue. Expires after the GoTrue lifetime (one hour by default). | wn-server does not obtain these. The existing routes accept them, so they are listed for completeness. |
| Server credential | A CLI token of the server's owner. It is the token that `wn-server login` receives from `POST /cli/token`, with the server's name as its label. | `wn-server login`. wn-server stores it in its data directory, readable only by the server's user. |

The server credential is an ordinary CLI token. wn-cloud does not tell it apart from other CLI tokens of the owner. A route that requires the server credential authenticates the bearer as a user and then checks that the user is the owner of the server in the path. Any bearer of the owner passes that check. wn-server always sends the server credential on those routes.

A bearer on the routes marked "bearer" below goes through v2's `requireUser` check:

| Status | Body | When |
|---|---|---|
| 401 | `{ "error": "Missing bearer token" }` | No `Authorization` header and no session cookies. |
| 401 | `{ "error": "Invalid or expired token" }` | The token is malformed, unknown, revoked, or expired. |
| 403 | `{ "error": "User not created", "code": "user_not_created" }` | A GoTrue account without a row in `users`. A CLI token always has a row, because the approval page creates it. |

## Existing routes

### `GET /me` (existing)

Returns the user that the bearer belongs to. wn-server uses it to resolve a client's CLI token to a waxnerd user id.

- Authentication: bearer (CLI token or GoTrue access token).
- Query parameters: none.
- Request body: none.

Response 200. The body is the row of `users`. Note the camelCase `createdAt`.

```json
{
  "id": "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70",
  "email": "owner@example.com",
  "createdAt": "2026-10-01T09:30:12.345Z"
}
```

Errors: the 401 and 403 answers of the bearer check above.

wn-server rule: see [Rules](#rules-wn-server-applies), `GET /me`.

### `GET /cli/auth` (existing)

The approval page of the CLI sign-in. wn-server does not call it. `wn-server login` opens it in the owner's browser.

- Authentication: a browser session at wn-cloud. A visitor without one is redirected to `/login` and returns to the page afterwards with the query intact.
- Query parameters:

| Parameter | Value |
|---|---|
| `state` | 16 to 128 base64url characters. Returned unchanged in the redirect. |
| `challenge` | 43 base64url characters: the S256 PKCE challenge, `base64url(sha256(verifier))`. |
| `port` | 1024 to 65535. The loopback port wn-server listens on. |
| `label` | Optional. At most 100 characters, no control characters. Shown on the page and stored with the token. wn-server sends the server's name. |

- Response: an HTML page with Approve and Cancel. Approve redirects the browser with 303 to `http://127.0.0.1:<port>/callback?code=<code>&state=<state>`. Cancel redirects to `http://127.0.0.1:<port>/callback?error=access_denied&state=<state>`. The host and the path are fixed. Only the port comes from the query.
- Errors: a malformed query gets status 400 and an HTML page. No redirect happens.
- The code is 43 base64url characters, valid for 120 seconds and for one exchange. It is bound to the user, the challenge, the port, and the label.

wn-server rule: wn-server checks `state` on the callback and stops the login on a mismatch or on `error`. It does not retry. The owner runs `wn-server login` again.

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

wn-server rule: one attempt. On any error, `wn-server login` stops and prints the error. A code cannot be retried, because a failed exchange consumes it.

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

wn-server rule: wn-server calls it in two cases. When a user signs out of the web UI, it revokes the token that the web UI sign-in obtained for that browser. When `wn-server login` replaces the server credential, it revokes the old one. It never revokes a token that a first-party app handed it, because the app still uses that token elsewhere. A 401 counts as success. When wn-cloud is unreachable, wn-server forgets the token locally and does not retry.

## Browser sign-in to a server (WAX-933)

The existing flow can only redirect to `127.0.0.1`. wn-server's web UI runs on another machine than the browser, and first-party apps cannot always listen on a loopback port. The variant below adds a `redirect_uri` that replaces `port`. The page, the code, the PKCE check, and the token are otherwise unchanged.

### `GET /cli/auth` with `redirect_uri` (new, WAX-933)

- Authentication: a browser session at wn-cloud, as in the existing flow.
- Query parameters:

| Parameter | Value |
|---|---|
| `state` | As in the existing flow. |
| `challenge` | As in the existing flow. |
| `redirect_uri` | An absolute URL. Exactly one of `port` and `redirect_uri` is present. |
| `server_id` | Optional. The id of a registered server. When present, `redirect_uri` must equal one of that server's registered redirect URIs, compared as exact strings. |
| `label` | As in the existing flow. When absent and `server_id` is present, wn-cloud stores the server's name as the label. |

Without `server_id`, `redirect_uri` must equal one entry of wn-cloud's first-party redirect list, compared as exact strings. That list is configuration in wn-cloud, not data. It holds the redirect URIs of wn-mobile and wn-desktop. **The variant must accept those first-party app redirects**, because wn-mobile and wn-desktop obtain their CLI tokens through it and then hand them to wn-server in `POST /api/v1/session`.

- Response: the same approval page. With `server_id`, the page also shows the server's name and the host of `redirect_uri`. Approve redirects with 303 to `<redirect_uri>?code=<code>&state=<state>`. Cancel redirects to `<redirect_uri>?error=access_denied&state=<state>`.
- Errors: a malformed query, an unknown `server_id`, or a `redirect_uri` that is not registered gets status 400 and an HTML page. wn-cloud never redirects to a URI it has not matched.
- The code is bound to the user, the challenge, the redirect URI, and the label. `cli_auth_codes` needs a `redirect_uri` column, and `redirect_port` becomes nullable.

wn-server's web UI sign-in: the browser opens wn-server's `/login`. wn-server stores `state` and the verifier for that browser, then redirects it to `<wn-cloud>/cli/auth?state&challenge&redirect_uri=<its registered callback>&server_id=<id>`. The callback on wn-server checks `state` and calls `POST /cli/token` with `redirect_uri`.

### `POST /cli/token` with `redirect_uri` (new, WAX-933)

- Authentication: none.
- Request body: `redirect_uri` takes the place of `redirect_port`. Exactly one of the two is present.

```json
{
  "code": "Jx3...",
  "code_verifier": "...",
  "redirect_uri": "http://192.168.1.20:4533/auth/callback"
}
```

- Response 200: the same body as the existing exchange.
- Errors: as in the existing exchange. Both or neither of `redirect_port` and `redirect_uri` is `invalid_request`. A `redirect_uri` that differs from the one bound to the code is `invalid_grant`.

wn-server rule: one attempt, as in the existing exchange. On failure the web UI shows the error and a link to start again.

## Servers and members (WAX-932)

A server is a record in wn-cloud that names its owner. Its members are the owner and up to 12 listeners. An invite creates a listener membership at once. There is no acceptance step (see [Open questions](#open-questions-for-the-operator)).

All error bodies below carry a `code`. All routes also answer the 401 and 403 answers of the bearer check.

Server object:

```json
{
  "id": "0193a0b2-77c1-7f00-8e21-3d9c5b6a4f10",
  "owner_id": "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70",
  "name": "living-room",
  "redirect_uris": ["http://192.168.1.20:4533/auth/callback"],
  "created_at": "2026-10-16T12:00:00Z"
}
```

Member object:

```json
{
  "user_id": "0192f9aa-0c11-7b3e-8f42-6a5d1e2c3b40",
  "email": "nina@example.com",
  "username": "nina",
  "role": "listener",
  "created_at": "2026-10-16T12:05:00Z"
}
```

`username` is the user's v2 profile username, or `null` when the user has no profile. wn-server uses it as the account's display name and falls back to `email`.

### `POST /servers` (new, WAX-932)

Registers a server under the caller, who becomes its owner.

- Authentication: the server credential (the CLI token that `wn-server login` just received).
- Query parameters: none.
- Request body:

```json
{ "name": "living-room" }
```

`name` is 1 to 100 characters after trimming, with no control characters.

- Response 201: the server object, with `redirect_uris` empty.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `name` is missing or malformed. |

wn-server rule: `wn-server login` calls it once, when the data directory has no server id. It stores the returned `id`. When a server id is already stored, `login` only replaces the server credential, after checking with `GET /me` that the new token belongs to the stored owner. On an unreachable wn-cloud, `login` stops and the owner runs it again. It does not retry on its own, so it does not register twice.

### `PUT /servers/:id/redirect-uris` (new, WAX-932, needed by WAX-933)

Replaces the set of redirect URIs that the browser sign-in may use for this server: the web UI callback on the LAN URL, and later on the relay URL.

- Authentication: the server credential.
- Query parameters: none.
- Request body:

```json
{ "redirect_uris": ["http://192.168.1.20:4533/auth/callback", "https://living-room.relay.waxnerd.com/auth/callback"] }
```

Rules for each entry: an absolute URL with scheme `https`, or `http` when the host is a loopback or private IP literal or ends in `.local`; no user info, no query, no fragment. At most 10 entries. An empty list is allowed and disables the web UI sign-in for the server.

- Response 200: the server object.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | The body or an entry breaks the rules. |
| 403 | `not_owner` | The bearer's user is not the owner of the server. |
| 404 | `server_not_found` | No server has this id. |

wn-server rule: wn-server computes its redirect URIs from its configured URLs at start. It sends them after `login` and at each start when they differ from the last set it sent. On an unreachable wn-cloud it retries on the member refresh tick until it succeeds.

### `GET /servers/:id/members` (new, WAX-932)

Returns the owner and the listeners of a server.

- Authentication: the server credential.
- Query parameters: none. There is no pagination. A server has at most 13 members.
- Request body: none.
- Response 200:

```json
{
  "server_id": "0193a0b2-77c1-7f00-8e21-3d9c5b6a4f10",
  "owner": {
    "user_id": "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70",
    "email": "owner@example.com",
    "username": "adnan",
    "role": "owner",
    "created_at": "2026-10-16T12:00:00Z"
  },
  "members": [
    {
      "user_id": "0192f9aa-0c11-7b3e-8f42-6a5d1e2c3b40",
      "email": "nina@example.com",
      "username": "nina",
      "role": "listener",
      "created_at": "2026-10-16T12:05:00Z"
    }
  ]
}
```

`members` holds the listeners only, ordered by `created_at`. The owner's `created_at` is the server's.

Errors:

| Status | Code | When |
|---|---|---|
| 403 | `not_owner` | The bearer's user is not the owner of the server. |
| 404 | `server_not_found` | No server has this id. |

wn-server rule: see [Rules](#rules-wn-server-applies), member refresh.

### `POST /servers/:id/invites` (new, WAX-932)

Makes a waxnerd user a listener of the server.

- Authentication: the server credential.
- Query parameters: none.
- Request body:

```json
{ "email": "nina@example.com" }
```

The email is matched against `users.email` without regard to case. The user must already have a waxnerd account.

- Response 201: the member object of the new listener.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `email` is missing or malformed. |
| 403 | `not_owner` | The bearer's user is not the owner of the server. |
| 404 | `server_not_found` | No server has this id. |
| 404 | `user_not_found` | No waxnerd user has this email. |
| 409 | `already_member` | The user is already the owner or a listener. |
| 409 | `member_limit` | The server already has 12 listeners. |

wn-server rule: wn-server checks the cap before it calls, and wn-cloud checks it again. On success wn-server adds the member to its cached list at once and creates the listener's account and app password. On an unreachable wn-cloud the web UI shows the error. Invites are not queued.

### `DELETE /servers/:id/members/:userId` (new, WAX-932)

Ends a listener's membership.

- Authentication: the server credential.
- Query parameters: none.
- Request body: none.
- Response: 204, no body.

Errors:

| Status | Code | When |
|---|---|---|
| 403 | `not_owner` | The bearer's user is not the owner of the server. |
| 404 | `server_not_found` | No server has this id. |
| 404 | `not_member` | The user is not a listener of the server. The owner is never a listener, so removing the owner also gets this answer. |

wn-server rule: on 204 or `not_member`, wn-server removes the member from its cached list at once and ends the listener's sessions. On an unreachable wn-cloud the web UI shows the error. The removal is not queued, and the listener keeps access until the owner retries.

## Ratings (WAX-942)

A rating is the caller's rating of one MusicBrainz subject. The routes read and write v2's `subjects` and `ratings` tables. The caller is always the bearer's user. wn-server calls them with the stored CLI token of the account whose rating it is, never with the server credential for another user.

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

Sets the caller's rating of a subject. The body replaces the whole value: a field that is `null` or absent is cleared. This matches `saveMark` in v2.

- Authentication: bearer (the account's CLI token).
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

wn-server rule: wn-server sends both fields from its local store, so a Subsonic `setRating` keeps the like level and a `star` keeps the stars. On an unreachable wn-cloud it writes the value to the outbox. See [Rules](#rules-wn-server-applies).

### `DELETE /ratings/:kind/:mbid` (new, WAX-942)

Clears the caller's rating of a subject. The `ratings` row is deleted. The `subjects` row stays.

- Authentication: bearer (the account's CLI token).
- Query parameters: none.
- Request body: none.
- Response: 204, also when the caller had no rating of the subject.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `:kind` or `:mbid` is malformed. |

wn-server rule: as for `PUT`.

### `GET /ratings/:kind/:mbid` (new, WAX-942)

Returns the caller's rating of one subject.

- Authentication: bearer (the account's CLI token).
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

- Authentication: bearer (the account's CLI token).
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

wn-server rule: see [Rules](#rules-wn-server-applies), rating sync.

## Rules wn-server applies

These restate decisions in `README.md` in terms of the routes above.

### `GET /me`

- wn-server calls `GET /me` when a first-party client exchanges its CLI token for a session (`POST /api/v1/session`), when the web UI sign-in completes, and in `wn-server login` to confirm the owner.
- wn-server caches a 200 answer in memory for 5 minutes, keyed by the SHA-256 hash of the token. Requests during that window do not call wn-cloud.
- A 401 or 403 answer is not cached and not retried. The sign-in is refused.
- When wn-cloud is unreachable, the sign-in fails with an error that tells the client to use the app password. wn-server does not retry.
- Requests that carry a wn-server session or an app password never call `GET /me`.

### Member refresh

- wn-server calls `GET /servers/:id/members` with the server credential every 5 minutes, and at once after a successful invite or removal through wn-server.
- When a user who is not in the cached list exchanges a token, wn-server refreshes once before it refuses the session. This picks up an invite made elsewhere.
- A 200 answer replaces the cached list and records the time as the last successful refresh. A listener who is no longer in the list has their sessions ended.
- When wn-cloud is unreachable, wn-server keeps the cached list and tries again at the next tick.
- A 401, `not_owner`, or `server_not_found` answer means the server credential no longer works. It counts as a failed refresh. The web UI tells the owner to run `wn-server login` again.

### Outage

- Sessions already issued keep working. wn-server checks them against its own store and does not call wn-cloud.
- New sign-ins that need `GET /me` or `POST /cli/token` fail while wn-cloud is unreachable. App passwords keep working.
- A revocation made in wn-cloud during the outage takes effect at the next successful member refresh.
- After 30 days without a successful member refresh, wn-server refuses listener sessions with a distinct error code until a refresh succeeds. The owner's sessions are never refused for this reason.
- The web UI shows a banner with the start of the outage and the cut-off date.

### Rating writes and the outbox

- wn-server writes a rating to its local store first, then calls `PUT` or `DELETE /ratings/:kind/:mbid` with the account's CLI token.
- When wn-cloud is unreachable, wn-server writes the change to a local outbox. The outbox holds one entry per account and subject. A later change replaces the earlier entry.
- wn-server drains the outbox after each successful member refresh, oldest entry first. A 2xx answer removes the entry. An unreachable answer stops the drain until the next tick.
- A 400 answer removes the entry and logs it. A 401 answer keeps the entry and marks the account as needing a new sign-in. The entry is sent when the account has a working token again.
- An account without a wn-cloud token (local-only mode, or a listener who only uses an app password) writes to the local store only. Nothing is queued for it.

### Rating sync

- After the outbox for an account is empty, wn-server calls `GET /ratings?since=<stored cursor>` for that account on each member refresh tick, and follows `more` until it is `false`. It stores `next` after each page.
- A change from wn-cloud overwrites the local value, and wn-server sends Navidrome the rounded value (half stars round up, `love` maps to starred). A `deleted` change clears the local value and unstars and unrates in Navidrome.
- Draining the outbox before pulling keeps a pending local change from being overwritten by an older value from wn-cloud.

## Open questions for the operator

1. **Shape of the server credential.** The README makes it the owner's CLI token, and this document follows that. That token has the owner's full rights at wn-cloud, and revoking it signs the server out. Recommendation: keep the owner's CLI token for v1. Consider a separate server secret, returned by `POST /servers`, when the relay lands.
2. **Scope of tokens issued through the server variant.** A token issued with `server_id` reaches wn-server, which keeps it. A malicious server owner would then hold a listener's full wn-cloud token. Recommendation: wn-cloud marks tokens issued with `server_id` as scoped to `GET /me` and the rating routes, and refuses them elsewhere with 403 `insufficient_scope`.
3. **Invite acceptance step.** This document has none: `POST /servers/:id/invites` creates the membership at once, and the listener's first sign-in at the server is the effective acceptance. Recommendation: keep it that way for v1. Add a pending state only if wn-cloud gets a notifications page.
4. **Invite by email or by user id.** This document takes an email of an existing waxnerd user. It answers `user_not_found` otherwise, which tells the owner whether an email has an account. Recommendation: email for v1. Inviting people without an account (an invite link) is a later item.
5. **Listener cap in wn-cloud.** The README says wn-server enforces the cap of 12. This document has wn-cloud enforce it as well, with `member_limit`. Recommendation: keep both checks, so a member list never exceeds what wn-server accepts.
6. **Pagination of members.** None, since a server has at most 13 members. Recommendation: none.
7. **Deletions in `GET /ratings?since=`.** v2 deletes a rating row on clear, so a delta cannot show clears. Recommendation: wn-cloud records clears in a `rating_deletions` table (user, subject, `deleted_at`) and merges it into the change feed. The alternative, a full re-pull on each tick, costs more as libraries grow.
8. **Ordering of outbox writes against changes made elsewhere.** A queued `PUT` replayed after an outage overwrites a newer value set in wn-cloud's web pages meanwhile. Recommendation: accept this for v1. Later, `PUT` can take an optional `updated_at` from the client, and wn-cloud ignores a write older than the stored value.
9. **Intervals.** The README says "every few minutes" for the member refresh and gives no `GET /me` cache time. This document uses 5 minutes for both, and ties the outbox drain and rating sync to the same tick. Recommendation: 5 minutes.
10. **Redirect URI rules.** This document allows `http` only for loopback and private IP literals and `.local` hosts, and at most 10 entries. Recommendation: accept, since a LAN server usually has no certificate.
11. **First-party app redirects.** The list is configuration in wn-cloud. Recommendation: `waxnerd://auth/callback` for wn-mobile, and the existing loopback `port` flow for wn-desktop, so wn-desktop needs no entry.
