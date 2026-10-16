package proxy

import (
	"bufio"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync/atomic"
	"testing"
)

func newProxy(t *testing.T, backend http.Handler, ready bool) (*httptest.Server, *atomic.Int32) {
	t.Helper()
	var hits atomic.Int32
	be := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		backend.ServeHTTP(w, r)
	}))
	t.Cleanup(be.Close)
	target, err := url.Parse(be.URL)
	if err != nil {
		t.Fatal(err)
	}
	ch := make(chan struct{})
	if ready {
		close(ch)
	}
	srv := httptest.NewServer(New(target, ch, nil))
	t.Cleanup(srv.Close)
	return srv, &hits
}

func TestStripsRemoteUser(t *testing.T) {
	var got []string
	srv, _ := newProxy(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Header.Values("Remote-User")
	}), true)
	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/rest/ping.view", nil)
	req.Header.Set("Remote-User", "admin")
	req.Header.Add("remote-user", "other")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if len(got) != 0 {
		t.Fatalf("backend received Remote-User %v", got)
	}
}

func TestNotReadyReturns503(t *testing.T) {
	srv, hits := newProxy(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}), false)
	resp, err := http.Get(srv.URL + "/rest/ping.view")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusServiceUnavailable {
		t.Fatalf("status = %d, want 503", resp.StatusCode)
	}
	if hits.Load() != 0 {
		t.Fatalf("backend received %d requests before ready", hits.Load())
	}
}

func TestStreamsChunks(t *testing.T) {
	const chunks = 5
	srv, _ := newProxy(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		for i := 0; i < chunks; i++ {
			_, _ = io.WriteString(w, "chunk\n")
			w.(http.Flusher).Flush()
		}
	}), true)
	resp, err := http.Get(srv.URL + "/rest/stream")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	sc := bufio.NewScanner(resp.Body)
	n := 0
	for sc.Scan() {
		if strings.TrimSpace(sc.Text()) != "chunk" {
			t.Fatalf("unexpected line %q", sc.Text())
		}
		n++
	}
	if n != chunks {
		t.Fatalf("got %d chunks, want %d", n, chunks)
	}
}
