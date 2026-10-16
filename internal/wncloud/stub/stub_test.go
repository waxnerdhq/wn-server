package stub_test

import (
	"bytes"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"regexp"
	"strings"
	"testing"

	"github.com/waxnerdhq/wn-server/internal/wncloud/stub"
)

const (
	ownerID   = "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70"
	releaseID = "6e12a11e-0ef9-49f3-b646-66929507d915"
	state     = "state-0123456789abcdef"
)

var codeRE = regexp.MustCompile(`<output id="code">([^<]+)</output>`)

type client struct {
	t    *testing.T
	base string
	http *http.Client
}

// do sends a request and decodes a JSON answer into out when out is not nil.
func (c *client) do(method, path, token string, body, out any) int {
	c.t.Helper()
	var r io.Reader
	if body != nil {
		b, _ := json.Marshal(body)
		r = bytes.NewReader(b)
	}
	req, _ := http.NewRequest(method, c.base+path, r)
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	res, err := c.http.Do(req)
	if err != nil {
		c.t.Fatal(err)
	}
	defer func() { _ = res.Body.Close() }()
	if out != nil {
		if err := json.NewDecoder(res.Body).Decode(out); err != nil {
			c.t.Fatalf("%s %s: %v", method, path, err)
		}
	}
	return res.StatusCode
}

// authorize opens GET /cli/auth with the given query plus state and challenge.
func (c *client) authorize(query url.Values) (*http.Response, string) {
	c.t.Helper()
	verifier := strings.Repeat("v", 43)
	sum := sha256.Sum256([]byte(verifier))
	query.Set("state", state)
	query.Set("challenge", base64.RawURLEncoding.EncodeToString(sum[:]))
	res, err := c.http.Get(c.base + "/cli/auth?" + query.Encode())
	if err != nil {
		c.t.Fatal(err)
	}
	return res, verifier
}

// signInPort runs the loopback sign-in and returns the code from the redirect.
func (c *client) signInPort(port string) (code, verifier string) {
	c.t.Helper()
	res, verifier := c.authorize(url.Values{"port": {port}, "label": {"living-room"}})
	_ = res.Body.Close()
	loc := res.Header.Get("Location")
	if res.StatusCode != http.StatusSeeOther || !strings.HasPrefix(loc, "http://127.0.0.1:"+port+"/callback?") {
		c.t.Fatalf("GET /cli/auth: %d %q", res.StatusCode, loc)
	}
	u, _ := url.Parse(loc)
	if u.Query().Get("state") != state {
		c.t.Fatalf("state not returned: %q", loc)
	}
	return u.Query().Get("code"), verifier
}

// signInDisplay runs the code-display sign-in and returns the code from the
// displayed value <code>.<state>.
func (c *client) signInDisplay() (code, verifier string) {
	c.t.Helper()
	res, verifier := c.authorize(url.Values{"display": {"code"}, "label": {"living-room"}})
	page, _ := io.ReadAll(res.Body)
	_ = res.Body.Close()
	m := codeRE.FindSubmatch(page)
	if res.StatusCode != http.StatusOK || m == nil {
		c.t.Fatalf("GET /cli/auth display=code: %d %s", res.StatusCode, page)
	}
	code, gotState, _ := strings.Cut(string(m[1]), ".")
	if gotState != state {
		c.t.Fatalf("state not returned: %q", m[1])
	}
	return code, verifier
}

type tokenAnswer struct {
	Token string `json:"token"`
	User  struct {
		ID string `json:"id"`
	} `json:"user"`
}

