package cli

import (
	"context"
	"errors"
	"log/slog"
	"net/http"
	"sync"
	"time"

	"github.com/waxnerdhq/wn-server/internal/config"
	"github.com/waxnerdhq/wn-server/internal/relay"
	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

const (
	// relayFetchInterval is the time between two successful calls of
	// GET /servers/:id.
	relayFetchInterval = time.Hour
	// relayFetchRetry is the time before the next call after a failed one.
	relayFetchRetry = 5 * time.Minute
	// relayCredentialPoll is how often serve reads the server credential,
	// so that GET /servers/:id runs within this time of a login.
	relayCredentialPoll = 10 * time.Second
	// relayStateWriteTimeout bounds one write of the relay_state row.
	relayStateWriteTimeout = 5 * time.Second
)

// relayHolder holds the relay state for the web UI's status page.
type relayHolder struct {
	mu sync.Mutex
	s  relay.State
}

func (h *relayHolder) set(s relay.State) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.s = s
}

// State answers the current relay state.
func (h *relayHolder) State() relay.State {
	h.mu.Lock()
	defer h.mu.Unlock()
	return h.s
}

// relayWiring connects the relay package to the store, wn-cloud, and the web
// UI (docs/relay.md, section 3).
type relayWiring struct {
	cfg         *config.Config
	st          *store.Store
	cloud       *wncloud.Client
	cloudStatus *wncloud.Status
	holder      *relayHolder
	logger      *slog.Logger
	now         func() time.Time
}

// start writes the first relay state, off or waiting, so that wn-server
// status shows no state of an earlier run. With relay = true it starts the
// calls of GET /servers/:id and the relay package, which serves tunneled
// requests with handler. The returned channel is closed when both have
// stopped after ctx ended and the state off is written. With relay = false
// nothing is dialed and the channel is closed at once.
func (w *relayWiring) start(ctx context.Context, handler http.Handler) <-chan struct{} {
	done := make(chan struct{})
	if !w.cfg.Relay {
		w.write(relay.State{Name: relay.Off, Since: w.now()})
		close(done)
		return done
	}
	w.write(relay.State{Name: relay.Waiting, Since: w.now()})
	rl := relay.New(relay.Options{
		Handler:    handler,
		Credential: func() (string, string, bool) { return w.credential(ctx) },
		ConnectURL: func() (string, bool) { return w.connectURL(ctx) },
		Version:    Version,
		OnState:    w.write,
		Logger:     w.logger,
	})
	first := make(chan struct{})
	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		w.fetchLoop(ctx, first)
	}()
	go func() {
		defer wg.Done()
		// The first call of GET /servers/:id runs before the first dial, so
		// that a fresh store has an edge address without a wait.
		select {
		case <-first:
		case <-ctx.Done():
		}
		rl.Run(ctx)
		// Run reports no state when ctx ends. The tunnel is closed now.
		w.write(relay.State{Name: relay.Off, Since: w.now()})
	}()
	go func() {
		wg.Wait()
		close(done)
	}()
	return done
}

// write records a relay state in the holder and in the relay_state row.
func (w *relayWiring) write(s relay.State) {
	w.holder.set(s)
	ctx, cancel := context.WithTimeout(context.Background(), relayStateWriteTimeout)
	defer cancel()
	err := w.st.SetRelayState(ctx, store.RelayState{
		State: string(s.Name), Since: s.Since, Host: s.Host, LastError: s.Err, WrittenAt: w.now(),
	})
	if err != nil {
		w.logger.Error("write the relay state", "state", string(s.Name), "error", err)
	}
}

// credential answers the server credential for the relay package.
func (w *relayWiring) credential(ctx context.Context) (token, serverID string, ok bool) {
	cred, err := w.st.ServerCredential(ctx)
	if err != nil {
		if !errors.Is(err, store.ErrNoServerCredential) && ctx.Err() == nil {
			w.logger.Error("read the server credential for the relay", "error", err)
		}
		return "", "", false
	}
	return cred.Token, cred.ServerID, true
}

