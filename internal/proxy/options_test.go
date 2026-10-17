package proxy

import (
	"log/slog"
	"net/http"
	"net/http/httptest"
	"net/url"
	"testing"
)

func TestWithUIServesBeforeReady(t *testing.T) {
	ui := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(http.StatusTeapot) })
	h := New(&url.URL{Scheme: "http", Host: "127.0.0.1:1"}, make(chan struct{}), nil, nil,
		slog.New(slog.DiscardHandler), WithUI(ui))
	for path, want := range map[string]int{
		"/":           http.StatusTeapot,
		"/ui":         http.StatusTeapot,
		"/ui/account": http.StatusTeapot,
		"/ui/../app/": http.StatusNotFound,
		"/uix":        http.StatusNotFound,
		"/app/":       http.StatusNotFound,
	} {
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "http://wn.test"+path, nil))
		if rec.Code != want {
			t.Errorf("%s: status %d, want %d", path, rec.Code, want)
		}
	}
}
