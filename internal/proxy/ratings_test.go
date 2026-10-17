package proxy

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"testing"

	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/ratings"
	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

const (
	matchedAlbum   = "al-matched"
	unmatchedAlbum = "al-unmatched"
	matchedSong    = "mf-matched"
	refusedSong    = "mf-refused"
	releaseMBID    = "6e12a11e-0ef9-49f3-b646-66929507d915"
	trackMBID      = "1f0d2b5e-3c4a-4b6d-8e9f-0a1b2c3d4e5f"
)

// fakeNavidrome answers the native song and album lookups and the Subsonic
// rating calls. It records the rating calls it received.
type fakeNavidrome struct {
	owner string
	mu    sync.Mutex
	calls []string
}

func (f *fakeNavidrome) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if strings.HasPrefix(r.URL.Path, "/api/") {
		if r.Header.Get("Remote-User") != f.owner {
			http.Error(w, "wrong user", http.StatusUnauthorized)
			return
		}
		body := map[string]map[string]string{
			"/api/song/" + matchedSong:     {"id": matchedSong, "mbzReleaseTrackId": trackMBID, "mbzRecordingID": "00000000-0000-4000-8000-000000000000"},
			"/api/song/" + refusedSong:     {"id": refusedSong, "mbzReleaseTrackId": trackMBID},
			"/api/album/" + matchedAlbum:   {"id": matchedAlbum, "mbzAlbumId": releaseMBID},
			"/api/album/" + unmatchedAlbum: {"id": unmatchedAlbum},
		}[r.URL.Path]
		if body == nil {
			http.Error(w, `{"error":"not found"}`, http.StatusNotFound)
			return
		}
		_ = json.NewEncoder(w).Encode(body)
		return
	}
	f.mu.Lock()
	f.calls = append(f.calls, strings.TrimPrefix(r.URL.Path, "/rest/")+"?"+r.URL.RawQuery)
	f.mu.Unlock()
	w.Header().Set("Content-Type", "application/json")
	if r.URL.Query().Get("id") == refusedSong {
		_, _ = io.WriteString(w, `{"subsonic-response":{"status":"failed","error":{"code":0,"message":"refused"}}}`)
		return
	}
	_, _ = io.WriteString(w, `{"subsonic-response":{"status":"ok","version":"1.16.1"}}`)
}

func (f *fakeNavidrome) count() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return len(f.calls)
}

type ratingEnv struct {
	*env
	nd *fakeNavidrome
}

func newRatingEnv(t *testing.T) *ratingEnv {
	t.Helper()
	nd := &fakeNavidrome{}
	e := newEnvOpts(t, nd, true, func(e *env) []Option {
		port, _ := strconv.Atoi(e.target.Port())
		svc := ratings.New(e.st, wncloud.New(e.cloudURL, "test"), navidrome.NewClient(port), &wncloud.Status{}, slog.New(slog.DiscardHandler))
		return []Option{WithRatings(svc)}
	})
	nd.owner = e.owner.ID
	e.cloud.AddUser(waxnerdID, "owner@example.com")
	e.cloud.AddToken(cloudToken, waxnerdID)
	if err := e.st.Link(context.Background(), e.owner.ID, store.ServerCredential{Token: cloudToken, ServerID: serverID, WaxnerdUserID: waxnerdID}); err != nil {
		t.Fatal(err)
	}
	return &ratingEnv{env: e, nd: nd}
}

// call sends a Subsonic call and fails unless the answer is Navidrome's.
func (e *ratingEnv) call(t *testing.T, query string) {
	t.Helper()
	resp, err := http.Get(e.srv.URL + "/rest/" + query + "&" + creds() + "&f=json")
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if !strings.Contains(string(body), `"version":"1.16.1"`) && !strings.Contains(string(body), "refused") {
		t.Fatalf("%s: body %s is not Navidrome's answer", query, body)
	}
}

