package proxy

import (
	"context"
	"encoding/json"
	"encoding/xml"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/relayreq"
	"github.com/waxnerdhq/wn-server/internal/store"
)

// link stores the server credential, as wn-server login does, and registers
// the owner and the server at the stub.
func (e *env) link(t *testing.T) {
	t.Helper()
	e.cloud.AddUser(waxnerdID, "owner@example.com")
	e.cloud.AddToken(cloudToken, waxnerdID)
	e.cloud.AddServer(serverID, waxnerdID, "test")
	err := e.st.Link(context.Background(), e.owner.ID, store.ServerCredential{Token: cloudToken, ServerID: serverID, WaxnerdUserID: waxnerdID})
	if err != nil {
		t.Fatal(err)
	}
}

// do sends a request with an optional Bearer session and returns the status
// and the body.
func (e *env) do(t *testing.T, method, path, session, body string) (int, []byte) {
	t.Helper()
	req, _ := http.NewRequest(method, e.srv.URL+path, strings.NewReader(body))
	if session != "" {
		req.Header.Set("Authorization", "Bearer "+session)
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, b
}

// exchange posts a code and checks the status and the error code.
func (e *env) exchange(t *testing.T, code string, wantStatus int, wantCode string) exchangeResponse {
	t.Helper()
	body, _ := json.Marshal(map[string]string{"code": code, "device": "test phone"})
	status, b := e.do(t, http.MethodPost, "/api/v1/session", "", string(body))
	if status != wantStatus {
		t.Fatalf("exchange %q: status %d, want %d; body %s", code, status, wantStatus, b)
	}
	if wantStatus != http.StatusOK {
		var eb apiError
		if err := json.Unmarshal(b, &eb); err != nil || eb.Code != wantCode {
			t.Fatalf("exchange %q: body %s, want code %s", code, b, wantCode)
		}
		return exchangeResponse{}
	}
	var out exchangeResponse
	if err := json.Unmarshal(b, &out); err != nil {
		t.Fatalf("exchange body %s: %v", b, err)
	}
	return out
}

// subsonicCode returns the error code of a Subsonic XML answer, or -1 when
// the request was forwarded.
func subsonicCode(t *testing.T, e *env, path, session string) int {
	t.Helper()
	before := e.hits.Load()
	_, b := e.do(t, http.MethodGet, path, session, "")
	if e.hits.Load() > before {
		return -1
	}
	var v struct {
		Error struct {
			Code int `xml:"code,attr"`
		} `xml:"error"`
	}
	if err := xml.Unmarshal(b, &v); err != nil {
		t.Fatalf("body %s: %v", b, err)
	}
	return v.Error.Code
}

func TestSessionExchange(t *testing.T) {
	var remoteUser, auth string
	var query url.Values
	e := newEnv(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		remoteUser, auth, query = r.Header.Get("Remote-User"), r.Header.Get("Authorization"), r.URL.Query()
	}), true)

	e.exchange(t, "K7QM-4XPA", http.StatusConflict, "not_linked")
	e.link(t)
	e.exchange(t, "K7QM", http.StatusBadRequest, "invalid_request")
	e.exchange(t, "K7QM-4XPA", http.StatusUnauthorized, "invalid_code")
	const otherID = "11111111-2222-4333-8444-555555555555"
	e.cloud.AddUser(otherID, "other@example.com")
	e.exchange(t, e.cloud.IssueDeviceCode(serverID, otherID), http.StatusForbidden, "not_a_member")

	ctx := context.Background()
	// A server credential that wn-cloud does not know.
	if err := e.st.Link(ctx, e.owner.ID, store.ServerCredential{Token: "wn_revoked", ServerID: serverID, WaxnerdUserID: waxnerdID}); err != nil {
		t.Fatal(err)
	}
	e.exchange(t, e.cloud.IssueDeviceCode(serverID, waxnerdID), http.StatusBadGateway, "cloud_error")
	if err := e.st.Unlink(ctx, e.owner.ID); err != nil {
		t.Fatal(err)
	}
	e.exchange(t, e.cloud.IssueDeviceCode(serverID, waxnerdID), http.StatusConflict, "not_linked")
	e.link(t)

	// The code is accepted in lower case and with spaces.
	code := strings.ToLower(strings.Replace(e.cloud.IssueDeviceCode(serverID, waxnerdID), "-", " ", 1))
	out := e.exchange(t, code, http.StatusOK, "")
	if !strings.HasPrefix(out.Token, store.SessionTokenPrefix) || out.Account.ID != e.owner.ID || out.Account.Name != ownerName {
		t.Fatalf("exchange answer %+v", out)
	}
	if list, _ := e.st.Sessions(context.Background(), e.owner.ID); len(list) != 1 || list[0].Device != "test phone" {
		t.Fatalf("sessions %+v", list)
	}

	// apiKey and the Bearer header both forward as the owner.
	if c := subsonicCode(t, e, "/rest/ping.view?v=1.16.1&c=test&apiKey="+out.Token, ""); c != -1 {
		t.Fatalf("apiKey: error %d, want forwarded", c)
	}
	if remoteUser != e.owner.ID || query.Has("apiKey") || query.Get("c") != "test" {
		t.Fatalf("backend received Remote-User %q, query %v", remoteUser, query)
	}
	remoteUser = ""
	if c := subsonicCode(t, e, "/rest/ping.view?v=1.16.1&c=test", out.Token); c != -1 {
		t.Fatalf("Bearer: error %d, want forwarded", c)
	}
	if remoteUser != e.owner.ID || auth != "" {
		t.Fatalf("backend received Remote-User %q, Authorization %q", remoteUser, auth)
	}

	if c := subsonicCode(t, e, "/rest/ping.view?u=owner&apiKey="+out.Token, ""); c != errMultipleAuth {
		t.Fatalf("apiKey with u: error %d, want %d", c, errMultipleAuth)
	}
	if c := subsonicCode(t, e, "/rest/ping.view?apiKey="+out.Token, out.Token); c != errMultipleAuth {
		t.Fatalf("apiKey with Bearer: error %d, want %d", c, errMultipleAuth)
	}
	if c := subsonicCode(t, e, "/rest/ping.view?"+creds(), out.Token); c != errMultipleAuth {
		t.Fatalf("Bearer with u, t, s: error %d, want %d", c, errMultipleAuth)
	}
	if c := subsonicCode(t, e, "/rest/ping.view?apiKey=wns_unknown", ""); c != errInvalidAPIKey {
		t.Fatalf("unknown apiKey: error %d, want %d", c, errInvalidAPIKey)
	}

	// wn-server's own routes need the Bearer session.
	if status, _ := e.do(t, http.MethodGet, "/api/v1/nothing", "", ""); status != http.StatusUnauthorized {
		t.Fatalf("/api/v1 without session: status %d, want 401", status)
	}
	if status, _ := e.do(t, http.MethodGet, "/api/v1/nothing", out.Token, ""); status != http.StatusNotFound {
		t.Fatalf("/api/v1 with session: status %d, want 404", status)
	}

	// A revoked session is refused on the next request.
	if status, b := e.do(t, http.MethodDelete, "/api/v1/session", out.Token, ""); status != http.StatusNoContent {
		t.Fatalf("DELETE: status %d, body %s", status, b)
	}
	if c := subsonicCode(t, e, "/rest/ping.view?apiKey="+out.Token, ""); c != errInvalidAPIKey {
		t.Fatalf("revoked apiKey: error %d, want %d", c, errInvalidAPIKey)
	}
	if status, _ := e.do(t, http.MethodDelete, "/api/v1/session", out.Token, ""); status != http.StatusUnauthorized {
		t.Fatalf("second DELETE: status %d, want 401", status)
	}
}

