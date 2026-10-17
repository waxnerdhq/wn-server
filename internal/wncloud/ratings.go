package wncloud

import (
	"context"
	"net/http"
	"net/url"
	"strconv"
	"time"
)

// Rating is the caller's rating of one MusicBrainz subject at wn-cloud
// (docs/wn-cloud-api.md, Ratings). HalfStars is 1 to 10 or nil. LikeLevel is
// "like", "love", or nil.
type Rating struct {
	ID        string    `json:"id"`
	Kind      string    `json:"kind"`
	MBID      string    `json:"mbid"`
	HalfStars *int      `json:"half_stars"`
	LikeLevel *string   `json:"like_level"`
	CreatedAt time.Time `json:"created_at"`
	UpdatedAt time.Time `json:"updated_at"`
	// Deleted is set only on entries of GET /ratings?since=.
	Deleted bool `json:"deleted"`
}

// RatingChanges is one page of GET /ratings?since=.
type RatingChanges struct {
	Changes []Rating `json:"changes"`
	// Next is the cursor after the last change. It is empty when the
	// request had no cursor and there were no changes.
	Next string `json:"next"`
	More bool   `json:"more"`
}

func ratingPath(kind, mbid string) string {
	return "/ratings/" + url.PathEscape(kind) + "/" + url.PathEscape(mbid)
}

// PutRating sets the caller's rating of a subject (PUT /ratings/:kind/:mbid).
// Both values replace the stored ones. Both nil is refused by wn-cloud with
// ErrInvalidRequest; use DeleteRating.
func (c *Client) PutRating(ctx context.Context, token, kind, mbid string, halfStars *int, likeLevel *string) (Rating, error) {
	body := struct {
		HalfStars *int    `json:"half_stars"`
		LikeLevel *string `json:"like_level"`
	}{halfStars, likeLevel}
	var out Rating
	err := c.do(ctx, "PUT /ratings/:kind/:mbid", http.MethodPut, ratingPath(kind, mbid), token, body, http.StatusOK, &out)
	return out, err
}

// DeleteRating clears the caller's rating of a subject
// (DELETE /ratings/:kind/:mbid). Clearing a missing rating succeeds.
func (c *Client) DeleteRating(ctx context.Context, token, kind, mbid string) error {
	return c.do(ctx, "DELETE /ratings/:kind/:mbid", http.MethodDelete, ratingPath(kind, mbid), token, nil, http.StatusNoContent, nil)
}

// GetRating returns the caller's rating of a subject
// (GET /ratings/:kind/:mbid). A missing rating gives ErrNotFound.
func (c *Client) GetRating(ctx context.Context, token, kind, mbid string) (Rating, error) {
	var out Rating
	err := c.do(ctx, "GET /ratings/:kind/:mbid", http.MethodGet, ratingPath(kind, mbid), token, nil, http.StatusOK, &out)
	return out, err
}

// ListRatings returns the caller's rating changes after the cursor since,
// oldest first (GET /ratings?since=). An empty since starts from the
// beginning. limit 0 uses wn-cloud's default.
func (c *Client) ListRatings(ctx context.Context, token, since string, limit int) (RatingChanges, error) {
	q := url.Values{}
	if since != "" {
		q.Set("since", since)
	}
	if limit > 0 {
		q.Set("limit", strconv.Itoa(limit))
	}
	path := "/ratings"
	if len(q) > 0 {
		path += "?" + q.Encode()
	}
	var out struct {
		Changes []Rating `json:"changes"`
		Next    *string  `json:"next"`
		More    bool     `json:"more"`
	}
	if err := c.do(ctx, "GET /ratings", http.MethodGet, path, token, nil, http.StatusOK, &out); err != nil {
		return RatingChanges{}, err
	}
	page := RatingChanges{Changes: out.Changes, More: out.More}
	if out.Next != nil {
		page.Next = *out.Next
	}
	return page, nil
}
