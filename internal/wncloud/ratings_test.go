package wncloud

import (
	"context"
	"errors"
	"net/http/httptest"
	"testing"

	"github.com/waxnerdhq/wn-server/internal/wncloud/stub"
)

func TestRatingRoutes(t *testing.T) {
	ctx := context.Background()
	s := stub.New()
	s.AddUser(ownerID, "owner@example.com")
	s.AddToken("wn_owner", ownerID)
	srv := httptest.NewServer(s)
	defer srv.Close()
	c := New(srv.URL, "test")
	const mbid = "6e12a11e-0ef9-49f3-b646-66929507d915"

	nine, love := 9, "love"
	got, err := c.PutRating(ctx, "wn_owner", "release", mbid, &nine, &love)
	if err != nil || got.HalfStars == nil || *got.HalfStars != 9 || got.LikeLevel == nil || *got.LikeLevel != "love" {
		t.Fatalf("PutRating = %+v, %v", got, err)
	}
	if got, err := c.GetRating(ctx, "wn_owner", "release", mbid); err != nil || got.ID == "" {
		t.Fatalf("GetRating = %+v, %v", got, err)
	}
	page, err := c.ListRatings(ctx, "wn_owner", "", 0)
	if err != nil || len(page.Changes) != 1 || page.Next == "" || page.More {
		t.Fatalf("ListRatings = %+v, %v", page, err)
	}
	if _, err := c.PutRating(ctx, "wn_owner", "release", mbid, nil, nil); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("PutRating(nil, nil) = %v, want ErrInvalidRequest", err)
	}
	if err := c.DeleteRating(ctx, "wn_owner", "release", mbid); err != nil {
		t.Fatal(err)
	}
	if _, err := c.GetRating(ctx, "wn_owner", "release", mbid); !errors.Is(err, ErrNotFound) {
		t.Fatalf("GetRating after delete = %v, want ErrNotFound", err)
	}
	next, err := c.ListRatings(ctx, "wn_owner", page.Next, 10)
	if err != nil || len(next.Changes) != 1 || !next.Changes[0].Deleted {
		t.Fatalf("ListRatings since = %+v, %v", next, err)
	}
	if err := c.DeleteRating(ctx, "wn_revoked", "release", mbid); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("DeleteRating with an unknown token = %v, want ErrUnauthorized", err)
	}
}