func TestSessionExchangeRateLimit(t *testing.T) {
	e := newEnv(t, http.NotFoundHandler(), true)
	e.link(t)
	for range exchangeMaxPerAddr {
		e.exchange(t, "K7QM-4XPA", http.StatusUnauthorized, "invalid_code")
	}
	// A valid code is refused too, without calling wn-cloud, so it stays valid.
	code := e.cloud.IssueDeviceCode(serverID, waxnerdID)
	e.exchange(t, code, http.StatusTooManyRequests, "rate_limited")
	e.exchange(t, code, http.StatusTooManyRequests, "rate_limited")
}

func TestLimiterCountsRelayedClientsSeparately(t *testing.T) {
	// Two relayed clients share one TCP peer address, the relay edge's.
	req := func(client string) *http.Request {
		r := httptest.NewRequest(http.MethodPost, "/api/v1/session", nil)
		r.RemoteAddr = "192.168.1.5:40000"
		if client != "" {
			r = r.WithContext(relayreq.WithRelay(r.Context(), client))
		}
		return r
	}
	l := newLimiter(time.Now, relayedMaxPerAddr, relayedMaxTotal)
	for range relayedMaxPerAddr {
		l.fail(relayreq.ClientAddr(req("203.0.113.9")))
	}
	if l.acquire(relayreq.ClientAddr(req("203.0.113.9"))) {
		t.Fatal("a relayed client over the limit was allowed")
	}
	for _, client := range []string{"203.0.113.10", ""} {
		addr := relayreq.ClientAddr(req(client))
		if !l.acquire(addr) {
			t.Fatalf("client %q was locked out by another relayed client", client)
		}
		l.release(addr, false)
	}
}

