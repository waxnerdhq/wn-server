package navidrome

import (
	"context"
	"encoding/json"
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
			var f map[string]string
			if err := json.Unmarshal([]byte(r.URL.Query().Get("filter")), &f); err != nil {
				http.Error(w, err.Error(), http.StatusBadRequest)
				return
			}
			field := map[string]string{"/api/album": "mbz_album_id", "/api/song": "mbz_release_track_id"}[r.URL.Path]
			if f[field] != mbid || r.URL.Query().Get("_end") == "" {
				http.Error(w, "bad filter", http.StatusBadRequest)
				return
			}
			_, _ = io.WriteString(w, `[{"id":"a"},{"id":"b"}]`)
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
		ids, err := f(ctx, "acct", mbid)
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
