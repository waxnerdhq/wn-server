package cli

import (
	"bytes"
	"context"
	"errors"
	"html"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"testing"

	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
	"github.com/waxnerdhq/wn-server/internal/wncloud/stub"
)

const testOwnerID = "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70"

var (
	authURLRE = regexp.MustCompile(`https?://\S+/cli/auth\?\S+`)
	codeRE    = regexp.MustCompile(`<output id="code">([^<]+)</output>`)
)

// cloud is the stub behind a handler that counts token exchanges and can
// make POST /servers fail.
type cloud struct {
	stub *stub.Stub
	srv  *httptest.Server

	mu            sync.Mutex
	exchanges     int
	failServers   bool
	serversBearer string
}

func newCloud(t *testing.T) *cloud {
	c := &cloud{stub: stub.New()}
	c.stub.AddUser(testOwnerID, "owner@example.com")
	c.stub.SignInAs(testOwnerID)
	c.srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c.mu.Lock()
		if r.URL.Path == "/cli/token" {
			c.exchanges++
		}
		fail := c.failServers && r.URL.Path == "/servers"
		if fail {
			c.serversBearer = strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
		}
		c.mu.Unlock()
		if fail {
			http.Error(w, "unavailable", http.StatusServiceUnavailable)
			return
		}
		c.stub.ServeHTTP(w, r)
	}))
	t.Cleanup(c.srv.Close)
	return c
}

// paste is a stdin that, on the first read, opens the URL that login
// printed, reads the displayed code, and returns edit(code) as one line.
type paste struct {
	t    *testing.T
	out  *bytes.Buffer
	edit func(displayed string) string
	r    io.Reader
}

func (p *paste) Read(b []byte) (int, error) {
	if p.r == nil {
		u := authURLRE.FindString(p.out.String())
		if u == "" {
			p.t.Fatalf("no URL in output %q", p.out.String())
		}
		res, err := http.Get(u)
		if err != nil {
			p.t.Fatal(err)
		}
		page, _ := io.ReadAll(res.Body)
		res.Body.Close()
		m := codeRE.FindSubmatch(page)
		if m == nil {
			p.t.Fatalf("no code on page: %d %s", res.StatusCode, page)
		}
		p.r = strings.NewReader(p.edit(html.UnescapeString(string(m[1]))) + "\n")
	}
	return p.r.Read(b)
}

// setup runs init with wn_cloud_url pointing at the cloud and returns a
// function that runs a command with a pasted code.
func setup(t *testing.T, c *cloud) (cfg string, run func(edit func(string) string, args ...string) (string, error)) {
	t.Helper()
	cfg = filepath.Join(t.TempDir(), "config.toml")
	root := newRootCmd()
	root.SetOut(io.Discard)
	root.SetArgs([]string{"--config", cfg, "init", "--folder", t.TempDir()})
	if err := root.Execute(); err != nil {
		t.Fatal(err)
	}
	body, err := os.ReadFile(cfg)
	if err != nil {
		t.Fatal(err)
	}
	body = bytes.Replace(body, []byte("https://waxnerd.com"), []byte(c.srv.URL), 1)
	if err := os.WriteFile(cfg, body, 0o600); err != nil {
		t.Fatal(err)
	}
	run = func(edit func(string) string, args ...string) (string, error) {
		var out bytes.Buffer
		root := newRootCmd()
		root.SetOut(&out)
		root.SetErr(io.Discard)
		root.SetIn(&paste{t: t, out: &out, edit: edit})
		root.SetArgs(append([]string{"--config", cfg}, args...))
		err := root.Execute()
		return out.String(), err
	}
	return cfg, run
}

func credential(t *testing.T, cfg string) (store.ServerCredential, error) {
	t.Helper()
	st, err := store.Open(context.Background(), store.Path(filepath.Dir(cfg)))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	return st.ServerCredential(context.Background())
}

func same(s string) string { return s }

