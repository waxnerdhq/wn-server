package proxy

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"regexp"
	"strings"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/store"
)

type apiRating struct {
	Kind      string  `json:"kind"`
	MBID      string  `json:"mbid"`
	HalfStars *int    `json:"half_stars"`
	LikeLevel *string `json:"like_level"`
	Deleted   bool    `json:"deleted"`
	UpdatedAt string  `json:"updated_at"`
}

// api sends a request to the rating API with a session and decodes a JSON
// answer into out, when out is not nil.
func (e *ratingEnv) api(t *testing.T, method, path, session, body string, out any) int {
	t.Helper()
	status, data := e.do(t, method, path, session, body)
	if out != nil && status/100 == 2 {
		if err := json.Unmarshal(data, out); err != nil {
			t.Fatalf("%s %s: %v in %s", method, path, err, data)
		}
	}
	return status
}

func (e *ratingEnv) session(t *testing.T) string {
	t.Helper()
	token, _, err := e.st.CreateSession(context.Background(), e.owner.ID, "test")
	if err != nil {
		t.Fatal(err)
	}
	return token
}

func TestRatingAPIPut(t *testing.T) {
	e := newRatingEnv(t)
	s := e.session(t)

	if got := e.api(t, http.MethodPut, "/api/v1/ratings/"+matchedAlbum, "", `{"half_stars":7}`, nil); got != http.StatusUnauthorized {
		t.Fatalf("PUT without a session: %d, want 401", got)
	}

	var r apiRating
	if got := e.api(t, http.MethodPut, "/api/v1/ratings/"+matchedAlbum, s, `{"half_stars":7,"like_level":"love"}`, &r); got != http.StatusOK {
		t.Fatalf("PUT: %d", got)
	}
	if r.Kind != "release" || r.MBID != releaseMBID || r.HalfStars == nil || *r.HalfStars != 7 || r.LikeLevel == nil || *r.LikeLevel != "love" || r.UpdatedAt == "" {
		t.Fatalf("PUT answered %+v", r)
	}
	if !regexp.MustCompile(`^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{6}Z$`).MatchString(r.UpdatedAt) {
		t.Fatalf("updated_at %q, want UTC with six fractional digits", r.UpdatedAt)
	}
	if got := value(e.local(t, "release", releaseMBID)); got != "7/love" {
		t.Fatalf("local rating %s, want 7/love", got)
	}
	// Seven half stars round up to four stars, and love is starred.
	if e.nd.count() != 2 || !strings.HasPrefix(e.nd.calls[0], "setRating?") || !strings.Contains(e.nd.calls[0], "rating=4") ||
		!strings.Contains(e.nd.calls[0], "id="+matchedAlbum) || !strings.HasPrefix(e.nd.calls[1], "star?") {
		t.Fatalf("Navidrome calls %v", e.nd.calls)
	}
	if got := value(e.remote(t, "release", releaseMBID)); got != "7/love" {
		t.Fatalf("wn-cloud rating %s, want 7/love", got)
	}

	// A null field clears it, and an absent field keeps its value.
	if got := e.api(t, http.MethodPut, "/api/v1/ratings/"+matchedAlbum, s, `{"like_level":null}`, &r); got != http.StatusOK {
		t.Fatalf("PUT null like level: %d", got)
	}
	if r.HalfStars == nil || *r.HalfStars != 7 || r.LikeLevel != nil {
		t.Fatalf("PUT answered %+v, want 7 half stars and no like level", r)
	}
	if got := value(e.local(t, "release", releaseMBID)); got != "7/nil" {
		t.Fatalf("local rating %s, want 7/nil", got)
	}
	if e.nd.count() != 4 || !strings.Contains(e.nd.calls[2], "rating=4") || !strings.HasPrefix(e.nd.calls[3], "unstar?") {
		t.Fatalf("Navidrome calls %v", e.nd.calls)
	}

	for name, c := range map[string]struct {
		id, body string
		want     int
	}{
		"unknown id":     {"nope", `{"half_stars":3}`, http.StatusNotFound},
		"no MBID":        {unmatchedAlbum, `{"half_stars":3}`, http.StatusUnprocessableEntity},
		"not JSON":       {matchedAlbum, `half_stars=3`, http.StatusBadRequest},
		"too many stars": {matchedAlbum, `{"half_stars":11}`, http.StatusBadRequest},
		"half of a half": {matchedAlbum, `{"half_stars":2.5}`, http.StatusBadRequest},
		"bad like level": {matchedAlbum, `{"like_level":"adore"}`, http.StatusBadRequest},
		"no field":       {matchedAlbum, `{}`, http.StatusBadRequest},
		"unknown field":  {matchedAlbum, `{"stars":3}`, http.StatusBadRequest},
	} {
		if got := e.api(t, http.MethodPut, "/api/v1/ratings/"+c.id, s, c.body, nil); got != c.want {
			t.Errorf("%s: PUT answered %d, want %d", name, got, c.want)
		}
	}
	if got := value(e.local(t, "release", releaseMBID)); got != "7/nil" {
		t.Fatalf("local rating %s after refused requests, want 7/nil", got)
	}
}

