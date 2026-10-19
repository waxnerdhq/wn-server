package proxy

import (
	"bytes"
	"context"
	"encoding/json"
	"encoding/xml"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/relayreq"
	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
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
	e.exchange(t, e.cloud.IssueDeviceCode(serverID, otherID), http.StatusForbidden, "not_owner")

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

func TestSessionExchangeRefusedThroughRelay(t *testing.T) {
	e := newEnv(t, http.NotFoundHandler(), true)
	e.link(t)
	ready := make(chan struct{})
	close(ready)
	h := New(e.rt, ready, e.st, wncloud.New(e.cloudURL, "test"), slog.New(slog.DiscardHandler))
	code := e.cloud.IssueDeviceCode(serverID, waxnerdID)
	body, _ := json.Marshal(map[string]string{"code": code, "device": "test phone"})
	// More requests than the limits allow: none counts.
	for range 2 * exchangeMaxTotal {
		req := httptest.NewRequest(http.MethodPost, "/api/v1/session", bytes.NewReader(body))
		req.RemoteAddr = "192.168.1.5:40000"
		req = req.WithContext(relayreq.WithRelay(req.Context(), "203.0.113.9"))
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, req)
		var eb apiError
		if rec.Code != http.StatusForbidden || json.Unmarshal(rec.Body.Bytes(), &eb) != nil || eb.Code != "not_available" || eb.Error == "" {
			t.Fatalf("relayed exchange: status %d, body %s", rec.Code, rec.Body)
		}
	}
	// The code was not spent and the LAN limit was not used.
	req := httptest.NewRequest(http.MethodPost, "/api/v1/session", bytes.NewReader(body))
	req.RemoteAddr = "192.168.1.5:40000"
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("LAN exchange after relayed refusals: status %d, body %s", rec.Code, rec.Body)
	}
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
	l := newLimiter(time.Now)
	for range exchangeMaxPerAddr {
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
	now := time.Date(2026, 10, 16, 12, 0, 0, 0, time.UTC)
	l := newLimiter(func() time.Time { return now })
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
	for i := range exchangeMaxTotal {
		if !fail(string(rune('a' + i/exchangeMaxPerAddr))) {
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
	for range exchangeMaxPerAddr - 1 {
		if !l.acquire("b") {
			t.Fatal("pending refused early")
		}
	}
	if !l.acquire("b") || l.acquire("b") {
		t.Fatal("pending exchanges not counted")
	}
}
