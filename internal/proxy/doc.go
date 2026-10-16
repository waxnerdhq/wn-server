// Package proxy is the HTTP front door. It forwards the Subsonic API
// (/rest/*) after checking the account's credentials, and public shares
// (/share/*) without a check, to Navidrome on loopback once the supervisor
// reports ready. Every other path answers 404. Sessions and rating
// intercepts are added in later issues.
package proxy
