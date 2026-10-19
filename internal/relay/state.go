package relay

import "time"

// StateName names a state of the relay package. The texts that each state
// renders are in the table "What the owner sees" in docs/relay.md, section 3.
type StateName string

const (
	// Off means relay is switched off, by the config key or by the web UI's
	// toggle. The package produces it while Options.Enabled answers false.
	// The CLI and the web UI render the off line through State.Text as well.
	Off StateName = "off"
	// Waiting means no server credential or no edge address is available yet.
	Waiting StateName = "waiting"
	// Connecting means the package is dialing and no attempt has failed yet.
	Connecting StateName = "connecting"
	// Unreachable means the package is in backoff after a dropped tunnel, a
	// dial failure, a 429, a 503, or another unexpected answer. Err holds the
	// last error.
	Unreachable StateName = "unreachable"
	// Connected means the tunnel is open. Host is the relay host from
	// Wn-Relay-Host.
	Connected StateName = "connected"
	// NotEnabled means the edge answered 402: the server is not entitled to
	// relay. The package dials again every 10 minutes.
	NotEnabled StateName = "not_enabled"
	// CredentialRejected means the edge answered 401 or 403. The package dials
	// again only with a different token.
	CredentialRejected StateName = "credential_rejected"
	// Refused means the edge answered 400 invalid_request. The package dials
	// again only with a different token.
	Refused StateName = "refused"
	// UpdateRequired means the edge answered 409: it does not speak this
	// version's tunnel protocol. The package dials again only with a different
	// token.
	UpdateRequired StateName = "update_required"
)

// State is the relay package's state as OnState reports it.
type State struct {
	// Name is the state.
	Name StateName
	// Since is when the package entered the state. Repeated failures in the
	// same state keep the first time.
	Since time.Time
	// Host is the relay host from Wn-Relay-Host. It is set in Connected only.
	Host string
	// Err is the last error text. It is empty in Waiting, Connecting, and
	// Connected.
	Err string
}

// Text renders the state as the one line that wn-server status and the web
// UI's status page show.
func (s State) Text() string {
	switch s.Name {
	case Off:
		return "relay: off"
	case Waiting:
		return "relay: waiting for wn-server login"
	case Connecting:
		return "relay: connecting"
	case Unreachable:
		return "relay: unreachable since " + when(s.Since) + ": " + s.Err
	case Connected:
		return "relay: connected since " + when(s.Since) + " at https://" + s.Host
	case NotEnabled:
		return "relay: not enabled at wn-cloud"
	case CredentialRejected:
		return "relay: credential rejected, run wn-server logout, then wn-server login"
	case Refused:
		return "relay: refused by the edge: invalid_request"
	case UpdateRequired:
		return "relay: update wn-server"
	}
	return "relay: " + string(s.Name)
}

// when formats a time as the web UI's outage banner does.
func when(t time.Time) string { return t.Local().Format("2006-01-02 15:04 MST") }
