package proxy

import (
	"bufio"
	"context"
	"crypto/md5"
	"encoding/hex"
	"encoding/json"
	"encoding/xml"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
	"github.com/waxnerdhq/wn-server/internal/wncloud/stub"
)

const (
	ownerName  = "owner"
	ownerPass  = "s3cretPassw0rd"
	waxnerdID  = "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70"
	serverID   = "7f3c9a2e-1b4d-4e8f-9a6b-2c5d8e1f0a3b"
	cloudToken = "wn_servercredential"
)

// creds is a valid token-scheme query for the owner.
func creds() string {
	sum := md5.Sum([]byte(ownerPass + "abc123"))
	return "u=" + ownerName + "&t=" + hex.EncodeToString(sum[:]) + "&s=abc123&v=1.16.1&c=test"
}

// env is a proxy in front of a fake Navidrome, with a real store and the
// wn-cloud stub.
type env struct {
	srv   *httptest.Server
	hits  *atomic.Int32
	st    *store.Store
	owner store.Account
	cloud *stub.Stub
	// rt dials the fake Navidrome's Unix socket. cloudURL is the wn-cloud
	// stub.
	rt       http.RoundTripper
	cloudURL string
}

// listenUnix listens on a Unix socket in a short temporary directory. macOS
// limits socket paths to 104 bytes, and t.TempDir paths can be longer.
func listenUnix(t *testing.T) (net.Listener, string) {
	t.Helper()
	dir, err := os.MkdirTemp("", "wn")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	sock := filepath.Join(dir, "nd.sock")
	ln, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	return ln, sock
}

func newEnv(t *testing.T, backend http.Handler, ready bool) *env {
	t.Helper()
	return newEnvOpts(t, backend, ready, nil)
}

// newEnvOpts is newEnv with proxy options built from the environment.
func newEnvOpts(t *testing.T, backend http.Handler, ready bool, opts func(*env) []Option) *env {
	t.Helper()
	ctx := context.Background()
	var hits atomic.Int32
	be := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		backend.ServeHTTP(w, r)
	}))
	ln, sock := listenUnix(t)
	be.Listener.Close()
	be.Listener = ln
	be.Start()
	t.Cleanup(be.Close)
	rt := navidrome.NewTransport(sock)
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
	cloud := stub.New()
	cloudSrv := httptest.NewServer(cloud)
	t.Cleanup(cloudSrv.Close)
	ch := make(chan struct{})
	if ready {
		close(ch)
	}
	logger := slog.New(slog.DiscardHandler)
	e := &env{hits: &hits, st: st, owner: owner, cloud: cloud, rt: rt, cloudURL: cloudSrv.URL}
	var o []Option
	if opts != nil {
		o = opts(e)
	}
	e.srv = httptest.NewServer(New(rt, ch, st, wncloud.New(cloudSrv.URL, "test"), logger, o...))
	t.Cleanup(e.srv.Close)
	return e
}

func newProxy(t *testing.T, backend http.Handler, ready bool) (*httptest.Server, *atomic.Int32) {
	t.Helper()
	e := newEnv(t, backend, ready)
	return e.srv, e.hits
}

func TestRemoteUserIsAccountID(t *testing.T) {
	var got []string
	var query url.Values
	e := newEnv(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Header.Values("Remote-User")
		query = r.URL.Query()
	}), true)
	req, _ := http.NewRequest(http.MethodGet, e.srv.URL+"/rest/ping.view?"+creds(), nil)
	req.Header.Set("Remote-User", "admin")
	req.Header.Add("remote-user", "other")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if len(got) != 1 || got[0] != e.owner.ID {
		t.Fatalf("backend received Remote-User %v, want [%s]", got, e.owner.ID)
	}
	for _, k := range []string{"u", "t", "s"} {
		if query.Has(k) {
			t.Fatalf("backend received credential parameter %q", k)
		}
	}
	if query.Get("c") != "test" || query.Get("v") != "1.16.1" {
		t.Fatalf("backend query %v lost c or v", query)
	}
}

func TestForwardedHeadersComeFromTheRequest(t *testing.T) {
	var got http.Header
	srv, _ := newProxy(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Header.Clone()
	}), true)
	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/share/x", nil)
	req.Host = "music.lan:4533"
	req.Header.Set("X-Forwarded-Host", "evil.example")
	req.Header.Set("X-Forwarded-Proto", "https")
	req.Header.Set("X-Forwarded-For", "10.0.0.9")
	req.Header.Set("X-Forwarded-Scheme", "https")
	req.Header.Set("Forwarded", "for=10.0.0.9")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if v := got.Values("X-Forwarded-Host"); len(v) != 1 || v[0] != "music.lan:4533" {
		t.Fatalf("X-Forwarded-Host %v, want [music.lan:4533]", v)
	}
	if v := got.Values("X-Forwarded-Proto"); len(v) != 1 || v[0] != "http" {
		t.Fatalf("X-Forwarded-Proto %v, want [http]", v)
	}
	for _, h := range []string{"X-Forwarded-For", "X-Forwarded-Scheme", "Forwarded"} {
		if v := got.Values(h); len(v) != 0 {
			t.Fatalf("backend received %s %v", h, v)
		}
	}
}