func TestRatingAPIGetAndDelete(t *testing.T) {
	e := newRatingEnv(t)
	s := e.session(t)
	e.api(t, http.MethodPut, "/api/v1/ratings/"+matchedAlbum, s, `{"half_stars":8}`, nil)

	var out struct {
		Ratings map[string]apiRating `json:"ratings"`
	}
	path := "/api/v1/ratings?ids=" + matchedAlbum + "," + unmatchedAlbum + ",nope," + matchedSong
	if got := e.api(t, http.MethodGet, path, s, "", &out); got != http.StatusOK {
		t.Fatalf("GET: %d", got)
	}
	// The song has no stored rating, the other two ids do not resolve.
	if r, ok := out.Ratings[matchedAlbum]; len(out.Ratings) != 1 || !ok || *r.HalfStars != 8 || r.Kind != "release" || r.MBID != releaseMBID {
		t.Fatalf("GET answered %+v", out.Ratings)
	}

	if got := e.api(t, http.MethodGet, "/api/v1/ratings?ids=", s, "", nil); got != http.StatusBadRequest {
		t.Fatalf("GET without ids: %d, want 400", got)
	}
	ids := make([]string, 201)
	for i := range ids {
		ids[i] = "id" + string(rune('a'+i%26)) + strings.Repeat("x", i/26)
	}
	if got := e.api(t, http.MethodGet, "/api/v1/ratings?ids="+strings.Join(ids, ","), s, "", nil); got != http.StatusBadRequest {
		t.Fatalf("GET with 201 ids: %d, want 400", got)
	}

	before := e.nd.count()
	if got := e.api(t, http.MethodDelete, "/api/v1/ratings/"+matchedAlbum, s, "", nil); got != http.StatusNoContent {
		t.Fatalf("DELETE: %d, want 204", got)
	}
	if e.nd.count() != before+2 || !strings.Contains(e.nd.calls[before], "rating=0") || !strings.HasPrefix(e.nd.calls[before+1], "unstar?") {
		t.Fatalf("Navidrome calls %v", e.nd.calls)
	}
	if got := value(e.local(t, "release", releaseMBID)); got != "nil/nil" {
		t.Fatalf("local rating %s after DELETE", got)
	}
	if got := value(e.remote(t, "release", releaseMBID)); got != "nil/nil" {
		t.Fatalf("wn-cloud rating %s after DELETE", got)
	}
	out.Ratings = nil
	e.api(t, http.MethodGet, path, s, "", &out)
	if len(out.Ratings) != 0 {
		t.Fatalf("GET after DELETE answered %+v", out.Ratings)
	}
}

