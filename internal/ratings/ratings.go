package ratings

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"regexp"
	"strings"
	"sync"
	"time"

	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

// Subject kinds that wn-server syncs. wn-cloud also knows release_group and
// artist.
const (
	KindRelease = "release"
	KindTrack   = "track"
)

// Like levels.
const (
	LikeLike = "like"
	LikeLove = "love"
)

// Intervals of the outbox drain.
const (
	// DrainInterval is the time between two drains of the outbox.
	DrainInterval = time.Minute
	// firstBackoff is the wait after the first failed attempt of an entry.
	// Each further failure doubles it, up to maxBackoff.
	firstBackoff = time.Minute
	maxBackoff   = time.Hour
	// unreachableLogInterval is the least time between two log lines about
	// wn-cloud being unreachable.
	unreachableLogInterval = time.Minute
)

var (
	// ErrNotSynced is returned by Resolve for a song or album without a
	// MusicBrainz id. Such items are rated in Navidrome only.
	ErrNotSynced = errors.New("item has no MusicBrainz id")
	// ErrUnknownItem is returned by Resolve for an id that is neither a song
	// nor an album in Navidrome, such as an artist, a playlist, or an
	// unknown id.
	ErrUnknownItem = errors.New("id is not a song or an album")
)

var (
	kinds  = map[string]bool{"release_group": true, KindRelease: true, KindTrack: true, "artist": true}
	uuidRE = regexp.MustCompile(`^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$`)
)

// Subject is a MusicBrainz subject of a rating.
type Subject struct {
	Kind string
	MBID string // lower case
}

// Mirror returns the value that Navidrome holds for a rating: whole stars
// from 0 to 5, with half stars rounded up, and starred for love.
func Mirror(halfStars *int, likeLevel *string) (stars int, starred bool) {
	if halfStars != nil {
		stars = (*halfStars + 1) / 2
	}
	return stars, likeLevel != nil && *likeLevel == LikeLove
}

// FromSubsonic returns the half stars of a Subsonic rating from 1 to 5, or
// nil for 0, which clears the stars. ok is false outside 0 to 5.
func FromSubsonic(rating int) (halfStars *int, ok bool) {
	if rating < 0 || rating > 5 {
		return nil, false
	}
	if rating == 0 {
		return nil, true
	}
	h := rating * 2
	return &h, true
}

// Store is the part of the store that the service uses. *store.Store
// implements it.
type Store interface {
	Rating(ctx context.Context, accountID, kind, mbid string) (store.Rating, error)
	PutRating(ctx context.Context, r store.Rating) error
	ServerCredential(ctx context.Context) (store.ServerCredential, error)
	EnqueueRating(ctx context.Context, e store.OutboxEntry) error
	NextOutbox(ctx context.Context, now time.Time) (store.OutboxEntry, error)
	DeleteOutbox(ctx context.Context, id int64) error
	DeleteOutboxSubject(ctx context.Context, kind, mbid string) error
	FailOutbox(ctx context.Context, id int64, attempts int, nextAt time.Time, lastError string) error
}

// Cloud is the part of the wn-cloud client that the service uses.
// *wncloud.Client implements it.
type Cloud interface {
	PutRating(ctx context.Context, token, kind, mbid string, halfStars *int, likeLevel *string) (wncloud.Rating, error)
	DeleteRating(ctx context.Context, token, kind, mbid string) error
}

// Navidrome looks up songs and albums as a Navidrome user.
// *navidrome.Client implements it.
type Navidrome interface {
	Song(ctx context.Context, user, id string) (navidrome.Song, error)
	Album(ctx context.Context, user, id string) (navidrome.Album, error)
}

// Service records ratings. Writes to the store and to wn-cloud run one at a
// time, so a drained outbox entry never overtakes a newer write of the same
// subject.
type Service struct {
	store  Store
	cloud  Cloud
	nd     Navidrome
	status *wncloud.Status
	logger *slog.Logger
	now    func() time.Time

	mu sync.Mutex

	logMu   sync.Mutex
	lastLog time.Time
}

// New returns a service. status records whether wn-cloud answered and may be
// nil. A nil logger selects slog.Default.
func New(st Store, cloud Cloud, nd Navidrome, status *wncloud.Status, logger *slog.Logger) *Service {
	if logger == nil {
		logger = slog.Default()
	}
	return &Service{store: st, cloud: cloud, nd: nd, status: status, logger: logger, now: time.Now}
}

// Resolve maps a Navidrome song or album id to its subject, as the Navidrome
// user accountID. A song maps to a track through its release track id. An
// album maps to a release through its album id. It returns ErrNotSynced for
// an item without a MusicBrainz id, ErrUnknownItem for any other id, and
// another error when Navidrome cannot be asked.
func (s *Service) Resolve(ctx context.Context, accountID, id string) (Subject, error) {
	song, err := s.nd.Song(ctx, accountID, id)
	if err == nil {
		return subjectOf(KindTrack, song.MbzReleaseTrackID)
	}
	if !errors.Is(err, navidrome.ErrNotFound) {
		return Subject{}, err
	}
	album, err := s.nd.Album(ctx, accountID, id)
	if err == nil {
		return subjectOf(KindRelease, album.MbzAlbumID)
	}
	if errors.Is(err, navidrome.ErrNotFound) {
		return Subject{}, ErrUnknownItem
	}
	return Subject{}, err
}

