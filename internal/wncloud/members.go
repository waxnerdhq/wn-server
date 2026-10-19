package wncloud

import (
	"context"
	"net/http"
	"net/url"
	"time"
)

// Member is one member of a server in GET /servers/:id/members.
type Member struct {
	UserID    string    `json:"user_id"`
	Name      string    `json:"name"`
	Role      string    `json:"role"` // RoleOwner or RoleListener
	CreatedAt time.Time `json:"created_at"`
}

// ListMembers returns the members of a server (GET /servers/:id/members),
// the owner first. The token's user must own the server; another user's
// server, or an unknown id, gives a 403 forbidden (ErrForbidden), and a
// revoked token gives ErrUnauthorized.
func (c *Client) ListMembers(ctx context.Context, token, serverID string) ([]Member, error) {
	var out struct {
		Members []Member `json:"members"`
	}
	err := c.do(ctx, "GET /servers/:id/members", http.MethodGet, "/servers/"+url.PathEscape(serverID)+"/members", token, nil, http.StatusOK, &out)
	if err != nil {
		return nil, err
	}
	return out.Members, nil
}
