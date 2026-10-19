// Package relay opens the Waxnerd Relay tunnel to the edge and serves the
// requests that come down it with wn-server's own HTTP handler
// (docs/relay.md, sections 2 and 3).
//
// The package dials the edge over TLS, runs the GET /v1/tunnel handshake with
// the server credential, and then runs yamux as the server on the connection.
// The yamux session is the net.Listener of an http.Server. Each request has
// its X-Relay-Client header moved into the request context with
// relayreq.WithRelay before the handler sees it. The package reconnects with
// backoff and reports its state through Options.OnState.
//
// The package imports the standard library, yamux, and internal/relayreq
// only. Everything else it needs comes through Options.
package relay

import (
	"bufio"
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"math/rand/v2"
	"net"
	"net/http"
	"net/url"
	"sync"
	"sync/atomic"
	"time"

	"github.com/hashicorp/yamux"

	"github.com/waxnerdhq/wn-server/internal/relayreq"
)

// Protocol is the Upgrade token of the tunnel handshake.
const Protocol = "wn-relay/1"

// missedPongLimit is the number of pings in a row without a pong after which
// the package closes the session. A ping is sent every config.keepAlive and
// waits config.writeTimeout for the pong, so with the defaults a silent drop
// is seen within about 110 seconds (docs/relay.md, section 3). The values are
// not configuration keys.
const missedPongLimit = 3

// clientHeader carries the client's address from the edge.
const clientHeader = "X-Relay-Client"

// Options configures a Relay. Handler, Credential, and ConnectURL are
// required.
type Options struct {
	// Handler serves every tunneled request.
	Handler http.Handler
	// Credential answers the server credential and the server id, and ok
	// false while there is none.
	Credential func() (token, serverID string, ok bool)
	// ConnectURL answers the edge's tunnel URL, such as
	// https://connect.relay.waxnerd.com/v1/tunnel, and ok false while none is
	// known.
	ConnectURL func() (string, bool)
	// Version is sent as Wn-Server-Version.
	Version string
	// Enabled answers whether the tunnel should be open. Run reads it on the
	// poll of Credential, from a second goroutine as well, so it must be safe
	// for concurrent use. While it answers false Run dials nothing, holds no
	// tunnel, and reports Off. Nil means true.
	Enabled func() bool
	// OnState is called on every change of the state, never concurrently
	// with itself. It may be nil.
	OnState func(State)
	// Logger receives the package's log lines. Nil discards them.
	Logger *slog.Logger
}

// config holds the intervals and bounds. Tests shorten them.
type config struct {
	waitPoll         time.Duration // re-read of Credential and ConnectURL while either is missing
	refusedPoll      time.Duration // re-read of Credential after a refusal, and redial after a 402
	backoffMin       time.Duration
	backoffMax       time.Duration
	keepAlive        time.Duration
	writeTimeout     time.Duration
	handshakeTimeout time.Duration
	shutdownGrace    time.Duration
	roots            *x509.CertPool // nil means the system roots
}

func defaultConfig() config {
	return config{
		waitPoll:         10 * time.Second,
		refusedPoll:      10 * time.Minute,
		backoffMin:       time.Second,
		backoffMax:       5 * time.Minute,
		keepAlive:        30 * time.Second,
		writeTimeout:     10 * time.Second,
		handshakeTimeout: 10 * time.Second,
		shutdownGrace:    5 * time.Second,
	}
}

// Relay keeps the tunnel open. Create it with New and call Run once.
type Relay struct {
	opts  Options
	cfg   config
	log   *slog.Logger
	state State
}

// New returns a Relay for opts.
func New(opts Options) *Relay {
	log := opts.Logger
	if log == nil {
		log = slog.New(slog.DiscardHandler)
	}
	return &Relay{opts: opts, cfg: defaultConfig(), log: log}
}

// statusError is a handshake answer other than 101.
type statusError struct {
	status int
	code   string
}

