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
	"strings"
	"testing"

	"github.com/waxnerdhq/wn-server/internal/wncloud/stub"
)

const (
	ownerID    = "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70"
	listenerID = "0192f9aa-0c11-7b3e-8f42-6a5d1e2c3b40"
	releaseID  = "6e12a11e-0ef9-49f3-b646-66929507d915"
	webUIURI   = "http://192.168.1.20:4533/auth/callback"
)

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

// signIn runs the browser sign-in with the given query (port or redirect_uri)
// and returns the code from the redirect.
func (c *client) signIn(query url.Values, wantPrefix string) (code, verifier string) {
	c.t.Helper()
	verifier = strings.Repeat("v", 43)
	sum := sha256.Sum256([]byte(verifier))
	query.Set("state", "state-0123456789abcdef")
	query.Set("challenge", base64.RawURLEncoding.EncodeToString(sum[:]))
	res, err := c.http.Get(c.base + "/cli/auth?" + query.Encode())
	if err != nil {
		c.t.Fatal(err)
	}
	_ = res.Body.Close()
	loc := res.Header.Get("Location")
	if res.StatusCode != http.StatusSeeOther || !strings.HasPrefix(loc, wantPrefix+"?") {
		c.t.Fatalf("GET /cli/auth: %d %q", res.StatusCode, loc)
	}
	u, _ := url.Parse(loc)
	if u.Query().Get("state") != "state-0123456789abcdef" {
		c.t.Fatalf("state not returned: %q", loc)
	}
	return u.Query().Get("code"), verifier
}

type tokenAnswer struct {
	Token string `json:"token"`
	User  struct {
		ID string `json:"id"`
	} `json:"user"`
}

func TestRoutes(t *testing.T) {
	s := stub.New()
	s.AddUser(ownerID, "owner@example.com", "adnan")
	s.AddUser(listenerID, "nina@example.com", "nina")
	s.SignInAs(ownerID)
	srv := httptest.NewServer(s)
	defer srv.Close()
	c := &client{t: t, base: srv.URL, http: &http.Client{
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}}

	// GET /cli/auth with port, then POST /cli/token with redirect_port.
	code, verifier := c.signIn(url.Values{"port": {"53122"}, "label": {"living-room"}}, "http://127.0.0.1:53122/callback")
	var tok tokenAnswer
	if st := c.do("POST", "/cli/token", "", map[string]any{"code": code, "code_verifier": verifier, "redirect_port": 53122}, &tok); st != 200 || !strings.HasPrefix(tok.Token, "wn_") || tok.User.ID != ownerID {
		t.Fatalf("POST /cli/token: %d %+v", st, tok)
	}
	owner := tok.Token

	// GET /me.
	var me map[string]any
	if st := c.do("GET", "/me", owner, nil, &me); st != 200 || me["id"] != ownerID {
		t.Fatalf("GET /me: %d %v", st, me)
	}

	// POST /servers.
	var server struct {
		ID string `json:"id"`
	}
	if st := c.do("POST", "/servers", owner, map[string]string{"name": "living-room"}, &server); st != 201 || server.ID == "" {
		t.Fatalf("POST /servers: %d", st)
	}
	base := "/servers/" + server.ID

	// PUT /servers/:id/redirect-uris.
	if st := c.do("PUT", base+"/redirect-uris", owner, map[string]any{"redirect_uris": []string{webUIURI}}, nil); st != 200 {
		t.Fatalf("PUT redirect-uris: %d", st)
	}

	// GET /cli/auth with redirect_uri and server_id, then POST /cli/token with redirect_uri.
	code, verifier = c.signIn(url.Values{"redirect_uri": {webUIURI}, "server_id": {server.ID}}, webUIURI)
	if st := c.do("POST", "/cli/token", "", map[string]any{"code": code, "code_verifier": verifier, "redirect_uri": webUIURI}, &tok); st != 200 {
		t.Fatalf("POST /cli/token with redirect_uri: %d", st)
	}

	// POST /servers/:id/invites.
	var member map[string]any
	if st := c.do("POST", base+"/invites", owner, map[string]string{"email": "Nina@example.com"}, &member); st != 201 || member["user_id"] != listenerID {
		t.Fatalf("POST invites: %d %v", st, member)
	}

	// GET /servers/:id/members.
	var members struct {
		Owner   map[string]any   `json:"owner"`
		Members []map[string]any `json:"members"`
	}
	if st := c.do("GET", base+"/members", owner, nil, &members); st != 200 || members.Owner["user_id"] != ownerID || len(members.Members) != 1 {
		t.Fatalf("GET members: %d %+v", st, members)
	}

	// DELETE /servers/:id/members/:userId.
	if st := c.do("DELETE", base+"/members/"+listenerID, owner, nil, nil); st != 204 {
		t.Fatalf("DELETE member: %d", st)
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
	if st := c.do("GET", "/me", owner, nil, nil); st != 401 {
		t.Fatalf("GET /me after logout: %d", st)
	}
}