func TestRatingAPIChanges(t *testing.T) {
	e := newRatingEnv(t)
	s := e.session(t)
	e.api(t, http.MethodPut, "/api/v1/ratings/"+matchedAlbum, s, `{"half_stars":8}`, nil)
	e.api(t, http.MethodPut, "/api/v1/ratings/"+matchedSong, s, `{"like_level":"like"}`, nil)

	type page struct {
		Changes []apiRating `json:"changes"`
		Next    string      `json:"next"`
	}
	get := func(since string) page {
		t.Helper()
		var p page
		if got := e.api(t, http.MethodGet, "/api/v1/ratings/changes?limit=1&since="+since, s, "", &p); got != http.StatusOK {
			t.Fatalf("GET changes since %q: %d", since, got)
		}
		return p
	}

	p1 := get("")
	if len(p1.Changes) != 1 || p1.Changes[0].Kind != "release" || p1.Next == "" {
		t.Fatalf("first page %+v", p1)
	}
	p2 := get(p1.Next)
	if len(p2.Changes) != 1 || p2.Changes[0].Kind != "track" || p2.Changes[0].LikeLevel == nil || p2.Next == p1.Next {
		t.Fatalf("second page %+v", p2)
	}
	p3 := get(p2.Next)
	if len(p3.Changes) != 0 || p3.Next != p2.Next {
		t.Fatalf("third page %+v, want no changes and the same cursor", p3)
	}

	// A cleared rating comes as a deletion after the cursor, and a feed that
	// starts from nothing leaves it out.
	e.api(t, http.MethodDelete, "/api/v1/ratings/"+matchedAlbum, s, "", nil)
	p4 := get(p2.Next)
	if len(p4.Changes) != 1 || !p4.Changes[0].Deleted || p4.Changes[0].HalfStars != nil || p4.Changes[0].MBID != releaseMBID {
		t.Fatalf("page after the delete %+v", p4)
	}
	var all page
	e.api(t, http.MethodGet, "/api/v1/ratings/changes", s, "", &all)
	if len(all.Changes) != 1 || all.Changes[0].Kind != "track" {
		t.Fatalf("feed from the start %+v, want only the track", all)
	}

	if got := e.api(t, http.MethodGet, "/api/v1/ratings/changes?since=garbage", s, "", nil); got != http.StatusBadRequest {
		t.Fatalf("malformed cursor: %d, want 400", got)
	}
	if got := e.api(t, http.MethodGet, "/api/v1/ratings/changes", "", "", nil); got != http.StatusUnauthorized {
		t.Fatalf("changes without a session: %d, want 401", got)
	}
}

