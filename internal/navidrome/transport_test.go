package navidrome

import (
	"context"
	"errors"
	"log/slog"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/config"
)

// shortTempDir returns a temporary directory with a short path. macOS limits
// socket paths to 104 bytes, and t.TempDir paths can be longer.
func shortTempDir(t *testing.T) string {
	t.Helper()
	dir, err := os.MkdirTemp("", "wn")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	return dir
}

// TestRunRefusesSocketInUse checks that Run stops, without starting a child,
// when a process already listens on the socket.
func TestRunRefusesSocketInUse(t *testing.T) {
	dir := shortTempDir(t)
	cfg := &config.Config{DataDir: dir, SortedDir: filepath.Join(dir, "sorted"), NavidromeBin: filepath.Join(dir, "no-such-binary")}
	ln, err := net.Listen("unix", cfg.NavidromeSocketPath())
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	s := NewSupervisor(cfg, Account{}, NewTransport(cfg.NavidromeSocketPath()), slog.New(slog.DiscardHandler))
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := s.Run(ctx); !errors.Is(err, ErrSocketInUse) {
		t.Fatalf("Run = %v, want ErrSocketInUse", err)
	}
	if _, err := os.Stat(cfg.NavidromeSocketPath()); err != nil {
		t.Fatalf("socket file removed: %v", err)
	}
}

// TestCheckSocket checks the three states of the socket path: absent is fine,
// a stale file is removed, and a listener gives ErrSocketInUse.
func TestCheckSocket(t *testing.T) {
	sock := filepath.Join(shortTempDir(t), "nd.sock")
	if err := CheckSocket(sock); err != nil {
		t.Fatalf("absent socket: %v", err)
	}
	ln, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	if err := CheckSocket(sock); !errors.Is(err, ErrSocketInUse) {
		t.Fatalf("listening socket: %v, want ErrSocketInUse", err)
	}
	// Closing a UnixListener removes the file, so keep it.
	ln.(*net.UnixListener).SetUnlinkOnClose(false)
	ln.Close()
	if _, err := os.Stat(sock); err != nil {
		t.Fatalf("stale socket file missing: %v", err)
	}
	if err := CheckSocket(sock); err != nil {
		t.Fatalf("stale socket: %v", err)
	}
	if _, err := os.Stat(sock); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("stale socket file not removed: %v", err)
	}
}

// TestWaitReadyOnSocket starts listening on the socket after the poll has
// begun, as Navidrome does once its migrations finish.
func TestWaitReadyOnSocket(t *testing.T) {
	dir := shortTempDir(t)
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
