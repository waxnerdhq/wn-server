package ratings

import (
	"context"
	"errors"
	"fmt"
	"strings"

	"github.com/waxnerdhq/wn-server/internal/store"
)

// Patch is a change to a rating from a first-party client. A field whose Set
// flag is false keeps its current value. A set field with a nil value
// clears it.
type Patch struct {
	SetHalfStars bool
	HalfStars    *int
	SetLikeLevel bool
	LikeLevel    *string
}

// Patch applies p to the account's rating of a subject, writes the result
// to the local store and the outbox, and returns the rating as stored. A
// rating that ends with neither value is cleared.
func (s *Service) Patch(ctx context.Context, accountID string, sub Subject, p Patch) (store.Rating, error) {
	return s.update(ctx, accountID, sub, p.HalfStars, p.LikeLevel, func(r *store.Rating) {
		if p.SetHalfStars {
			r.HalfStars = p.HalfStars
		}
		if p.SetLikeLevel {
			r.LikeLevel = p.LikeLevel
		}
	})
}

// Clear clears the account's rating of a subject and returns the cleared
// rating with its update time.
func (s *Service) Clear(ctx context.Context, accountID string, sub Subject) (store.Rating, error) {
	return s.Patch(ctx, accountID, sub, Patch{SetHalfStars: true, SetLikeLevel: true})
}

// Get returns the account's rating of a subject, or store.ErrRatingNotFound.
func (s *Service) Get(ctx context.Context, accountID string, sub Subject) (store.Rating, error) {
	return s.store.Rating(ctx, accountID, sub.Kind, strings.ToLower(sub.MBID))
}

// SetMirror sets Navidrome's rounded rating and starred flag of the song or
// album id, as the Navidrome user accountID, to the mirror of the given
// values. Both nil sets rating 0 and unstars.
func (s *Service) SetMirror(ctx context.Context, accountID, id string, halfStars *int, likeLevel *string) error {
	stars, starred := Mirror(halfStars, likeLevel)
	if err := s.nd.SetRating(ctx, accountID, id, stars); err != nil {
		return fmt.Errorf("set rating in navidrome: %w", err)
	}
	if err := s.nd.SetStarred(ctx, accountID, id, starred); err != nil {
		return fmt.Errorf("set starred in navidrome: %w", err)
	}
	return nil
}

// Changes returns up to limit rating rows of the account that changed after
// the cursor since, oldest first, with the cursor after the last one. An
// empty since starts at the beginning and leaves out cleared ratings. next
// equals since when there are no changes. It returns store.ErrBadCursor for a
// malformed since.
func (s *Service) Changes(ctx context.Context, accountID, since string, limit int) (changes []store.RatingChange, next string, err error) {
	var after *store.ChangeCursor
	if since != "" {
		c, err := store.ParseChangeCursor(since)
		if err != nil {
			return nil, "", err
		}
		after = &c
	}
	changes, err = s.store.RatingChanges(ctx, accountID, after, limit)
	if err != nil {
		return nil, "", err
	}
	next = since
	if len(changes) > 0 {
		next = changes[len(changes)-1].Cursor().String()
	}
	return changes, next, nil
}

// IsNotRated reports whether err says that the account has no rating of a
// subject.
func IsNotRated(err error) bool { return errors.Is(err, store.ErrRatingNotFound) }
