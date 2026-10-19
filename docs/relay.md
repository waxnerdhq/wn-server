# Waxnerd Relay

Design record for Waxnerd Relay, written 2026-10-18. It states the design of relay v1 and the plan for the phase after it. The research behind it is in [relay-research.md](relay-research.md). The wn-cloud routes it adds are in section 11 and go into [wn-cloud-api.md](wn-cloud-api.md) when the work starts. The diagram is [relay-flow.svg](relay-flow.svg). The Linear project is "Waxnerd Relay" (P-WAX-45, workspace waxnerd, team WAX).

Terms: "the edge" is the public service in wn-cloud that holds the TLS certificate, accepts the tunnels, and forwards requests. "The relay package" is `internal/relay` in wn-server, which opens the tunnel and hands requests to wn-server's own HTTP handler. "The relay host" is a server's public address, `<server id>.relay.waxnerd.com`. There is no separate agent program.

## 1. Overview and user stories

Waxnerd Relay lets the owner of a wn-server reach it from outside the home network without port forwarding, a public IP, or a domain. wn-server opens one outbound connection to the edge and keeps it open. The edge gives the server a public HTTPS address and forwards every request for that address down the connection. Relay is a paid wn-cloud feature.

Relay v1 is a plain proxy. The edge terminates TLS, forwards every request for a server down that server's tunnel, and checks nothing per request. Authentication stays in wn-server, which checks sessions and the app password as it does today. The edge is not a trust boundary.

v1 supports wn-server only. The client in v1 is the owner's browser, using wn-server's web UI. Third-party Subsonic clients are not blocked; the owner uses one in the first milestone to verify audio through the relay. First-party apps do not exist yet. wn-cloud keeps a members list per server from the start; the owner is the only member in v1.

User stories:

1. **The owner enables relay and connects (v1).** The owner has run `wn-server login`, so the server holds a server credential and a server id. The owner sets `relay = true` in `config.toml` and restarts `wn-server serve`. The relay package reads the edge address from wn-cloud, opens the tunnel with the server credential, and names the server id. The edge checks that the credential's user owns the server and that the server is entitled to relay. `wn-server status` and the web UI's status page show "relay: connected since <time> at https://<id>.relay.waxnerd.com". The server page at wn-cloud shows the server as connected.
2. **The owner opens the web UI from outside the home (v1).** The owner opens `https://<id>.relay.waxnerd.com/ui/` on any device, signs in with the account name and the app password, and uses the web UI as on the LAN. The session cookie carries `Secure`. Share links made through the relay carry the relay host. The owner also enters the relay address and the app password in a third-party Subsonic client and hears audio. The web UI does not play audio; that is separate work outside this project.
3. **The owner invites a listener, who streams from a phone outside the home (after v1).** The owner invites a waxnerd user by user name or email at wn-cloud. The invitee accepts at wn-cloud and becomes a member with the role `listener`. The listener's first-party client signs in at wn-cloud, reads the server's addresses from the server record, and obtains a wn-server session through the device code exchange, which wn-server verifies with wn-cloud and checks against its cached member list. wn-server creates a listener account and a non-admin Navidrome user for the listener. The listener streams through the relay. The owner sees members on the server page and can revoke one. This phase is designed in section 5 and built after v1.

## 2. Components

| Component | Where | What it does |
|---|---|---|
| The edge | wn-cloud, a Go service in `relay/` of the v2 repository, Deployment `relay-edge`, one replica | Terminates TLS for `*.relay.waxnerd.com` and `connect.relay.waxnerd.com`. Accepts tunnels at `GET /v1/tunnel` and checks the server credential, the server's owner, and the entitlement once per tunnel. Forwards every request for `<id>.relay.waxnerd.com` down the tunnel of server `<id>` and checks nothing per request. Enforces the caps of section 9 and counts bytes. Writes the relay columns of `servers` and the `relay_usage` table in Postgres with the same database credential as the web service. |
| The relay package | wn-server, `internal/relay` | Dials the edge, runs the handshake, serves the tunneled requests with Go's `net/http` on the multiplexed connection, marks each request as relayed with the client's address, and hands it to wn-server's HTTP handler in process. Reconnects with backoff. Reports its state. |
| `internal/relayreq` | wn-server, standard library only | Holds the request context mark that says a request arrived through the relay and carries the client address. Three functions: `WithRelay(ctx, clientAddr) context.Context`, `IsRelayed(ctx) bool`, and `ClientAddr(r *http.Request) string`, which answers the relayed address when the mark is set and `r.RemoteAddr` without the port otherwise. The web UI, the proxy, and the limiters read it. Only the relay package calls `WithRelay`. |
| wn-cloud pages and routes | wn-cloud, the `web` service | `GET /servers/:id` answers the relay state and the addresses list. The server page shows connected, last seen, streams in progress, and egress this month. The entitlement columns on `servers` gate the tunnel. After v1: members, invites, and the device code page for members. |
| The browser | the owner's device | Uses the web UI at the relay host. Nothing in the browser changes for relay. |
| Navidrome | the server machine | Unchanged. It receives requests on its Unix socket from wn-server's proxy as today, with `X-Forwarded-Host` and `X-Forwarded-Proto` naming the relay host and `https`. |

Authentication happens in wn-server. The edge holds no session, no app password, and no account. It sees the request line and the headers of every request, which it needs to route by host, and it never logs the query string (section 10).

The diagram [relay-flow.svg](relay-flow.svg) shows the tunnel setup, an owner's request through the relay, and the listener sign-in of the later phase.

## 3. Connection

### Transport

The tunnel is one outbound TLS connection from wn-server to `connect.relay.waxnerd.com:443`, multiplexed with yamux (`hashicorp/yamux`, MPL-2.0). After the handshake the edge is the yamux client and opens one stream per forwarded request. wn-server is the yamux server; a yamux session is a `net.Listener`, so the relay package serves it with Go's `http.Server`, which gives each tunneled request an ordinary `*http.Request` with a streaming body, a working `http.Flusher`, and unchanged `Range` and chunked handling. There is no loopback request.

### Handshake

