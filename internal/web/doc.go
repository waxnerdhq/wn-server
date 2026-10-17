// Package web serves wn-server's own pages under /ui/: owner sign-in with
// the account name and the app password, the status page, and the account
// page. A sign-in creates a wn-server session, which the browser holds in
// the wn_session cookie. The match page lists the match queue and starts
// apply, revert, and import runs as background jobs.
package web