func TestShareStripsRemoteUser(t *testing.T) {
	var got []string
	srv, hits := newProxy(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Header.Values("Remote-User")
	}), true)
	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/share/x", nil)
	req.Header.Set("Remote-User", "admin")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK || hits.Load() != 1 {
		t.Fatalf("status %d, %d backend hits; want 200 and 1", resp.StatusCode, hits.Load())
	}
	if len(got) != 0 {
		t.Fatalf("backend received Remote-User %v", got)
	}
}

func TestSubsonicAuth(t *testing.T) {
	srv, hits := newProxy(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}), true)
	encPass := "enc:" + hex.EncodeToString([]byte(ownerPass))
	tests := []struct {
		name  string
		query string
		code  int // 0 means forwarded
		json  bool
	}{
		{"token", creds(), 0, false},
		{"plain", "u=owner&p=" + ownerPass + "&v=1&c=x", 0, false},
		{"enc", "u=owner&p=" + encPass + "&v=1&c=x", 0, false},
		{"wrong xml", "u=owner&p=nope&v=1&c=x", 40, false},
		{"wrong json", "u=owner&p=nope&v=1&c=x&f=json", 40, true},
		{"unknown user", "u=bob&p=" + ownerPass + "&v=1&c=x", 40, false},
		{"missing u", "p=" + ownerPass + "&v=1&c=x", 10, false},
		{"missing credential", "u=owner&v=1&c=x&f=json", 10, true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			before := hits.Load()
			resp, err := http.Get(srv.URL + "/rest/ping.view?" + tt.query)
			if err != nil {
				t.Fatal(err)
			}
			body, _ := io.ReadAll(resp.Body)
			resp.Body.Close()
			forwarded := hits.Load() > before
			if tt.code == 0 {
				if !forwarded {
					t.Fatalf("not forwarded; body %s", body)
				}
				return
			}
			if forwarded || resp.StatusCode != http.StatusOK {
				t.Fatalf("forwarded=%v status=%d; want an error answer with 200", forwarded, resp.StatusCode)
			}
			var code int
			if tt.json {
				var v struct {
					R struct {
						Status string `json:"status"`
						Error  struct {
							Code int `json:"code"`
						} `json:"error"`
					} `json:"subsonic-response"`
				}
				if err := json.Unmarshal(body, &v); err != nil || v.R.Status != "failed" {
					t.Fatalf("body %s: %v", body, err)
				}
				code = v.R.Error.Code
			} else {
				var v struct {
					Status string `xml:"status,attr"`
					Error  struct {
						Code int `xml:"code,attr"`
					} `xml:"error"`
				}
				if err := xml.Unmarshal(body, &v); err != nil || v.Status != "failed" {
					t.Fatalf("body %s: %v", body, err)
				}
				code = v.Error.Code
			}
			if code != tt.code {
				t.Fatalf("code %d, want %d; body %s", code, tt.code, body)
			}
		})
	}
}

func TestOtherPathsAre404(t *testing.T) {
	srv, hits := newProxy(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}), true)
	for _, p := range []string{"/app/", "/api/user", "/auth/login", "/rest/../app/", "/",
		"//rest/ping.view", "/rest/%2e%2e/app/", "/share/..%2f/app/", "/REST/ping.view"} {
		resp, err := http.Get(srv.URL + p)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		if resp.StatusCode != http.StatusNotFound {
			t.Fatalf("%s: status %d, want 404", p, resp.StatusCode)
		}
	}
	if hits.Load() != 0 {
		t.Fatalf("backend received %d requests", hits.Load())
	}
}

func TestFormBodyMergedIntoQuery(t *testing.T) {
	for _, method := range []string{http.MethodPost, http.MethodPut} {
		t.Run(method, func(t *testing.T) {
			var query url.Values
			var body []byte
			var contentType string
			var contentLength int64
			srv, hits := newProxy(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				query = r.URL.Query()
				body, _ = io.ReadAll(r.Body)
				contentType = r.Header.Get("Content-Type")
				contentLength = r.ContentLength
			}), true)
			req, _ := http.NewRequest(method, srv.URL+"/rest/ping.view?"+creds(), strings.NewReader("id=42&name=x"))
			req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
			resp, err := http.DefaultClient.Do(req)
			if err != nil {
				t.Fatal(err)
			}
			resp.Body.Close()
			if hits.Load() != 1 {
				t.Fatalf("%d backend hits, want 1", hits.Load())
			}
			if query.Get("id") != "42" || query.Get("name") != "x" || query.Get("c") != "test" {
				t.Fatalf("backend query %v lacks the body parameters", query)
			}
			if query.Has("u") || query.Has("t") || query.Has("s") {
				t.Fatalf("backend query %v has credentials", query)
			}
			if len(body) != 0 || contentLength > 0 {
				t.Fatalf("backend body %q, content length %d; want empty", body, contentLength)
			}
			if contentType != "" {
				t.Fatalf("backend Content-Type %q, want none", contentType)
			}
		})
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
	resp, err := client.Get(srv.URL + "/rest/stream?" + creds())
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
