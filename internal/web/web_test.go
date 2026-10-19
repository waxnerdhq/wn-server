package web

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"net/url"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/match"
	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/relay"
	"github.com/waxnerdhq/wn-server/internal/relayreq"
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

// fakeMatcher stands in for the match service. Apply blocks until release is
// closed.
type fakeMatcher struct {
	started chan int64
	release chan struct{}
}

func (f *fakeMatcher) Run(context.Context) (match.RunResult, error) {
	return match.RunResult{Reported: true, Pending: 0}, nil
}

func (f *fakeMatcher) Apply(_ context.Context, id int64, _ string) (match.ApplyResult, error) {
	f.started <- id
	<-f.release
	return match.ApplyResult{ID: id, Folder: "/music/x", CopiedTo: "/sorted/x"}, nil
}

func (f *fakeMatcher) Dismiss(context.Context, int64) (string, error) { return "/music/x", nil }

func (f *fakeMatcher) Revert(context.Context, int64) (match.RevertResult, error) {
	return match.RevertResult{}, errors.New("not used")
}

type env struct {
	srv   *httptest.Server
	st    *store.Store
	owner store.Account
	match *fakeMatcher
	jobs  *match.Jobs
}

func newEnv(t *testing.T, opts ...func(*Options)) *env {
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
	fake := &fakeMatcher{started: make(chan int64, 4), release: make(chan struct{})}
	jobs := match.NewJobs(context.Background(), slog.New(slog.DiscardHandler))
	o := Options{
		Store:            st,
		Match:            fake,
		Jobs:             jobs,
		Navidrome:        fakeScanner{},
		Ready:            ready,
		ServerName:       "test",
		NavidromeVersion: navidrome.PinnedVersion,
		Logger:           slog.New(slog.DiscardHandler),
	}
	for _, f := range opts {
		f(&o)
	}
	srv := httptest.NewServer(New(o))
	t.Cleanup(srv.Close)
	return &env{srv: srv, st: st, owner: owner, match: fake, jobs: jobs}
}

// do sends a request without following redirects. form, when not nil, is
// sent as a urlencoded body.
func (e *env) do(t *testing.T, method, path string, form url.Values, cookie *http.Cookie, origin string) *http.Response {
	t.Helper()
	resp, _ := e.doBody(t, method, path, form, cookie, origin)
	return resp
}

// doBody is do with the response body.
func (e *env) doBody(t *testing.T, method, path string, form url.Values, cookie *http.Cookie, origin string) (*http.Response, string) {
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
	b, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	return resp, string(b)
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
	if !c.HttpOnly || c.SameSite != http.SameSiteLaxMode || c.Path != "/ui/" || c.MaxAge != cookieMaxAge || c.Secure {
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
		if !l.begin("a", false) {
			t.Fatal("refused below the limit")
		}
	}
	if l.begin("a", false) {
		t.Fatal("allowed above the limit")
	}
	if !l.begin("b", false) {
		t.Fatal("refused another address")
	}
	l.forget("a", false)
	if !l.begin("a", false) {
		t.Fatal("forget did not free a slot")
	}
	now = now.Add(signInWindow + time.Second)
	if !l.begin("a", false) {
		t.Fatal("the window did not expire")
	}
	if limitKey("2001:db8::1") != limitKey("2001:db8::2") {
		t.Fatal("IPv6 addresses in one /64 have different keys")
	}
}

func TestLimiterTotal(t *testing.T) {
	l := newLimiter(time.Now)
	for i := range signInMaxTotal {
		if !l.begin(fmt.Sprintf("10.0.%d.%d", i/250, i%250), false) {
			t.Fatalf("refused attempt %d below the total", i)
		}
	}
	if l.begin("192.0.2.1", false) {
		t.Fatal("allowed a new address above the total")
	}
}

