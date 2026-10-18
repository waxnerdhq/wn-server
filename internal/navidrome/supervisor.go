package navidrome

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/exec"
	"sync"
	"syscall"
	"time"

	"github.com/waxnerdhq/wn-server/internal/config"
)

const (
	// readyTimeout is how long Navidrome gets to answer /ping. It runs
	// database migrations before it listens, and a migration after a version
	// bump can take minutes.
	readyTimeout  = 120 * time.Second
	readyInterval = 250 * time.Millisecond
	stopTimeout   = 10 * time.Second
	// accountRetry is the wait between attempts to create the owner's
	// Navidrome user.
	accountRetry = 5 * time.Second
)

// Account names the wn-server account whose Navidrome user the supervisor
// creates before it reports ready.
type Account struct {
	ID   string
	Name string
}

// Supervisor runs the Navidrome child process.
type Supervisor struct {
	cfg    *config.Config
	log    *slog.Logger
	policy RestartPolicy
	rt     http.RoundTripper
	client *Client
	owner  Account

	readyOnce sync.Once
	ready     chan struct{}
}

// NewSupervisor returns a supervisor for the Navidrome described by cfg.
// owner is the account whose Navidrome user must exist before ready. rt is
// the transport from NewTransport for cfg.NavidromeSocketPath. A nil logger
// selects slog.Default.
func NewSupervisor(cfg *config.Config, owner Account, rt http.RoundTripper, logger *slog.Logger) *Supervisor {
	if logger == nil {
		logger = slog.Default()
	}
	return &Supervisor{
		cfg:    cfg,
		log:    logger,
		policy: DefaultRestartPolicy(),
		rt:     rt,
		client: NewClient(rt),
		owner:  owner,
		ready:  make(chan struct{}),
	}
}

// Ready is closed the first time Navidrome answers /ping, the owner's
// Navidrome user exists, and the libraries have been synced.
func (s *Supervisor) Ready() <-chan struct{} { return s.ready }

// Run writes navidrome.toml and keeps Navidrome running until ctx is
// cancelled. The caller checks the binary with VerifyBinary first. On
// cancellation it sends SIGTERM, waits up to 10 s, then SIGKILL, and returns
// nil. It returns an error only when the socket path is too long,
// navidrome.toml cannot be written, or another process listens on the
// socket (ErrSocketInUse).
func (s *Supervisor) Run(ctx context.Context) error {
	if sock := s.cfg.NavidromeSocketPath(); len(sock) > maxSocketPath {
		return fmt.Errorf("navidrome socket path %s is longer than %d bytes; use a shorter data directory", sock, maxSocketPath)
	}
	if err := s.cfg.WriteNavidrome(); err != nil {
		return err
	}
	var delay time.Duration
	for {
		started := time.Now()
		err := s.runOnce(ctx)
		if ctx.Err() != nil {
			return nil
		}
		if errors.Is(err, ErrSocketInUse) {
			return err
		}
		uptime := time.Since(started)
		delay = s.policy.Next(delay, uptime)
		s.log.Warn("navidrome exited; restarting", "error", err, "uptime", uptime.Round(time.Millisecond), "delay", delay)
		select {
		case <-ctx.Done():
			return nil
		case <-time.After(delay):
		}
	}
}

// ErrSocketInUse means a process already accepts connections on Navidrome's
// socket, for example a Navidrome left from another wn-server. Run returns
// it instead of restarting.
var ErrSocketInUse = errors.New("another Navidrome is listening on the socket")

// staleDialTimeout is how long CheckSocket waits for a connection.
const staleDialTimeout = 500 * time.Millisecond

// CheckSocket removes the socket file that a stopped child left. It
// first dials the socket: a dead child's file gives ECONNREFUSED, and an
// absent file gives ENOENT. A successful dial gives ErrSocketInUse and
// leaves the file. Navidrome also removes the file before it listens.
//
// serve calls it before it binds the proxy port, and runOnce calls it before
// each start of the child.
func CheckSocket(sock string) error {
	if conn, err := net.DialTimeout("unix", sock, staleDialTimeout); err == nil {
		conn.Close()
		return fmt.Errorf("%w: %s", ErrSocketInUse, sock)
	}
	if err := os.Remove(sock); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("remove stale navidrome socket: %w", err)
	}
	return nil
}

