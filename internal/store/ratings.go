package store

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"time"
)

// ErrRatingNotFound is returned when the store holds no rating of a subject.
var ErrRatingNotFound = errors.New("rating not found")

// timeLayout is the layout of rating and outbox times: UTC with
// microseconds and a fixed width, so that the text sorts in time order.
const timeLayout = "2006-01-02T15:04:05.000000Z"

func formatMicros(t time.Time) string { return t.UTC().Format(timeLayout) }

// Rating is an account's rating of one MusicBrainz subject. HalfStars is 1
// to 10 or nil. LikeLevel is "like", "love", or nil. UpdatedAt is the time
// of the change as the owner made it: the local write, or the updated_at
// that wn-cloud reported.
type Rating struct {
	AccountID string
	Kind      string
	MBID      string
	HalfStars *int
	LikeLevel *string
	UpdatedAt time.Time
}

// Cleared reports whether the rating holds neither value.
func (r Rating) Cleared() bool { return r.HalfStars == nil && r.LikeLevel == nil }

// Rating returns the account's rating of the subject, or ErrRatingNotFound.
// A cleared rating, which the store keeps as a tombstone, is not found.
func (s *Store) Rating(ctx context.Context, accountID, kind, mbid string) (Rating, error) {
	r := Rating{AccountID: accountID, Kind: kind, MBID: mbid}
	var (
		half    sql.NullInt64
		like    sql.NullString
		updated string
	)
	err := s.db.QueryRowContext(ctx,
		`SELECT half_stars, like_level, updated_at FROM ratings WHERE account_id = ? AND kind = ? AND mbid = ? AND deleted = 0`,
		accountID, kind, mbid).Scan(&half, &like, &updated)
	if errors.Is(err, sql.ErrNoRows) {
		return Rating{}, ErrRatingNotFound
	}
	if err != nil {
		return Rating{}, err
	}
	if half.Valid {
		v := int(half.Int64)
		r.HalfStars = &v
	}
	if like.Valid {
		r.LikeLevel = &like.String
	}
	if r.UpdatedAt, err = time.Parse(timeLayout, updated); err != nil {
		return Rating{}, fmt.Errorf("rating %s %s: bad updated_at: %w", kind, mbid, err)
	}
	return r, nil
}

// PutRating stores r, replacing the stored rating of the subject. A cleared
// rating turns the stored row into a tombstone, and does nothing when the
// subject has no rating. A zero UpdatedAt is set to the current time.
func (s *Store) PutRating(ctx context.Context, r Rating) error {
	return putRating(ctx, s.db, r, time.Now())
}

func putRating(ctx context.Context, db execer, r Rating, now time.Time) error {
	if r.UpdatedAt.IsZero() {
		r.UpdatedAt = now
	}
	if r.Cleared() {
		_, err := db.ExecContext(ctx,
			`UPDATE ratings SET half_stars = NULL, like_level = NULL, deleted = 1, updated_at = ?, changed_at = ?
			 WHERE account_id = ? AND kind = ? AND mbid = ? AND deleted = 0`,
			formatMicros(r.UpdatedAt), formatMicros(now), r.AccountID, r.Kind, r.MBID)
		if err != nil {
			return fmt.Errorf("clear rating: %w", err)
		}
		return nil
	}
	_, err := db.ExecContext(ctx,
		`INSERT INTO ratings (account_id, kind, mbid, half_stars, like_level, deleted, updated_at, changed_at)
		 VALUES (?, ?, ?, ?, ?, 0, ?, ?)
		 ON CONFLICT (account_id, kind, mbid) DO UPDATE SET
		   half_stars = excluded.half_stars, like_level = excluded.like_level, deleted = 0,
		   updated_at = excluded.updated_at, changed_at = excluded.changed_at`,
		r.AccountID, r.Kind, r.MBID, nullInt(r.HalfStars), nullString(r.LikeLevel),
		formatMicros(r.UpdatedAt), formatMicros(now))
	if err != nil {
		return fmt.Errorf("put rating: %w", err)
	}
	return nil
}

// RemoteResult is what ApplyRemoteRating did with a change from wn-cloud.
type RemoteResult int

const (
	// RemoteUnchanged means the store already held the values.
	RemoteUnchanged RemoteResult = iota
	// RemoteApplied means the store now holds the values of the change.
	RemoteApplied
	// RemoteKeptLocal means an outbox entry of the subject is newer than the
	// change, so the store kept the local values.
	RemoteKeptLocal
)