// connectURL answers relay_edge_url when it is set, else the stored
// connect_url from GET /servers/:id.
func (w *relayWiring) connectURL(ctx context.Context) (string, bool) {
	if w.cfg.RelayEdgeURL != "" {
		return w.cfg.RelayEdgeURL, true
	}
	rs, err := w.st.RelaySettings(ctx)
	if err != nil {
		if !errors.Is(err, store.ErrNoRelaySettings) && ctx.Err() == nil {
			w.logger.Error("read the relay settings", "error", err)
		}
		return "", false
	}
	return rs.ConnectURL, rs.ConnectURL != ""
}

// fetchLoop calls GET /servers/:id at start, every hour, and within
// relayCredentialPoll of a new server credential, until ctx ends. It closes
// first after its first pass.
func (w *relayWiring) fetchLoop(ctx context.Context, first chan<- struct{}) {
	t := time.NewTicker(relayCredentialPoll)
	defer t.Stop()
	var (
		token string
		next  time.Time
	)
	for {
		cred, err := w.st.ServerCredential(ctx)
		switch {
		case err == nil:
			if cred.Token != token || !w.now().Before(next) {
				token = cred.Token
				next = w.now().Add(w.fetch(ctx, cred))
			}
		case errors.Is(err, store.ErrNoServerCredential):
			token = ""
		case ctx.Err() == nil:
			w.logger.Error("read the server credential for the relay", "error", err)
		}
		if first != nil {
			close(first)
			first = nil
		}
		select {
		case <-ctx.Done():
			return
		case <-t.C:
		}
	}
}

// fetch calls GET /servers/:id, stores the relay settings, and records in
// the shared status whether wn-cloud answered and accepted the credential.
// It answers the time until the next call.
func (w *relayWiring) fetch(ctx context.Context, cred store.ServerCredential) time.Duration {
	srv, err := w.cloud.GetServer(ctx, cred.Token, cred.ServerID)
	if ctx.Err() != nil {
		return 0
	}
	switch {
	case errors.Is(err, wncloud.ErrUnreachable):
		w.cloudStatus.MarkUnreachable(w.now())
		w.logger.Warn("wn-cloud is unreachable; the relay uses the stored edge address", "error", err)
		return relayFetchRetry
	case errors.Is(err, wncloud.ErrUnauthorized):
		w.cloudStatus.MarkReachable()
		w.cloudStatus.SetCredentialRejected(true)
		w.logger.Error("wn-cloud refused the server credential for the relay settings; run `wn-server logout`, then `wn-server login`", "error", err)
		return relayFetchInterval
	case err != nil:
		w.cloudStatus.MarkReachable()
		w.logger.Warn("read the relay settings from wn-cloud", "error", err)
		return relayFetchRetry
	}
	w.cloudStatus.MarkReachable()
	w.cloudStatus.SetCredentialRejected(false)
	addrs := make([]store.RelayAddress, 0, len(srv.Addresses))
	for _, a := range srv.Addresses {
		addrs = append(addrs, store.RelayAddress{Kind: a.Kind, URL: a.URL})
	}
	err = w.st.SetRelaySettings(ctx, store.RelaySettings{
		ConnectURL: srv.Relay.ConnectURL, Host: srv.Relay.Host, Addresses: addrs, FetchedAt: w.now(),
	})
	if err != nil {
		w.logger.Error("store the relay settings", "error", err)
		return relayFetchRetry
	}
	return relayFetchInterval
}

// relayStateLine renders the relay_state row as wn-server status prints it.
func relayStateLine(r store.RelayState) string {
	s := relay.State{Name: relay.StateName(r.State), Since: r.Since, Host: r.Host, Err: r.LastError}
	return s.Text() + " (written " + r.WrittenAt.Format(time.RFC3339) + ")"
}
