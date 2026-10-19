package wncloud

import (
	"context"
	"net/http"
	"net/url"
	"time"
)

// Server is the server object of GET /servers/:id (docs/wn-cloud-api.md,
// Servers and Relay).
type Server struct {
	ID        string      `json:"id"`
	OwnerID   string      `json:"owner_id"`
	Name      string      `json:"name"`
	CreatedAt time.Time   `json:"created_at"`
	Relay     ServerRelay `json:"relay"`
	Addresses []Address   `json:"addresses"`
}

// ServerRelay is the relay field of the server object.
type ServerRelay struct {
	// Enabled is true when the server is entitled to relay.
	Enabled bool `json:"enabled"`
	// Until is the end of the paid period, or nil.
	Until *time.Time `json:"until"`
	// ConnectURL is the edge's tunnel URL.
	ConnectURL string `json:"connect_url"`
	// Host is the relay host, <server id>.relay.waxnerd.com.
	Host string `json:"host"`
	// Connected is true while a tunnel of the server is open at the edge.
	Connected bool `json:"connected"`
	// LastSeenAt is the last time the edge saw the tunnel, or nil.
	LastSeenAt *time.Time `json:"last_seen_at"`
}

// Address is one connection candidate of the server. v1 has the kind
// "relay" only.
type Address struct {
	Kind string `json:"kind"`
	URL  string `json:"url"`
}

// GetServer returns one server (GET /servers/:id). The token's user must own
// it; another user's server, or an unknown id, gives a 403 forbidden
// (ErrForbidden), and a revoked token gives ErrUnauthorized.
func (c *Client) GetServer(ctx context.Context, token, serverID string) (Server, error) {
	var out Server
	err := c.do(ctx, "GET /servers/:id", http.MethodGet, "/servers/"+url.PathEscape(serverID), token, nil, http.StatusOK, &out)
	return out, err
}