// ApplyRemoteRating writes a rating change that wn-cloud reported. r.UpdatedAt
// is the updated_at from wn-cloud. deleted means the rating was cleared, and
// then r holds no values. The change does not go into the outbox.
//
// An outbox entry of the same subject that was created after r.UpdatedAt
// wins: the store keeps the local values and the entry. An older entry is
// removed, because the change from wn-cloud is newer than the local write.
func (s *Store) ApplyRemoteRating(ctx context.Context, accountID string, r Rating, deleted bool) (RemoteResult, error) {
	r.AccountID = accountID
	result := RemoteUnchanged
	err := s.inTx(ctx, func(tx *sql.Tx) error {
		var created string
		err := tx.QueryRowContext(ctx, `SELECT created_at FROM rating_outbox WHERE kind = ? AND mbid = ?`, r.Kind, r.MBID).Scan(&created)
		switch {
		case errors.Is(err, sql.ErrNoRows):
		case err != nil:
			return err
		default:
			t, err := time.Parse(timeLayout, created)
			if err != nil {
				return fmt.Errorf("outbox entry %s %s: bad created_at: %w", r.Kind, r.MBID, err)
			}
			if t.After(r.UpdatedAt) {
				result = RemoteKeptLocal
				return nil
			}
			if _, err := tx.ExecContext(ctx, `DELETE FROM rating_outbox WHERE kind = ? AND mbid = ?`, r.Kind, r.MBID); err != nil {
				return err
			}
		}

		var (
			half  sql.NullInt64
			like  sql.NullString
			gone  bool
			found = true
		)
		err = tx.QueryRowContext(ctx,
			`SELECT half_stars, like_level, deleted FROM ratings WHERE account_id = ? AND kind = ? AND mbid = ?`,
			accountID, r.Kind, r.MBID).Scan(&half, &like, &gone)
		if errors.Is(err, sql.ErrNoRows) {
			found = false
		} else if err != nil {
			return err
		}
		held := found && !gone
		if deleted {
			r.HalfStars, r.LikeLevel = nil, nil
		}
		switch {
		case deleted && !held:
			return nil
		case !deleted && held && half == nullInt(r.HalfStars) && like == nullString(r.LikeLevel):
			return nil
		}
		result = RemoteApplied
		return putRating(ctx, tx, r, time.Now())
	})
	return result, err
}

// RatingSyncCursor returns the cursor of GET /ratings?since= at wn-cloud, or
// an empty string when none is stored.
func (s *Store) RatingSyncCursor(ctx context.Context) (string, error) {
	var c string
	err := s.db.QueryRowContext(ctx, `SELECT cursor FROM rating_sync WHERE id = 1`).Scan(&c)
	if errors.Is(err, sql.ErrNoRows) {
		return "", nil
	}
	return c, err
}

// SetRatingSyncCursor stores the cursor of GET /ratings?since=.
func (s *Store) SetRatingSyncCursor(ctx context.Context, cursor string) error {
	_, err := s.db.ExecContext(ctx,
		`INSERT INTO rating_sync (id, cursor) VALUES (1, ?) ON CONFLICT (id) DO UPDATE SET cursor = excluded.cursor`, cursor)
	return err
}

// ChangeCursor is a position in the change feed of the local ratings: the
// time and id of the last row that was read.
type ChangeCursor struct {
	ChangedAt time.Time
	ID        int64
}

// String returns the cursor as text: the time with microseconds and the id,
// joined by a comma.
func (c ChangeCursor) String() string {
	return formatMicros(c.ChangedAt) + "," + strconv.FormatInt(c.ID, 10)
}

// ErrBadCursor is returned by ParseChangeCursor for text that is not a cursor.
var ErrBadCursor = errors.New("malformed cursor")

// ParseChangeCursor parses the text that ChangeCursor.String returns.
func ParseChangeCursor(text string) (ChangeCursor, error) {
	ts, id, ok := strings.Cut(text, ",")
	if !ok {
		return ChangeCursor{}, ErrBadCursor
	}
	t, err := time.Parse(timeLayout, ts)
	if err != nil {
		return ChangeCursor{}, ErrBadCursor
	}
	n, err := strconv.ParseInt(id, 10, 64)
	if err != nil || n < 0 {
		return ChangeCursor{}, ErrBadCursor
	}
	return ChangeCursor{ChangedAt: t, ID: n}, nil
}

// RatingChange is a row of the change feed. Deleted is true for a cleared
// rating, which has no values.
type RatingChange struct {
	Rating
	ID        int64
	ChangedAt time.Time
	Deleted   bool
}

// Cursor returns the position after this change.
func (c RatingChange) Cursor() ChangeCursor { return ChangeCursor{ChangedAt: c.ChangedAt, ID: c.ID} }

