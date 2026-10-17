package web

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"net/url"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/store"
)

const (
	ownerName = "owner"
	ownerPass = "s3cretPassw0rd"
)

type fakeScanner struct{}

func (fakeScanner) ScanStatus(context.Context) (navidrome.ScanStatus, error) {
	return navidrome.ScanStatus{Count: 12, FolderCount: 3}, nil
}

type env struct {
	srv   *httptest.Server
	st    *store.Store
	owner store.Account
}

func newEnv(t *testing.T) *env {
	t.Helper()
	ctx := context.Background()
	st, err := store.Open(ctx, filepath.Join(t.TempDir(), store.FileName))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { st.Close() })
	owner, err := st.CreateOwner(ctx, ownerName)
	if err != nil {
		t.Fatal(err)
	}
	if err := st.SetAppPassword(ctx, owner.ID, ownerPass); err != nil {
		t.Fatal(err)
	}
	ready := make(chan struct{})
	close(ready)
	srv := httptest.NewServer(New(Options{
		Store:            st,
		Navidrome:        fakeScanner{},
		Ready:            ready,
		ServerName:       "test",
		NavidromeVersion: navidrome.PinnedVersion,
		Logger:           slog.New(slog.DiscardHandler),
	}))
	t.Cleanup(srv.Close)
	return &env{srv: srv, st: st, owner: owner}
}

// do sends a request without following redirects. form, when not nil, is
// sent as a urlencoded body.
func (e *env) do(t *testing.T, method, path string, form url.Values, cookie *http.Cookie, origin string) *http.Response {
	t.Helper()
	var body io.Reader
	if form != nil {
		body = strings.NewReader(form.Encode())
	}
	req, err := http.NewRequest(method, e.srv.URL+path, body)
	if err != nil {
		t.Fatal(err)
	}
	if form != nil {
		req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	}
	if cookie != nil {
		req.AddCookie(cookie)
	}
	if origin != "" {
		req.Header.Set("Origin", origin)
	}
	client := &http.Client{CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	resp, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	_, _ = io.Copy(io.Discard, resp.Body)
	resp.Body.Close()
	return resp
}

func sessionCookie(resp *http.Response) *http.Cookie {
	for _, c := range resp.Cookies() {
		if c.Name == CookieName {
			return c
		}
	}
	return nil
}

// signIn signs in as the owner and returns the session cookie.
func (e *env) signIn(t *testing.T) *http.Cookie {
	t.Helper()
	resp := e.do(t, http.MethodPost, "/ui/login", url.Values{"name": {"OWNER"}, "password": {ownerPass}}, nil, "")
	if resp.StatusCode != http.StatusSeeOther || resp.Header.Get("Location") != "/ui/" {
		t.Fatalf("sign-in: status %d, location %q", resp.StatusCode, resp.Header.Get("Location"))
	}
	c := sessionCookie(resp)
	if c == nil || c.Value == "" {
		t.Fatal("sign-in set no session cookie")
	}
	if !c.HttpOnly || c.SameSite != http.SameSiteLaxMode || c.Path != "/" || c.Secure {
		t.Fatalf("cookie attributes: %+v", c)
	}
	return c
}

func TestSignIn(t *testing.T) {
	e := newEnv(t)
	resp := e.do(t, http.MethodPost, "/ui/login", url.Values{"name": {ownerName}, "password": {"wrong"}}, nil, "")
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("bad password: status %d, want 401", resp.StatusCode)
	}
	if sessionCookie(resp) != nil {
		t.Fatal("bad password set a cookie")
	}

	c := e.signIn(t)
	sessions, err := e.st.Sessions(context.Background(), e.owner.ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(sessions) != 1 || sessions[0].Device != "web Go-http-client/1.1" {
		t.Fatalf("sessions: %+v", sessions)
	}
	for _, p := range []string{"/ui/", "/ui/account"} {
		if resp := e.do(t, http.MethodGet, p, nil, c, ""); resp.StatusCode != http.StatusOK {
			t.Fatalf("%s: status %d, want 200", p, resp.StatusCode)
		}
	}
}

func TestGuard(t *testing.T) {
	e := newEnv(t)
	resp := e.do(t, http.MethodGet, "/ui/", nil, nil, "")
	if resp.StatusCode != http.StatusSeeOther || resp.Header.Get("Location") != "/ui/login" {
		t.Fatalf("no cookie: status %d, location %q", resp.StatusCode, resp.Header.Get("Location"))
	}

	c := e.signIn(t)
	if _, err := e.st.RevokeSessionByToken(context.Background(), c.Value); err != nil {
		t.Fatal(err)
	}
	resp = e.do(t, http.MethodGet, "/ui/account", nil, c, "")
	if resp.StatusCode != http.StatusSeeOther || resp.Header.Get("Location") != "/ui/login" {
		t.Fatalf("revoked session: status %d, location %q", resp.StatusCode, resp.Header.Get("Location"))
	}
}

func TestOriginCheck(t *testing.T) {
	e := newEnv(t)
	c := e.signIn(t)
	resp := e.do(t, http.MethodPost, "/ui/account/password/reset", url.Values{}, c, "http://evil.example")
	if resp.StatusCode != http.StatusForbidden {
		t.Fatalf("cross origin: status %d, want 403", resp.StatusCode)
	}
	owner, err := e.st.Owner(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if owner.AppPassword != ownerPass {
		t.Fatal("a cross-origin request changed the app password")
	}
}

func TestPasswordReset(t *testing.T) {
	e := newEnv(t)
	c := e.signIn(t)
	resp := e.do(t, http.MethodPost, "/ui/account/password/reset", url.Values{}, c, e.srv.URL)
	if resp.StatusCode != http.StatusSeeOther {
		t.Fatalf("reset: status %d, want 303", resp.StatusCode)
	}
	owner, err := e.st.Owner(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if owner.AppPassword == ownerPass || len(owner.AppPassword) != store.AppPasswordLength {
		t.Fatalf("app password after reset: %q", owner.AppPassword)
	}
}

func TestRevokeOwnSessionClearsCookie(t *testing.T) {
	e := newEnv(t)
	c := e.signIn(t)
	sess, err := e.st.SessionByToken(context.Background(), c.Value)
	if err != nil {
		t.Fatal(err)
	}
	resp := e.do(t, http.MethodPost, "/ui/sessions/"+sess.ID+"/revoke", url.Values{}, c, "")
	if resp.StatusCode != http.StatusSeeOther || resp.Header.Get("Location") != "/ui/login" {
		t.Fatalf("revoke: status %d, location %q", resp.StatusCode, resp.Header.Get("Location"))
	}
	if got := sessionCookie(resp); got == nil || got.MaxAge >= 0 {
		t.Fatalf("revoke did not clear the cookie: %+v", got)
	}
	if _, err := e.st.SessionByToken(context.Background(), c.Value); !errors.Is(err, store.ErrSessionNotFound) {
		t.Fatalf("session after revoke: %v", err)
	}
}

func TestLimiter(t *testing.T) {
	now := time.Unix(1_800_000_000, 0)
	l := newLimiter(func() time.Time { return now })
	for range signInMaxPerAddr {
		if !l.begin("a") {
			t.Fatal("refused below the limit")
		}
	}
	if l.begin("a") {
		t.Fatal("allowed above the limit")
	}
	if !l.begin("b") {
		t.Fatal("refused another address")
	}
	l.forget("a")
	if !l.begin("a") {
		t.Fatal("forget did not free a slot")
	}
	now = now.Add(signInWindow + time.Second)
	if !l.begin("a") {
		t.Fatal("the window did not expire")
	}
}
