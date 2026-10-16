package navidrome

import (
	"bytes"
	"context"
	"fmt"
	"log/slog"
	"net/http"
	"os/exec"
	"strconv"
	"sync"
	"syscall"
	"time"

	"github.com/waxnerdhq/wn-server/internal/config"
)

const (
	readyTimeout  = 30 * time.Second
	readyInterval = 250 * time.Millisecond
	stopTimeout   = 10 * time.Second
)

// Supervisor runs the Navidrome child process.
type Supervisor struct {
	cfg    *config.Config
	log    *slog.Logger
	policy RestartPolicy
	client *Client

	readyOnce sync.Once
	ready     chan struct{}
}

// NewSupervisor returns a supervisor for the Navidrome described by cfg. A
// nil logger selects slog.Default.
func NewSupervisor(cfg *config.Config, logger *slog.Logger) *Supervisor {
	if logger == nil {
		logger = slog.Default()
	}
	return &Supervisor{
		cfg:    cfg,
		log:    logger,
		policy: DefaultRestartPolicy(),
		client: NewClient(cfg.NavidromePort),
		ready:  make(chan struct{}),
	}
}

// Ready is closed the first time Navidrome answers /ping and its libraries
// have been synced.
func (s *Supervisor) Ready() <-chan struct{} { return s.ready }

// Run verifies the binary, writes navidrome.toml, and keeps Navidrome running
// until ctx is cancelled. On cancellation it sends SIGTERM, waits up to 10 s,
// then SIGKILL, and returns nil. It returns an error only when Navidrome
// cannot be started at all: a version mismatch or a config write failure.
func (s *Supervisor) Run(ctx context.Context) error {
	if err := VerifyBinary(ctx, s.cfg.NavidromeBin); err != nil {
		return err
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

// runOnce starts one child and blocks until it exits.
func (s *Supervisor) runOnce(ctx context.Context) error {
	runCtx, cancel := context.WithCancel(ctx)
	defer cancel()

	cmd := exec.CommandContext(runCtx, s.cfg.NavidromeBin, "-c", s.cfg.NavidromeConfigPath())
	stdout, stderr := &lineWriter{log: s.log}, &lineWriter{log: s.log}
	cmd.Stdout, cmd.Stderr = stdout, stderr
	cmd.Cancel = func() error { return cmd.Process.Signal(syscall.SIGTERM) }
	cmd.WaitDelay = stopTimeout
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start %s: %w", s.cfg.NavidromeBin, err)
	}
	s.log.Info("navidrome started", "pid", cmd.Process.Pid, "port", s.cfg.NavidromePort)

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

// waitReady polls /ping until it answers 200, for up to 30 s.
func (s *Supervisor) waitReady(ctx context.Context) error {
	ctx, cancel := context.WithTimeout(ctx, readyTimeout)
	defer cancel()
	hc := &http.Client{Timeout: 2 * time.Second}
	url := "http://127.0.0.1:" + strconv.Itoa(s.cfg.NavidromePort) + "/ping"
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

// markReady creates wn-server's Navidrome user, syncs libraries, and
// reports ready. A failure here is logged and the child keeps running.
func (s *Supervisor) markReady(ctx context.Context) {
	if err := s.client.EnsureUser(ctx); err != nil {
		if ctx.Err() == nil {
			s.log.Error("create wn-server user in navidrome", "error", err)
		}
		return
	}
	paths := append(append([]string{}, s.cfg.MusicFolders...), s.cfg.SortedDir)
	created, err := SyncLibraries(ctx, s.client, paths)
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
