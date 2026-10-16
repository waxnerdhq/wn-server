package navidrome

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"time"
)

// Identity is the Remote-User value of wn-server's own Navidrome user.
// Navidrome creates the user on the first web request that carries the
// header, and the first user created is admin.
const Identity = "wn-server"

// userHeader is the header named by ExtAuth.UserHeader in navidrome.toml.
const userHeader = "Remote-User"

// Library is a Navidrome library as returned by /api/library.
type Library struct {
	ID   int    `json:"id"`
	Name string `json:"name"`
	Path string `json:"path"`
}

// Client calls Navidrome on loopback as wn-server's identity.
type Client struct {
	base string
	user string
	http *http.Client
}

// NewClient returns a client for the Navidrome child listening on
// 127.0.0.1:port.
func NewClient(port int) *Client {
	return newClient("http://127.0.0.1:"+strconv.Itoa(port), Identity)
}

func newClient(base, user string) *Client {
	return &Client{base: base, user: user, http: &http.Client{Timeout: 30 * time.Second}}
}

func (c *Client) do(ctx context.Context, method, path string, body any) ([]byte, error) {
	var rd io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return nil, err
		}
		rd = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.base+path, rd)
	if err != nil {
		return nil, err
	}
	req.Header.Set(userHeader, c.user)
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if err != nil {
		return nil, err
	}
	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		return nil, fmt.Errorf("%s %s: %s: %s", method, path, resp.Status, bytes.TrimSpace(data))
	}
	return data, nil
}

// EnsureUser makes Navidrome create the user for this identity. Navidrome
// creates a header user only on the web UI route (GET /app/, which injects
// the login state into index.html), not on /api/* or /rest/*. The first user
// created is admin, so this must be the first request to a new Navidrome.
func (c *Client) EnsureUser(ctx context.Context) error {
	_, err := c.do(ctx, http.MethodGet, "/app/", nil)
	return err
}

// ListLibraries returns all libraries. The route is admin only.
func (c *Client) ListLibraries(ctx context.Context) ([]Library, error) {
	data, err := c.do(ctx, http.MethodGet, "/api/library", nil)
	if err != nil {
		return nil, err
	}
	var libs []Library
	if err := json.Unmarshal(data, &libs); err != nil {
		return nil, fmt.Errorf("decode libraries: %w", err)
	}
	return libs, nil
}

// CreateLibrary adds a library. Navidrome requires an absolute path to an
// existing directory and a unique name and path.
func (c *Client) CreateLibrary(ctx context.Context, name, path string) error {
	_, err := c.do(ctx, http.MethodPost, "/api/library", map[string]string{"name": name, "path": path})
	return err
}