// local returns the stored rating, or nil values when there is none.
func (e *ratingEnv) local(t *testing.T, kind, mbid string) (*int, *string) {
	t.Helper()
	r, err := e.st.Rating(context.Background(), e.owner.ID, kind, mbid)
	if errors.Is(err, store.ErrRatingNotFound) {
		return nil, nil
	}
	if err != nil {
		t.Fatal(err)
	}
	return r.HalfStars, r.LikeLevel
}

// remote returns the wn-cloud rating, or nil values when there is none.
func (e *ratingEnv) remote(t *testing.T, kind, mbid string) (*int, *string) {
	t.Helper()
	r, err := wncloud.New(e.cloudURL, "test").GetRating(context.Background(), cloudToken, kind, mbid)
	if errors.Is(err, wncloud.ErrNotFound) {
		return nil, nil
	}
	if err != nil {
		t.Fatal(err)
	}
	return r.HalfStars, r.LikeLevel
}

func value(half *int, like *string) string {
	h, l := "nil", "nil"
	if half != nil {
		h = strconv.Itoa(*half)
	}
	if like != nil {
		l = *like
	}
	return h + "/" + l
}

func TestSetRatingOnAlbum(t *testing.T) {
	e := newRatingEnv(t)
	e.call(t, "setRating.view?id="+matchedAlbum+"&rating=4")
	if e.nd.count() != 1 || !strings.Contains(e.nd.calls[0], "rating=4") {
		t.Fatalf("Navidrome calls %v", e.nd.calls)
	}
	if got := value(e.local(t, "release", releaseMBID)); got != "8/nil" {
		t.Fatalf("local rating %s, want 8/nil", got)
	}
	if got := value(e.remote(t, "release", releaseMBID)); got != "8/nil" {
		t.Fatalf("wn-cloud rating %s, want 8/nil", got)
	}

	// An album without a MusicBrainz id is rated in Navidrome only.
	e.call(t, "setRating?id="+unmatchedAlbum+"&rating=2")
	if e.nd.count() != 2 {
		t.Fatalf("Navidrome calls %v", e.nd.calls)
	}
	page, err := wncloud.New(e.cloudURL, "test").ListRatings(context.Background(), cloudToken, "", 0)
	if err != nil || len(page.Changes) != 1 {
		t.Fatalf("wn-cloud ratings %+v, %v; want only the matched album", page, err)
	}
}

func TestStarAndRatingOnSong(t *testing.T) {
	e := newRatingEnv(t)
	steps := []struct{ query, want string }{
		{"star.view?id=" + matchedSong + "&artistId=ar-1", "nil/love"},
		{"setRating?id=" + matchedSong + "&rating=3", "6/love"},
		{"setRating?id=" + matchedSong + "&rating=0", "nil/love"},
		{"setRating?id=" + matchedSong + "&rating=5", "10/love"},
		{"unstar?id=" + matchedSong, "10/nil"},
		{"setRating?id=" + matchedSong + "&rating=0", "nil/nil"},
	}
	for i, s := range steps {
		e.call(t, s.query)
		if e.nd.count() != i+1 {
			t.Fatalf("%s: Navidrome calls %v", s.query, e.nd.calls)
		}
		if got := value(e.local(t, "track", trackMBID)); got != s.want {
			t.Fatalf("%s: local rating %s, want %s", s.query, got, s.want)
		}
		if got := value(e.remote(t, "track", trackMBID)); got != s.want {
			t.Fatalf("%s: wn-cloud rating %s, want %s", s.query, got, s.want)
		}
	}
}

func TestRatingRefusedByNavidrome(t *testing.T) {
	e := newRatingEnv(t)
	e.call(t, "star?id="+refusedSong)
	if e.nd.count() != 1 {
		t.Fatalf("Navidrome calls %v", e.nd.calls)
	}
	if got := value(e.local(t, "track", trackMBID)); got != "nil/nil" {
		t.Fatalf("local rating %s after a refused call", got)
	}
	if got := value(e.remote(t, "track", trackMBID)); got != "nil/nil" {
		t.Fatalf("wn-cloud rating %s after a refused call", got)
	}
}
