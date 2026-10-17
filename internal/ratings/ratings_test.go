package ratings

import (
	"context"
	"errors"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
	"github.com/waxnerdhq/wn-server/internal/wncloud/stub"
)

const (
	waxnerdID  = "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70"
	serverID   = "7f3c9a2e-1b4d-4e8f-9a6b-2c5d8e1f0a3b"
	cloudToken = "wn_servercredential"
	releaseID  = "6e12a11e-0ef9-49f3-b646-66929507d915"
)

func TestMirror(t *testing.T) {
	for half := 1; half <= 10; half++ {
		h := half
		stars, starred := Mirror(&h, nil)
		if want := (half + 1) / 2; stars != want || starred {
			t.Errorf("Mirror(%d) = %d, %v; want %d, false", half, stars, starred, want)
		}
	}
	love, like := LikeLove, LikeLike
	if stars, starred := Mirror(nil, &love); stars != 0 || !starred {
		t.Errorf("Mirror(nil, love) = %d, %v", stars, starred)
	}
	if _, starred := Mirror(nil, &like); starred {
		t.Error("like is starred")
	}
	if got := []time.Duration{Backoff(1), Backoff(2), Backoff(3), Backoff(7), Backoff(50)}; got[0] != time.Minute ||
		got[1] != 2*time.Minute || got[2] != 4*time.Minute || got[3] != time.Hour || got[4] != time.Hour {
		t.Errorf("Backoff = %v", got)
	}
}

type env struct {
	st    *store.Store
	owner store.Account
	cloud *stub.Stub
	down  *atomic.Bool
	puts  *atomic.Int32
	svc   *Service
}

// newEnv returns a service with a real store and the wn-cloud stub behind a
// switch that makes it answer 503.
func newEnv(t *testing.T, linked bool) *env {
	t.Helper()
	ctx := context.Background()
	st, err := store.Open(ctx, filepath.Join(t.TempDir(), store.FileName))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { st.Close() })
	owner, err := st.CreateOwner(ctx, "owner")
	if err != nil {
		t.Fatal(err)
	}
	if linked {
		if err := st.Link(ctx, owner.ID, store.ServerCredential{Token: cloudToken, ServerID: serverID, WaxnerdUserID: waxnerdID}); err != nil {
			t.Fatal(err)
		}
	}
	cloud := stub.New()
	cloud.AddUser(waxnerdID, "owner@example.com")
	cloud.AddToken(cloudToken, waxnerdID)
	var down atomic.Bool
	var puts atomic.Int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if down.Load() {
			http.Error(w, "unavailable", http.StatusServiceUnavailable)
			return
		}
		puts.Add(1)
		cloud.ServeHTTP(w, r)
	}))
	t.Cleanup(srv.Close)
	svc := New(st, wncloud.New(srv.URL, "test"), nil, &wncloud.Status{}, slog.New(slog.DiscardHandler))
	return &env{st: st, owner: owner, cloud: cloud, down: &down, puts: &puts, svc: svc}
}

func (e *env) cloudRating(t *testing.T) (wncloud.Rating, error) {
	t.Helper()
	e.down.Store(false)
	return wncloud.New(e.svcURL(), "test").GetRating(context.Background(), cloudToken, KindRelease, releaseID)
}

func (e *env) svcURL() string { return e.svc.cloud.(*wncloud.Client).BaseURL }

func TestOutbox(t *testing.T) {
	ctx := context.Background()
	e := newEnv(t, true)
	e.down.Store(true)
	six, love := 6, LikeLove
	if err := e.svc.Set(ctx, e.owner.ID, KindRelease, releaseID, &six, &love); err != nil {
		t.Fatal(err)
	}
	if r, err := e.st.Rating(ctx, e.owner.ID, KindRelease, releaseID); err != nil || *r.HalfStars != 6 || *r.LikeLevel != LikeLove {
		t.Fatalf("store rating = %+v, %v", r, err)
	}
	box, err := e.st.Outbox(ctx)
	if err != nil || len(box) != 1 || box[0].Attempts != 1 || box[0].LastError == "" {
		t.Fatalf("outbox = %+v, %v; want one entry after one attempt", box, err)
	}
	if _, down := e.svc.status.UnreachableSince(); !down {
		t.Fatal("status is reachable after a 503")
	}

	// While wn-cloud is marked unreachable, a write is queued without a call.
	before := e.puts.Load()
	if err := e.svc.SetLike(ctx, e.owner.ID, Subject{KindRelease, releaseID}, nil); err != nil {
		t.Fatal(err)
	}
	if e.puts.Load() != before {
		t.Fatal("wn-cloud was called while marked unreachable")
	}

	// wn-cloud returns. The replaced entry is due at once.
	e.down.Store(false)
	if err := e.svc.Drain(ctx); err != nil {
		t.Fatal(err)
	}
	if box, _ := e.st.Outbox(ctx); len(box) != 0 {
		t.Fatalf("outbox after drain = %+v", box)
	}
	got, err := e.cloudRating(t)
	if err != nil || got.HalfStars == nil || *got.HalfStars != 6 || got.LikeLevel != nil {
		t.Fatalf("wn-cloud rating = %+v, %v; want 6 half stars and no like level", got, err)
	}
	if _, down := e.svc.status.UnreachableSince(); down {
		t.Fatal("status is unreachable after a drain")
	}

	// A failed entry waits for its backoff.
	e.down.Store(true)
	if err := e.svc.Delete(ctx, e.owner.ID, KindRelease, releaseID); err != nil {
		t.Fatal(err)
	}
	e.svc.status.MarkReachable()
	e.down.Store(false)
	if err := e.svc.Drain(ctx); err != nil {
		t.Fatal(err)
	}
	if box, _ := e.st.Outbox(ctx); len(box) != 1 || !box[0].Deleted {
		t.Fatalf("outbox = %+v; want the delete waiting for its backoff", box)
	}
	e.svc.now = func() time.Time { return time.Now().Add(2 * time.Minute) }
	if err := e.svc.Drain(ctx); err != nil {
		t.Fatal(err)
	}
	if _, err := e.cloudRating(t); !errors.Is(err, wncloud.ErrNotFound) {
		t.Fatalf("wn-cloud rating after the drained delete: %v, want ErrNotFound", err)
	}
}

func TestNotLinked(t *testing.T) {
	ctx := context.Background()
	e := newEnv(t, false)
	eight := 8
	if err := e.svc.Set(ctx, e.owner.ID, KindRelease, releaseID, &eight, nil); err != nil {
		t.Fatal(err)
	}
	if r, err := e.st.Rating(ctx, e.owner.ID, KindRelease, releaseID); err != nil || *r.HalfStars != 8 {
		t.Fatalf("store rating = %+v, %v", r, err)
	}
	if e.puts.Load() != 0 {
		t.Fatal("wn-cloud was called in local-only mode")
	}
	if box, _ := e.st.Outbox(ctx); len(box) != 0 {
		t.Fatalf("outbox = %+v", box)
	}

	// The first login queues the local ratings, and the next drain sends them.
	if err := e.st.Link(ctx, e.owner.ID, store.ServerCredential{Token: cloudToken, ServerID: serverID, WaxnerdUserID: waxnerdID}); err != nil {
		t.Fatal(err)
	}
	if err := e.svc.Drain(ctx); err != nil {
		t.Fatal(err)
	}
	if got, err := e.cloudRating(t); err != nil || *got.HalfStars != 8 {
		t.Fatalf("wn-cloud rating after login = %+v, %v", got, err)
	}
}
