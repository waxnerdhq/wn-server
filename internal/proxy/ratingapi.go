package proxy

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/waxnerdhq/wn-server/internal/ratings"
	"github.com/waxnerdhq/wn-server/internal/store"
)

// Limits of the rating API.
const (
	maxRatingIDs          = 200
	maxRatingRequestBytes = 4 << 10
	defaultChangesLimit   = 200
	maxChangesLimit       = 500
)

// ratingAPI serves the rating routes under /api/v1/ratings. Every route
// needs a session and resolves Subsonic ids through Navidrome, so the routes
// wait for the supervisor to report ready.
type ratingAPI struct {
	ratings Ratings
	logger  *slog.Logger
}

// apiTimeLayout is the store's time layout: UTC with six fractional digits.
const apiTimeLayout = "2006-01-02T15:04:05.000000Z"

// apiTime marshals as UTC truncated to microseconds, the string the store
// writes and wn-cloud answers, whatever zone the time carries.
type apiTime time.Time

func (t apiTime) MarshalJSON() ([]byte, error) {
	return []byte(`"` + time.Time(t).UTC().Format(apiTimeLayout) + `"`), nil
}

// ratingBody is a rating as the API answers it.
type ratingBody struct {
	Kind      string  `json:"kind"`
	MBID      string  `json:"mbid"`
	HalfStars *int    `json:"half_stars"`
	LikeLevel *string `json:"like_level"`
	UpdatedAt apiTime `json:"updated_at"`
}

func bodyOf(r store.Rating) ratingBody {
	return ratingBody{Kind: r.Kind, MBID: r.MBID, HalfStars: r.HalfStars, LikeLevel: r.LikeLevel, UpdatedAt: apiTime(r.UpdatedAt)}
}

// mount adds the rating routes to mux. wrap puts the session check and the
// ready gate around a handler.
func (a *ratingAPI) mount(mux *http.ServeMux, wrap func(http.Handler) http.Handler) {
	mux.Handle("PUT /api/v1/ratings/{id}", wrap(http.HandlerFunc(a.put)))
	mux.Handle("DELETE /api/v1/ratings/{id}", wrap(http.HandlerFunc(a.delete)))
	mux.Handle("GET /api/v1/ratings", wrap(http.HandlerFunc(a.get)))
	mux.Handle("GET /api/v1/ratings/changes", wrap(http.HandlerFunc(a.changes)))
}

// gateReadyJSON answers 503 with an API error until ready is closed.
func gateReadyJSON(ready <-chan struct{}) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			select {
			case <-ready:
				next.ServeHTTP(w, r)
			default:
				w.Header().Set("Retry-After", "5")
				writeAPIError(w, http.StatusServiceUnavailable, "starting", "wn-server is starting; try again shortly")
			}
		})
	}
}

// resolve maps a Subsonic id to its subject. It writes the error answer and
// returns false when it cannot.
func (a *ratingAPI) resolve(w http.ResponseWriter, r *http.Request, accountID, id string) (ratings.Subject, bool) {
	sub, err := a.ratings.Resolve(r.Context(), accountID, id)
	switch {
	case err == nil:
		return sub, true
	case r.Context().Err() != nil:
	case errors.Is(err, ratings.ErrUnknownItem):
		writeAPIError(w, http.StatusNotFound, "not_found", "no song or album has this id")
	case errors.Is(err, ratings.ErrNotSynced):
		writeAPIError(w, http.StatusUnprocessableEntity, "no_mbid", "the item has no MusicBrainz id, so its rating is not synced")
	default:
		a.logger.Warn("rating api: look up the item in Navidrome failed", "id", id, "error", err)
		writeAPIError(w, http.StatusBadGateway, "navidrome_error", "Navidrome could not be asked")
	}
	return ratings.Subject{}, false
}

// ratingRequest is the body of PUT /api/v1/ratings/{subsonicId}. A field that
// is absent keeps its value, and a field that is null clears it.
type ratingRequest struct {
	HalfStars json.RawMessage `json:"half_stars"`
	LikeLevel json.RawMessage `json:"like_level"`
}