func TestLimiterSplitTotals(t *testing.T) {
	fill := func(l *limiter, relayed bool) {
		for i := range signInMaxTotal {
			if !l.begin(fmt.Sprintf("10.0.%d.%d", i/250, i%250), relayed) {
				t.Fatalf("refused attempt %d below the total, relayed=%v", i, relayed)
			}
		}
	}
	for _, relayed := range []bool{false, true} {
		l := newLimiter(time.Now)
		fill(l, relayed)
		if l.begin("192.0.2.1", relayed) {
			t.Fatalf("allowed above the total, relayed=%v", relayed)
		}
		if !l.begin("192.0.2.1", !relayed) {
			t.Fatalf("a full pool locked the other class, relayed=%v", relayed)
		}
	}
	// The same address counts separately in each class.
	l := newLimiter(time.Now)
	for range signInMaxPerAddr {
		l.begin("192.0.2.2", false)
	}
	if l.begin("192.0.2.2", false) || !l.begin("192.0.2.2", true) {
		t.Fatal("LAN failures of an address counted against the same address relayed")
	}
}

func TestMatchPageGuard(t *testing.T) {
	e := newEnv(t)
	for _, tc := range []struct{ method, path string }{
		{http.MethodGet, "/ui/match"},
		{http.MethodPost, "/ui/match/run"},
		{http.MethodPost, "/ui/match/1/apply"},
	} {
		resp := e.do(t, tc.method, tc.path, url.Values{}, nil, "")
		if resp.StatusCode != http.StatusSeeOther || resp.Header.Get("Location") != "/ui/login" {
			t.Fatalf("%s %s without a cookie: status %d, location %q", tc.method, tc.path, resp.StatusCode, resp.Header.Get("Location"))
		}
	}
}