// A listener's ratings are written to the listener's rows, as the
// listener's Navidrome user, and never reach the owner's rows or the outbox.
func TestListenerRatings(t *testing.T) {
	ctx := context.Background()
	e := newRatingEnv(t)
	owner := e.session(t)
	acct, err := e.st.CreateListener(ctx, "0192f1c4-0000-7000-8000-000000000001", "ada@example.com")
	if err != nil {
		t.Fatal(err)
	}
	e.nd.listener = acct.ID
	if _, err := e.st.ReplaceMembers(ctx, []store.Member{{UserID: "0192f1c4-0000-7000-8000-000000000001", Role: "listener"}}, time.Now()); err != nil {
		t.Fatal(err)
	}
	ls, _, err := e.st.CreateListenerSession(ctx, acct.ID, "test")
	if err != nil {
		t.Fatal(err)
	}
	row := func(account, kind, mbid string) string {
		t.Helper()
		r, err := e.st.Rating(ctx, account, kind, mbid)
		if errors.Is(err, store.ErrRatingNotFound) {
			return "none"
		}
		if err != nil {
			t.Fatal(err)
		}
		return value(r.HalfStars, r.LikeLevel)
	}
	outbox := func() int {
		t.Helper()
		box, err := e.st.Outbox(ctx)
		if err != nil {
			t.Fatal(err)
		}
		return len(box)
	}

	// The owner rates the album: one row, one outbox entry.
	e.call(t, "setRating.view?id="+matchedAlbum+"&rating=5")
	if got := row(e.owner.ID, "release", releaseMBID); got != "10/nil" || outbox() != 1 {
		t.Fatalf("owner's row %s, outbox %d; want 10/nil and 1", got, outbox())
	}

	// A listener's Subsonic calls change the listener's rows only.
	for _, q := range []string{"setRating.view?id=" + matchedAlbum + "&rating=2", "star.view?id=" + matchedSong} {
		resp, err := http.Get(e.srv.URL + "/rest/" + q + "&apiKey=" + ls + "&f=json")
		if err != nil {
			t.Fatal(err)
		}
		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		if !strings.Contains(string(body), `"status":"ok"`) {
			t.Fatalf("%s: %s", q, body)
		}
	}
	if got := row(acct.ID, "release", releaseMBID); got != "4/nil" {
		t.Fatalf("listener's album row %s, want 4/nil", got)
	}
	if got := row(acct.ID, "track", trackMBID); got != "nil/love" {
		t.Fatalf("listener's song row %s, want nil/love", got)
	}
	if got := row(e.owner.ID, "release", releaseMBID); got != "10/nil" {
		t.Fatalf("owner's album row %s after the listener's call, want 10/nil", got)
	}
	if got := row(e.owner.ID, "track", trackMBID); got != "none" {
		t.Fatalf("owner's song row %s after the listener's call, want none", got)
	}
	if outbox() != 1 {
		t.Fatalf("outbox has %d entries after the listener's calls, want 1", outbox())
	}
	for i, u := range e.nd.users[1:] {
		if u != acct.ID {
			t.Fatalf("Navidrome call %d ran as %q, want the listener %q", i+1, u, acct.ID)
		}
	}

	// The rating API answers the caller's rows.
	var got struct {
		Ratings map[string]apiRating `json:"ratings"`
	}
	ids := "/api/v1/ratings?ids=" + matchedAlbum + "," + matchedSong
	if code := e.api(t, http.MethodGet, ids, ls, "", &got); code != http.StatusOK || len(got.Ratings) != 2 ||
		*got.Ratings[matchedAlbum].HalfStars != 4 || *got.Ratings[matchedSong].LikeLevel != "love" {
		t.Fatalf("listener GET: %d %+v", code, got.Ratings)
	}
	got.Ratings = nil
	if code := e.api(t, http.MethodGet, ids, owner, "", &got); code != http.StatusOK || len(got.Ratings) != 1 || *got.Ratings[matchedAlbum].HalfStars != 10 {
		t.Fatalf("owner GET: %d %+v", code, got.Ratings)
	}
	var feed struct {
		Changes []apiRating `json:"changes"`
	}
	if code := e.api(t, http.MethodGet, "/api/v1/ratings/changes", ls, "", &feed); code != http.StatusOK || len(feed.Changes) != 2 {
		t.Fatalf("listener changes: %d %+v", code, feed.Changes)
	}
	for _, c := range feed.Changes {
		if c.HalfStars != nil && *c.HalfStars == 10 {
			t.Fatalf("listener's feed holds the owner's row: %+v", c)
		}
	}

	// PUT and DELETE write the listener's rows and queue nothing.
	if code := e.api(t, http.MethodPut, "/api/v1/ratings/"+matchedAlbum, ls, `{"half_stars":6}`, nil); code != http.StatusOK {
		t.Fatalf("listener PUT: %d", code)
	}
	if code := e.api(t, http.MethodDelete, "/api/v1/ratings/"+matchedSong, ls, "", nil); code != http.StatusNoContent {
		t.Fatalf("listener DELETE: %d", code)
	}
	if a, s := row(acct.ID, "release", releaseMBID), row(acct.ID, "track", trackMBID); a != "6/nil" || s != "none" {
		t.Fatalf("listener's rows %s and %s, want 6/nil and none", a, s)
	}
	if got := row(e.owner.ID, "release", releaseMBID); got != "10/nil" || outbox() != 1 {
		t.Fatalf("owner's row %s, outbox %d after the listener's PUT and DELETE; want 10/nil and 1", got, outbox())
	}
}