func (e *statusError) Error() string {
	if e.code == "" {
		return fmt.Sprintf("edge answered %d", e.status)
	}
	return fmt.Sprintf("edge answered %d %s", e.status, e.code)
}

// Run keeps the tunnel open until ctx ends. On ctx end it closes the tunnel;
// in-flight requests end as on http.Server.Shutdown. While Options.Enabled
// answers false it holds no tunnel and reports Off. When Enabled turns false,
// Run closes the tunnel the way a ctx end does; when it turns true again, Run
// dials again.
func (r *Relay) Run(ctx context.Context) {
	for ctx.Err() == nil {
		if r.opts.Enabled != nil && !r.opts.Enabled() {
			r.set(Off, "", "")
			if !sleep(ctx, r.cfg.waitPoll) {
				return
			}
			continue
		}
		ectx, cancel := context.WithCancel(ctx)
		if r.opts.Enabled != nil {
			go r.watchEnabled(ectx, cancel)
		}
		r.dial(ectx)
		cancel()
	}
}

// watchEnabled calls cancel when Enabled answers false, until ctx ends.
func (r *Relay) watchEnabled(ctx context.Context, cancel context.CancelFunc) {
	for sleep(ctx, r.cfg.waitPoll) {
		if !r.opts.Enabled() {
			cancel()
			return
		}
	}
}

// dial keeps the tunnel open until ctx ends. Run ends ctx when Enabled turns
// false.
func (r *Relay) dial(ctx context.Context) {
	backoff := r.cfg.backoffMin
	// refused is the token that the edge refused with 400, 401, 403, or 409.
	// The package does not dial with it again.
	refused := ""
	for ctx.Err() == nil {
		token, serverID, ok := r.opts.Credential()
		connectURL, urlOK := r.opts.ConnectURL()
		if !ok || !urlOK {
			r.set(Waiting, "", "")
			if !sleep(ctx, r.cfg.waitPoll) {
				return
			}
			continue
		}
		if refused != "" && token == refused {
			if !sleep(ctx, r.cfg.refusedPoll) {
				return
			}
			continue
		}
		switch r.state.Name {
		case "", Off, Waiting, CredentialRejected, Refused, UpdateRequired:
			r.set(Connecting, "", "")
		}

		conn, host, session, err := r.handshake(ctx, connectURL, token, serverID)
		if ctx.Err() != nil {
			if conn != nil {
				_ = conn.Close()
			}
			return
		}
		var se *statusError
		switch {
		case err == nil:
			backoff = r.cfg.backoffMin
			refused = ""
			r.log.Info("relay connected", "host", host, "session", session)
			r.set(Connected, host, "")
			cause := r.serve(ctx, conn)
			if ctx.Err() != nil {
				return
			}
			r.log.Warn("relay tunnel closed", "err", cause)
			r.set(Unreachable, "", "tunnel closed: "+cause.Error())
		case errors.As(err, &se) && se.status == http.StatusPaymentRequired:
			r.log.Warn("relay not enabled at wn-cloud", "err", err)
			r.set(NotEnabled, "", err.Error())
			if !sleep(ctx, r.cfg.refusedPoll) {
				return
			}
			continue
		case errors.As(err, &se) && stops(se.status):
			r.log.Warn("relay refused by the edge; waiting for a new credential", "err", err)
			refused = token
			r.set(stopState(se.status), "", err.Error())
			continue
		default:
			r.log.Warn("relay dial failed", "err", err)
			r.set(Unreachable, "", err.Error())
		}
		if !sleep(ctx, rand.N(backoff)+1) {
			return
		}
		backoff = min(2*backoff, r.cfg.backoffMax)
	}
}

// stops reports whether a handshake status stops the backoff loop.
func stops(status int) bool {
	switch status {
	case http.StatusBadRequest, http.StatusUnauthorized, http.StatusForbidden, http.StatusConflict:
		return true
	}
	return false
}

func stopState(status int) StateName {
	switch status {
	case http.StatusBadRequest:
		return Refused
	case http.StatusConflict:
		return UpdateRequired
	}
	return CredentialRejected
}

