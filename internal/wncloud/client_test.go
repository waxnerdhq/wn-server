package wncloud

import (
	"context"
	"errors"
	"html"
	"io"
	"net/http"
	"net/http/httptest"
	"regexp"
	"strings"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/wncloud/stub"
)

const ownerID = "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70"

var codeRE = regexp.MustCompile(`<output id="code">([^<]+)</output>`)

func TestChallengeVector(t *testing.T) {
	// RFC 7636, appendix B.
	if got, want := Challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"; got != want {
		t.Fatalf("Challenge = %q, want %q", got, want)
	}
	v, _ := NewVerifier()
	s, _ := NewState()
	if len(v) != 43 || len(s) != 22 {
		t.Fatalf("verifier %q, state %q", v, s)
	}
}

// signIn opens the AuthURL page and returns the displayed code and the
// verifier.
func signIn(t *testing.T, c *Client) (code, verifier string) {
	t.Helper()
	verifier, _ = NewVerifier()
	state, _ := NewState()
	res, err := http.Get(c.AuthURL(state, Challenge(verifier), "living-room"))
	if err != nil {
		t.Fatal(err)
	}
	page, _ := io.ReadAll(res.Body)
	res.Body.Close()
	m := codeRE.FindSubmatch(page)
	if m == nil {
		t.Fatalf("no code on page: %d %s", res.StatusCode, page)
	}
	code, got, _ := strings.Cut(html.UnescapeString(string(m[1])), ".")
	if got != state {
		t.Fatalf("displayed state %q, want %q", got, state)
	}
	return code, verifier
}

func TestClient(t *testing.T) {
	ctx := context.Background()
	s := stub.New()
	s.AddUser(ownerID, "owner@example.com")
	s.SignInAs(ownerID)
	srv := httptest.NewServer(s)
	defer srv.Close()
	c := New(srv.URL+"/", "wn-server/test")

	// ExchangeCode: success, then the same code again.
	code, verifier := signIn(t, c)
	token, userID, err := c.ExchangeCode(ctx, code, verifier)
	if err != nil || !strings.HasPrefix(token, "wn_") || userID != ownerID {
		t.Fatalf("ExchangeCode = %q, %q, %v", token, userID, err)
	}
	if _, _, err := c.ExchangeCode(ctx, code, verifier); !errors.Is(err, ErrInvalidGrant) {
		t.Fatalf("second ExchangeCode = %v, want ErrInvalidGrant", err)
	}

	// RegisterServer: success, then a malformed name.
	serverID, err := c.RegisterServer(ctx, token, "living-room")
	if err != nil || serverID == "" {
		t.Fatalf("RegisterServer = %q, %v", serverID, err)
	}
	if _, err := c.RegisterServer(ctx, token, " "); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("RegisterServer with empty name = %v, want ErrInvalidRequest", err)
	}

	// VerifyDeviceCode: success, then a server the caller does not own.
	dc := s.IssueDeviceCode(serverID, ownerID)
	gotUser, email, err := c.VerifyDeviceCode(ctx, token, serverID, dc)
	if err != nil || gotUser != ownerID || email != "owner@example.com" {
		t.Fatalf("VerifyDeviceCode = %q, %q, %v", gotUser, email, err)
	}
	if _, _, err := c.VerifyDeviceCode(ctx, token, "0193a0b2-77c1-7f00-8e21-3d9c5b6a4f10", dc); !errors.Is(err, ErrForbidden) {
		t.Fatalf("VerifyDeviceCode on another server = %v, want ErrForbidden", err)
	}

	// GetServer: the default relay fields, the seeded ones, a server the
	// caller does not own, and the owner member row of POST /servers.
	got, err := c.GetServer(ctx, token, serverID)
	if err != nil || got.ID != serverID || got.OwnerID != ownerID || got.Relay.Enabled || got.Relay.Until != nil ||
		got.Relay.Host != serverID+".relay.waxnerd.com" || got.Relay.ConnectURL != "https://connect.relay.waxnerd.com/v1/tunnel" ||
		len(got.Addresses) != 1 || got.Addresses[0] != (Address{Kind: "relay", URL: "https://" + serverID + ".relay.waxnerd.com"}) {
		t.Fatalf("GetServer = %+v, %v", got, err)
	}
	until := time.Now().Add(-6 * 24 * time.Hour).UTC().Truncate(time.Microsecond)
	s.SetRelay(serverID, true, until)
	s.SetRelayConnected(serverID, true)
	s.SetRelayConnectURL("https://edge.test:8443/v1/tunnel")
	got, err = c.GetServer(ctx, token, serverID)
	if err != nil || !got.Relay.Enabled || got.Relay.Until == nil || !got.Relay.Until.Equal(until) || !got.Relay.Connected ||
		got.Relay.LastSeenAt == nil || got.Relay.ConnectURL != "https://edge.test:8443/v1/tunnel" {
		t.Fatalf("GetServer after seeding = %+v, %v", got, err)
	}
	s.SetRelay(serverID, true, time.Now().Add(-8*24*time.Hour))
	if got, err = c.GetServer(ctx, token, serverID); err != nil || got.Relay.Enabled {
		t.Fatalf("GetServer after the grace period = %+v, %v", got.Relay, err)
	}
	if _, err := c.GetServer(ctx, token, "0193a0b2-77c1-7f00-8e21-3d9c5b6a4f10"); !errors.Is(err, ErrForbidden) {
		t.Fatalf("GetServer on another server = %v, want ErrForbidden", err)
	}
	if m := s.Members(serverID); len(m) != 1 || m[ownerID] != "owner" {
		t.Fatalf("members = %v, want the owner row", m)
	}

	// Logout: success, then the revoked token.
	if err := c.Logout(ctx, token); err != nil {
		t.Fatalf("Logout = %v", err)
	}
	if err := c.Logout(ctx, token); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("second Logout = %v, want ErrUnauthorized", err)
	}
	if _, err := c.GetServer(ctx, token, serverID); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("GetServer with a revoked token = %v, want ErrUnauthorized", err)
	}
}

func TestClientUnreachable(t *testing.T) {
	ctx := context.Background()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Error(w, "bad gateway", http.StatusBadGateway)
	}))
	c := New(srv.URL, "")
	if _, err := c.RegisterServer(ctx, "wn_x", "n"); !errors.Is(err, ErrUnreachable) {
		t.Fatalf("5xx = %v, want ErrUnreachable", err)
	}
	srv.Close()
	if err := c.Logout(ctx, "wn_x"); !errors.Is(err, ErrUnreachable) {
		t.Fatalf("closed server = %v, want ErrUnreachable", err)
	}
}