1. wn-server dials TLS to the edge address with ALPN `http/1.1` and verifies the certificate against the system roots.
2. It sends one HTTP/1.1 request: `GET /v1/tunnel` with `Authorization: Bearer <server credential>`, `Upgrade: wn-relay/1`, `Connection: Upgrade`, `Wn-Server-Id: <server id>`, and `Wn-Server-Version: <wn-server version>`.
3. The edge checks the bearer as wn-cloud checks any CLI token, checks that the token's user owns the server, and checks the entitlement (`relay_enabled`, `relay_until`, the grace period of section 9). It answers one of the errors below and closes, or `101 Switching Protocols` with `Wn-Relay-Host: <id>.relay.waxnerd.com` and `Wn-Relay-Session: <session id>`.
4. Both sides start yamux on the same connection. The edge records the session in its in-memory map from server id to session and writes `servers.relay_connected_at` and `servers.relay_last_seen_at`.

The checks run in this order, and the first that fails answers. Steps 2 to 5 count as a failed handshake for the rate limit.

1. The headers: `Upgrade` names a protocol the edge speaks (else 409), and `Wn-Server-Id` is a UUID (else 400).
2. The rate limit: 10 or more failed handshakes from this client address in the last 10 minutes answer 429. The count is in memory in the edge.
3. The bearer check, as for any CLI token (401).
4. The token's user owns the server (403).
5. The entitlement (402).

Handshake errors:

| Status | Code | When | wn-server |
|---|---|---|---|
| 400 | `invalid_request` | A header is missing or malformed, or `Wn-Server-Id` is not a UUID. | Stops dialing; shows "refused by the edge: invalid_request". |
| 401 | (bearer check) | The token is unknown or revoked. | Stops dialing; shows "credential rejected, run wn-server logout, then wn-server login". |
| 402 | `relay_not_enabled` | The server is not entitled, or the grace period has ended. | Shows "not enabled at wn-cloud" and dials again every 10 minutes. |
| 403 | `forbidden` | The token's user does not own the server, or no server has that id. | Stops dialing; shows "credential rejected, run wn-server logout, then wn-server login". |
| 409 | `unsupported_protocol` | The edge does not speak the `Upgrade` protocol version. | Stops dialing; shows "update wn-server". |
| 429 | `rate_limited` | 10 or more failed handshakes from the client address in 10 minutes. | Treated as a drop: backoff and retry. |
| 503 | `cloud_unavailable` | Postgres does not answer. | Treated as a drop: backoff and retry. |

The route also answers the 401 and 403 answers of the bearer check.

A second tunnel for the same server id replaces the first. The edge closes the earlier session. This covers a home IP change that leaves a stale connection, and a restart of wn-server before the edge noticed the old connection drop.

### Keepalive and reconnect

- yamux sends a keepalive ping every 30 seconds on both sides with a 10-second write timeout, so a silent drop is seen within 40 seconds. In-flight streams fail; the client sees a truncated response. A Subsonic `download` resumes with a `Range` request; a transcoded `stream` has no fixed byte offsets and resumption is client-specific.
- The edge writes `servers.relay_last_seen_at` every 60 seconds while the session is alive. It writes `relay_connected_at` and `relay_session_id` at the handshake, and clears both on close only when `relay_session_id` still holds this session's id, so a replacing tunnel is not cleared by the one it replaced. At start the edge clears every `relay_connected_at`, because with one replica no tunnel survives a restart.
- The edge re-checks the credential and the entitlement of every connected tunnel every 10 minutes and closes the session when either fails, with the close reason in the yamux `GoAway` and a log line. wn-server reconnects and receives the handshake error.
- After a drop, a dial failure, a 429, or a 503, wn-server reconnects with exponential backoff: 1 second, doubling to 5 minutes, with full jitter. A 101 resets the backoff.
- A 400, 401, 403, or 409 stops the backoff loop. The package then reads `Credential` every 10 minutes and dials again when the token differs from the one that was refused. With the same token it dials again only after a restart. `wn-server login` writes the store, so a running `serve` picks up a new token within 10 minutes without a restart.
- A 402 dials again every 10 minutes with the same token.
- On `serve` shutdown the relay package closes the tunnel. In-flight relayed requests end the way `srv.Shutdown` ends LAN requests.

### The edge address

wn-server does not derive the edge address from `wn_cloud_url`. While the relay is on (`relay = true` with no switch set, or the switch on), `serve` calls `GET /servers/:id` (section 11) with the server credential at start, every hour, and within 10 seconds of a server credential appearing in the store, and stores `relay.connect_url`, `relay.host`, and `addresses`. The relay package reads the stored `connect_url` through `Options.ConnectURL`. When wn-cloud is unreachable and a value is stored, the relay package dials the stored address, so a wn-cloud outage does not stop a reconnect. When nothing is stored, the relay package waits for the next successful call. The config key `relay_edge_url` overrides the stored value, for a test edge.

### What the owner sees

`serve` writes every change of the relay state to the store (table `relay_state`, one row: state, since, host, last error, written at). The web UI's status page reads it from the in-process holder; `wn-server status` reads the row from the store, without calling wn-cloud, and prints it with the time it was written. Both show one line:

| State | Text |
|---|---|
| `relay = false` or not set with no switch set, or the switch off | `relay: off` |
| no server credential, or no edge address stored yet | `relay: waiting for wn-server login` |
| dialing, no failure yet | `relay: connecting` |
| in backoff after a drop, a dial failure, a 429, or a 503 | `relay: unreachable since <time>: <last error>` |
| connected | `relay: connected since <time> at https://<host>` where `<host>` is `Wn-Relay-Host` from the 101 answer |
| 402 | `relay: not enabled at wn-cloud` |
| 401 or 403 | `relay: credential rejected, run wn-server logout, then wn-server login` |
| 400 | `relay: refused by the edge: invalid_request` |
| 409 | `relay: update wn-server` |

The server page at wn-cloud shows: connected or not, last seen, streams in progress, and egress this month against the caps, with a warning at the soft cap. Section 9 names the columns.

### Configuration

| Key | Default | Meaning |
|---|---|---|
| `relay` | `false` | Open the tunnel when a server credential exists. |
| `relay_edge_url` | empty | Overrides the edge address that `GET /servers/:id` gives. For tests. |
| `relay_max_bitrate` | `0` | Transcoding ceiling in kbps for relayed `/rest/stream` requests. `0` means no cap. Built after milestone 1 (section 9). |

The status page of the web UI has an enable relay and disable relay form for the owner. It stores a switch in the column `enabled` of the `relay_settings` row (migration 012), which is null until the owner sets it. While the column is null the key `relay` applies; once set it overrides the key, and the key is not written. The relay package reads the switch through `Options.Enabled` on the same 10 second poll as the credential: when it turns false the package closes the tunnel the way a context end does and reports `off`, and when it turns true the package dials again. The call of `GET /servers/:id` runs only while the switch is on, so with it off nothing is dialed and wn-cloud is not called. The status page shows the relay host next to the state: the connected host, else `relay_settings.host` when wn-cloud has given one.