func TestLimiter(t *testing.T) {
	const perAddr, total = 5, 10
	now := time.Date(2026, 10, 16, 12, 0, 0, 0, time.UTC)
	l := newLimiter(func() time.Time { return now }, perAddr, total)
	fail := func(addr string) bool {
		if !l.acquire(addr) {
			return false
		}
		l.release(addr, true)
		return true
	}
	// Successes do not count.
	for range 10 {
		if !l.acquire("a") {
			t.Fatal("success counted")
		}
		l.release("a", false)
	}
	// The total limit holds across addresses.
	for i := range total {
		if !fail(string(rune('a' + i/perAddr))) {
			t.Fatalf("failure %d refused", i)
		}
	}
	if l.acquire("z") {
		t.Fatal("total limit not applied")
	}
	// Failures expire after the window.
	now = now.Add(exchangeWindow + time.Second)
	if !fail("a") {
		t.Fatal("failures did not expire")
	}
	// An exchange in progress counts until it ends.
	for range perAddr - 1 {
		if !l.acquire("b") {
			t.Fatal("pending refused early")
		}
	}
	if !l.acquire("b") || l.acquire("b") {
		t.Fatal("pending exchanges not counted")
	}
}

// userNavidrome serves the native user and library routes that a listener's
// first sign-in calls, and records the Remote-User of every /rest/ call.
type userNavidrome struct {
	mu         sync.Mutex
	users      []navidrome.User
	libraries  map[string][]int // Navidrome user id to library ids
	remoteUser string
}

func (f *userNavidrome) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	switch {
	case r.Method == http.MethodGet && r.URL.Path == "/api/user":
		_ = json.NewEncoder(w).Encode(f.users)
	case r.Method == http.MethodPost && r.URL.Path == "/api/user":
		var u navidrome.User
		_ = json.NewDecoder(r.Body).Decode(&u)
		u.ID = "nd" + u.UserName
		f.users = append(f.users, u)
		_ = json.NewEncoder(w).Encode(map[string]string{"id": u.ID})
	case r.Method == http.MethodGet && r.URL.Path == "/api/library":
		_, _ = w.Write([]byte(`[{"id":1,"name":"Music","path":"/music"},{"id":2,"name":"Other","path":"/other"}]`))
	case r.Method == http.MethodPut && strings.HasPrefix(r.URL.Path, "/api/user/"):
		var body struct {
			LibraryIDs []int `json:"libraryIds"`
		}
		_ = json.NewDecoder(r.Body).Decode(&body)
		f.libraries[strings.TrimSuffix(strings.TrimPrefix(r.URL.Path, "/api/user/"), "/library")] = body.LibraryIDs
		_, _ = w.Write([]byte(`[]`))
	default:
		f.remoteUser = r.Header.Get("Remote-User")
		_, _ = w.Write([]byte(`<subsonic-response status="ok"/>`))
	}
}

// relayed sends a request through the proxy handler as the relay package
// does, from the relayed client address client.
func (e *env) relayed(t *testing.T, method, path, client, body string) *httptest.ResponseRecorder {
	t.Helper()
	req := httptest.NewRequest(method, path, strings.NewReader(body))
	req.RemoteAddr = "127.0.0.1:40000"
	req = req.WithContext(relayreq.WithRelay(req.Context(), client))
	rec := httptest.NewRecorder()
	e.h.ServeHTTP(rec, req)
	return rec
}

// relayedExchange posts a code through the relay and checks the answer.
func (e *env) relayedExchange(t *testing.T, client, code string, wantStatus int, wantCode string) exchangeResponse {
	t.Helper()
	body, _ := json.Marshal(map[string]string{"code": code, "device": "listener phone"})
	rec := e.relayed(t, http.MethodPost, "/api/v1/session", client, string(body))
	if rec.Code != wantStatus {
		t.Fatalf("relayed exchange: status %d, want %d; body %s", rec.Code, wantStatus, rec.Body)
	}
	var out exchangeResponse
	if wantStatus != http.StatusOK {
		var eb apiError
		if err := json.Unmarshal(rec.Body.Bytes(), &eb); err != nil || eb.Code != wantCode {
			t.Fatalf("relayed exchange: body %s, want code %s", rec.Body, wantCode)
		}
		return out
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &out); err != nil {
		t.Fatal(err)
	}
	return out
}

