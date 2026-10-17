package navidrome

import (
	"context"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/config"
)

// TestWaitReadyOnSocket starts listening on the socket after the poll has
// begun, as Navidrome does once its migrations finish.
func TestWaitReadyOnSocket(t *testing.T) {
	// macOS limits socket paths to 104 bytes, and t.TempDir paths can be
	// longer.
	dir, err := os.MkdirTemp("", "wn")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	cfg := &config.Config{DataDir: dir}
	sock := cfg.NavidromeSocketPath()

	hosts := make(chan string, 1)
	srv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		select {
		case hosts <- r.Host:
		default:
		}
		if r.URL.Path != "/ping" {
			http.NotFound(w, r)
		}
	})}
	t.Cleanup(func() { srv.Close() })
	go func() {
		time.Sleep(3 * readyInterval)
		ln, err := net.Listen("unix", sock)
		if err != nil {
			t.Error(err)
			return
		}
		_ = srv.Serve(ln)
	}()

	s := NewSupervisor(cfg, Account{}, NewTransport(sock), nil)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := s.waitReady(ctx); err != nil {
		t.Fatal(err)
	}
	if h := <-hosts; h != Host {
		t.Fatalf("Host = %q, want %q", h, Host)
	}
	if filepath.Dir(sock) != dir {
		t.Fatalf("socket %s is not in the data directory", sock)
	}
}
