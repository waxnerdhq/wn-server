package relay

import (
	"bytes"
	"context"
	"crypto/x509"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/hashicorp/yamux"

	"github.com/waxnerdhq/wn-server/internal/relayreq"
)

const (
	testServerID = "0193a0b2-77c1-7f00-8e21-3d9c5b6a4f10"
	testHost     = testServerID + ".relay.waxnerd.com"
)

// edge is an in-process edge: a loopback TLS listener that answers the
// handshake and acts as the yamux client. A new tunnel replaces the previous
// one, as the edge in wn-cloud does.
type edge struct {
	t   *testing.T
	srv *httptest.Server

	mu       sync.Mutex
	statuses []int // answers for the next handshakes; 101 when empty
	headers  []http.Header
	paths    []string
	sessions []*yamux.Session
	conns    []*holdConn
}

func newEdge(t *testing.T) *edge {
	e := &edge{t: t}
	e.srv = httptest.NewTLSServer(http.HandlerFunc(e.tunnel))
	t.Cleanup(func() {
		e.mu.Lock()
		for _, s := range e.sessions {
			_ = s.Close()
		}
		e.mu.Unlock()
		e.srv.Close()
	})
	return e
}

func (e *edge) url() string { return e.srv.URL + "/v1/tunnel" }

func (e *edge) roots() *x509.CertPool {
	p := x509.NewCertPool()
	p.AddCert(e.srv.Certificate())
	return p
}

func (e *edge) answer(statuses ...int) {
	e.mu.Lock()
	e.statuses = append(e.statuses, statuses...)
	e.mu.Unlock()
}

func (e *edge) dials() int {
	e.mu.Lock()
	defer e.mu.Unlock()
	return len(e.headers)
}

func (e *edge) session(i int) *yamux.Session {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.sessions[i]
}

func (e *edge) tunnel(w http.ResponseWriter, r *http.Request) {
	e.mu.Lock()
	e.headers = append(e.headers, r.Header.Clone())
	e.paths = append(e.paths, r.Method+" "+r.URL.Path)
	status := http.StatusSwitchingProtocols
	if len(e.statuses) > 0 {
		status, e.statuses = e.statuses[0], e.statuses[1:]
	}
	n := len(e.headers)
	e.mu.Unlock()

	if status != http.StatusSwitchingProtocols {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(status)
		_, _ = fmt.Fprintf(w, `{"error":"no","code":"code%d"}`, status)
		return
	}
	raw, _, err := w.(http.Hijacker).Hijack()
	if err != nil {
		e.t.Errorf("hijack: %v", err)
		return
	}
	conn := &holdConn{Conn: raw, closed: make(chan struct{})}
	cfg := yamux.DefaultConfig()
	cfg.EnableKeepAlive = false
	cfg.LogOutput = io.Discard
	sess, err := yamux.Client(conn, cfg)
	if err != nil {
		e.t.Errorf("yamux client: %v", err)
		return
	}
	e.mu.Lock()
	if len(e.sessions) > 0 {
		_ = e.sessions[len(e.sessions)-1].Close()
	}
	e.sessions = append(e.sessions, sess)
	e.conns = append(e.conns, conn)
	e.mu.Unlock()
	// The yamux client sends nothing until it opens a stream, so the 101
	// answer goes out after the session is recorded.
	_, _ = fmt.Fprintf(raw, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: %s\r\nConnection: Upgrade\r\nWn-Relay-Host: %s\r\nWn-Relay-Session: s%d\r\n\r\n", Protocol, testHost, n)
}

func (e *edge) conn(i int) *holdConn {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.conns[i]
}

// holdConn stops reading once hold is set, so the edge answers no ping: a
// silent drop as wn-server sees it.
type holdConn struct {
	net.Conn
	hold      atomic.Bool
	closed    chan struct{}
	closeOnce sync.Once
}

func (c *holdConn) Read(p []byte) (int, error) {
	if c.hold.Load() {
		<-c.closed
		return 0, io.EOF
	}
	return c.Conn.Read(p)
}

func (c *holdConn) Close() error {
	c.closeOnce.Do(func() { close(c.closed) })
	return c.Conn.Close()
}

// client sends requests down session i of the edge.
func (e *edge) client(i int) *http.Client {
	sess := e.session(i)
	return &http.Client{Transport: &http.Transport{
		DialContext:       func(context.Context, string, string) (net.Conn, error) { return sess.Open() },
		DisableKeepAlives: true,
	}}
}

// harness runs a Relay against an edge and records its states.
type harness struct {
	edge   *edge
	relay  *Relay
	token  atomic.Value
	reads  atomic.Int32
	cancel context.CancelFunc
	done   chan struct{}

	mu     sync.Mutex
	states []State
}

func start(t *testing.T, e *edge, h http.Handler, tune func(*config)) *harness {
	t.Helper()
	hs := &harness{edge: e, done: make(chan struct{})}
	hs.token.Store("token-1")
	hs.relay = New(Options{
		Handler: h,
		Credential: func() (string, string, bool) {
			hs.reads.Add(1)
			tok := hs.token.Load().(string)
			return tok, testServerID, tok != ""
		},
		ConnectURL: func() (string, bool) { return e.url(), true },
		Version:    "v1.2.3",
		OnState: func(s State) {
			hs.mu.Lock()
			hs.states = append(hs.states, s)
			hs.mu.Unlock()
		},
	})
	cfg := &hs.relay.cfg
	cfg.roots = e.roots()
	cfg.waitPoll = 5 * time.Millisecond
	cfg.refusedPoll = 10 * time.Millisecond
	cfg.backoffMin = time.Millisecond
	cfg.backoffMax = 5 * time.Millisecond
	cfg.shutdownGrace = 2 * time.Second
	if tune != nil {
		tune(cfg)
	}
	ctx, cancel := context.WithCancel(context.Background())
	hs.cancel = cancel
	go func() { hs.relay.Run(ctx); close(hs.done) }()
	t.Cleanup(hs.stop)
	return hs
}

func (h *harness) stop() {
	h.cancel()
	<-h.done
}

func (h *harness) names() []StateName {
	h.mu.Lock()
	defer h.mu.Unlock()
	var out []StateName
	for _, s := range h.states {
		out = append(out, s.Name)
	}
	return out
}

func (h *harness) last() State {
	h.mu.Lock()
	defer h.mu.Unlock()
	if len(h.states) == 0 {
		return State{}
	}
	return h.states[len(h.states)-1]
}

func waitFor(t *testing.T, what string, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for %s", what)
		}
		time.Sleep(time.Millisecond)
	}
}

