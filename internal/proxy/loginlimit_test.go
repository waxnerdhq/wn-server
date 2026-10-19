package proxy

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/relayreq"
	"github.com/waxnerdhq/wn-server/internal/store"
)

// failingStore fails AccountByName with a store error while fail is set.
type failingStore struct {
	Store
	fail *bool
}

func (f failingStore) AccountByName(ctx context.Context, name string) (store.Account, error) {
	if *f.fail {
		return store.Account{}, errors.New("database is locked")
	}
	return f.Store.AccountByName(ctx, name)
}

func TestSubsonicLoginLimit(t *testing.T) {
	e := newEnv(t, http.NotFoundHandler(), true)
	now := time.Date(2026, 10, 17, 12, 0, 0, 0, time.UTC)
	var logs bytes.Buffer
	logger := slog.New(slog.NewTextHandler(&logs, nil))
	lim := newLoginLimiter(func() time.Time { return now })
	forwarded := 0
	storeFails := false
	st := failingStore{Store: e.st, fail: &storeFails}
	h := subsonicAuth(st, newSessions(st, nil, logger), lim, logger,
		http.HandlerFunc(func(http.ResponseWriter, *http.Request) { forwarded++ }))
	// call sends a request from addr and returns the Subsonic error code, or
	// 0 when the request was forwarded.
	call := func(addr, query string) int {
		t.Helper()
		before := forwarded
		r := httptest.NewRequest(http.MethodGet, "/rest/ping.view?"+query+"&f=json", nil)
		r.RemoteAddr = addr
		w := httptest.NewRecorder()
		h.ServeHTTP(w, r)
		if forwarded > before {
			return 0
		}
		var v struct {
			R struct {
				Error struct {
					Code int `json:"code"`
				} `json:"error"`
			} `json:"subsonic-response"`
		}
		if err := json.Unmarshal(w.Body.Bytes(), &v); err != nil {
			t.Fatalf("body %s: %v", w.Body, err)
		}
		return v.R.Error.Code
	}
	bad := "u=owner&p=nope&v=1&c=x"
	good := "u=owner&p=" + ownerPass + "&v=1&c=x"
	const a = "192.0.2.1:5000"

	// A success resets the count, so 9 failures before it do not add up.
	for range 2 {
		for range loginMaxPerAddr - 1 {
			if c := call(a, bad); c != 40 {
				t.Fatalf("bad credentials: code %d, want 40", c)
			}
		}
		if c := call(a, good); c != 0 {
			t.Fatalf("good credentials: code %d, want forwarded", c)
		}
	}
	// A missing credential does not count.
	for range 3 {
		if c := call(a, "u=owner&v=1&c=x"); c != 10 {
			t.Fatalf("missing credential: code %d, want 10", c)
		}
	}
	// A store error does not count.
	storeFails = true
	for range 2 * loginMaxPerAddr {
		if c := call(a, good); c != 40 {
			t.Fatalf("store error: code %d, want 40", c)
		}
	}
	storeFails = false
	for range loginMaxPerAddr - 1 {
		call(a, bad)
	}
	if c := call(a, good); c != 0 {
		t.Fatalf("after store errors: code %d, want forwarded", c)
	}
	// After 10 failures, an unknown user among them, the correct password is
	// refused without a check.
	for range loginMaxPerAddr - 1 {
		call(a, bad)
	}
	call(a, "u=nobody&p=nope&v=1&c=x")
	for range 3 {
		if c := call("192.0.2.1:6000", good); c != 40 {
			t.Fatalf("limited address: code %d, want 40", c)
		}
	}
	if n := strings.Count(logs.String(), "rate limited"); n != 1 {
		t.Fatalf("%d limit log lines, want 1:\n%s", n, logs.String())
	}
	// Other addresses are not affected. IPv6 counts by its /64.
	if c := call("192.0.2.2:5000", good); c != 0 {
		t.Fatalf("other address: code %d, want forwarded", c)
	}
	for i := range loginMaxPerAddr {
		call(fmt.Sprintf("[2001:db8:0:1::%x]:5000", i+1), bad)
	}
	if c := call("[2001:db8:0:1::ffff]:5000", good); c != 40 {
		t.Fatalf("same /64: code %d, want 40", c)
	}
	if c := call("[2001:db8:0:2::1]:5000", good); c != 0 {
		t.Fatalf("other /64: code %d, want forwarded", c)
	}
	// After the window the failures expire and the address is checked again.
	now = now.Add(loginWindow + time.Second)
	if c := call(a, good); c != 0 {
		t.Fatalf("after the window: code %d, want forwarded", c)
	}
	// Idle entries are evicted, so the map stays bounded.
	now = now.Add(3 * loginWindow)
	call("192.0.2.3:5000", good)
	if len(lim.entries) > 1 {
		t.Fatalf("%d entries after the idle time, want at most 1", len(lim.entries))
	}
}

func TestSubsonicLoginLimitCountsRelayedClientsSeparately(t *testing.T) {
	e := newEnv(t, http.NotFoundHandler(), true)
	logger := slog.New(slog.DiscardHandler)
	lim := newLoginLimiter(time.Now)
	forwarded := 0
	h := subsonicAuth(e.st, newSessions(e.st, nil, logger), lim, logger,
		http.HandlerFunc(func(http.ResponseWriter, *http.Request) { forwarded++ }))
	// Every request has the TCP peer address of the relay edge.
	call := func(client, query string) bool {
		t.Helper()
		before := forwarded
		r := httptest.NewRequest(http.MethodGet, "/rest/ping.view?"+query+"&f=json", nil)
		r.RemoteAddr = "192.168.1.5:40000"
		if client != "" {
			r = r.WithContext(relayreq.WithRelay(r.Context(), client))
		}
		h.ServeHTTP(httptest.NewRecorder(), r)
		return forwarded > before
	}
	bad := "u=owner&p=nope&v=1&c=x"
	good := "u=owner&p=" + ownerPass + "&v=1&c=x"
	for range loginMaxPerAddr {
		call("203.0.113.9", bad)
	}
	if call("203.0.113.9", good) {
		t.Fatal("a relayed client over the limit was allowed")
	}
	if !call("203.0.113.10", good) {
		t.Fatal("another relayed client was locked out")
	}
	if !call("", good) {
		t.Fatal("the TCP peer address was locked out by relayed clients")
	}
}
