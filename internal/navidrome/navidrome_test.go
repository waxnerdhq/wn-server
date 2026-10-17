package navidrome

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestParseVersion(t *testing.T) {
	tests := []struct {
		in      string
		want    string
		wantErr bool
	}{
		{"0.64.2 (101145742)\n", "0.64.2", false},
		{"v0.64.2\n", "0.64.2", false},
		{"dev\n", "dev", false},
		{"  \n", "", true},
	}
	for _, tt := range tests {
		got, err := ParseVersion(tt.in)
		if (err != nil) != tt.wantErr || got != tt.want {
			t.Errorf("ParseVersion(%q) = %q, %v; want %q, err=%v", tt.in, got, err, tt.want, tt.wantErr)
		}
	}
}

func TestFullVersion(t *testing.T) {
	for in, want := range map[string]string{
		"0.64.2 (10114574)\n": "0.64.2 (10114574)",
		"v0.64.2\n":           "0.64.2",
	} {
		if got := fullVersion(in); got != want {
			t.Errorf("fullVersion(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestCheckVersion(t *testing.T) {
	if err := CheckVersion(PinnedVersion); err != nil {
		t.Fatal(err)
	}
	err := CheckVersion("0.63.2")
	if err == nil || !strings.Contains(err.Error(), "0.63.2") || !strings.Contains(err.Error(), PinnedVersion) {
		t.Fatalf("got %v, want an error naming both versions", err)
	}
}

func TestRestartPolicyNext(t *testing.T) {
	p := DefaultRestartPolicy()
	s := time.Second
	tests := []struct {
		prev, uptime, want time.Duration
	}{
		{0, 0, 1 * s},
		{1 * s, 2 * s, 2 * s},
		{2 * s, 0, 4 * s},
		{16 * s, 0, 30 * s},
		{30 * s, 0, 30 * s},
		{30 * s, 60 * s, 1 * s},
		{8 * s, 61 * s, 1 * s},
	}
	for _, tt := range tests {
		if got := p.Next(tt.prev, tt.uptime); got != tt.want {
			t.Errorf("Next(%v, %v) = %v, want %v", tt.prev, tt.uptime, got, tt.want)
		}
	}
}

type fakeNavidrome struct {
	mu       sync.Mutex
	libs     []Library
	created  []Library
	users    []string
	notAdmin bool
	failPath string
}

func (f *fakeNavidrome) handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /app/", func(w http.ResponseWriter, r *http.Request) {
		f.mu.Lock()
		f.users = append(f.users, r.Header.Get("Remote-User"))
		admin := !f.notAdmin
		f.mu.Unlock()
		cfg, _ := json.Marshal(map[string]any{
			"version": "0.64.2",
			"auth":    map[string]any{"id": "u1", "username": Identity, "isAdmin": admin},
		})
		lit, _ := json.Marshal(string(cfg))
		_, _ = w.Write([]byte("<script>\n  window.__APP_CONFIG__ = " + string(lit) + "\n</script>\n"))
	})
	mux.HandleFunc("GET /api/library", func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Remote-User") != Identity {
			http.Error(w, "Not authenticated", http.StatusUnauthorized)
			return
		}
		f.mu.Lock()
		defer f.mu.Unlock()
		_ = json.NewEncoder(w).Encode(f.libs)
	})
	mux.HandleFunc("POST /api/library", func(w http.ResponseWriter, r *http.Request) {
		var l Library
		if err := json.NewDecoder(r.Body).Decode(&l); err != nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		f.mu.Lock()
		defer f.mu.Unlock()
		if l.Path == f.failPath {
			http.Error(w, "boom", http.StatusInternalServerError)
			return
		}
		l.ID = len(f.libs) + len(f.created) + 1
		f.created = append(f.created, l)
		_ = json.NewEncoder(w).Encode(map[string]string{"id": "x"})
	})
	return mux
}

