package proxy

import (
	"bufio"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"sync/atomic"
	"testing"
	"time"
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
	firstRead := make(chan struct{})
	srv, _ := newProxy(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "one\n")
		w.(http.Flusher).Flush()
		// A buffering proxy never delivers the first chunk until the handler
		// returns, so the client cannot signal and this wait times out.
		select {
		case <-firstRead:
			_, _ = io.WriteString(w, "two\n")
		case <-time.After(5 * time.Second):
		case <-r.Context().Done():
		}
	}), true)
	client := &http.Client{Timeout: 10 * time.Second}
	resp, err := client.Get(srv.URL + "/rest/stream")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	br := bufio.NewReader(resp.Body)

	type result struct {
		line string
		err  error
	}
	readLine := func() <-chan result {
		ch := make(chan result, 1)
		go func() {
			l, err := br.ReadString('\n')
			ch <- result{l, err}
		}()
		return ch
	}
	wait := func(ch <-chan result, want string) {
		t.Helper()
		select {
		case r := <-ch:
			if r.err != nil || r.line != want {
				t.Fatalf("read %q, %v; want %q", r.line, r.err, want)
			}
		case <-time.After(3 * time.Second):
			t.Fatalf("timed out waiting for %q: proxy is buffering", want)
		}
	}
	wait(readLine(), "one\n")
	close(firstRead)
	wait(readLine(), "two\n")
}
