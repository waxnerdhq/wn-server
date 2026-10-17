package navidrome

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"regexp"
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

// EnsureUser makes Navidrome create the user for this identity and reports
// whether that user is admin. Navidrome creates a header user only on the web
// UI route (GET /app/, which injects the login state into index.html), not on
// /api/* or /rest/*. The first user created is admin, so this must be the
// first request to a new Navidrome.
func (c *Client) EnsureUser(ctx context.Context) (isAdmin bool, err error) {
	data, err := c.do(ctx, http.MethodGet, "/app/", nil)
	if err != nil {
		return false, err
	}
	return parseIsAdmin(data)
}

// appConfigRe finds the script line that index.html fills from the
// AppConfig template field. Navidrome renders the config as JSON and
// html/template emits that string as a JS string literal.
var appConfigRe = regexp.MustCompile(`window\.__APP_CONFIG__\s*=\s*("(?:[^"\\]|\\.)*")`)

// parseIsAdmin reads auth.isAdmin from the config that GET /app/ injects into
// index.html. It is false when the page carries no auth object.
func parseIsAdmin(page []byte) (bool, error) {
	m := appConfigRe.FindSubmatch(page)
	if m == nil {
		return false, errors.New("no app config in /app/ response")
	}
	var raw string
	if err := json.Unmarshal(m[1], &raw); err != nil {
		return false, fmt.Errorf("decode app config literal: %w", err)
	}
	var cfg struct {
		Auth struct {
			IsAdmin bool `json:"isAdmin"`
		} `json:"auth"`
	}
	if err := json.Unmarshal([]byte(raw), &cfg); err != nil {
		return false, fmt.Errorf("decode app config: %w", err)
	}
	return cfg.Auth.IsAdmin, nil
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

// subsonicAPIVersion is the Subsonic API version wn-server sends on /rest.
const subsonicAPIVersion = "1.16.1"

// StartScan starts a scan of all libraries. Navidrome's native API has no
// scan route, so this calls the Subsonic route /rest/startScan, which is
// admin only. wn-server's identity is admin, and Navidrome accepts the
// Remote-User header on /rest from the trusted source. Navidrome answers once
// the scan has started and scans in the background.
func (c *Client) StartScan(ctx context.Context) error {
	q := url.Values{"v": {subsonicAPIVersion}, "c": {Identity}, "f": {"json"}}
	data, err := c.do(ctx, http.MethodGet, "/rest/startScan?"+q.Encode(), nil)
	if err != nil {
		return err
	}
	return subsonicError(data)
}

// ScanStatus is the scan state that Navidrome reports on /rest/getScanStatus.
type ScanStatus struct {
	Scanning    bool      `json:"scanning"`
	Count       int64     `json:"count"` // tracks
	FolderCount int64     `json:"folderCount"`
	LastScan    time.Time `json:"lastScan"` // zero when Navidrome has not scanned
	Error       string    `json:"error"`
}

// ScanStatus returns the scan state. It calls the Subsonic route
// /rest/getScanStatus as wn-server's identity, as StartScan does.
func (c *Client) ScanStatus(ctx context.Context) (ScanStatus, error) {
	q := url.Values{"v": {subsonicAPIVersion}, "c": {Identity}, "f": {"json"}}
	data, err := c.do(ctx, http.MethodGet, "/rest/getScanStatus?"+q.Encode(), nil)
	if err != nil {
		return ScanStatus{}, err
	}
	if err := subsonicError(data); err != nil {
		return ScanStatus{}, err
	}
	var resp struct {
		R struct {
			ScanStatus *ScanStatus `json:"scanStatus"`
		} `json:"subsonic-response"`
	}
	if err := json.Unmarshal(data, &resp); err != nil {
		return ScanStatus{}, fmt.Errorf("decode scan status: %w", err)
	}
	if resp.R.ScanStatus == nil {
		return ScanStatus{}, errors.New("no scanStatus in getScanStatus response")
	}
	return *resp.R.ScanStatus, nil
}

// subsonicError returns the error in a Subsonic JSON response. Navidrome
// sends Subsonic errors with HTTP status 200.
func subsonicError(data []byte) error {
	var resp struct {
		R struct {
			Status string `json:"status"`
			Error  *struct {
				Code    int    `json:"code"`
				Message string `json:"message"`
			} `json:"error"`
		} `json:"subsonic-response"`
	}
	if err := json.Unmarshal(data, &resp); err != nil {
		return fmt.Errorf("decode subsonic response: %w", err)
	}
	if resp.R.Status == "ok" {
		return nil
	}
	if resp.R.Error != nil {
		return fmt.Errorf("subsonic error %d: %s", resp.R.Error.Code, resp.R.Error.Message)
	}
	return fmt.Errorf("subsonic status %q", resp.R.Status)
}