func TestTunnel(t *testing.T) {
	e := newEdge(t)
	content := []byte("0123456789abcdefghijklmnopqrstuvwxyz")
	type seen struct {
		relayed bool
		addr    string
		header  string
	}
	got := make(chan seen, 1)
	mux := http.NewServeMux()
	mux.HandleFunc("/mark", func(w http.ResponseWriter, r *http.Request) {
		got <- seen{relayreq.IsRelayed(r.Context()), relayreq.ClientAddr(r), r.Header.Get("X-Relay-Client")}
	})
	mux.HandleFunc("/file", func(w http.ResponseWriter, r *http.Request) {
		http.ServeContent(w, r, "f.flac", time.Unix(0, 0), bytes.NewReader(content))
	})
	h := start(t, e, mux, func(*config) {})
	// Start without a credential: the package waits, then dials.
	h.token.Store("")
	waitFor(t, "waiting", func() bool { return h.last().Name == Waiting })
	h.token.Store("token-1")
	waitFor(t, "connected", func() bool { return h.last().Name == Connected })

	if names := h.names(); fmt.Sprint(names) != "[waiting connecting connected]" {
		t.Fatalf("states = %v", names)
	}
	if st := h.last(); st.Host != testHost || !strings.HasPrefix(st.Text(), "relay: connected since ") || !strings.HasSuffix(st.Text(), " at https://"+testHost) {
		t.Fatalf("state = %+v, text %q", st, st.Text())
	}
	hdr := e.headers[0]
	want := map[string]string{
		"Authorization":     "Bearer token-1",
		"Upgrade":           "wn-relay/1",
		"Connection":        "Upgrade",
		"Wn-Server-Id":      testServerID,
		"Wn-Server-Version": "v1.2.3",
	}
	for k, v := range want {
		if hdr.Get(k) != v {
			t.Errorf("%s = %q, want %q", k, hdr.Get(k), v)
		}
	}
	if e.paths[0] != "GET /v1/tunnel" {
		t.Errorf("request = %q", e.paths[0])
	}

	c := e.client(0)
	req, _ := http.NewRequest("GET", "http://"+testHost+"/mark", nil)
	req.Header.Set("X-Relay-Client", "203.0.113.7")
	res, err := c.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	res.Body.Close()
	if s := <-got; !s.relayed || s.addr != "203.0.113.7" || s.header != "" {
		t.Fatalf("handler saw %+v", s)
	}

	req, _ = http.NewRequest("GET", "http://"+testHost+"/file", nil)
	req.Header.Set("Range", "bytes=10-19")
	res, err = c.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(res.Body)
	res.Body.Close()
	if res.StatusCode != http.StatusPartialContent || res.Header.Get("Content-Range") != "bytes 10-19/36" || string(body) != "abcdefghij" {
		t.Fatalf("range answer %d %q %q", res.StatusCode, res.Header.Get("Content-Range"), body)
	}
}