// set records a state and calls OnState when anything changed. Since stays
// when the name does not change.
func (r *Relay) set(name StateName, host, errText string) {
	next := State{Name: name, Since: r.state.Since, Host: host, Err: errText}
	if name != r.state.Name {
		next.Since = time.Now()
	}
	if next == r.state {
		return
	}
	r.state = next
	if r.opts.OnState != nil {
		r.opts.OnState(next)
	}
}

// handshake dials the edge and runs GET /v1/tunnel. On a 101 it answers the
// connection, ready for yamux, with Wn-Relay-Host and Wn-Relay-Session.
func (r *Relay) handshake(ctx context.Context, rawURL, token, serverID string) (net.Conn, string, string, error) {
	u, err := url.Parse(rawURL)
	if err != nil || u.Scheme != "https" || u.Host == "" {
		return nil, "", "", fmt.Errorf("invalid connect URL %q", rawURL)
	}
	if u.Path == "" {
		u.Path = "/v1/tunnel"
	}
	addr := u.Host
	if u.Port() == "" {
		addr = net.JoinHostPort(u.Hostname(), "443")
	}
	d := &tls.Dialer{
		NetDialer: &net.Dialer{Timeout: r.cfg.handshakeTimeout},
		Config: &tls.Config{
			RootCAs:    r.cfg.roots,
			ServerName: u.Hostname(),
			NextProtos: []string{"http/1.1"},
			MinVersion: tls.VersionTLS12,
		},
	}
	conn, err := d.DialContext(ctx, "tcp", addr)
	if err != nil {
		return nil, "", "", err
	}
	_ = conn.SetDeadline(time.Now().Add(r.cfg.handshakeTimeout))
	stop := context.AfterFunc(ctx, func() { _ = conn.SetDeadline(time.Unix(1, 0)) })
	defer stop()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, u.String(), nil)
	if err != nil {
		_ = conn.Close()
		return nil, "", "", err
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Upgrade", Protocol)
	req.Header.Set("Connection", "Upgrade")
	req.Header.Set("Wn-Server-Id", serverID)
	req.Header.Set("Wn-Server-Version", r.opts.Version)
	if err := req.Write(conn); err != nil {
		_ = conn.Close()
		return nil, "", "", err
	}
	br := bufio.NewReader(conn)
	resp, err := http.ReadResponse(br, req)
	if err != nil {
		_ = conn.Close()
		return nil, "", "", err
	}
	if resp.StatusCode != http.StatusSwitchingProtocols {
		var body struct {
			Code string `json:"code"`
		}
		_ = json.NewDecoder(io.LimitReader(resp.Body, 4096)).Decode(&body)
		_ = conn.Close()
		return nil, "", "", &statusError{status: resp.StatusCode, code: body.Code}
	}
	if !stop() {
		_ = conn.Close()
		return nil, "", "", ctx.Err()
	}
	_ = conn.SetDeadline(time.Time{})
	return &bufferedConn{Conn: conn, r: br}, resp.Header.Get("Wn-Relay-Host"), resp.Header.Get("Wn-Relay-Session"), nil
}

// bufferedConn reads the bytes that the handshake reader buffered past the
// 101 answer before reading the connection.
type bufferedConn struct {
	net.Conn
	r *bufio.Reader
}

func (c *bufferedConn) Read(p []byte) (int, error) { return c.r.Read(p) }