func subjectOf(kind, mbid string) (Subject, error) {
	if !uuidRE.MatchString(mbid) {
		return Subject{}, ErrNotSynced
	}
	return Subject{Kind: kind, MBID: strings.ToLower(mbid)}, nil
}

// Set replaces the account's rating of a subject with both values. Both nil
// clears the rating. It writes the local store, then wn-cloud with the
// server credential. When wn-cloud is unreachable or refuses the write, the
// write goes to the outbox. Without a server credential nothing is sent. The
// returned error is about the local store only.
func (s *Service) Set(ctx context.Context, accountID, kind, mbid string, halfStars *int, likeLevel *string) error {
	if err := validate(kind, mbid, halfStars, likeLevel); err != nil {
		return err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	r := store.Rating{AccountID: accountID, Kind: kind, MBID: strings.ToLower(mbid), HalfStars: halfStars, LikeLevel: likeLevel}
	return s.write(ctx, r)
}

// Delete clears the account's rating of a subject, as Set with both values
// nil does.
func (s *Service) Delete(ctx context.Context, accountID, kind, mbid string) error {
	return s.Set(ctx, accountID, kind, mbid, nil, nil)
}

// SetHalfStars sets the half stars of a rating and keeps its like level.
// nil clears the stars.
func (s *Service) SetHalfStars(ctx context.Context, accountID string, sub Subject, halfStars *int) error {
	return s.update(ctx, accountID, sub, halfStars, nil, func(r *store.Rating) { r.HalfStars = halfStars })
}

// SetLike sets the like level of a rating and keeps its half stars. nil
// clears the like level.
func (s *Service) SetLike(ctx context.Context, accountID string, sub Subject, likeLevel *string) error {
	return s.update(ctx, accountID, sub, nil, likeLevel, func(r *store.Rating) { r.LikeLevel = likeLevel })
}

func (s *Service) update(ctx context.Context, accountID string, sub Subject, halfStars *int, likeLevel *string, change func(*store.Rating)) error {
	if err := validate(sub.Kind, sub.MBID, halfStars, likeLevel); err != nil {
		return err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	mbid := strings.ToLower(sub.MBID)
	r, err := s.store.Rating(ctx, accountID, sub.Kind, mbid)
	if errors.Is(err, store.ErrRatingNotFound) {
		r, err = store.Rating{AccountID: accountID, Kind: sub.Kind, MBID: mbid}, nil
	}
	if err != nil {
		return err
	}
	change(&r)
	r.UpdatedAt = time.Time{}
	return s.write(ctx, r)
}

func validate(kind, mbid string, halfStars *int, likeLevel *string) error {
	switch {
	case !kinds[kind]:
		return fmt.Errorf("rating: unknown kind %q", kind)
	case !uuidRE.MatchString(mbid):
		return fmt.Errorf("rating: mbid %q is not a UUID", mbid)
	case halfStars != nil && (*halfStars < 1 || *halfStars > 10):
		return fmt.Errorf("rating: half stars %d out of range", *halfStars)
	case likeLevel != nil && *likeLevel != LikeLike && *likeLevel != LikeLove:
		return fmt.Errorf("rating: unknown like level %q", *likeLevel)
	}
	return nil
}

// write stores r and sends it to wn-cloud. s.mu is held.
func (s *Service) write(ctx context.Context, r store.Rating) error {
	if r.UpdatedAt.IsZero() {
		r.UpdatedAt = s.now()
	}
	if err := s.store.PutRating(ctx, r); err != nil {
		return fmt.Errorf("store rating: %w", err)
	}
	cred, err := s.store.ServerCredential(ctx)
	if errors.Is(err, store.ErrNoServerCredential) {
		return nil // local-only mode
	}
	if err != nil {
		return err
	}
	now := s.now()
	e := store.OutboxEntry{
		Kind: r.Kind, MBID: r.MBID, HalfStars: r.HalfStars, LikeLevel: r.LikeLevel,
		Deleted: r.Cleared(), CreatedAt: now,
	}
	if _, down := s.status.UnreachableSince(); down {
		// Do not make the client wait for a timeout. The next drain sends it.
		e.NextAt = now
		return s.store.EnqueueRating(ctx, e)
	}
	sendErr := s.send(ctx, cred.Token, e)
	if sendErr == nil {
		return s.store.DeleteOutboxSubject(ctx, e.Kind, e.MBID)
	}
	e.Attempts, e.NextAt, e.LastError = s.failure(e, 1, sendErr)
	return s.store.EnqueueRating(ctx, e)
}

// send writes one entry to wn-cloud and records in the status whether
// wn-cloud answered.
func (s *Service) send(ctx context.Context, token string, e store.OutboxEntry) error {
	var err error
	if e.Deleted {
		err = s.cloud.DeleteRating(ctx, token, e.Kind, e.MBID)
	} else {
		_, err = s.cloud.PutRating(ctx, token, e.Kind, e.MBID, e.HalfStars, e.LikeLevel)
	}
	if errors.Is(err, wncloud.ErrUnreachable) {
		s.status.MarkUnreachable(s.now())
	} else {
		s.status.MarkReachable()
	}
	return err
}

// failure logs a failed write of e and returns the attempt count, the time
// of the next attempt, and the message to store. A zero time keeps the
// entry out of the retry loop.
func (s *Service) failure(e store.OutboxEntry, attempts int, err error) (int, time.Time, string) {
	msg := err.Error()
	switch {
	case errors.Is(err, wncloud.ErrUnreachable):
		s.logUnreachable(err)
	case errors.Is(err, wncloud.ErrInvalidRequest), errors.Is(err, wncloud.ErrForbidden):
		s.logger.Warn("wn-cloud rejected a rating write; it is not retried", "kind", e.Kind, "mbid", e.MBID, "error", err)
		return attempts, time.Time{}, msg
	case errors.Is(err, wncloud.ErrUnauthorized):
		s.logger.Error("wn-cloud refused the server credential; run wn-server logout, then wn-server login", "error", err)
	default:
		s.logger.Warn("rating write to wn-cloud failed; it is retried", "kind", e.Kind, "mbid", e.MBID, "error", err)
	}
	return attempts, s.now().Add(Backoff(attempts)), msg
}

// Backoff returns the wait after the given number of failed attempts: 1, 2,
// 4, ... minutes, at most one hour.
func Backoff(attempts int) time.Duration {
	d := firstBackoff
	for i := 1; i < attempts && d < maxBackoff; i++ {
		d *= 2
	}
	return min(d, maxBackoff)
}

// logUnreachable logs that wn-cloud is unreachable, at most once per
// unreachableLogInterval.
func (s *Service) logUnreachable(err error) {
	s.logMu.Lock()
	now := s.now()
	quiet := !s.lastLog.IsZero() && now.Sub(s.lastLog) < unreachableLogInterval
	if !quiet {
		s.lastLog = now
	}
	s.logMu.Unlock()
	if !quiet {
		s.logger.Warn("wn-cloud is unreachable; rating writes wait in the outbox", "error", err)
	}
}

// Drain sends the due outbox entries to wn-cloud with the server credential,
// oldest first. An accepted entry is removed. An entry that wn-cloud rejects
// with invalid_request or forbidden stays with its error and is not retried.
// Any other failure sets the time of the next attempt. An unreachable
// wn-cloud, a refused credential, or a rate limit stops the drain. Without a
// server credential Drain does nothing.
func (s *Service) Drain(ctx context.Context) error {
	cred, err := s.store.ServerCredential(ctx)
	if errors.Is(err, store.ErrNoServerCredential) {
		return nil
	}
	if err != nil {
		return err
	}
	for ctx.Err() == nil {
		stop, err := s.drainOne(ctx, cred.Token)
		if err != nil || stop {
			return err
		}
	}
	return nil
}

// drainOne sends the oldest due entry. stop is true when the outbox has no
// due entry or the drain should wait for the next run.
func (s *Service) drainOne(ctx context.Context, token string) (stop bool, err error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	e, err := s.store.NextOutbox(ctx, s.now())
	if errors.Is(err, store.ErrOutboxEmpty) {
		return true, nil
	}
	if err != nil {
		return true, err
	}
	sendErr := s.send(ctx, token, e)
	if ctx.Err() != nil {
		return true, nil // shutting down; the entry stays as it was
	}
	if sendErr == nil {
		return false, s.store.DeleteOutbox(ctx, e.ID)
	}
	attempts, next, msg := s.failure(e, e.Attempts+1, sendErr)
	if err := s.store.FailOutbox(ctx, e.ID, attempts, next, msg); err != nil {
		return true, err
	}
	stop = errors.Is(sendErr, wncloud.ErrUnreachable) || errors.Is(sendErr, wncloud.ErrUnauthorized) ||
		errors.Is(sendErr, wncloud.ErrRateLimited)
	return stop, nil
}

// Run drains the outbox at once and then every DrainInterval until ctx is
// done.
func (s *Service) Run(ctx context.Context) {
	t := time.NewTicker(DrainInterval)
	defer t.Stop()
	for {
		if err := s.Drain(ctx); err != nil && ctx.Err() == nil {
			s.logger.Error("drain the rating outbox", "error", err)
		}
		select {
		case <-ctx.Done():
			return
		case <-t.C:
		}
	}
}