func TestShutdownWaitsForInFlight(t *testing.T) {
	e := newEdge(t)
	entered := make(chan struct{})
	release := make(chan struct{})
	h := start(t, e, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		close(entered)
		<-release
		_, _ = io.WriteString(w, "done")
	}), nil)
	waitFor(t, "connected", func() bool { return h.last().Name == Connected })

	type result struct {
		body string
		err  error
	}
	resc := make(chan result, 1)
	go func() {
		res, err := e.client(0).Get("http://" + testHost + "/slow")
		if err != nil {
			resc <- result{err: err}
			return
		}
		b, err := io.ReadAll(res.Body)
		res.Body.Close()
		resc <- result{string(b), err}
	}()
	<-entered
	h.cancel()
	select {
	case <-h.done:
		t.Fatal("Run returned before the in-flight request ended")
	case <-time.After(50 * time.Millisecond):
	}
	close(release)
	if r := <-resc; r.err != nil || r.body != "done" {
		t.Fatalf("in-flight request: %q, %v", r.body, r.err)
	}
	<-h.done
	waitFor(t, "session closed", func() bool { return e.session(0).IsClosed() })
}

func TestHandshakeErrors(t *testing.T) {
	cases := []struct {
		status int
		state  StateName
		stops  bool
		text   string
	}{
		{400, Refused, true, "relay: refused by the edge: invalid_request"},
		{401, CredentialRejected, true, "relay: credential rejected, run wn-server logout, then wn-server login"},
		{403, CredentialRejected, true, "relay: credential rejected, run wn-server logout, then wn-server login"},
		{409, UpdateRequired, true, "relay: update wn-server"},
		{402, NotEnabled, false, "relay: not enabled at wn-cloud"},
		{429, Unreachable, false, ""},
		{503, Unreachable, false, ""},
		{500, Unreachable, false, ""},
	}
	for _, tc := range cases {
		t.Run(fmt.Sprint(tc.status), func(t *testing.T) {
			e := newEdge(t)
			e.answer(tc.status)
			h := start(t, e, http.NotFoundHandler(), nil)
			waitFor(t, string(tc.state), func() bool { return h.last().Name == tc.state })
			st := h.last()
			if tc.text != "" && st.Text() != tc.text {
				t.Errorf("text = %q, want %q", st.Text(), tc.text)
			}
			if tc.state == Unreachable && !strings.Contains(st.Text(), fmt.Sprintf("edge answered %d", tc.status)) {
				t.Errorf("text = %q", st.Text())
			}
			if tc.stops {
				// The package re-reads the credential and does not dial with
				// the refused token.
				reads := h.reads.Load()
				waitFor(t, "credential re-reads", func() bool { return h.reads.Load() >= reads+3 })
				if n := e.dials(); n != 1 {
					t.Fatalf("dials with the refused token = %d, want 1", n)
				}
				h.token.Store("token-2")
			}
			waitFor(t, "connected", func() bool { return h.last().Name == Connected })
			if n := e.dials(); n != 2 {
				t.Fatalf("dials = %d, want 2", n)
			}
			if tc.stops && e.headers[1].Get("Authorization") != "Bearer token-2" {
				t.Fatalf("second dial used %q", e.headers[1].Get("Authorization"))
			}
			if !tc.stops && e.headers[1].Get("Authorization") != "Bearer token-1" {
				t.Fatalf("second dial used %q", e.headers[1].Get("Authorization"))
			}
		})
	}
}

func TestReconnectAfterDrop(t *testing.T) {
	e := newEdge(t)
	h := start(t, e, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "ok")
	}), nil)
	waitFor(t, "connected", func() bool { return h.last().Name == Connected })
	_ = e.conn(0).Conn.Close()
	waitFor(t, "second tunnel", func() bool { return e.dials() == 2 && h.last().Name == Connected })
	if names := fmt.Sprint(h.names()); names != "[connecting connected unreachable connected]" {
		t.Fatalf("states = %s", names)
	}
	h.mu.Lock()
	st := h.states[2]
	h.mu.Unlock()
	if !strings.HasPrefix(st.Err, "tunnel closed: ") {
		t.Fatalf("unreachable error = %q", st.Err)
	}
	res, err := e.client(1).Get("http://" + testHost + "/")
	if err != nil {
		t.Fatal(err)
	}
	res.Body.Close()
}

func TestReplacement(t *testing.T) {
	e := newEdge(t)
	h := start(t, e, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "ok")
	}), func(c *config) {
		c.keepAlive = 20 * time.Millisecond
		c.writeTimeout = 20 * time.Millisecond
	})
	waitFor(t, "connected", func() bool { return h.last().Name == Connected })
	// The first tunnel goes silent. wn-server sees the keepalive fail and
	// opens a second tunnel, and the edge closes the first one.
	e.conn(0).hold.Store(true)
	waitFor(t, "second tunnel", func() bool { return e.dials() == 2 && h.last().Name == Connected })
	waitFor(t, "first session closed", func() bool { return e.session(0).IsClosed() })
	time.Sleep(100 * time.Millisecond)
	if n := e.dials(); n != 2 || h.last().Name != Connected {
		t.Fatalf("dials = %d, state %s after the replacement", n, h.last().Name)
	}
	res, err := e.client(1).Get("http://" + testHost + "/")
	if err != nil {
		t.Fatal(err)
	}
	b, _ := io.ReadAll(res.Body)
	res.Body.Close()
	if string(b) != "ok" {
		t.Fatalf("body %q", b)
	}
}
