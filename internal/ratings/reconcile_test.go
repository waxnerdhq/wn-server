package ratings

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

const trackID = "1f0d2b5e-3c4a-4b6d-8e9f-0a1b2c3d4e5f"

// fakeND is a Navidrome that holds the albums and songs given by MBID and
// records the rating calls it receives.
type fakeND struct {
	mu     sync.Mutex
	albums map[string][]string
	songs  map[string][]string
	calls  []string
}

func (f *fakeND) Song(context.Context, string, string) (navidrome.Song, error) {
	return navidrome.Song{}, navidrome.ErrNotFound
}

func (f *fakeND) Album(context.Context, string, string) (navidrome.Album, error) {
	return navidrome.Album{}, navidrome.ErrNotFound
}

func (f *fakeND) AlbumsByMBID(_ context.Context, _, mbid string) ([]string, error) {
	return f.albums[mbid], nil
}

func (f *fakeND) SongsByMBID(_ context.Context, _, mbid string) ([]string, error) {
	return f.songs[mbid], nil
}

func (f *fakeND) SetRating(_ context.Context, _, id string, stars int) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.calls = append(f.calls, fmt.Sprintf("rating %s %d", id, stars))
	return nil
}

func (f *fakeND) SetStarred(_ context.Context, _, id string, starred bool) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.calls = append(f.calls, fmt.Sprintf("starred %s %v", id, starred))
	return nil
}

// take returns the recorded calls and forgets them.
func (f *fakeND) take() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	c := f.calls
	f.calls = nil
	return c
}