### The relay package

`relay.New(relay.Options{...})` and `go rl.Run(ctx)` in `serve`, next to the supervisor and the outbox drain. `Run` returns when `ctx` ends. The options are:

| Option | Type | Who provides it |
|---|---|---|
| `Handler` | `http.Handler` | The proxy handler that `serve` gives to `http.Server`. |
| `Credential` | `func() (token, serverID string, ok bool)` | The CLI, from a store read. |
| `ConnectURL` | `func() (url string, ok bool)` | The CLI, from the stored value or `relay_edge_url`. |
| `Version` | `string` | The CLI. |
| `OnState` | `func(relay.State)` | The CLI wires it to the in-process holder the web UI reads and writes each state to the `relay_state` row for `wn-server status`. |
| `Logger` | `*slog.Logger` | The CLI. |

Before `ServeHTTP` the package reads `X-Relay-Client`, deletes it, and calls `relayreq.WithRelay(ctx, clientAddr)`. `Run` reads `Credential` and `ConnectURL` every 10 seconds until both answer `ok`, so a `wn-server login` after start is picked up without a restart. The package imports the standard library, yamux, and `internal/relayreq`, and nothing else from `internal/`. Everything it needs comes through `Options`, so it can be extracted into its own binary later; that extraction is not designed here.

## 4. Addressing and TLS

- **Public address.** Each server has one host, `https://<server id>.relay.waxnerd.com`, where the label is the server's UUID (36 characters). A shorter label assigned by wn-cloud can be added later as an alias in a new column without changing this one.
- **Why a host per server.** The web UI's cookie (`Path=/ui/`, no `Domain`), its `Origin` check against `r.Host`, and its absolute-path links need no change. Subsonic clients take a plain base URL. Navidrome builds share links from `X-Forwarded-Host` and `X-Forwarded-Proto`, which the proxy sets from the relay host and the relayed mark, so a share made through the relay reads `https://<id>.relay.waxnerd.com/share/<share id>`. A path prefix would need a base path through the web UI and Navidrome, and Navidrome's `BasePath` is process-wide.
- **DNS.** Two records at Cloudflare, both DNS-only (not proxied): `*.relay.waxnerd.com` and `connect.relay.waxnerd.com`, pointing at the cluster's address. Cloudflare's Universal SSL does not cover second-level names, and its CDN terms limit audio traffic on the proxied plans in use.
- **Certificate.** One wildcard certificate for `*.relay.waxnerd.com` and `relay.waxnerd.com`, issued by cert-manager through a DNS-01 solver for Cloudflare with a scoped API token (`Zone:DNS:Edit`, `Zone:Zone:Read`) in a Secret. The `Certificate` lives in the `waxnerd` namespace and the edge mounts its Secret.
- **Exposure.** An `IngressRouteTCP` on Traefik's `websecure` entrypoint with `HostSNI` for `connect.relay.waxnerd.com` and `HostSNIRegexp` for `*.relay.waxnerd.com`, with `tls.passthrough`, so the edge terminates TLS itself. No new port and no firewall change. Whether Traefik's 60-second entrypoint read timeout cuts a passthrough connection is unverified; the first cluster issue tests it and, if needed, sets it to 0 on `websecure` through a k3s `HelmChartConfig`.
- **The client address behind the passthrough.** The edge's TCP peer is Traefik, not the client. The `IngressRouteTCP` service sets `proxyProtocol: { version: 2 }`, so Traefik sends the client address in a PROXY protocol header and the edge reads it before the TLS handshake. Traefik itself sees the client address only when the k3s ServiceLB preserves it, which needs `externalTrafficPolicy: Local` on Traefik's Service; whether the cluster's Traefik Service is set so is unverified, and the first cluster issue checks it. Without both, the per-IP cap at the edge, `X-Relay-Client`, and wn-server's per-address limiters see one address for every client. This is decision 21.
- **Share links.** A share made through the relay carries the relay host, and a share made on the LAN carries the LAN address the client used, as WAX-965 decided. v1 does not set Navidrome's `ShareURL` to the relay host: a LAN-made share would then point outside, and would fail while the relay is off or not entitled. A `ShareURL` setting for owners who want every share to carry the relay host is decision 22.
- **The addresses list.** `GET /servers/:id` answers `addresses`, a list of connection candidates. v1 has one entry, `{ "kind": "relay", "url": "https://<id>.relay.waxnerd.com" }`. First-party clients, when they exist, read the list from wn-cloud and never hard-code the relay host. Later entries of kind `lan` and `direct` come first in the list, and a client tries the entries in order and uses the first that answers. wn-server is the source of the later entries and publishes them to wn-cloud; v1 does not build that.

## 5. Members and listeners

This section is designed now and built in the milestone after v1, except the `server_members` table and the owner row, which v1 creates.

### Membership (v1 creates the table)

`server_members (server_id uuid, user_id uuid, role text, created_at timestamptz)`, primary key `(server_id, user_id)`, `role` in `owner` or `listener`, rows deleted with their server or user. This revives the table of the original WAX-932 text with an `owner` row added. `POST /servers` inserts the owner row, and the migration backfills a row for every existing server. In v1 the owner is the only member, and no route changes membership.

### Invites (after v1)

- The owner invites by waxnerd user name or email on the server page at wn-cloud. wn-cloud resolves a user name to a user at once. An email that belongs to no user is held until a user signs up with it.
- `server_invites (id uuid, server_id, invited_by, email, user_id null, status, created_at, expires_at, decided_at)`, `status` in `pending`, `accepted`, `declined`, `revoked`, `expired`. An invite expires after 14 days.
- The invitee sees pending invites on a page at wn-cloud and accepts or declines. Acceptance inserts a `listener` row in `server_members`. The cap is 12 listeners per server (WAX-936); the owner is not counted. It is checked twice: creating an invite is refused with 409 `member_cap` when the server already has 12 listeners, and acceptance is refused with the same code when the count reached 12 meanwhile. Acceptance is the check that holds.
- The owner revokes a pending invite or removes a member on the server page. Removal deletes the `server_members` row.
- Routes. `GET /servers/:id/members` takes the server credential or the owner's browser session. `POST /servers/:id/invites`, `DELETE /servers/:id/invites/:inviteId`, and `DELETE /servers/:id/members/:userId` take the owner's browser session only; wn-server never changes membership. `GET /invites`, `POST /invites/:id/accept`, and `POST /invites/:id/decline` take the invitee's browser session. Section 11 gives the shapes.
- Invite emails are not in this design. The invitee finds the invite on the page after signing in.