func TestMatchApplyJob(t *testing.T) {
	const release = "1a2b3c4d-5e6f-7081-92a3-b4c5d6e7f809"
	e := newEnv(t)
	c := e.signIn(t)
	id, err := e.st.RecordMatch(context.Background(), store.MatchLine{
		Folder: "/music/Rip 2004", Decision: store.LineSkipped, Artist: "Various", Album: "Rip 2004", TrackCount: 11,
		Candidates: []store.MatchCandidate{{ReleaseID: release, Artist: "Various Artists", Album: "Rip It Up", Year: 2004, Distance: 0.41}},
	}, time.Now())
	if err != nil {
		t.Fatal(err)
	}

	resp, body := e.doBody(t, http.MethodGet, "/ui/match", nil, c, "")
	if resp.StatusCode != http.StatusOK || !strings.Contains(body, "/music/Rip 2004") ||
		!strings.Contains(body, "https://musicbrainz.org/release/"+release) || strings.Contains(body, "http-equiv=\"refresh\"") {
		t.Fatalf("idle page: status %d\n%s", resp.StatusCode, body)
	}

	applyPath := fmt.Sprintf("/ui/match/%d/apply", id)
	resp = e.do(t, http.MethodPost, applyPath, url.Values{"release": {release}}, c, e.srv.URL)
	if resp.StatusCode != http.StatusSeeOther || resp.Header.Get("Location") != "/ui/match" {
		t.Fatalf("apply: status %d, location %q", resp.StatusCode, resp.Header.Get("Location"))
	}
	select {
	case got := <-e.match.started:
		if got != id {
			t.Fatalf("job applied entry %d, want %d", got, id)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("the apply job did not start")
	}

	// A second POST while the job runs answers 409.
	for _, path := range []string{applyPath, "/ui/match/run", fmt.Sprintf("/ui/match/%d/dismiss", id)} {
		resp, body = e.doBody(t, http.MethodPost, path, url.Values{}, c, "")
		if resp.StatusCode != http.StatusConflict || !strings.Contains(body, "Another match job is running") {
			t.Fatalf("POST %s while a job runs: status %d", path, resp.StatusCode)
		}
	}
	resp, body = e.doBody(t, http.MethodGet, "/ui/match", nil, c, "")
	if resp.StatusCode != http.StatusOK || !strings.Contains(body, "http-equiv=\"refresh\"") || !strings.Contains(body, "apply of entry") ||
		!strings.Contains(body, "disabled") {
		t.Fatalf("page while a job runs: status %d\n%s", resp.StatusCode, body)
	}

	// After the job, the page shows its result once.
	close(e.match.release)
	deadline := time.Now().Add(5 * time.Second)
	for e.jobs.Status().Running != nil {
		if time.Now().After(deadline) {
			t.Fatal("the apply job did not finish")
		}
		time.Sleep(5 * time.Millisecond)
	}
	_, body = e.doBody(t, http.MethodGet, "/ui/match", nil, c, "")
	if !strings.Contains(body, "applied entry") || strings.Contains(body, "http-equiv=\"refresh\"") {
		t.Fatalf("page after the job:\n%s", body)
	}
	_, body = e.doBody(t, http.MethodGet, "/ui/match", nil, c, "")
	if strings.Contains(body, "Last job") {
		t.Fatalf("the result showed twice:\n%s", body)
	}
}

func TestMatchAllShowsDuplicateRelease(t *testing.T) {
	const release = "1a2b3c4d-5e6f-7081-92a3-b4c5d6e7f809"
	e := newEnv(t)
	c := e.signIn(t)
	if _, err := e.st.RecordMatch(context.Background(), store.MatchLine{
		Folder: "/music/Copy 2", Decision: store.LineDuplicate, Artist: "Ar", Album: "Al", ReleaseID: release,
	}, time.Now()); err != nil {
		t.Fatal(err)
	}
	resp, body := e.doBody(t, http.MethodGet, "/ui/match?all=1", nil, c, "")
	if resp.StatusCode != http.StatusOK || !strings.Contains(body, "https://musicbrainz.org/release/"+release) {
		t.Fatalf("all view: status %d\n%s", resp.StatusCode, body)
	}
}

func TestCookieSecureForRelayedRequest(t *testing.T) {
	for _, tc := range []struct {
		name    string
		relayed bool
		want    bool
	}{{"plain", false, false}, {"relayed", true, true}} {
		t.Run(tc.name, func(t *testing.T) {
			r := httptest.NewRequest(http.MethodPost, "/ui/login", nil)
			if tc.relayed {
				r = r.WithContext(relayreq.WithRelay(r.Context(), "203.0.113.9"))
			}
			for name, write := range map[string]func(http.ResponseWriter){
				"set":   func(w http.ResponseWriter) { setCookie(w, r, "token") },
				"clear": func(w http.ResponseWriter) { clearCookie(w, r) },
			} {
				rec := httptest.NewRecorder()
				write(rec)
				c := sessionCookie(rec.Result())
				if c == nil || c.Secure != tc.want {
					t.Fatalf("%s cookie: %+v, want Secure=%v", name, c, tc.want)
				}
			}
		})
	}
}

func TestStatusPageRelayLine(t *testing.T) {
	e := newEnv(t)
	_, body := e.doBody(t, http.MethodGet, "/ui/", nil, e.signIn(t), "")
	if !strings.Contains(body, "relay: off") {
		t.Fatalf("status page without a relay lacks the off line:\n%s", body)
	}
	since := time.Now()
	e = newEnv(t, func(o *Options) {
		o.Relay = func() relay.State { return relay.State{Name: relay.Connected, Since: since, Host: "id.relay.test"} }
	})
	_, body = e.doBody(t, http.MethodGet, "/ui/", nil, e.signIn(t), "")
	want := "relay: connected since " + since.Local().Format("2006-01-02 15:04 MST") + ` at <a href="https://id.relay.test">https://id.relay.test</a>`
	if !strings.Contains(body, want) {
		t.Fatalf("status page lacks %q:\n%s", want, body)
	}
}