func TestListenerSignInThroughRelay(t *testing.T) {
	const (
		listenerID = "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a71"
		strangerID = "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a72"
	)
	ctx := context.Background()
	now := time.Date(2026, 11, 2, 10, 0, 0, 0, time.UTC)
	nd := &userNavidrome{libraries: map[string][]int{}}
	e := newEnvOpts(t, nd, true, func(e *env) []Option {
		return []Option{WithNavidromeUsers(navidrome.NewClient(e.rt)), withNow(func() time.Time { return now })}
	})
	e.link(t)
	e.cloud.AddUser(listenerID, "ada@example.com")
	e.cloud.AddMember(serverID, listenerID, "listener")
	e.cloud.AddUser(strangerID, "eve@example.com")
	e.cloud.AddMember(serverID, strangerID, "listener")

	// Before any refresh of the member list a listener is refused as for an
	// outage, and the failure does not count.
	e.relayedExchange(t, "203.0.113.9", e.cloud.IssueDeviceCode(serverID, listenerID), http.StatusServiceUnavailable, "cloud_unreachable")
	members := []store.Member{{UserID: waxnerdID, Role: "owner"}, {UserID: listenerID, Role: "listener"}}
	if _, err := e.st.ReplaceMembers(ctx, members, now); err != nil {
		t.Fatal(err)
	}

	// A listener in the cached list gets a session through the relay.
	out := e.relayedExchange(t, "203.0.113.9", e.cloud.IssueDeviceCode(serverID, listenerID), http.StatusOK, "")
	acct, err := e.st.ListenerByUserID(ctx, listenerID)
	if err != nil || out.Account.ID != acct.ID || out.Account.Name != "ada" || acct.Kind != store.KindListener {
		t.Fatalf("answer %+v, account %+v, %v", out, acct, err)
	}
	if len(nd.users) != 1 || nd.users[0].UserName != acct.ID || nd.users[0].IsAdmin || nd.users[0].Name != "ada" {
		t.Fatalf("navidrome users %+v, want one non-admin user named after the account id", nd.users)
	}
	if libs := nd.libraries["nd"+acct.ID]; len(libs) != 2 {
		t.Fatalf("listener libraries %v, want both", libs)
	}
	// The proxy forwards with the listener's Remote-User.
	if rec := e.relayed(t, http.MethodGet, "/rest/ping.view?apiKey="+out.Token, "203.0.113.9", ""); rec.Code != http.StatusOK || nd.remoteUser != acct.ID {
		t.Fatalf("listener ping: status %d, Remote-User %q", rec.Code, nd.remoteUser)
	}
	// Rating writes and the web UI are the owner's.
	nd.remoteUser = ""
	if c := subsonicCode(t, e, "/rest/setRating.view?id=x&rating=5&apiKey="+out.Token, ""); c != errNotAuthorized {
		t.Fatalf("listener setRating: error %d, want %d", c, errNotAuthorized)
	}
	if c := subsonicCode(t, e, "/rest/ping.view?u=ada&p="+acct.AppPassword, ""); c != errWrongCredential {
		t.Fatalf("listener password sign-in: error %d, want %d", c, errWrongCredential)
	}
	// A second sign-in reuses the account and the Navidrome user.
	e.relayedExchange(t, "203.0.113.9", e.cloud.IssueDeviceCode(serverID, listenerID), http.StatusOK, "")
	if len(nd.users) != 1 {
		t.Fatalf("navidrome users %+v after a second sign-in", nd.users)
	}

	// A member at wn-cloud who is not in the cached list is refused, and it
	// counts: 2 failures per relayed address.
	for range relayedMaxPerAddr {
		e.relayedExchange(t, "203.0.113.10", e.cloud.IssueDeviceCode(serverID, strangerID), http.StatusForbidden, "not_a_member")
	}
	e.relayedExchange(t, "203.0.113.10", e.cloud.IssueDeviceCode(serverID, listenerID), http.StatusTooManyRequests, "rate_limited")

	// 31 days after the last refresh a listener is refused and the owner
	// is served.
	now = now.Add(31 * 24 * time.Hour)
	e.relayedExchange(t, "203.0.113.9", e.cloud.IssueDeviceCode(serverID, listenerID), http.StatusServiceUnavailable, "cloud_unreachable")
	e.relayedExchange(t, "203.0.113.9", e.cloud.IssueDeviceCode(serverID, waxnerdID), http.StatusOK, "")
	e.exchange(t, e.cloud.IssueDeviceCode(serverID, waxnerdID), http.StatusOK, "")

	// A refresh without the listener ends the session; re-adding enables
	// the account again.
	res, err := e.st.ReplaceMembers(ctx, members[:1], now)
	if err != nil || len(res.Disabled) != 1 || res.SessionsEnded != 2 {
		t.Fatalf("refresh = %+v, %v", res, err)
	}
	if c := subsonicCode(t, e, "/rest/ping.view?apiKey="+out.Token, ""); c != errInvalidAPIKey {
		t.Fatalf("removed listener: error %d, want %d", c, errInvalidAPIKey)
	}
	if res, err := e.st.ReplaceMembers(ctx, members, now); err != nil || len(res.Enabled) != 1 {
		t.Fatalf("refresh with the listener back = %+v, %v", res, err)
	}
	e.relayedExchange(t, "203.0.113.9", e.cloud.IssueDeviceCode(serverID, listenerID), http.StatusOK, "")
}
