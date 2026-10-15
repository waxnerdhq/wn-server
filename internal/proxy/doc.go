// Package proxy is the HTTP front door. It verifies waxnerd JWTs, checks
// server membership, forwards requests to Navidrome with the Remote-User
// header, and intercepts rating and favorite calls.
package proxy