func TestRoutes(t *testing.T) {
	s := stub.New()
	s.AddUser(ownerID, "owner@example.com")
	s.SignInAs(ownerID)
	srv := httptest.NewServer(s)
	defer srv.Close()
	c := &client{t: t, base: srv.URL, http: &http.Client{
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}}

	// GET /cli/auth with port, then POST /cli/token with redirect_port.
	code, verifier := c.signInPort("53122")
	var tok tokenAnswer
	if st := c.do("POST", "/cli/token", "", map[string]any{"code": code, "code_verifier": verifier, "redirect_port": 53122}, &tok); st != 200 || !strings.HasPrefix(tok.Token, "wn_") {
		t.Fatalf("POST /cli/token with redirect_port: %d %+v", st, tok)
	}

	// GET /cli/auth with display=code, then POST /cli/token without redirect_port.
	code, verifier = c.signInDisplay()
	if st := c.do("POST", "/cli/token", "", map[string]any{"code": code, "code_verifier": verifier}, &tok); st != 200 || !strings.HasPrefix(tok.Token, "wn_") || tok.User.ID != ownerID {
		t.Fatalf("POST /cli/token: %d %+v", st, tok)
	}
	owner := tok.Token

	// POST /servers.
	var server struct {
		ID string `json:"id"`
	}
	if st := c.do("POST", "/servers", owner, map[string]string{"name": "living-room"}, &server); st != 201 || server.ID == "" {
		t.Fatalf("POST /servers: %d", st)
	}
	// PUT, GET, DELETE /ratings/:kind/:mbid and GET /ratings?since=.
	path := "/ratings/release/" + releaseID
	if st := c.do("PUT", path, owner, map[string]any{"half_stars": 9, "like_level": "love"}, nil); st != 200 {
		t.Fatalf("PUT rating: %d", st)
	}
	var rating map[string]any
	if st := c.do("GET", path, owner, nil, &rating); st != 200 || rating["half_stars"] != float64(9) || rating["like_level"] != "love" {
		t.Fatalf("GET rating: %d %v", st, rating)
	}
	var feed struct {
		Changes []map[string]any `json:"changes"`
		Next    string           `json:"next"`
	}
	if st := c.do("GET", "/ratings", owner, nil, &feed); st != 200 || len(feed.Changes) != 1 {
		t.Fatalf("GET /ratings: %d %+v", st, feed)
	}
	if st := c.do("DELETE", path, owner, nil, nil); st != 204 {
		t.Fatalf("DELETE rating: %d", st)
	}
	if st := c.do("GET", "/ratings?since="+url.QueryEscape(feed.Next), owner, nil, &feed); st != 200 || len(feed.Changes) != 1 || feed.Changes[0]["deleted"] != true {
		t.Fatalf("GET /ratings?since=: %d %+v", st, feed)
	}

	// POST /cli/logout, after which the token no longer works.
	if st := c.do("POST", "/cli/logout", owner, nil, nil); st != 204 {
		t.Fatalf("POST /cli/logout: %d", st)
	}
	if st := c.do("GET", "/ratings", owner, nil, nil); st != 401 {
		t.Fatalf("GET /ratings after logout: %d", st)
	}
}

func TestDeviceCodes(t *testing.T) {
	const (
		otherID  = "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a71"
		serverID = "0193a0b2-77c1-7f00-8e21-3d9c5b6a4f10"
		owner    = "wn_owner"
		other    = "wn_other"
	)
	s := stub.New()
	s.AddUser(ownerID, "owner@example.com")
	s.AddUser(otherID, "other@example.com")
	s.AddToken(owner, ownerID)
	s.AddToken(other, otherID)
	s.AddServer(serverID, ownerID, "living-room")
	srv := httptest.NewServer(s)
	defer srv.Close()
	c := &client{t: t, base: srv.URL, http: srv.Client()}
	path := "/servers/" + serverID + "/device-codes/verify"

	// Success, then the spent code.
	code := s.IssueDeviceCode(serverID, ownerID)
	var got struct {
		UserID string `json:"user_id"`
		Email  string `json:"email"`
	}
	if st := c.do("POST", path, owner, map[string]string{"code": strings.ToLower(code)}, &got); st != 200 || got.UserID != ownerID || got.Email != "owner@example.com" {
		t.Fatalf("verify: %d %+v", st, got)
	}
	var e struct {
		Code string `json:"code"`
	}
	if st := c.do("POST", path, owner, map[string]string{"code": code}, &e); st != 400 || e.Code != "invalid_grant" {
		t.Fatalf("verify spent code: %d %+v", st, e)
	}

	// An expired code.
	code = s.IssueDeviceCode(serverID, ownerID)
	s.ExpireDeviceCode(code)
	if st := c.do("POST", path, owner, map[string]string{"code": code}, &e); st != 400 || e.Code != "invalid_grant" {
		t.Fatalf("verify expired code: %d %+v", st, e)
	}

	// A credential that does not own the server. The code stays valid.
	code = s.IssueDeviceCode(serverID, ownerID)
	if st := c.do("POST", path, other, map[string]string{"code": code}, &e); st != 403 || e.Code != "forbidden" {
		t.Fatalf("verify with another credential: %d %+v", st, e)
	}
	if st := c.do("POST", path, owner, map[string]string{"code": code}, nil); st != 200 {
		t.Fatalf("verify after 403: %d", st)
	}
}