func TestSyncLibraries(t *testing.T) {
	fake := &fakeNavidrome{libs: []Library{
		{ID: 1, Name: "Music Library", Path: "/data/sorted"},
		{ID: 2, Name: "Other", Path: "/elsewhere/keep"},
	}}
	srv := httptest.NewServer(fake.handler())
	defer srv.Close()
	c := newClient(srv.URL, Identity)
	ctx := context.Background()

	admin, err := c.EnsureUser(ctx)
	if err != nil || !admin {
		t.Fatalf("EnsureUser = %v, %v; want admin", admin, err)
	}
	if !reflect.DeepEqual(fake.users, []string{Identity}) {
		t.Fatalf("EnsureUser sent Remote-User %v", fake.users)
	}
	fake.notAdmin = true
	if admin, err := c.EnsureUser(ctx); err != nil || admin {
		t.Fatalf("EnsureUser = %v, %v; want not admin", admin, err)
	}

	folders := []string{"/music/a", "/music/b/", "/other/Other"}
	created, err := SyncLibraries(ctx, c, "/data/sorted", folders)
	if err != nil {
		t.Fatal(err)
	}
	if want := []string{"/music/a", "/music/b", "/other/Other"}; !reflect.DeepEqual(created, want) {
		t.Fatalf("created %v, want %v", created, want)
	}
	want := []Library{
		{ID: 3, Name: "a", Path: "/music/a"},
		{ID: 4, Name: "b", Path: "/music/b"},
		{ID: 5, Name: "Other 2", Path: "/other/Other"},
	}
	if !reflect.DeepEqual(fake.created, want) {
		t.Fatalf("POST bodies %+v, want %+v", fake.created, want)
	}

	fake.libs = append(fake.libs, fake.created...)
	fake.created = nil
	created, err = SyncLibraries(ctx, c, "/data/sorted", folders)
	if err != nil || len(created) != 0 || len(fake.created) != 0 {
		t.Fatalf("second sync created %v (err %v), want nothing", created, err)
	}
}

func TestSyncLibrariesCollectsErrors(t *testing.T) {
	fake := &fakeNavidrome{failPath: "/music/a"}
	srv := httptest.NewServer(fake.handler())
	defer srv.Close()
	c := newClient(srv.URL, Identity)

	created, err := SyncLibraries(context.Background(), c, "/data/sorted", []string{"/music/a", "/music/b"})
	if want := []string{"/music/b"}; !reflect.DeepEqual(created, want) {
		t.Fatalf("created %v, want %v", created, want)
	}
	if err == nil || !strings.Contains(err.Error(), "/music/a") || !strings.Contains(err.Error(), "sorted_dir /data/sorted") {
		t.Fatalf("got %v, want errors for /music/a and the missing sorted_dir", err)
	}
}

func TestEnsureAccountUser(t *testing.T) {
	var (
		mu     sync.Mutex
		listed []User
		posted []User
	)
	mux := http.NewServeMux()
	mux.HandleFunc("GET /api/user", func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Remote-User") != Identity {
			http.Error(w, "Not authenticated", http.StatusUnauthorized)
			return
		}
		mu.Lock()
		defer mu.Unlock()
		_ = json.NewEncoder(w).Encode(listed)
	})
	mux.HandleFunc("POST /api/user", func(w http.ResponseWriter, r *http.Request) {
		var u User
		if err := json.NewDecoder(r.Body).Decode(&u); err != nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		mu.Lock()
		defer mu.Unlock()
		posted = append(posted, u)
		_ = json.NewEncoder(w).Encode(map[string]string{"id": "n1"})
	})
	srv := httptest.NewServer(mux)
	defer srv.Close()
	c := newClient(srv.URL, Identity)
	ctx := context.Background()

	created, err := EnsureAccountUser(ctx, c, "0123456789abcdef", "owner")
	if err != nil || !created {
		t.Fatalf("EnsureAccountUser on empty list = %v, %v; want created", created, err)
	}
	if len(posted) != 1 {
		t.Fatalf("POST called %d times, want 1", len(posted))
	}
	if u := posted[0]; u.UserName != "0123456789abcdef" || u.Name != "owner" || !u.IsAdmin || u.Password == "" {
		t.Fatalf("POST body %+v", u)
	}

	mu.Lock()
	listed = []User{{ID: "a", UserName: Identity, IsAdmin: true}, {ID: "n1", UserName: "0123456789abcdef"}}
	mu.Unlock()
	created, err = EnsureAccountUser(ctx, c, "0123456789abcdef", "owner")
	if err != nil || created || len(posted) != 1 {
		t.Fatalf("EnsureAccountUser with existing user = %v, %v, %d posts; want no create", created, err, len(posted))
	}
}

func TestStartScan(t *testing.T) {
	status := `{"subsonic-response":{"status":"ok","version":"1.16.1"}}`
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		q := r.URL.Query()
		if r.URL.Path != "/rest/startScan" || r.Header.Get("Remote-User") != Identity ||
			q.Get("f") != "json" || q.Get("c") != Identity || q.Get("v") == "" {
			http.Error(w, "bad request", http.StatusBadRequest)
			return
		}
		w.Write([]byte(status))
	}))
	defer srv.Close()
	c := newClient(srv.URL, Identity)
	if err := c.StartScan(context.Background()); err != nil {
		t.Fatalf("StartScan: %v", err)
	}
	status = `{"subsonic-response":{"status":"failed","error":{"code":50,"message":"not authorized"}}}`
	if err := c.StartScan(context.Background()); err == nil || !strings.Contains(err.Error(), "not authorized") {
		t.Fatalf("StartScan with failed status = %v; want the Subsonic error", err)
	}
}