func equal(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func reconcileEnv(t *testing.T) (*env, *fakeND) {
	t.Helper()
	e := newEnv(t, true)
	nd := &fakeND{albums: map[string][]string{releaseID: {"al-1"}}, songs: map[string][]string{}}
	e.svc.nd = nd
	return e, nd
}

func (e *env) local(t *testing.T, kind, mbid string) string {
	t.Helper()
	r, err := e.st.Rating(context.Background(), e.owner.ID, kind, mbid)
	if errors.Is(err, store.ErrRatingNotFound) {
		return "none"
	}
	if err != nil {
		t.Fatal(err)
	}
	h, l := "nil", "nil"
	if r.HalfStars != nil {
		h = fmt.Sprint(*r.HalfStars)
	}
	if r.LikeLevel != nil {
		l = *r.LikeLevel
	}
	return h + "/" + l
}

func TestReconcile(t *testing.T) {
	ctx := context.Background()
	e, nd := reconcileEnv(t)

	// A rating set at wn-cloud appears locally and, rounded, in Navidrome.
	// A track that the library does not hold is stored and not pushed.
	e.cloud.AddRating(waxnerdID, KindRelease, releaseID, 9, LikeLove)
	e.cloud.AddRating(waxnerdID, KindTrack, trackID, 3, "")
	if err := e.svc.Reconcile(ctx); err != nil {
		t.Fatal(err)
	}
	if got := e.local(t, KindRelease, releaseID); got != "9/love" {
		t.Fatalf("local release rating %s, want 9/love", got)
	}
	if got := e.local(t, KindTrack, trackID); got != "3/nil" {
		t.Fatalf("local track rating %s, want 3/nil", got)
	}
	if got := nd.take(); !equal(got, []string{"rating al-1 5", "starred al-1 true"}) {
		t.Fatalf("Navidrome calls %v", got)
	}
	if box, _ := e.st.Outbox(ctx); len(box) != 0 {
		t.Fatalf("outbox %+v; a change from wn-cloud must not be queued", box)
	}
	if c, _ := e.st.RatingSyncCursor(ctx); c == "" {
		t.Fatal("no cursor stored")
	}

	// The stored cursor makes the next run empty.
	if err := e.svc.Reconcile(ctx); err != nil {
		t.Fatal(err)
	}
	if got := nd.take(); len(got) != 0 {
		t.Fatalf("Navidrome calls %v on a run without changes", got)
	}

	// A deletion at wn-cloud clears the local rating and Navidrome's mirror.
	cloud := wncloud.New(e.svcURL(), "test")
	if err := cloud.DeleteRating(ctx, cloudToken, KindRelease, releaseID); err != nil {
		t.Fatal(err)
	}
	if err := e.svc.Reconcile(ctx); err != nil {
		t.Fatal(err)
	}
	if got := e.local(t, KindRelease, releaseID); got != "none" {
		t.Fatalf("local release rating %s after a deletion, want none", got)
	}
	if got := nd.take(); !equal(got, []string{"rating al-1 0", "starred al-1 false"}) {
		t.Fatalf("Navidrome calls %v", got)
	}
	changes, _, err := e.svc.Changes(ctx, e.owner.ID, "", 10)
	if err != nil || len(changes) != 1 || changes[0].Kind != KindTrack {
		t.Fatalf("feed from the start = %+v, %v; want only the track", changes, err)
	}
}

func TestReconcilePage(t *testing.T) {
	ctx := context.Background()
	e, _ := reconcileEnv(t)
	// More changes than a page: the loop follows the pages with the cursor.
	for i := 0; i < 3; i++ {
		e.cloud.AddRating(waxnerdID, KindTrack, fmt.Sprintf("1f0d2b5e-3c4a-4b6d-8e9f-0a1b2c3d4e5%d", i), 4, "")
	}
	paged := &pagedCloud{Cloud: e.svc.cloud, limit: 2}
	e.svc.cloud = paged
	if err := e.svc.Reconcile(ctx); err != nil {
		t.Fatal(err)
	}
	changes, _, _ := e.svc.Changes(ctx, e.owner.ID, "", 10)
	if len(changes) != 3 || paged.calls != 2 {
		t.Fatalf("%d ratings stored after %d calls, want 3 after 2", len(changes), paged.calls)
	}
}

// pagedCloud limits the page size of ListRatings and counts the calls.
type pagedCloud struct {
	Cloud
	limit int
	calls int
}

func (p *pagedCloud) ListRatings(ctx context.Context, token, since string, _ int) (wncloud.RatingChanges, error) {
	p.calls++
	return p.Cloud.ListRatings(ctx, token, since, p.limit)
}

func TestReconcileOutboxPrecedence(t *testing.T) {
	ctx := context.Background()
	e, nd := reconcileEnv(t)
	five := 5

	// A local write that is newer than the change at wn-cloud wins, and its
	// outbox entry stays.
	e.svc.now = func() time.Time { return time.Now().Add(time.Hour) }
	if err := e.svc.Set(ctx, e.owner.ID, KindRelease, releaseID, &five, nil); err != nil {
		t.Fatal(err)
	}
	e.cloud.AddRating(waxnerdID, KindRelease, releaseID, 9, LikeLove)
	if err := e.svc.Reconcile(ctx); err != nil {
		t.Fatal(err)
	}
	if got := e.local(t, KindRelease, releaseID); got != "5/nil" {
		t.Fatalf("local rating %s, want the newer local 5/nil", got)
	}
	if box, _ := e.st.Outbox(ctx); len(box) != 1 {
		t.Fatalf("outbox %+v, want the newer local write kept", box)
	}
	if got := nd.take(); len(got) != 0 {
		t.Fatalf("Navidrome calls %v for a change that was kept out", got)
	}

	// A local write that is older than the change at wn-cloud loses, and its
	// outbox entry is dropped.
	e.svc.now = func() time.Time { return time.Now().Add(-time.Hour) }
	if err := e.svc.Set(ctx, e.owner.ID, KindRelease, releaseID, &five, nil); err != nil {
		t.Fatal(err)
	}
	e.cloud.AddRating(waxnerdID, KindRelease, releaseID, 9, LikeLove)
	if err := e.svc.Reconcile(ctx); err != nil {
		t.Fatal(err)
	}
	if got := e.local(t, KindRelease, releaseID); got != "9/love" {
		t.Fatalf("local rating %s, want the newer remote 9/love", got)
	}
	if box, _ := e.st.Outbox(ctx); len(box) != 0 {
		t.Fatalf("outbox %+v, want the older local write dropped", box)
	}
	if got := nd.take(); !equal(got, []string{"rating al-1 5", "starred al-1 true"}) {
		t.Fatalf("Navidrome calls %v", got)
	}
}

func TestReconcileCredentialAndOutage(t *testing.T) {
	ctx := context.Background()

	// Not linked: nothing is called.
	e := newEnv(t, false)
	if err := e.svc.Reconcile(ctx); err != nil || e.puts.Load() != 0 {
		t.Fatalf("Reconcile without a link = %v after %d calls", err, e.puts.Load())
	}

	// A server credential that wn-cloud does not know is recorded.
	bad := store.ServerCredential{Token: "wn_unknown", ServerID: serverID, WaxnerdUserID: waxnerdID}
	if err := e.st.Link(ctx, e.owner.ID, bad); err != nil {
		t.Fatal(err)
	}
	if err := e.svc.Reconcile(ctx); err != nil {
		t.Fatal(err)
	}
	if !e.svc.status.CredentialRejected() {
		t.Fatal("status does not show the rejected credential")
	}

	// A working credential clears it, and an outage is recorded.
	if err := e.st.Unlink(ctx, e.owner.ID); err != nil {
		t.Fatal(err)
	}
	if err := e.st.Link(ctx, e.owner.ID, store.ServerCredential{Token: cloudToken, ServerID: serverID, WaxnerdUserID: waxnerdID}); err != nil {
		t.Fatal(err)
	}
	if err := e.svc.Reconcile(ctx); err != nil {
		t.Fatal(err)
	}
	if e.svc.status.CredentialRejected() {
		t.Fatal("status shows a rejected credential after a successful run")
	}
	e.down.Store(true)
	if err := e.svc.Reconcile(ctx); err != nil {
		t.Fatal(err)
	}
	if _, down := e.svc.status.UnreachableSince(); !down {
		t.Fatal("status is reachable after a 503")
	}
}

// A listener's writes stay out of the outbox, and the owner's reconcile
// neither reads nor changes the listener's rows.
func TestListenerRatingsStayLocal(t *testing.T) {
	ctx := context.Background()
	e, _ := reconcileEnv(t)
	listener, err := e.st.CreateListener(ctx, "0192f1c4-0000-7000-8000-000000000001", "ada@example.com")
	if err != nil {
		t.Fatal(err)
	}
	two, love := 2, LikeLove
	if err := e.svc.Set(ctx, listener.ID, KindRelease, releaseID, &two, &love); err != nil {
		t.Fatal(err)
	}
	if box, _ := e.st.Outbox(ctx); len(box) != 0 {
		t.Fatalf("outbox %+v after a listener's write", box)
	}
	if len(e.svc.kick) != 0 {
		t.Fatal("a listener's write kicked the drain")
	}

	// The owner's rating of the same subject arrives from wn-cloud.
	e.cloud.AddRating(waxnerdID, KindRelease, releaseID, 9, "")
	if err := e.svc.Reconcile(ctx); err != nil {
		t.Fatal(err)
	}
	if got := e.local(t, KindRelease, releaseID); got != "9/nil" {
		t.Fatalf("owner's rating %s, want 9/nil", got)
	}
	r, err := e.st.Rating(ctx, listener.ID, KindRelease, releaseID)
	if err != nil || r.HalfStars == nil || *r.HalfStars != 2 || r.LikeLevel == nil || *r.LikeLevel != LikeLove {
		t.Fatalf("listener's rating %+v, %v; want 2 half stars and love", r, err)
	}
}