// parsePatch reads and checks the body of a PUT.
func parsePatch(w http.ResponseWriter, r *http.Request) (ratings.Patch, string) {
	var req ratingRequest
	dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, maxRatingRequestBytes))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&req); err != nil {
		return ratings.Patch{}, "the body is not valid JSON with half_stars and like_level"
	}
	if _, err := dec.Token(); !errors.Is(err, io.EOF) {
		return ratings.Patch{}, "the body has data after the JSON object"
	}
	var p ratings.Patch
	if req.HalfStars != nil {
		p.SetHalfStars = true
		if !isNull(req.HalfStars) {
			var n int
			if err := json.Unmarshal(req.HalfStars, &n); err != nil || n < 1 || n > 10 {
				return ratings.Patch{}, "half_stars must be a whole number from 1 to 10, or null"
			}
			p.HalfStars = &n
		}
	}
	if req.LikeLevel != nil {
		p.SetLikeLevel = true
		if !isNull(req.LikeLevel) {
			var l string
			if err := json.Unmarshal(req.LikeLevel, &l); err != nil || (l != ratings.LikeLike && l != ratings.LikeLove) {
				return ratings.Patch{}, `like_level must be "like", "love", or null`
			}
			p.LikeLevel = &l
		}
	}
	if !p.SetHalfStars && !p.SetLikeLevel {
		return ratings.Patch{}, "set half_stars or like_level"
	}
	return p, ""
}

func isNull(raw json.RawMessage) bool { return bytes.Equal(bytes.TrimSpace(raw), []byte("null")) }

// put serves PUT /api/v1/ratings/{subsonicId}: it changes the rating in the
// local store and the outbox, then sets Navidrome's rounded mirror.
func (a *ratingAPI) put(w http.ResponseWriter, r *http.Request) {
	accountID, _ := r.Context().Value(accountIDKey{}).(string)
	id := r.PathValue("id")
	p, msg := parsePatch(w, r)
	if msg != "" {
		writeAPIError(w, http.StatusBadRequest, "invalid_request", msg)
		return
	}
	sub, ok := a.resolve(w, r, accountID, id)
	if !ok {
		return
	}
	// The rating is stored once it is resolved, also when the client goes away.
	ctx := context.WithoutCancel(r.Context())
	stored, err := a.ratings.Patch(ctx, accountID, sub, p)
	if err != nil {
		a.logger.Error("rating api: store rating", "kind", sub.Kind, "mbid", sub.MBID, "error", err)
		writeAPIError(w, http.StatusInternalServerError, "internal", "the rating could not be stored")
		return
	}
	a.mirror(ctx, accountID, id, sub, stored.HalfStars, stored.LikeLevel)
	writeJSON(w, http.StatusOK, bodyOf(stored))
}

// delete serves DELETE /api/v1/ratings/{subsonicId}: it clears both values
// and sets Navidrome to rating 0, unstarred.
func (a *ratingAPI) delete(w http.ResponseWriter, r *http.Request) {
	accountID, _ := r.Context().Value(accountIDKey{}).(string)
	id := r.PathValue("id")
	sub, ok := a.resolve(w, r, accountID, id)
	if !ok {
		return
	}
	ctx := context.WithoutCancel(r.Context())
	if _, err := a.ratings.Clear(ctx, accountID, sub); err != nil {
		a.logger.Error("rating api: clear rating", "kind", sub.Kind, "mbid", sub.MBID, "error", err)
		writeAPIError(w, http.StatusInternalServerError, "internal", "the rating could not be cleared")
		return
	}
	a.mirror(ctx, accountID, id, sub, nil, nil)
	w.WriteHeader(http.StatusNoContent)
}

// mirror sets Navidrome's rounded values. The rating is already stored and
// queued for wn-cloud, so a failure is logged and the client still gets its
// answer.
func (a *ratingAPI) mirror(ctx context.Context, accountID, id string, sub ratings.Subject, half *int, like *string) {
	if err := a.ratings.SetMirror(ctx, accountID, id, half, like); err != nil {
		a.logger.Warn("rating api: the rating is stored; setting Navidrome's rounded rating failed",
			"kind", sub.Kind, "mbid", sub.MBID, "id", id, "error", err)
	}
}

