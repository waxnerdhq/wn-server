package proxy

import (
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"

	"github.com/waxnerdhq/wn-server/internal/relayreq"
)

func TestRelayMaxBitrate(t *testing.T) {
	tests := []struct {
		name       string
		cap        int
		relayed    bool
		method     string
		path       string
		query      string
		form       string
		wantRate   string // "" means the parameter is absent
		wantFormat string
	}{
		{name: "higher is lowered and raw dropped", cap: 128, relayed: true, path: "/rest/stream", query: "id=1&maxBitRate=320&format=raw", wantRate: "128"},
		{name: "lower is kept", cap: 128, relayed: true, path: "/rest/stream", query: "id=1&maxBitRate=96", wantRate: "96"},
		{name: "equal is kept", cap: 128, relayed: true, path: "/rest/stream", query: "id=1&maxBitRate=128", wantRate: "128"},
		{name: "absent is set", cap: 128, relayed: true, path: "/rest/stream.view", query: "id=1", wantRate: "128"},
		{name: "zero means unlimited and is set", cap: 128, relayed: true, path: "/rest/stream", query: "id=1&maxBitRate=0", wantRate: "128"},
		{name: "not a number is set", cap: 128, relayed: true, path: "/rest/stream", query: "id=1&maxBitRate=high", wantRate: "128"},
		{name: "negative is set", cap: 128, relayed: true, path: "/rest/stream", query: "id=1&maxBitRate=-5", wantRate: "128"},
		{name: "other format is kept", cap: 128, relayed: true, path: "/rest/stream", query: "id=1&format=mp3", wantRate: "128", wantFormat: "mp3"},
		{name: "posted form", cap: 128, relayed: true, method: http.MethodPost, path: "/rest/stream", form: "id=1&maxBitRate=320&format=raw", wantRate: "128"},
		{name: "LAN request is untouched", cap: 128, path: "/rest/stream", query: "id=1&maxBitRate=320&format=raw", wantRate: "320", wantFormat: "raw"},
		{name: "download is untouched", cap: 128, relayed: true, path: "/rest/download", query: "id=1&format=raw", wantFormat: "raw"},
		{name: "no cap changes nothing", cap: 0, relayed: true, path: "/rest/stream", query: "id=1&maxBitRate=320&format=raw", wantRate: "320", wantFormat: "raw"},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			var got url.Values
			e := newEnv(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				got = r.URL.Query()
			}), true)
			ready := make(chan struct{})
			close(ready)
			h := New(e.rt, ready, e.st, nil, nil, WithRelayMaxBitrate(tc.cap))
			method := tc.method
			if method == "" {
				method = http.MethodGet
			}
			target := tc.path + "?" + creds()
			if tc.query != "" {
				target += "&" + tc.query
			}
			req := httptest.NewRequest(method, target, strings.NewReader(tc.form))
			if tc.form != "" {
				req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
			}
			if tc.relayed {
				req = req.WithContext(relayreq.WithRelay(req.Context(), "203.0.113.9"))
			}
			h.ServeHTTP(httptest.NewRecorder(), req)
			if got == nil {
				t.Fatal("Navidrome was not reached")
			}
			if v := got.Get("maxBitRate"); v != tc.wantRate {
				t.Errorf("maxBitRate %q, want %q", v, tc.wantRate)
			}
			if v := got.Get("format"); v != tc.wantFormat {
				t.Errorf("format %q, want %q", v, tc.wantFormat)
			}
			if got.Get("id") != "1" {
				t.Errorf("id lost: %v", got)
			}
		})
	}
}
