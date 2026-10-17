package ratings

import (
	"context"
	"errors"
	"net/http"
	"strings"
	"time"

	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

// credentialAdvice tells the owner what to do when wn-cloud refuses the
// server credential.
const credentialAdvice = "run `wn-server logout`, then `wn-server login`"

// ReconcileResult counts what one run of Reconcile did.
type ReconcileResult struct {
	Pages int
	// Pulled is the number of changes wn-cloud reported.
	Pulled int
	// Applied is the number of changes that altered the local store.
	Applied int
	// Unchanged is the number of changes that the local store already held.
	// They are pushed into Navidrome as well.
	Unchanged int
	// KeptLocal is the number of changes that a newer outbox entry of the
	// subject overruled.
	KeptLocal int
	// Invalid is the number of changes that wn-server could not read.
	Invalid int
	// Pushed is the number of Navidrome items that got the rounded values.
	Pushed int
	// NotInLibrary is the number of applied changes whose subject the
	// library does not hold, or whose kind wn-server does not mirror.
	NotInLibrary int
	// PushFailed is the number of Navidrome lookups or items that failed.
	PushFailed int
}

// Reconcile brings the owner's rating changes at wn-cloud into the local
// store and into Navidrome. It calls GET /ratings?since= with the stored
// cursor and the server credential, follows the pages, and stores the cursor
// after each page. Each change overwrites the local values, unless an outbox
// entry of the subject is newer than the change. Applied changes are pushed
// into Navidrome as rounded stars and the starred flag, for the items that
// the library holds. Changes from wn-cloud do not go into the outbox.
// Without a server credential Reconcile does nothing. A failed call to
// wn-cloud is logged and recorded in the status, and Reconcile returns nil.
func (s *Service) Reconcile(ctx context.Context) error {
	cred, err := s.store.ServerCredential(ctx)
	if errors.Is(err, store.ErrNoServerCredential) {
		s.status.SetCredentialRejected(false)
		return nil
	}
	if err != nil {
		return err
	}
	owner, err := s.store.Owner(ctx)
	if err != nil {
		return err
	}
	cursor, err := s.store.RatingSyncCursor(ctx)
	if err != nil {
		return err
	}
	var res ReconcileResult
	for ctx.Err() == nil {
		page, err := s.cloud.ListRatings(ctx, cred.Token, cursor, 0)
		if err != nil {
			s.reconcileFailure(ctx, err)
			return nil
		}
		s.status.MarkReachable()
		s.status.SetCredentialRejected(false)
		res.Pages++
		for _, c := range page.Changes {
			if ctx.Err() != nil {
				return nil
			}
			if err := s.applyRemote(ctx, owner.ID, c, &res); err != nil {
				return err
			}
		}
		// A page that does not move the cursor ends the run, so a page that
		// repeats itself cannot loop.
		advanced := page.Next != "" && page.Next != cursor
		if advanced {
			if err := s.store.SetRatingSyncCursor(ctx, page.Next); err != nil {
				return err
			}
			cursor = page.Next
		}
		if !advanced || !page.More || len(page.Changes) == 0 {
			break
		}
	}
	if ctx.Err() != nil {
		return nil
	}
	s.logger.Info("rating sync finished", "pages", res.Pages, "pulled", res.Pulled, "applied", res.Applied,
		"unchanged", res.Unchanged, "kept_local", res.KeptLocal, "invalid", res.Invalid,
		"pushed", res.Pushed, "not_in_library", res.NotInLibrary, "push_failed", res.PushFailed)
	return nil
}

// reconcileFailure records and logs a failed GET /ratings?since=.
func (s *Service) reconcileFailure(ctx context.Context, err error) {
	if ctx.Err() != nil {
		return
	}
	switch {
	case errors.Is(err, wncloud.ErrUnreachable):
		s.status.MarkUnreachable(s.now())
		s.logUnreachable(err)
	case errors.Is(err, wncloud.ErrUnauthorized), httpStatus(err) == http.StatusForbidden:
		s.status.MarkReachable()
		s.status.SetCredentialRejected(true)
		s.logger.Error("wn-cloud refused the server credential for the rating sync; "+credentialAdvice, "error", err)
	case errors.Is(err, wncloud.ErrRateLimited):
		s.status.MarkReachable()
		s.logger.Warn("wn-cloud rate-limits the rating sync; it waits for the next run", "error", err)
	default:
		s.status.MarkReachable()
		s.logger.Warn("rating sync failed; it is retried at the next run", "error", err)
	}
}

// applyRemote writes one change from wn-cloud to the local store and, when
// it did not lose to a newer local write, pushes it into Navidrome. A returned error is a
// store failure.
func (s *Service) applyRemote(ctx context.Context, accountID string, c wncloud.Rating, res *ReconcileResult) error {
	res.Pulled++
	mbid := strings.ToLower(c.MBID)
	err := validate(c.Kind, mbid, c.HalfStars, c.LikeLevel)
	if err == nil && (c.UpdatedAt.IsZero() || (!c.Deleted && c.HalfStars == nil && c.LikeLevel == nil)) {
		err = errors.New("no update time, or no values on a change that is not a deletion")
	}
	if err != nil {
		s.logger.Warn("rating sync: skipped a change that wn-server cannot read", "kind", c.Kind, "mbid", c.MBID, "error", err)
		res.Invalid++
		return nil
	}
	r := store.Rating{AccountID: accountID, Kind: c.Kind, MBID: mbid, HalfStars: c.HalfStars, LikeLevel: c.LikeLevel, UpdatedAt: c.UpdatedAt}
	if c.Deleted {
		r.HalfStars, r.LikeLevel = nil, nil
	}
	s.mu.Lock()
	result, err := s.store.ApplyRemoteRating(ctx, accountID, r, c.Deleted)
	s.mu.Unlock()
	if err != nil {
		return err
	}
	switch result {
	case store.RemoteUnchanged:
		// The store holds the values, but Navidrome may not: an earlier push
		// can have failed or been interrupted, or a scan can have added the
		// item. Pushing again repairs the mirror.
		res.Unchanged++
		s.pushRemote(ctx, accountID, r, res)
	case store.RemoteKeptLocal:
		res.KeptLocal++
	case store.RemoteApplied:
		res.Applied++
		s.pushRemote(ctx, accountID, r, res)
	}
	return nil
}

// pushRemote sets the rounded values of r on every Navidrome item that holds
// the subject. The subject is looked up through Navidrome's native API by its
// MusicBrainz id. A subject that the library does not hold is skipped. A
// failure is logged and counted, and the run goes on.
func (s *Service) pushRemote(ctx context.Context, accountID string, r store.Rating, res *ReconcileResult) {
	var lookup func(context.Context, string, string) ([]string, error)
	switch {
	case s.nd == nil:
		res.NotInLibrary++
		return
	case r.Kind == KindRelease:
		lookup = s.nd.AlbumsByMBID
	case r.Kind == KindTrack:
		lookup = s.nd.SongsByMBID
	default:
		res.NotInLibrary++
		return
	}
	ids, err := lookup(ctx, accountID, r.MBID)
	if err != nil {
		if ctx.Err() == nil {
			s.logger.Warn("rating sync: look up the subject in Navidrome failed", "kind", r.Kind, "mbid", r.MBID, "error", err)
			res.PushFailed++
		}
		return
	}
	if len(ids) == 0 {
		res.NotInLibrary++
		return
	}
	for _, id := range ids {
		if err := s.SetMirror(ctx, accountID, id, r.HalfStars, r.LikeLevel); err != nil {
			if ctx.Err() == nil {
				s.logger.Warn("rating sync: set the rounded rating in Navidrome failed", "kind", r.Kind, "mbid", r.MBID, "id", id, "error", err)
				res.PushFailed++
			}
			continue
		}
		res.Pushed++
	}
}

// RunReconcile runs the reconcile loop until ctx is done. It waits for ready
// to close, because the loop writes into Navidrome. Then it drains the outbox
// and runs Reconcile, at once and every ReconcileInterval. The drain comes
// first, so that a pending local change reaches wn-cloud before the pull.
func (s *Service) RunReconcile(ctx context.Context, ready <-chan struct{}) {
	select {
	case <-ctx.Done():
		return
	case <-ready:
	}
	t := time.NewTicker(ReconcileInterval)
	defer t.Stop()
	for {
		if err := s.Drain(ctx); err != nil && ctx.Err() == nil {
			s.logger.Error("drain the rating outbox", "error", err)
		}
		if err := s.Reconcile(ctx); err != nil && ctx.Err() == nil {
			s.logger.Error("rating sync", "error", err)
		}
		select {
		case <-ctx.Done():
			return
		case <-t.C:
		}
	}
}