// runOnce starts one child and blocks until it exits.
func (s *Supervisor) runOnce(ctx context.Context) error {
	runCtx, cancel := context.WithCancel(ctx)
	defer cancel()

	sock := s.cfg.NavidromeSocketPath()
	if err := CheckSocket(sock); err != nil {
		return err
	}

	cmd := exec.CommandContext(runCtx, s.cfg.NavidromeBin, "-c", s.cfg.NavidromeConfigPath())
	stdout, stderr := &lineWriter{log: s.log}, &lineWriter{log: s.log}
	cmd.Stdout, cmd.Stderr = stdout, stderr
	cmd.SysProcAttr = childAttr()
	// The child leads its own process group, so the signal goes to the group.
	cmd.Cancel = func() error { return syscall.Kill(-cmd.Process.Pid, syscall.SIGTERM) }
	cmd.WaitDelay = stopTimeout
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start %s: %w", s.cfg.NavidromeBin, err)
	}
	s.log.Info("navidrome started", "pid", cmd.Process.Pid, "socket", sock)

	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		if err := s.waitReady(runCtx); err != nil {
			if runCtx.Err() == nil {
				s.log.Error("navidrome did not become ready; stopping it", "error", err)
				cancel()
			}
			return
		}
		s.markReady(runCtx)
	}()

	err := cmd.Wait()
	stdout.flush()
	stderr.flush()
	cancel()
	wg.Wait()
	return err
}

// waitReady polls /ping on the socket until it answers 200, for up to
// readyTimeout. Until Navidrome listens, the dial fails and the poll retries.
func (s *Supervisor) waitReady(ctx context.Context) error {
	ctx, cancel := context.WithTimeout(ctx, readyTimeout)
	defer cancel()
	hc := &http.Client{Transport: s.rt, Timeout: 2 * time.Second}
	url := Target().String() + "/ping"
	for {
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
		if err != nil {
			return err
		}
		resp, err := hc.Do(req)
		if err == nil {
			resp.Body.Close()
			if resp.StatusCode == http.StatusOK {
				return nil
			}
		}
		select {
		case <-ctx.Done():
			return fmt.Errorf("no 200 from /ping within %s", readyTimeout)
		case <-time.After(readyInterval):
		}
	}
}

// markReady creates wn-server's Navidrome user and the owner's, syncs
// libraries, and reports ready. Creating the owner's user is retried until it
// succeeds. Any other failure is logged and the child keeps running without
// ready.
func (s *Supervisor) markReady(ctx context.Context) {
	isAdmin, err := s.client.EnsureUser(ctx)
	if err != nil {
		if ctx.Err() == nil {
			s.log.Error("create wn-server user in navidrome", "error", err)
		}
		return
	}
	if !isAdmin {
		s.log.Error("navidrome user wn-server is not admin; another client created the first user before wn-server, and library sync will fail", "user", Identity)
	}
	for {
		created, err := EnsureAccountUser(ctx, s.client, s.owner.ID, s.owner.Name)
		if err == nil {
			if created {
				s.log.Info("created owner's navidrome user", "account", s.owner.ID)
			} else {
				s.log.Info("owner's navidrome user exists", "account", s.owner.ID)
			}
			break
		}
		if ctx.Err() != nil {
			return
		}
		s.log.Error("create owner's navidrome user; retrying", "account", s.owner.ID, "error", err, "delay", accountRetry)
		select {
		case <-ctx.Done():
			return
		case <-time.After(accountRetry):
		}
	}
	created, err := SyncLibraries(ctx, s.client, s.cfg.SortedDir, s.cfg.MusicFolders)
	if err != nil {
		if ctx.Err() == nil {
			s.log.Error("sync navidrome libraries", "error", err)
		}
		return
	}
	s.log.Info("navidrome ready", "libraries_created", len(created))
	s.readyOnce.Do(func() { close(s.ready) })
}

// lineWriter logs each complete line written to it with the "navidrome: "
// prefix.
type lineWriter struct {
	log *slog.Logger
	buf []byte
}

func (w *lineWriter) Write(p []byte) (int, error) {
	w.buf = append(w.buf, p...)
	for {
		i := bytes.IndexByte(w.buf, '\n')
		if i < 0 {
			break
		}
		w.emit(w.buf[:i])
		w.buf = w.buf[i+1:]
	}
	return len(p), nil
}

func (w *lineWriter) flush() {
	w.emit(w.buf)
	w.buf = nil
}

func (w *lineWriter) emit(line []byte) {
	line = bytes.TrimRight(line, "\r")
	if len(line) == 0 {
		return
	}
	w.log.Info("navidrome: " + string(line))
}
