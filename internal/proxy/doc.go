// Package proxy is the HTTP front door. It forwards requests to Navidrome
// on loopback once the supervisor reports ready. Session checks, the
// Remote-User header for authenticated requests, and rating intercepts are
// added in later issues.
package proxy