func TestLoginLogoutStatus(t *testing.T) {
	c := newCloud(t)
	cfg, run := setup(t, c)

	out, err := run(same, "status")
	if err != nil || !strings.Contains(out, "not linked; run wn-server login") {
		t.Fatalf("status before login: %v %q", err, out)
	}
	out, err = run(same, "login", "--name", "living-room")
	if err != nil {
		t.Fatalf("login: %v\n%s", err, out)
	}
	for _, want := range []string{"label=living-room", "display=code", "code: ", `linked owner "owner" to waxnerd user ` + testOwnerID, "server id: ", "This app password keeps working when wn-cloud is unreachable."} {
		if !strings.Contains(out, want) {
			t.Fatalf("login output lacks %q:\n%s", want, out)
		}
	}
	cred, err := credential(t, cfg)
	if err != nil || !strings.HasPrefix(cred.Token, "wn_") || cred.WaxnerdUserID != testOwnerID {
		t.Fatalf("credential = %+v, %v", cred, err)
	}
	out, err = run(same, "status")
	if err != nil || !strings.Contains(out, "server id: "+cred.ServerID) || !strings.Contains(out, "wn-cloud: "+c.srv.URL) {
		t.Fatalf("status after login: %v %q", err, out)
	}
	if _, err := run(same, "login"); err == nil || !strings.Contains(err.Error(), "already linked to server "+cred.ServerID) {
		t.Fatalf("second login: %v", err)
	}

	out, err = run(same, "logout")
	if err != nil || !strings.Contains(out, "revoked the server credential at wn-cloud") {
		t.Fatalf("logout: %v %q", err, out)
	}
	if _, err := credential(t, cfg); !errors.Is(err, store.ErrNoServerCredential) {
		t.Fatalf("credential after logout: %v", err)
	}
	if err := wncloud.New(c.srv.URL, "").Logout(context.Background(), cred.Token); !errors.Is(err, wncloud.ErrUnauthorized) {
		t.Fatalf("token after logout: %v, want ErrUnauthorized", err)
	}
	if out, err := run(same, "status"); err != nil || !strings.Contains(out, "not linked") {
		t.Fatalf("status after logout: %v %q", err, out)
	}
}

func TestLoginFailures(t *testing.T) {
	t.Run("state mismatch", func(t *testing.T) {
		c := newCloud(t)
		_, run := setup(t, c)
		_, err := run(func(s string) string {
			code, _, _ := strings.Cut(s, ".")
			return code + ".another-state-0123456"
		}, "login", "--name", "n")
		if !errors.Is(err, errWrongState) {
			t.Fatalf("err = %v, want errWrongState", err)
		}
		if c.exchanges != 0 {
			t.Fatalf("%d calls to POST /cli/token, want 0", c.exchanges)
		}
	})

	t.Run("wrong code", func(t *testing.T) {
		c := newCloud(t)
		cfg, run := setup(t, c)
		_, err := run(func(s string) string {
			_, state, _ := strings.Cut(s, ".")
			return strings.Repeat("A", 43) + "." + state
		}, "login", "--name", "n")
		if !errors.Is(err, wncloud.ErrInvalidGrant) {
			t.Fatalf("err = %v, want ErrInvalidGrant", err)
		}
		if _, err := credential(t, cfg); !errors.Is(err, store.ErrNoServerCredential) {
			t.Fatalf("credential: %v", err)
		}
	})

	t.Run("register fails", func(t *testing.T) {
		c := newCloud(t)
		c.failServers = true
		cfg, run := setup(t, c)
		_, err := run(same, "login", "--name", "n")
		if !errors.Is(err, wncloud.ErrUnreachable) {
			t.Fatalf("err = %v, want ErrUnreachable", err)
		}
		if _, err := credential(t, cfg); !errors.Is(err, store.ErrNoServerCredential) {
			t.Fatalf("credential: %v", err)
		}
		// login revoked the token it received.
		if c.serversBearer == "" {
			t.Fatal("POST /servers was not called")
		}
		if err := wncloud.New(c.srv.URL, "").Logout(context.Background(), c.serversBearer); !errors.Is(err, wncloud.ErrUnauthorized) {
			t.Fatalf("token after failed login: %v, want ErrUnauthorized", err)
		}
	})
}