// RatingChanges returns up to limit rating rows of the account that changed
// after the cursor, oldest first. A nil cursor starts at the beginning and
// leaves out tombstones, because a reader that starts from nothing has no
// rating to delete.
func (s *Store) RatingChanges(ctx context.Context, accountID string, after *ChangeCursor, limit int) ([]RatingChange, error) {
	q := `SELECT id, kind, mbid, half_stars, like_level, deleted, updated_at, changed_at FROM ratings WHERE account_id = ?`
	args := []any{accountID}
	if after == nil {
		q += ` AND deleted = 0`
	} else {
		q += ` AND (changed_at > ? OR (changed_at = ? AND id > ?))`
		at := formatMicros(after.ChangedAt)
		args = append(args, at, at, after.ID)
	}
	q += ` ORDER BY changed_at, id LIMIT ?`
	args = append(args, limit)
	rows, err := s.db.QueryContext(ctx, q, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var list []RatingChange
	for rows.Next() {
		c := RatingChange{Rating: Rating{AccountID: accountID}}
		var (
			half             sql.NullInt64
			like             sql.NullString
			updated, changed string
		)
		if err := rows.Scan(&c.ID, &c.Kind, &c.MBID, &half, &like, &c.Deleted, &updated, &changed); err != nil {
			return nil, err
		}
		if half.Valid {
			v := int(half.Int64)
			c.HalfStars = &v
		}
		if like.Valid {
			c.LikeLevel = &like.String
		}
		if c.UpdatedAt, err = time.Parse(timeLayout, updated); err != nil {
			return nil, fmt.Errorf("rating %d: bad updated_at: %w", c.ID, err)
		}
		if c.ChangedAt, err = time.Parse(timeLayout, changed); err != nil {
			return nil, fmt.Errorf("rating %d: bad changed_at: %w", c.ID, err)
		}
		list = append(list, c)
	}
	return list, rows.Err()
}

func nullInt(p *int) sql.NullInt64 {
	if p == nil {
		return sql.NullInt64{}
	}
	return sql.NullInt64{Int64: int64(*p), Valid: true}
}

func nullString(p *string) sql.NullString {
	if p == nil {
		return sql.NullString{}
	}
	return sql.NullString{String: *p, Valid: true}
}

func nullTime(t time.Time) sql.NullString {
	if t.IsZero() {
		return sql.NullString{}
	}
	return sql.NullString{String: formatMicros(t), Valid: true}
}

// ErrOutboxEmpty is returned by NextOutbox when no entry is due.
var ErrOutboxEmpty = errors.New("no outbox entry is due")

// OutboxEntry is a rating write that wn-cloud has not accepted yet.
type OutboxEntry struct {
	ID        int64
	Kind      string
	MBID      string
	HalfStars *int
	LikeLevel *string
	Deleted   bool // the rating is cleared and is sent as DELETE
	CreatedAt time.Time
	Attempts  int
	// NextAt is the earliest time of the next attempt. Zero means that
	// wn-cloud rejected the entry and that it is not retried.
	NextAt    time.Time
	LastError string
	// Version grows with every change of the entry.
	Version int64
}

// EnqueueRating writes e to the outbox. An entry of the same subject is
// replaced, keeps its id and so its place in the order, and gets a new
// version. A zero
// CreatedAt is set to the current time. A zero NextAt keeps the entry out of
// the retry loop.
func (s *Store) EnqueueRating(ctx context.Context, e OutboxEntry) error {
	if e.CreatedAt.IsZero() {
		e.CreatedAt = time.Now()
	}
	_, err := s.db.ExecContext(ctx,
		`INSERT INTO rating_outbox (kind, mbid, half_stars, like_level, deleted, created_at, attempts, next_at, last_error)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
		 ON CONFLICT (kind, mbid) DO UPDATE SET
		   half_stars = excluded.half_stars, like_level = excluded.like_level, deleted = excluded.deleted,
		   created_at = excluded.created_at, attempts = excluded.attempts, next_at = excluded.next_at,
		   last_error = excluded.last_error, version = rating_outbox.version + 1`,
		e.Kind, e.MBID, nullInt(e.HalfStars), nullString(e.LikeLevel), e.Deleted,
		formatMicros(e.CreatedAt), e.Attempts, nullTime(e.NextAt), sql.NullString{String: e.LastError, Valid: e.LastError != ""})
	if err != nil {
		return fmt.Errorf("enqueue rating: %w", err)
	}
	return nil
}

const outboxColumns = `id, kind, mbid, half_stars, like_level, deleted, created_at, attempts, next_at, last_error, version`

func scanOutbox(row rowScanner) (OutboxEntry, error) {
	var (
		e                OutboxEntry
		half             sql.NullInt64
		like, next, last sql.NullString
		created          string
	)
	if err := row.Scan(&e.ID, &e.Kind, &e.MBID, &half, &like, &e.Deleted, &created, &e.Attempts, &next, &last, &e.Version); err != nil {
		return OutboxEntry{}, err
	}
	if half.Valid {
		v := int(half.Int64)
		e.HalfStars = &v
	}
	if like.Valid {
		e.LikeLevel = &like.String
	}
	e.LastError = last.String
	var err error
	if e.CreatedAt, err = time.Parse(timeLayout, created); err != nil {
		return OutboxEntry{}, fmt.Errorf("outbox entry %d: bad created_at: %w", e.ID, err)
	}
	if next.Valid {
		if e.NextAt, err = time.Parse(timeLayout, next.String); err != nil {
			return OutboxEntry{}, fmt.Errorf("outbox entry %d: bad next_at: %w", e.ID, err)
		}
	}
	return e, nil
}

// NextOutbox returns the oldest entry whose next attempt is due at now, or
// ErrOutboxEmpty.
func (s *Store) NextOutbox(ctx context.Context, now time.Time) (OutboxEntry, error) {
	e, err := scanOutbox(s.db.QueryRowContext(ctx,
		`SELECT `+outboxColumns+` FROM rating_outbox WHERE next_at IS NOT NULL AND next_at <= ? ORDER BY id LIMIT 1`,
		formatMicros(now)))
	if errors.Is(err, sql.ErrNoRows) {
		return OutboxEntry{}, ErrOutboxEmpty
	}
	return e, err
}

// Outbox returns every outbox entry, oldest first.
func (s *Store) Outbox(ctx context.Context) ([]OutboxEntry, error) {
	rows, err := s.db.QueryContext(ctx, `SELECT `+outboxColumns+` FROM rating_outbox ORDER BY id`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var list []OutboxEntry
	for rows.Next() {
		e, err := scanOutbox(rows)
		if err != nil {
			return nil, err
		}
		list = append(list, e)
	}
	return list, rows.Err()
}

// DeleteOutbox removes the entry with the given id after wn-cloud accepted
// it. An entry whose version is no longer the given one changed during the
// send and stays.
func (s *Store) DeleteOutbox(ctx context.Context, id, version int64) error {
	_, err := s.db.ExecContext(ctx, `DELETE FROM rating_outbox WHERE id = ? AND version = ?`, id, version)
	return err
}

// FailOutbox records a failed attempt of the entry with the given id. A zero
// nextAt keeps the entry out of the retry loop. An entry whose version is no
// longer the given one changed during the send and stays as it is.
func (s *Store) FailOutbox(ctx context.Context, id, version int64, attempts int, nextAt time.Time, lastError string) error {
	_, err := s.db.ExecContext(ctx,
		`UPDATE rating_outbox SET attempts = ?, next_at = ?, last_error = ? WHERE id = ? AND version = ?`,
		attempts, nullTime(nextAt), lastError, id, version)
	return err
}

// queueAllRatings copies every rating of the account into the outbox, due
// at now. An entry of a rated subject takes the stored values. Every other
// entry that waits for a retry becomes due at now. Entries that wn-cloud
// rejected stay out of the retry loop unless the subject is rated.
func queueAllRatings(ctx context.Context, tx *sql.Tx, accountID string, now time.Time) error {
	at := formatMicros(now)
	_, err := tx.ExecContext(ctx,
		`INSERT INTO rating_outbox (kind, mbid, half_stars, like_level, deleted, created_at, attempts, next_at, last_error)
		 SELECT kind, mbid, half_stars, like_level, 0, ?, 0, ?, NULL FROM ratings WHERE account_id = ? AND deleted = 0 ORDER BY updated_at
		 ON CONFLICT (kind, mbid) DO UPDATE SET
		   half_stars = excluded.half_stars, like_level = excluded.like_level, deleted = 0,
		   attempts = 0, next_at = excluded.next_at, last_error = NULL, version = rating_outbox.version + 1`,
		at, at, accountID)
	if err != nil {
		return fmt.Errorf("queue ratings: %w", err)
	}
	return retryOutboxNow(ctx, tx, now)
}

// retryOutboxNow makes every entry that waits for a retry due at now.
// Entries that wn-cloud rejected stay out of the retry loop.
func retryOutboxNow(ctx context.Context, tx *sql.Tx, now time.Time) error {
	_, err := tx.ExecContext(ctx,
		`UPDATE rating_outbox SET attempts = 0, next_at = ?, version = version + 1 WHERE next_at IS NOT NULL`, formatMicros(now))
	if err != nil {
		return fmt.Errorf("retry outbox: %w", err)
	}
	return nil
}
