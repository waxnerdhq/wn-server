package proxy

import (
	"bytes"
	"context"
	"encoding/json"
	"encoding/xml"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"net/url"
	"strconv"
	"strings"

	"github.com/waxnerdhq/wn-server/internal/ratings"
)

// Ratings records the rating changes of intercepted Subsonic calls.
// *ratings.Service implements it.
type Ratings interface {
	Resolve(ctx context.Context, accountID, id string) (ratings.Subject, error)
	SetHalfStars(ctx context.Context, accountID string, sub ratings.Subject, halfStars *int) error
	SetLike(ctx context.Context, accountID string, sub ratings.Subject, likeLevel *string) error
}

// maxRatingBody is the largest setRating, star, or unstar response that is
// read. Navidrome answers these calls with an empty Subsonic response.
const maxRatingBody = 64 << 10

type ratingPlanKey struct{}

// ratingChange is one change to record after Navidrome accepted the call.
// stars selects the half stars; otherwise the like level changes.
type ratingChange struct {
	sub       ratings.Subject
	stars     bool
	halfStars *int
	likeLevel *string
}

// ratingPlan holds the changes of one intercepted call.
type ratingPlan struct {
	accountID string
	changes   []ratingChange
	ratings   Ratings
	logger    *slog.Logger
}

// ratingCall returns "setRating", "star", or "unstar" when p is that call on
// /rest/, with or without the .view suffix, and "" otherwise.
func ratingCall(p string) string {
	switch c := strings.TrimSuffix(strings.TrimPrefix(p, "/rest/"), ".view"); c {
	case "setRating", "star", "unstar":
		return c
	}
	return ""
}

// interceptRatings resolves the items of setRating, star, and unstar to
// MusicBrainz subjects before the call is forwarded. recordRatings writes
// the changes once Navidrome has accepted the call. Items without a
// MusicBrainz id and failed lookups are forwarded without a record. A nil rt
// turns the intercept off.
func interceptRatings(rt Ratings, logger *slog.Logger, next http.Handler) http.Handler {
	if rt == nil {
		return next
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		call := ratingCall(r.URL.Path)
		accountID, _ := r.Context().Value(accountIDKey{}).(string)
		if call == "" || accountID == "" {
			next.ServeHTTP(w, r)
			return
		}
		changes := planRatings(r.Context(), rt, logger, accountID, call, r.URL.Query())
		if len(changes) > 0 {
			plan := &ratingPlan{accountID: accountID, changes: changes, ratings: rt, logger: logger}
			r = r.WithContext(context.WithValue(r.Context(), ratingPlanKey{}, plan))
		}
		next.ServeHTTP(w, r)
	})
}

// planRatings returns the changes that the call makes to synced subjects.
// setRating takes id and a rating from 0 to 5: 1 to 5 sets twice as many
// half stars, and 0 clears the stars. star and unstar take id and albumId,
// and set or clear the like level love. artistId is not synced.
func planRatings(ctx context.Context, rt Ratings, logger *slog.Logger, accountID, call string, q url.Values) []ratingChange {
	resolved := map[string]bool{}
	resolve := func(id string) (ratings.Subject, bool) {
		if id == "" || resolved[id] {
			return ratings.Subject{}, false
		}
		resolved[id] = true
		sub, err := rt.Resolve(ctx, accountID, id)
		switch {
		case err == nil:
			return sub, true
		case ctx.Err() != nil:
		case errors.Is(err, ratings.ErrNotSynced), errors.Is(err, ratings.ErrUnknownItem):
			logger.Debug("rating not synced", "call", call, "id", id, "reason", err)
		default:
			logger.Warn("rating lookup in Navidrome failed; the call is forwarded without a sync", "call", call, "id", id, "error", err)
		}
		return ratings.Subject{}, false
	}

	var changes []ratingChange
	switch call {
	case "setRating":
		n, err := strconv.Atoi(q.Get("rating"))
		if err != nil {
			return nil
		}
		half, ok := ratings.FromSubsonic(n)
		if !ok {
			return nil
		}
		if sub, ok := resolve(q.Get("id")); ok {
			changes = append(changes, ratingChange{sub: sub, stars: true, halfStars: half})
		}
	case "star", "unstar":
		var like *string
		if call == "star" {
			love := ratings.LikeLove
			like = &love
		}
		for _, id := range append(q["id"], q["albumId"]...) {
			if sub, ok := resolve(id); ok {
				changes = append(changes, ratingChange{sub: sub, likeLevel: like})
			}
		}
	}
	return changes
}

// recordRatings writes the planned changes when Navidrome answered the call
// with status ok. A Subsonic error or any other answer writes nothing. The
// client gets Navidrome's answer either way.
func recordRatings(resp *http.Response) error {
	plan, ok := resp.Request.Context().Value(ratingPlanKey{}).(*ratingPlan)
	if !ok || resp.StatusCode != http.StatusOK {
		return nil
	}
	if resp.Header.Get("Content-Encoding") != "" {
		plan.logger.Warn("rating call answered with a compressed body; not synced")
		return nil
	}
	orig := resp.Body
	body, err := io.ReadAll(io.LimitReader(orig, maxRatingBody+1))
	if err != nil {
		return err
	}
	if len(body) > maxRatingBody {
		resp.Body = struct {
			io.Reader
			io.Closer
		}{io.MultiReader(bytes.NewReader(body), orig), orig}
		return nil
	}
	_ = orig.Close()
	resp.Body = io.NopCloser(bytes.NewReader(body))
	if !subsonicOK(resp.Header.Get("Content-Type"), body) {
		return nil
	}
	// Navidrome has applied the call, so the change is recorded even when the
	// client goes away now.
	ctx := context.WithoutCancel(resp.Request.Context())
	for _, c := range plan.changes {
		var err error
		if c.stars {
			err = plan.ratings.SetHalfStars(ctx, plan.accountID, c.sub, c.halfStars)
		} else {
			err = plan.ratings.SetLike(ctx, plan.accountID, c.sub, c.likeLevel)
		}
		if err != nil {
			plan.logger.Error("record rating", "kind", c.sub.Kind, "mbid", c.sub.MBID, "error", err)
		}
	}
	return nil
}

// subsonicOK reports whether a Subsonic response body in JSON, JSONP, or XML
// has status ok.
func subsonicOK(contentType string, body []byte) bool {
	switch {
	case strings.Contains(contentType, "json"), strings.Contains(contentType, "javascript"):
		start, end := bytes.IndexByte(body, '{'), bytes.LastIndexByte(body, '}')
		if start < 0 || end < start {
			return false
		}
		var r struct {
			R struct {
				Status string `json:"status"`
			} `json:"subsonic-response"`
		}
		return json.Unmarshal(body[start:end+1], &r) == nil && r.R.Status == "ok"
	case strings.Contains(contentType, "xml"):
		var r struct {
			Status string `xml:"status,attr"`
		}
		return xml.Unmarshal(body, &r) == nil && r.Status == "ok"
	}
	return false
}
