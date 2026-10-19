package cli

import (
	"context"
	"log/slog"
	"net"
	"net/http"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/config"
	"github.com/waxnerdhq/wn-server/internal/relay"
	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

// testEdge is a loopback TCP listener that counts the connections it
// accepts and closes each at once. The relay package's TLS handshake with it
// fails, so a dial ends in the state unreachable. The relay package trusts
// only the system roots, and its option for other roots is unexported, so
// the wiring is tested up to the dial.
type testEdge struct {
	ln    net.Listener
	dials atomic.Int32
}

func newTestEdge(t *testing.T) *testEdge {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	e := &testEdge{ln: ln}
	go func() {
		for {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			e.dials.Add(1)
			_ = conn.Close()
		}
	}()
	t.Cleanup(func() { _ = ln.Close() })
	return e
}

func (e *testEdge) url() string { return "https://" + e.ln.Addr().String() + "/v1/tunnel" }

func waitUntil(t *testing.T, what string, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for %s", what)
		}
		time.Sleep(20 * time.Millisecond)
	}
}

// startWiring opens the store of cfg and starts the relay wiring with c. It
// returns the store, the holder, and a stop function that waits for the
// wiring to end.
func startWiring(t *testing.T, cfg string, c *config.Config) (*store.Store, *relayHolder, func()) {
	t.Helper()
	st, err := store.Open(context.Background(), store.Path(filepath.Dir(cfg)))
	if err != nil {
		t.Fatal(err)
	}
	holder := &relayHolder{}
	ctx, cancel := context.WithCancel(context.Background())
	done := (&relayWiring{
		cfg: c, st: st, cloud: newClient(c), cloudStatus: &wncloud.Status{}, holder: holder,
		logger: slog.New(slog.DiscardHandler), now: time.Now,
	}).start(ctx, http.NotFoundHandler())
	stopped := false
	stop := func() {
		if stopped {
			return
		}
		stopped = true
		cancel()
		<-done
		st.Close()
	}
	t.Cleanup(stop)
	return st, holder, stop
}

func TestRelayWiring(t *testing.T) {
	cloud := newCloud(t)
	cfg, run := setup(t, cloud)
	if out, err := run(same, "status"); err != nil || !strings.Contains(out, "relay: no state written") {
		t.Fatalf("status before serve: %v %q", err, out)
	}
	if out, err := run(same, "login", "--name", "n"); err != nil {
		t.Fatalf("login: %v\n%s", err, out)
	}
	cred, err := credential(t, cfg)
	if err != nil {
		t.Fatal(err)
	}
	load := func() *config.Config {
		c, err := config.Load(cfg)
		if err != nil {
			t.Fatal(err)
		}
		return c
	}

	t.Run("relay_edge_url", func(t *testing.T) {
		edge := newTestEdge(t)
		// The stored address is not dialed while relay_edge_url is set.
		cloud.stub.SetRelayConnectURL("https://127.0.0.1:1/v1/tunnel")
		c := load()
		c.Relay, c.RelayEdgeURL = true, edge.url()
		st, holder, stop := startWiring(t, cfg, c)
		waitUntil(t, "a dial and the state unreachable", func() bool {
			return edge.dials.Load() > 0 && holder.State().Name == relay.Unreachable
		})
		rs, err := st.RelaySettings(context.Background())
		if err != nil || rs.Host != cred.ServerID+".relay.waxnerd.com" || len(rs.Addresses) != 1 {
			t.Fatalf("relay settings = %+v, %v", rs, err)
		}
		out, err := run(same, "status")
		if err != nil || !strings.Contains(out, "relay: unreachable since ") || !strings.Contains(out, "(written ") {
			t.Fatalf("status while unreachable: %v %q", err, out)
		}
		stop()
		out, err = run(same, "status")
		if err != nil || !strings.Contains(out, "relay: off (written ") {
			t.Fatalf("status after the wiring stopped: %v %q", err, out)
		}
	})

	t.Run("stored connect_url", func(t *testing.T) {
		edge := newTestEdge(t)
		cloud.stub.SetRelayConnectURL(edge.url())
		c := load()
		c.Relay = true
		st, holder, _ := startWiring(t, cfg, c)
		waitUntil(t, "a dial and the state unreachable", func() bool {
			return edge.dials.Load() > 0 && holder.State().Name == relay.Unreachable
		})
		if rs, err := st.RelaySettings(context.Background()); err != nil || rs.ConnectURL != edge.url() {
			t.Fatalf("relay settings = %+v, %v", rs, err)
		}
	})

	t.Run("relay off", func(t *testing.T) {
		edge := newTestEdge(t)
		cloud.stub.SetRelayConnectURL(edge.url())
		c := load()
		c.RelayEdgeURL = edge.url()
		st, holder, _ := startWiring(t, cfg, c)
		if holder.State().Name != relay.Off {
			t.Fatalf("state = %+v, want off", holder.State())
		}
		rs, err := st.RelayState(context.Background())
		if err != nil || rs.State != string(relay.Off) {
			t.Fatalf("relay_state = %+v, %v", rs, err)
		}
		time.Sleep(200 * time.Millisecond)
		if n := edge.dials.Load(); n != 0 {
			t.Fatalf("relay = false dialed the edge %d times", n)
		}
	})

	t.Run("no credential", func(t *testing.T) {
		if out, err := run(same, "logout"); err != nil {
			t.Fatalf("logout: %v\n%s", err, out)
		}
		edge := newTestEdge(t)
		c := load()
		c.Relay, c.RelayEdgeURL = true, edge.url()
		_, holder, _ := startWiring(t, cfg, c)
		time.Sleep(200 * time.Millisecond)
		if s := holder.State(); s.Name != relay.Waiting || edge.dials.Load() != 0 {
			t.Fatalf("state %+v and %d dials without a credential, want waiting and none", s, edge.dials.Load())
		}
	})
}
