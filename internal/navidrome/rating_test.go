package navidrome

import (
	"context"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
)

func TestLookupByMBIDAndRatingCalls(t *testing.T) {
	const mbid = "6e12a11e-0ef9-49f3-b646-66929507d915"
	var rest []string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get(userHeader) != "acct" {
			http.Error(w, "wrong user", http.StatusUnauthorized)
			return
		}
		switch r.URL.Path {
		case "/api/album", "/api/song":
			field, jsonField := "mbz_album_id", "mbzAlbumId"
			if r.URL.Path == "/api/song" {
				field, jsonField = "mbz_release_track_id", "mbzReleaseTrackId"
			}
			if r.URL.Query().Get(field) != mbid || r.URL.Query().Get("_end") == "" || r.URL.Query().Has("filter") {
				http.Error(w, "bad filter", http.StatusBadRequest)
				return
			}
			_, _ = io.WriteString(w, `[{"id":"a","`+jsonField+`":"`+mbid+`"},{"id":"b","`+jsonField+`":"`+strings.ToUpper(mbid)+`"}]`)
		default:
			rest = append(rest, strings.TrimPrefix(r.URL.Path, "/rest/")+"?"+r.URL.Query().Encode())
			if r.URL.Query().Get("id") == "refused" {
				_, _ = io.WriteString(w, `{"subsonic-response":{"status":"failed","error":{"code":70,"message":"gone"}}}`)
				return
			}
			_, _ = io.WriteString(w, `{"subsonic-response":{"status":"ok"}}`)
		}
	}))
	defer srv.Close()
	c := newClient(srv.URL, Identity)
	ctx := context.Background()

	for name, f := range map[string]func(context.Context, string, string) ([]string, error){"albums": c.AlbumsByMBID, "songs": c.SongsByMBID} {
		ids, err := f(ctx, "acct", strings.ToUpper(mbid))
		if err != nil || !reflect.DeepEqual(ids, []string{"a", "b"}) {
			t.Errorf("%s by MBID = %v, %v", name, ids, err)
		}
	}

	if err := c.SetRating(ctx, "acct", "a", 4); err != nil {
		t.Fatal(err)
	}
	if err := c.SetStarred(ctx, "acct", "a", true); err != nil {
		t.Fatal(err)
	}
	if err := c.SetStarred(ctx, "acct", "a", false); err != nil {
		t.Fatal(err)
	}
	want := []string{"setRating?c=wn-server&f=json&id=a&rating=4&v=1.16.1", "star?c=wn-server&f=json&id=a&v=1.16.1", "unstar?c=wn-server&f=json&id=a&v=1.16.1"}
	if !reflect.DeepEqual(rest, want) {
		t.Errorf("Subsonic calls %v, want %v", rest, want)
	}
	if err := c.SetRating(ctx, "acct", "refused", 1); err == nil {
		t.Error("a Subsonic error was not returned")
	}
}

// A server that ignores the lookup parameter answers with items of other
// MusicBrainz ids. The lookup returns ErrFilterIgnored and no ids.
func TestLookupByMBIDDetectsIgnoredFilter(t *testing.T) {
	const mbid = "6e12a11e-0ef9-49f3-b646-66929507d915"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		field := "mbzAlbumId"
		if r.URL.Path == "/api/song" {
			field = "mbzReleaseTrackId"
		}
		_, _ = io.WriteString(w, `[{"id":"a","`+field+`":"`+mbid+`"},{"id":"b","`+field+`":"`+mbid+`"},{"id":"c","`+field+`":"11111111-0ef9-49f3-b646-66929507d915"}]`)
	}))
	defer srv.Close()
	c := newClient(srv.URL, Identity)

	for name, f := range map[string]func(context.Context, string, string) ([]string, error){"albums": c.AlbumsByMBID, "songs": c.SongsByMBID} {
		ids, err := f(context.Background(), "acct", mbid)
		if !errors.Is(err, ErrFilterIgnored) || len(ids) != 0 {
			t.Errorf("%s by MBID = %v, %v; want ErrFilterIgnored", name, ids, err)
		}
	}
}