// serve runs yamux as the server on conn and serves the tunneled requests
// until the session ends or ctx ends. It answers why the session ended.
func (r *Relay) serve(ctx context.Context, conn net.Conn) error {
	ycfg := yamux.DefaultConfig()
	// yamux's own keepalive closes the session at the first missed pong.
	// heartbeat tolerates missedPongLimit misses in a row instead.
	ycfg.EnableKeepAlive = false
	ycfg.ConnectionWriteTimeout = r.cfg.writeTimeout
	ycfg.LogOutput = nil
	ycfg.Logger = slog.NewLogLogger(r.log.Handler(), slog.LevelDebug)
	sess, err := yamux.Server(conn, ycfg)
	if err != nil {
		_ = conn.Close()
		return err
	}
	lctx, stopAccept := context.WithCancel(context.Background())
	defer stopAccept()
	ln := &sessionListener{sess: sess, ctx: lctx, stop: stopAccept}
	srv := &http.Server{
		Handler:           http.HandlerFunc(r.serveHTTP),
		ReadHeaderTimeout: 10 * time.Second,
		ErrorLog:          slog.NewLogLogger(r.log.Handler(), slog.LevelDebug),
	}
	done := make(chan error, 1)
	go func() { done <- srv.Serve(ln) }()
	var missed atomic.Pointer[error]
	hbDone := make(chan struct{})
	go func() {
		defer close(hbDone)
		r.heartbeat(ctx, sess, &missed)
	}()
	defer func() { <-hbDone }()

	select {
	case <-ctx.Done():
		shutCtx, cancel := context.WithTimeout(context.Background(), r.cfg.shutdownGrace)
		if err := srv.Shutdown(shutCtx); err != nil {
			_ = srv.Close()
		}
		cancel()
		_ = sess.Close()
		<-done
		return ctx.Err()
	case err := <-done:
		_ = srv.Close()
		_ = sess.Close()
		// heartbeat stores its error before it closes the session, so a
		// session that heartbeat closed shows the missed pongs here and not
		// the error of the closed listener.
		if m := missed.Load(); m != nil {
			return *m
		}
		return err
	}
}

// heartbeat pings the peer every config.keepAlive. A ping that gets no pong
// within config.writeTimeout, or that cannot be written, is a miss, and a pong
// resets the count. After missedPongLimit misses in a row heartbeat stores an
// error that names them in missed and closes sess. It returns when sess is
// closed or ctx ends.
func (r *Relay) heartbeat(ctx context.Context, sess *yamux.Session, missed *atomic.Pointer[error]) {
	misses := 0
	for {
		select {
		case <-ctx.Done():
			return
		case <-sess.CloseChan():
			return
		case <-time.After(r.cfg.keepAlive):
		}
		_, err := sess.Ping()
		switch {
		case err == nil:
			misses = 0
		case errors.Is(err, yamux.ErrSessionShutdown):
			return
		default:
			misses++
			r.log.Debug("relay keepalive pong missed", "misses", misses, "err", err)
			if misses >= missedPongLimit {
				cause := fmt.Errorf("%d keepalive pongs missed", misses)
				missed.Store(&cause)
				_ = sess.Close()
				return
			}
		}
	}
}

// serveHTTP moves X-Relay-Client into the request context and calls the
// handler.
func (r *Relay) serveHTTP(w http.ResponseWriter, req *http.Request) {
	client := req.Header.Get(clientHeader)
	req.Header.Del(clientHeader)
	r.opts.Handler.ServeHTTP(w, req.WithContext(relayreq.WithRelay(req.Context(), client)))
}

// sessionListener is a yamux session as a net.Listener. Close stops Accept
// and sends a GoAway but leaves open streams running, so that
// http.Server.Shutdown waits for in-flight requests. The caller closes the
// session after the server.
type sessionListener struct {
	sess *yamux.Session
	ctx  context.Context
	stop context.CancelFunc
	once sync.Once
}

func (l *sessionListener) Accept() (net.Conn, error) {
	s, err := l.sess.AcceptStreamWithContext(l.ctx)
	if err != nil {
		if l.ctx.Err() != nil {
			return nil, net.ErrClosed
		}
		return nil, err
	}
	return s, nil
}

func (l *sessionListener) Close() error {
	l.once.Do(func() {
		l.stop()
		go func() { _ = l.sess.GoAway() }()
	})
	return nil
}

func (l *sessionListener) Addr() net.Addr { return l.sess.Addr() }

// sleep waits for d or the end of ctx. It answers false when ctx ended.
func sleep(ctx context.Context, d time.Duration) bool {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-t.C:
		return true
	}
}
