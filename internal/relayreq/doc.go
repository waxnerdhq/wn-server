// Package relayreq holds the request context mark that says a request
// arrived through the Waxnerd Relay, and the client address that came with
// it. Only the relay package calls WithRelay. The web UI, the proxy, and the
// failed-sign-in limiters call IsRelayed and ClientAddr. A header from a
// client never sets the mark. The package imports the standard library only.
package relayreq