### The listener's sign-in (after v1)

1. The listener signs in at wn-cloud in the first-party client with their waxnerd account.
2. The client lists the servers the user owns or is a member of, reads the addresses list of the chosen server, and obtains a device code bound to that server. The device code page at wn-cloud lists member servers next to owned servers. Whether the paste step remains is the client's concern; the relay design does not change the exchange.
3. The client posts the code to `POST /api/v1/session` at the relay host. The request arrives through the tunnel.
4. wn-server calls `POST /servers/:id/device-codes/verify` with the server credential. The answer gains `role`. wn-server accepts an `owner` as today and a `listener` only when its cached member list holds that user id. Any other user is refused with 403 `not_a_member`, which replaces today's `not_owner` in the listener phase and counts as a failure against the budget of the path the request came in on (section 9).
5. On the first sign-in wn-server creates a listener account (kind `listener`, display name from the answer's email local part, a generated app password that is not shown) and a non-admin Navidrome user through the native API, as it creates the owner's user today. It issues a `wns_` session bound to that account.
6. Every later request carries the session and is checked locally. The proxy forwards it with `Remote-User` set to the listener's Navidrome user name.

The web UI stays owner-only in the listener phase. A listener who opens `/ui/` sees the sign-in form and cannot sign in, because the form takes the owner's account name and app password only.

### The cached member list and the outage rule

- wn-server calls `GET /servers/:id/members` with the server credential every 5 minutes and at `serve` start, and persists the list with the time of the last successful call.
- A member removed at wn-cloud is seen at the next refresh. wn-server disables the listener's account, ends its sessions, and stops forwarding requests for it. The Navidrome user stays, because Navidrome 0.64.2 has no disable flag for a user. Re-adding the member re-enables the account.
- The owner is never refused because wn-cloud is unreachable. There is no cut-off for the owner.
- A listener is served from the cached list for 30 days after the last successful refresh. After 30 days wn-server refuses listener sessions, new and existing, with 503 `cloud_unreachable` until a refresh succeeds. On `/rest/*` the refusal is Subsonic error 0. The owner's sessions are not affected. This is the bound of WAX-945, decision 2, unchanged.
- New listener sessions need a verify call and fail while wn-cloud is unreachable, as owner sessions do today.

### Listener ratings

A listener's ratings in v1 of the listener phase stay in wn-server's store and in Navidrome (option (a) of the research). The `ratings` table already keys by account id. A listener's Subsonic `setRating`, `star`, or `unstar` is forwarded to Navidrome and, on `ok`, written to the local store under the listener's account. It is not queued in the outbox, which sends with the server credential as the owner. The reconcile loop writes the owner's rows only. `/api/v1/ratings` and `/api/v1/ratings/changes` for a listener session read and write the listener's rows. The design target is option (c): the first-party client writes the listener's ratings to wn-cloud with the listener's own account, and wn-server mirrors them from a per-member change feed it reads with the server credential. wn-server never writes to wn-cloud on behalf of a member. The change feed route is specified when the first-party client exists.

### Caps

12 listeners per server, enforced by wn-cloud when an invite is accepted. The edge cannot count identities; its caps are per server and per client address (section 9).

## 6. Owner access through the relay

- **The web UI (v1).** The owner opens `https://<id>.relay.waxnerd.com/ui/`. The sign-in form takes the account name and the app password, as on the LAN. The cookie is `wn_session`, `HttpOnly`, `SameSite=Lax`, `Path=/ui/`, and `Secure`, because the request carries the relayed mark. The `Origin` check compares with `r.Host`, which is the relay host. The web UI changes in two places: the `Secure` read, and the failed-sign-in limiter, which keys by `relayreq.ClientAddr(r)` and keeps separate totals for LAN and relayed requests (section 9).
- **Third-party Subsonic clients (not promised, used for verification).** The relay address is a base URL and the app password is the password. The first milestone verifies audio this way. Nothing is designed or tested beyond that.
- **First-party clients (when they exist).** The client signs in at wn-cloud, reads the addresses list, and runs the device code exchange against the relay host. v1 answers 403 `not_available` to `POST /api/v1/session` on a relayed request, because no first-party client exists and the web UI signs in with the app password. The listener milestone opens the route with the split budgets of section 9. A session issued through the relay is a wn-server session like any other: valid on this server only, on the LAN and through the relay alike.
- **Direct connection later.** A client with the addresses list tries a LAN or direct address before the relay address. wn-server does not change for this in v1; section 13 states what stays open.

## 7. What passes and what does not

The edge forwards every request for a known host down its tunnel. wn-server's router is the allowlist, unchanged from the LAN. The edge refuses three things before forwarding, none of which needs per-request authentication.

| Request | Through the relay | Where decided |
|---|---|---|
| `/rest/*` (Subsonic) | Passes. Session or app password checked by wn-server. | wn-server |
| `/share/*` | Passes without a check, as on the LAN. A share made through the relay is reachable by anyone with the link. | wn-server |
| `/api/v1/*` | Passes. `POST /api/v1/session` answers 403 `not_available` in v1. | wn-server |
| `/`, `/ui/*` | Passes. | wn-server |
| Any other path | 404 from wn-server's router, as on the LAN. Navidrome's native API, web UI, and `/api/events` are not reachable. | wn-server |
| `Upgrade` or `Connection: Upgrade` | 400 at the edge. Nothing behind the relay speaks WebSocket. | the edge |
| Method not GET, HEAD, POST, PUT, or DELETE | 405 at the edge. | the edge |
| Body over 1 MiB | 413 at the edge. | the edge |
| Host with no tunnel, including a host that no server has | 502 `server_offline` at the edge, as a JSON body or a plain HTML page by `Accept`. The edge looks up no database per request, so it does not tell the two apart. | the edge |

Headers. The edge forwards the request headers unchanged, except that it removes any incoming `X-Relay-Client`, `Forwarded`, and `X-Forwarded-*` header and sets `X-Relay-Client` to the client's IP address. The edge forwards with `httputil.ReverseProxy` and a `Rewrite` function, not a `Director`, so it adds no `X-Forwarded-For`. The relay package moves `X-Relay-Client` into the request context and deletes it. The mark is a context value, so a header from a LAN client sets nothing; a new step at the top of the LAN handler deletes an incoming `X-Relay-Client` so it reaches neither the logs nor Navidrome. Today the proxy deletes `Forwarded` and `X-Forwarded-*` on the outgoing request to Navidrome only; that stays, and wn-server trusts no `X-Forwarded-*` header from any path. Toward Navidrome the proxy sets `X-Forwarded-Host` from `r.Host` and `X-Forwarded-Proto` to `https` when the request is relayed, as it sets them from `r.TLS` today.

Responses pass unchanged. The edge reads the response `Content-Type` to count audio responses and counts the bytes it writes to the client. It never reads the query string for the caps.

## 8. Outage rules

The LAN keeps working in every case. The relay package logs its failures and never stops `serve`.

| Outage | Through the relay | On the LAN | What the owner sees |
|---|---|---|---|
| wn-cloud's web service or Postgres down | Open tunnels stay open; the edge keeps forwarding. New handshakes fail (the edge answers 503 `cloud_unavailable`) and wn-server retries with backoff. The 10-minute re-check treats a database error as "keep". A host with no tunnel gets 502 as always. `GET /servers/:id` fails; the stored edge address is used. | Unchanged. Sessions, the app password, and the outbox work as today. | "wn-cloud unreachable since <time>" from the existing status, and "relay: connecting" if the tunnel was not open. |
| The edge down or unreachable | The relay host answers nothing or a TLS error. wn-server reconnects with backoff. | Unchanged. | "relay: unreachable since <time>". |
| wn-server down | The edge answers 502 `server_offline` for the host. | Down. | Nothing until it restarts; the server page at wn-cloud shows "last seen <time>". |
| Navidrome down | wn-server answers 503 as on the LAN while the supervisor restarts Navidrome. The web UI's sign-in and status pages work. | Same. | The existing supervisor messages. |
| wn-server's credential revoked | The 10-minute re-check closes the tunnel; the handshake answers 401. | Unchanged; sessions and the app password do not depend on the credential. | "relay: credential rejected, run wn-server logout, then wn-server login". |

For listeners (after v1): the cached member list serves listeners through a wn-cloud outage for 30 days, then refuses them until a refresh succeeds (section 5). The owner is never refused.

## 9. Billing and limits

### Subscription state

v2 has no payment provider. The entitlement lives on `servers`:

| Column | Type | Meaning |
|---|---|---|
| `relay_enabled` | `boolean not null default false` | Relay is granted. |
| `relay_until` | `timestamptz null` | End of the paid period. `null` means no end. |

Relay is on when `relay_enabled` is true and `relay_until` is null or in the future. In v1 an operator sets the columns with SQL. Later a payment provider's webhook handler writes them, and a `subscriptions` table can be added then without changing the edge. The handshake query reads one row.

### Lapse

- The gate is at connect time. The edge checks the columns at every handshake and re-checks every connected tunnel every 10 minutes.
- After `relay_until` passes, a grace period of 7 days keeps the tunnel open and allows reconnects. The server page shows "relay ends on <date>" during the grace period.
- After the grace period the re-check closes the tunnel with the reason `relay_not_enabled`, and the handshake refuses with 402 `relay_not_enabled`. wn-server shows "relay: not enabled at wn-cloud" and retries every 10 minutes, so a renewed subscription reconnects without the owner's action.
- Setting `relay_enabled` to false has no grace period.

### Caps at the edge

| Cap | Enforced | Value | At the cap |
|---|---|---|---|
| In-flight requests per server | per tunnel | 32 | 429 with `Retry-After: 2` |
| Concurrent audio responses per server | per tunnel, by response `Content-Type` `audio/*` | 6 | 429. The edge cancels the stream and answers before writing a body. |
| Egress per server per calendar month (UTC) | bytes written to clients, per tunnel, persisted to `relay_usage` | 200 GB soft, 300 GB hard | Soft: a warning on the server page. Hard: 503 `egress_cap` on audio responses until the month ends; the web UI and the API still pass. |
| Request body | per request | 1 MiB | 413 |
| Requests per client IP, all servers | per address | 600 per minute | 429 |
| Listeners per server, the owner not counted | wn-cloud, at invite creation and at acceptance (after v1) | 12 | 409 `member_cap` |

Each number is a starting value and a config value of the edge; a change needs no code. The monthly egress counts bytes to clients only. The home upload to the edge is ingress, which the provider does not bill (unverified for the provider in use).

### Bitrate cap (after milestone 1)

On a relayed `/rest/stream` the proxy sets `maxBitRate` to `relay_max_bitrate` when the parameter is absent or higher, and drops `format=raw`, so Navidrome transcodes to the ceiling. `download` is not capped. The default is 0 (no cap). The cap reduces bytes per stream for transcodable formats; it does not limit the transfer rate of a `Range` request.

### Metrics at wn-cloud

The edge writes `servers.relay_connected_at`, `servers.relay_session_id`, `servers.relay_last_seen_at` (every 60 seconds), and `servers.relay_streams` (on change), and upserts `relay_usage (server_id, month, bytes)` every minute while bytes flow and at response end. The server page reads these. No Redis; Postgres is the only shared state.

### wn-server's limiters behind the relay

- The per-address limits of the three failed-sign-in limiters key by `relayreq.ClientAddr(r)`, which is the forwarded address for a relayed request and `r.RemoteAddr` otherwise.
- The web sign-in total of 100 failures per 10 minutes becomes two totals, 100 for LAN requests and 100 for relayed requests, so an attack from outside does not lock the LAN form.
- `POST /api/v1/session` answers 403 `not_available` on a relayed request in v1 and records no failure. The listener milestone opens it with a split of the total of 10 failures per 10 minutes: 5 for LAN and 5 for relayed requests. The per-address limit stays 5 for LAN addresses and is 2 per relayed client address. The totals together stay at the 10 that wn-cloud allows per server credential. `not_a_member` counts as a failure, as `not_owner` does today.
- The Subsonic password limit (10 per address per minute) is unchanged; it has no total.

## 10. Security

- The edge holds nothing that v2's web service does not already handle: it reads CLI tokens through the same hash lookup as `requireUser`, reads `servers`, and writes the relay columns. It holds no session, no app password, and no account. Its secrets are the wildcard certificate and the database credential, which is the one the web service uses.
- The edge checks nothing per request. It routes by host, enforces the caps, and forwards. Authentication is wn-server's, so a routing bug in the edge grants nothing that an unauthenticated LAN client does not have. The edge terminates TLS, so it sees every relayed request in clear, including the app password or token in a Subsonic query string and the `wn_session` cookie. It is trusted for confidentiality: it reads the request line and the headers to route and to count, keeps nothing, and logs no query string and no credential header.
- The relay package only hands requests to wn-server's own handler in process. There is no second listener and no loopback socket. The blocks on Navidrome's native API and web UI apply unchanged.
- The relayed mark is a context value that only in-process code can set. No header from any client sets it. The LAN handler deletes an incoming `X-Relay-Client`.
- A wn-server session is scoped to one server. It is a `wns_` token checked against the server's own store. It does not work on another server, through the relay or otherwise. Nothing from wn-cloud is stored for a device.
- The tunnel is authenticated once with the server credential, which identifies the owner, and names the server id. The edge refuses a server the token's user does not own. A second tunnel for the same server replaces the first, so a stolen credential could take over a server's relay host; `wn-server logout` revokes the token at wn-cloud and ends that.
- Rate limits at the edge: 600 requests per minute per client IP, 32 in-flight requests per server, 1 MiB bodies. wn-server's sign-in limiters key by the forwarded client address with split totals for relayed requests.
- Logging. The edge logs one line per request with the time, the server id, the client IP, the method, the path without the query string, the status, the bytes sent, and the duration. It never logs the query string, because Subsonic clients put `p`, `t`, `s`, and `apiKey` in it, and never logs the `Authorization` or `Cookie` headers. Handshake lines carry the server id and the user id, never the bearer. wn-server's request log lines for relayed requests carry `relay=true` and the forwarded client address.
- The relay is off by default. A server with `relay = false` and no switch set on the status page never dials the edge.

## 11. wn-cloud contract additions

These sections go into [wn-cloud-api.md](wn-cloud-api.md) in its format when the issues are filed. The issue numbers replace "WAX-relay".

### Relay (WAX-relay)

The server object gains two fields:

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

`relay.enabled` is true when `relay_enabled` is true and `relay_until` is null, in the future, or within the 7-day grace period. `relay.until` is `relay_until` or null. `relay.connected` is true when `relay_connected_at` is not null. `relay.last_seen_at` is `relay_last_seen_at` or null. `addresses` has one entry in v1; later kinds come before `relay`. `POST /servers` answers the same object with `relay.enabled` false.

#### `GET /servers/:id` (WAX-relay)

Answers one server.

- Authentication: bearer (the server credential). The bearer's user must own `:id`. A browser session of the owner is also accepted.
- Path parameters: `:id`, a UUID.
- Response 200: the server object above.

Errors:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `:id` is not a UUID. |
| 403 | `forbidden` | The caller does not own `:id`, or no server has that id. |

The route also answers the 401 and 403 answers of the bearer check.

The checks run in this order: the bearer check (401), `:id` is a UUID (400), the caller owns the server (403). The route has no rate limit, as `POST /servers` has none.

wn-server rule: `serve` calls it at start while the relay is on, every hour, and within 10 seconds of a server credential appearing in the store. It stores `relay.connect_url`, `relay.host`, and `addresses`, and uses the stored values when wn-cloud is unreachable. A 401 sets the shared wn-cloud status to "credential rejected" and the next call runs in one hour; the relay package itself stops dialing when the edge answers 401 for the same token.

#### `GET /v1/tunnel` on the edge (WAX-relay)

The tunnel handshake. The host is `connect.relay.waxnerd.com`, which is `relay.connect_url` from `GET /servers/:id`, not the API host.

- Authentication: bearer (the server credential). The bearer's user must own the server in `Wn-Server-Id`.
- Request headers: `Upgrade: wn-relay/1`, `Connection: Upgrade`, `Wn-Server-Id` (a UUID), `Wn-Server-Version`.
- Response 101 with `Wn-Relay-Host` and `Wn-Relay-Session`. After the response both sides speak yamux on the connection; the edge opens streams, wn-server accepts them and serves HTTP/1.1 on each.
- Errors and check order: the table and the list in section 3. 10 or more failed handshakes from one client address in 10 minutes answer 429 `rate_limited`; the count is in memory in the edge. The route also answers the 401 and 403 answers of the bearer check.

Relayed requests carry `X-Relay-Client` with the client's IP address and no other header the client did not send. The edge removes incoming `X-Relay-Client`, `Forwarded`, and `X-Forwarded-*` headers.

#### In wn-cloud (WAX-relay)

A migration adds `servers.relay_enabled`, `servers.relay_until`, `servers.relay_connected_at`, `servers.relay_session_id`, `servers.relay_last_seen_at`, `servers.relay_streams`, the table `relay_usage (server_id, month date, bytes bigint, primary key (server_id, month))`, and the table `server_members` with the owner rows. `POST /servers` inserts the owner row and answers the server object with the relay fields. The route is in `web/src/servers.ts`. The edge is in `relay/` and reads Postgres with the web service's `DATABASE_URL`.

Edits to the existing text of `wn-cloud-api.md`: the route summary gains `GET /servers/:id`; the statement that relay is not specified yet points to `relay.md`; the server object in "Servers (WAX-932)" gains the `relay` and `addresses` fields; the `POST /servers` section gains the owner row.

### Members and invites (WAX-relay, after v1)

Member object:

```json
{ "user_id": "0192f1c4-...", "name": "ada", "role": "listener", "created_at": "2026-11-02T10:00:00.123456Z" }
```

Invite object:

```json
{ "id": "0193b1c2-...", "server_id": "0193a0b2-...", "email": "ada@example.com", "user_id": null, "status": "pending", "created_at": "...", "expires_at": "...", "decided_at": null }
```

`status` is `pending`, `accepted`, `declined`, `revoked`, or `expired`. `user_id` is set when the invite names a user or when a user with the email signs up. Every route below also answers the 401 and 403 answers of the bearer or session check, and 400 `invalid_request` when a path id is not a UUID.

#### `GET /servers/:id/members` (WAX-relay)

Lists the members of a server.

- Authentication: bearer (the server credential), or a browser session of the owner. The caller must own `:id`.
- Response 200: `{ "members": [ ...member objects... ] }` ordered by `created_at`. The owner row comes first.

| Status | Code | When |
|---|---|---|
| 403 | `forbidden` | The caller does not own `:id`, or no server has that id. |

wn-server rule: called every 5 minutes and at `serve` start; the list and the time of the last success are persisted; listeners are served from the list for 30 days after that time, then refused with 503 `cloud_unreachable`; a listener removed from the list loses their sessions at the refresh that no longer holds them.

#### `POST /servers/:id/invites` (WAX-relay)

Invites a waxnerd user to a server as a listener.

- Authentication: a browser session of the owner only. A bearer gets 403 before the body is read.
- Request body: `{ "user": "ada" }` or `{ "user": "ada@example.com" }`. A string with `@` is an email; otherwise a user name. Trimmed; 1 to 320 characters.
- Response 201: the invite object. An invite expires 14 days after `created_at`.

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | The body is not a JSON object, or `user` is missing, not a string, or malformed. |
| 403 | `forbidden` | The caller does not own `:id`, or no server has that id. |
| 404 | `user_not_found` | A user name that no user has. An email is accepted whether or not a user has it. |
| 409 | `already_member` | The user is the owner or a listener of the server. |
| 409 | `already_invited` | A pending invite for the same user or email exists. |
| 409 | `member_cap` | The server has 12 listeners. |

#### `DELETE /servers/:id/invites/:inviteId` (WAX-relay)

Revokes a pending invite. Sets `status` to `revoked`.

- Authentication: a browser session of the owner only.
- Response 204.

| Status | Code | When |
|---|---|---|
| 403 | `forbidden` | The caller does not own `:id`. |
| 404 | `not_found` | No pending invite with that id belongs to `:id`. |

#### `DELETE /servers/:id/members/:userId` (WAX-relay)

Removes a listener. Deletes the `server_members` row.

- Authentication: a browser session of the owner only.
- Response 204.

| Status | Code | When |
|---|---|---|
| 400 | `invalid_request` | `:userId` is the owner. |
| 403 | `forbidden` | The caller does not own `:id`. |
| 404 | `not_found` | No listener row for `:userId` on `:id`. |

#### `GET /invites` (WAX-relay)

Lists the caller's pending invites, with the server name and the inviter's name.

- Authentication: a browser session.
- Response 200: `{ "invites": [ { ...invite object..., "server_name": "living-room", "invited_by_name": "adnan" } ] }`. Invites that name the caller's user id or the caller's verified email.

#### `POST /invites/:id/accept` (WAX-relay)

Accepts an invite. In one transaction: counts the server's listeners, inserts the `listener` row, and sets `status` to `accepted`.

- Authentication: a browser session of the invitee.
- Response 200: the member object.

| Status | Code | When |
|---|---|---|
| 404 | `not_found` | No pending invite with that id names the caller. |
| 409 | `member_cap` | The server has 12 listeners. |
| 410 | `expired` | `expires_at` has passed. The row is marked `expired`. |

#### `POST /invites/:id/decline` (WAX-relay)

Declines an invite. Sets `status` to `declined`.

- Authentication: a browser session of the invitee.
- Response 204.

| Status | Code | When |
|---|---|---|
| 404 | `not_found` | No pending invite with that id names the caller. |

#### `GET /servers` for a session (WAX-relay)

Lists the servers the caller owns or is a member of, for first-party clients.

- Authentication: a browser session, or a GoTrue bearer of a first-party client. Not the server credential.
- Response 200: `{ "servers": [ { ...server object..., "role": "owner" | "listener" } ] }` ordered by `created_at`.

#### Changes to existing routes (WAX-relay)

- `POST /servers/:id/device-codes/verify` answers `role` (`owner` or `listener`) next to `user_id` and `email`. The check order is unchanged.
- The device code page (`GET` and `POST /devices`) lists servers the user owns or is a member of, with the role shown, and `POST /devices` accepts a server the user is a member of. A server the user neither owns nor is a member of gets the page with an error line and status 400, as today for a server the user does not own.

wn-server rules: a `listener` role in the verify answer is accepted only when the cached list holds the user id, else 403 `not_a_member`, which counts as a failure against the budget of the path the request came in on; the owner is accepted as today.

### The stub (`internal/wncloud/stub`)

- v1: `GET /servers/{id}` with the relay and addresses fields, with `relay.connect_url` set to a value the test chooses; `Stub.SetRelay(serverID string, enabled bool, until time.Time)`, `Stub.SetRelayConnected(serverID string, connected bool)`, and `Stub.SetRelayConnectURL(url string)` as seed helpers; `serverJSON` gains the fields; `AddServer` and `createServer` insert the owner member row.
- The tunnel is not stubbed in `wncloud/stub`. The relay package's tests use an in-process edge in `internal/relay` that speaks the handshake and yamux over a `net.Pipe` or a loopback listener, with `relay_edge_url` pointing at it.
- After v1: the member and invite routes, `Stub.AddMember(serverID, userID, role string)`, and `role` in the verify answer.

## 12. Decisions

Decided by the owner on 2026-10-18, with the alternatives that were considered.

| # | Decision | Chosen | Not chosen |
|---|---|---|---|
| 1 | Tunnel transport | yamux over one outbound TLS connection to a custom Go edge. | Reversed HTTP/2 (deprecated x/net entry points, stdlib path unverified); WebSocket with yamux (only needed across an HTTP-terminating proxy); Cloudflare Tunnel and Tailscale Funnel (no per-connection gate for wn-cloud, vendor host names and terms). |
| 2 | Edge exposure on the cluster | TLS passthrough: `IngressRouteTCP` on `websecure` with SNI rules, the edge terminates TLS. The 60-second read timeout is tested first. | Traefik terminating TLS (forces WebSocket transport); a `hostPort` such as 8443 with a firewall change. |
| 3 | Wildcard TLS | cert-manager DNS-01 solver for Cloudflare with a scoped API token, one `Certificate` for `*.relay.waxnerd.com` and `relay.waxnerd.com`. | One certificate per server through HTTP-01 (an Ingress per server, Let's Encrypt limits). |
| 4 | DNS records | DNS-only records at Cloudflare for `*.relay.waxnerd.com` and `connect.relay.waxnerd.com`. | First-level names such as `<id>-relay.waxnerd.com` behind the proxy (CDN audio terms). |
| 5 | Edge code placement | A Go service in `relay/` of the v2 repository, a third image from the existing pipeline, deployed from `deploy/k8s`. | Inside the Express web service (no maintained yamux for Node; a web rollout drops every tunnel). |
| 6 | Edge replicas | One replica, `strategy: Recreate`, in-memory map from server id to tunnel. A rollout drops every tunnel; they reconnect within seconds. | A Postgres registry with forwarding between pods. |
| 7 | Public address | A host per server. | A path prefix on one host (base path through the web UI and Navidrome). |
| 8 | The host label | The server UUID. | A short label assigned at registration; can be added later as an alias. |
| 9 | The edge address in wn-server | Read from `GET /servers/:id`, cached in the store, `relay_edge_url` overrides. | A fixed default derived from `wn_cloud_url`. |
| 10 | The TLS mark and the client address | `internal/relayreq` context value set only by the relay package; `X-Relay-Client` from the edge, moved into the context and deleted; the LAN path strips it. | A synthetic `r.TLS` on the request. |
| 11 | The relay switch | `relay = true` in `config.toml`, default false; a switch on the web UI's status page overrides it without a restart. | Always dial when linked and let the edge refuse. |
| 12 | Device code exchange through the relay in v1 | 403 `not_available` on a relayed request. The listener milestone opens it with 5 LAN and 5 relayed failures per 10 minutes, 5 per LAN address as today and 2 per relayed address. | Open in v1 with the split. |
| 13 | Other limit totals | Web sign-in: 100 LAN and 100 relayed failures per 10 minutes. Per-address limits key by the forwarded address. | One shared total. |
| 14 | Caps at the edge | 32 in-flight, 6 audio, 200 GB soft and 300 GB hard per month, 1 MiB body, 600 requests per minute per client IP, 12 listeners. | Other numbers; the values are edge config. |
| 15 | Bitrate cap | Rewrite `maxBitRate` and drop `format=raw` on relayed `/rest/stream`; `relay_max_bitrate` default 0; built after milestone 1. | A separate Navidrome player for relayed requests; default 320. |
| 16 | Subscription state | `servers.relay_enabled` and `servers.relay_until`, written by SQL in v1 and by a webhook later. | A `subscriptions` table now. |
| 17 | Lapse | 7-day grace, then the 10-minute re-check closes the tunnel and the handshake answers 402 `relay_not_enabled`. | Another grace period. |
| 18 | Listener sign-in | Device code page lists owned and member servers; verify answers `role`; wn-server checks its cached member list (5-minute refresh, 30-day bound), creates a listener account and a non-admin Navidrome user. The web UI stays owner-only. | Listener sign-in at the edge (the edge is not a trust boundary). |
| 19 | Listener ratings | Target: the first-party client writes to wn-cloud with the listener's account and wn-server mirrors from a per-member feed. Until then, local only. | wn-server writing on behalf of a member with the server credential. |
| 20 | Invites | By user name or email, accept and decline by the invitee at wn-cloud, revoke by the owner, the WAX-932 routes revived, the cap of 12 listeners from WAX-936, checked at creation and at acceptance. | Invite emails; invites by link. |

Added after the design review and decided by the owner on 2026-10-18 with the recommendation of each. The cluster items are marked for the owner's review in their Linear issues (WAX-982, WAX-983, WAX-985).

| # | Decision | Chosen | Not chosen |
|---|---|---|---|
| 21 | The client address behind the TLS passthrough | Traefik sends PROXY protocol v2 to the edge (`proxyProtocol` on the `IngressRouteTCP` service) and the edge reads the client address from it. The first cluster issue checks that the k3s ServiceLB preserves the source address for Traefik (`externalTrafficPolicy: Local`) and sets it if not. | Accept one address for every client, which disables every per-address limit through the relay. |
| 22 | Share links made on the LAN | No `ShareURL` in v1. A share carries the address the client used when it made it. | A `relay_share_url` setting that passes the relay host to Navidrome as `ShareURL`, for owners who share from the LAN to people outside. |
| 23 | The DNS-01 issuer | A second `ClusterIssuer`, `letsencrypt-dns`, with the Cloudflare solver, so the existing `letsencrypt` HTTP-01 issuer and the sites that use it are untouched. | A DNS-01 solver added to the existing `letsencrypt` issuer with a selector. |
| 24 | The Cloudflare token Secret | In cert-manager's namespace, which is where a `ClusterIssuer` reads its Secrets (cert-manager's cluster resource namespace, `cert-manager` by default). The token has `Zone:DNS:Edit` and `Zone:Zone:Read` on `waxnerd.com` only. | A namespaced `Issuer` in `waxnerd` with the Secret next to it. |
| 25 | The edge's database access | The same `DATABASE_URL` Secret as the `web` Deployment, over Tailscale as the web pods reach Postgres today. | A separate Postgres role for the edge with grants on `servers`, `relay_usage`, and the token tables only. |
| 26 | The edge's probe and resources | Readiness and liveness on `GET /health` on a separate port that is not exposed, answering the tunnel count. Requests 100m CPU and 128 Mi, limits 500m and 512 Mi, to be adjusted from the first weeks of metrics. | Other values. |

Still open, to be settled by the first cluster issue: whether Traefik's entrypoint read timeout cuts a passthrough connection, whether the ServiceLB preserves the source address, and the exact scope of the Cloudflare API token for cert-manager. Unverified facts that the design relies on are marked in [relay-research.md](relay-research.md): the provider's ingress billing, the cluster's port speed, and the Cloudflare proxy range membership of `v2.waxnerd.com`.

## 13. Out of scope for v1

| Not in v1 | What v1 does to keep it possible |
|---|---|
| Invites and listeners | Creates `server_members` with the owner row. Designs the routes, the member cache, the 30-day bound, and the listener account (section 5). Keys the limiters by the forwarded client address and keeps the device code route refusable per path, so opening it is one change. |
| First-party apps | Adds the addresses list to the server record. Refuses `POST /api/v1/session` through the relay with a code the client can show. Changes nothing in the device code exchange. `GET /servers` for a session and `role` in the verify answer are specified in section 11. |
| Other Subsonic servers behind the relay | The edge forwards HTTP for a host and checks nothing per request, so another origin could sit behind a tunnel. The handshake, the entitlement, and the addresses list are per server record, not per wn-server. Nothing is tested or promised. |
| Third-party Subsonic clients through the relay | Not blocked. The owner's app password works at the relay host. The bitrate cap, when built, applies to them. |
| Direct connections (Plex-like) | The addresses list has a `kind` field and an order. wn-server will publish LAN and direct addresses to wn-cloud; the client picks the first that answers. The relayed mark and the forwarded address are per request, so a server that serves both paths needs no change in the handlers. |
| Per-server TLS for direct connections | Not designed. A direct address needs a certificate the client trusts; the relay host's certificate belongs to the edge. The `addresses` entry can carry a certificate fingerprint or a wn-cloud-issued name later. |
| More than one edge replica | The edge writes `relay_connected_at` and `relay_last_seen_at` to Postgres already. A registry table and forwarding between pods are added when needed. |
| Payments | The entitlement columns are the gate. A provider's webhook writes them later. |
| Audio playback in the web UI | Separate work outside this project. The relay passes audio for any client that requests it. |
