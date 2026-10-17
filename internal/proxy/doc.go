// Package proxy is the HTTP front door. It forwards the Subsonic API
// (/rest/*) after checking a wn-server session or the account's app
// password, and public shares (/share/*) without a check, to Navidrome on
// loopback once the supervisor reports ready. It serves wn-server's own
// /api/v1/* routes, among them the device code exchange at /api/v1/session.
// Every other path answers 404. Rating intercepts are added in later issues.
package proxy