// get serves GET /api/v1/ratings?ids=: the stored ratings of the given
// Subsonic ids, keyed by id. Ids that do not resolve and ids without a stored
// rating are left out.
func (a *ratingAPI) get(w http.ResponseWriter, r *http.Request) {
	accountID, _ := r.Context().Value(accountIDKey{}).(string)
	var ids []string
	seen := map[string]bool{}
	for _, id := range strings.Split(r.URL.Query().Get("ids"), ",") {
		if id = strings.TrimSpace(id); id != "" && !seen[id] {
			seen[id] = true
			ids = append(ids, id)
		}
	}
	switch {
	case len(ids) == 0:
		writeAPIError(w, http.StatusBadRequest, "invalid_request", "ids must list Subsonic ids, separated by commas")
		return
	case len(ids) > maxRatingIDs:
		writeAPIError(w, http.StatusBadRequest, "invalid_request", "at most "+strconv.Itoa(maxRatingIDs)+" ids per call")
		return
	}
	out := map[string]ratingBody{}
	for _, id := range ids {
		sub, err := a.ratings.Resolve(r.Context(), accountID, id)
		switch {
		case errors.Is(err, ratings.ErrUnknownItem), errors.Is(err, ratings.ErrNotSynced):
			continue
		case r.Context().Err() != nil:
			return
		case err != nil:
			a.logger.Warn("rating api: look up the item in Navidrome failed", "id", id, "error", err)
			writeAPIError(w, http.StatusBadGateway, "navidrome_error", "Navidrome could not be asked")
			return
		}
		rt, err := a.ratings.Get(r.Context(), accountID, sub)
		switch {
		case ratings.IsNotRated(err):
			continue
		case r.Context().Err() != nil:
			return
		case err != nil:
			a.logger.Error("rating api: read rating", "kind", sub.Kind, "mbid", sub.MBID, "error", err)
			writeAPIError(w, http.StatusInternalServerError, "internal", "the rating could not be read")
			return
		}
		out[id] = bodyOf(rt)
	}
	writeJSON(w, http.StatusOK, struct {
		Ratings map[string]ratingBody `json:"ratings"`
	}{out})
}

// changeBody is a row of GET /api/v1/ratings/changes.
type changeBody struct {
	Kind      string  `json:"kind"`
	MBID      string  `json:"mbid"`
	HalfStars *int    `json:"half_stars"`
	LikeLevel *string `json:"like_level"`
	Deleted   bool    `json:"deleted"`
	UpdatedAt apiTime `json:"updated_at"`
}

// changes serves GET /api/v1/ratings/changes?since=: the local rating rows
// changed after the cursor, oldest first, and the cursor after the last.
func (a *ratingAPI) changes(w http.ResponseWriter, r *http.Request) {
	accountID, _ := r.Context().Value(accountIDKey{}).(string)
	q := r.URL.Query()
	limit := defaultChangesLimit
	if v := q.Get("limit"); v != "" {
		n, err := strconv.Atoi(v)
		if err != nil || n < 1 || n > maxChangesLimit {
			writeAPIError(w, http.StatusBadRequest, "invalid_request", "limit must be from 1 to "+strconv.Itoa(maxChangesLimit))
			return
		}
		limit = n
	}
	rows, next, err := a.ratings.Changes(r.Context(), accountID, q.Get("since"), limit)
	switch {
	case errors.Is(err, store.ErrBadCursor):
		writeAPIError(w, http.StatusBadRequest, "invalid_request", "since is not a cursor from this server")
		return
	case r.Context().Err() != nil:
		return
	case err != nil:
		a.logger.Error("rating api: read changes", "error", err)
		writeAPIError(w, http.StatusInternalServerError, "internal", "the changes could not be read")
		return
	}
	changes := make([]changeBody, 0, len(rows))
	for _, c := range rows {
		changes = append(changes, changeBody{
			Kind: c.Kind, MBID: c.MBID, HalfStars: c.HalfStars, LikeLevel: c.LikeLevel, Deleted: c.Deleted, UpdatedAt: apiTime(c.UpdatedAt),
		})
	}
	writeJSON(w, http.StatusOK, struct {
		Changes []changeBody `json:"changes"`
		Next    string       `json:"next"`
	}{changes, next})
}
