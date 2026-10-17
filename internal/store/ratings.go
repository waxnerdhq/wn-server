package store

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"time"
)

// ErrRatingNotFound is returned when the store holds no rating of a subject.
var ErrRatingNotFound = errors.New("rating not found")

// timeLayout is the layout of rating and outbox times: UTC with
// microseconds and a fixed width, so that the text sorts in time order.
const timeLayout = "2006-01-02T15:04:05.000000Z"

func formatMicros(t time.Time) string { return t.UTC().Format(timeLayout) }

// Rating is an account's rating of one MusicBrainz subject. HalfStars is 1
// to 10 or nil. LikeLevel is "like", "love", or nil.
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
func (s *Store) Rating(ctx context.Context, accountID, kind, mbid string) (Rating, error) {
	r := Rating{AccountID: accountID, Kind: kind, MBID: mbid}
	var (
		half    sql.NullInt64
		like    sql.NullString
		updated string
	)
	err := s.db.QueryRowContext(ctx,
		`SELECT half_stars, like_level, updated_at FROM ratings WHERE account_id = ? AND kind = ? AND mbid = ?`,
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
// rating deletes the row. A zero UpdatedAt is set to the current time.
func (s *Store) PutRating(ctx context.Context, r Rating) error {
	if r.Cleared() {
		_, err := s.db.ExecContext(ctx,
			`DELETE FROM ratings WHERE account_id = ? AND kind = ? AND mbid = ?`, r.AccountID, r.Kind, r.MBID)
		return err
	}
	if r.UpdatedAt.IsZero() {
		r.UpdatedAt = time.Now()
	}
	_, err := s.db.ExecContext(ctx,
		`INSERT INTO ratings (account_id, kind, mbid, half_stars, like_level, updated_at) VALUES (?, ?, ?, ?, ?, ?)
		 ON CONFLICT (account_id, kind, mbid) DO UPDATE SET
		   half_stars = excluded.half_stars, like_level = excluded.like_level, updated_at = excluded.updated_at`,
		r.AccountID, r.Kind, r.MBID, nullInt(r.HalfStars), nullString(r.LikeLevel), formatMicros(r.UpdatedAt))
	if err != nil {
		return fmt.Errorf("put rating: %w", err)
	}
	return nil
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
}

// EnqueueRating writes e to the outbox. An entry of the same subject is
// replaced and keeps its id, so it keeps its place in the order. A zero
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
		   last_error = excluded.last_error`,
		e.Kind, e.MBID, nullInt(e.HalfStars), nullString(e.LikeLevel), e.Deleted,
		formatMicros(e.CreatedAt), e.Attempts, nullTime(e.NextAt), sql.NullString{String: e.LastError, Valid: e.LastError != ""})
	if err != nil {
		return fmt.Errorf("enqueue rating: %w", err)
	}
	return nil
}

const outboxColumns = `id, kind, mbid, half_stars, like_level, deleted, created_at, attempts, next_at, last_error`

func scanOutbox(row rowScanner) (OutboxEntry, error) {
	var (
		e                OutboxEntry
		half             sql.NullInt64
		like, next, last sql.NullString
		created          string
	)
	if err := row.Scan(&e.ID, &e.Kind, &e.MBID, &half, &like, &e.Deleted, &created, &e.Attempts, &next, &last); err != nil {
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
// it.
func (s *Store) DeleteOutbox(ctx context.Context, id int64) error {
	_, err := s.db.ExecContext(ctx, `DELETE FROM rating_outbox WHERE id = ?`, id)
	return err
}

// DeleteOutboxSubject removes the entry of a subject after wn-cloud accepted
// a newer write of it.
func (s *Store) DeleteOutboxSubject(ctx context.Context, kind, mbid string) error {
	_, err := s.db.ExecContext(ctx, `DELETE FROM rating_outbox WHERE kind = ? AND mbid = ?`, kind, mbid)
	return err
}

// FailOutbox records a failed attempt of the entry with the given id. A zero
// nextAt keeps the entry out of the retry loop.
func (s *Store) FailOutbox(ctx context.Context, id int64, attempts int, nextAt time.Time, lastError string) error {
	_, err := s.db.ExecContext(ctx,
		`UPDATE rating_outbox SET attempts = ?, next_at = ?, last_error = ? WHERE id = ?`,
		attempts, nullTime(nextAt), lastError, id)
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
		 SELECT kind, mbid, half_stars, like_level, 0, ?, 0, ?, NULL FROM ratings WHERE account_id = ? ORDER BY updated_at
		 ON CONFLICT (kind, mbid) DO UPDATE SET
		   half_stars = excluded.half_stars, like_level = excluded.like_level, deleted = 0,
		   attempts = 0, next_at = excluded.next_at, last_error = NULL`,
		at, at, accountID)
	if err != nil {
		return fmt.Errorf("queue ratings: %w", err)
	}
	if _, err := tx.ExecContext(ctx, `UPDATE rating_outbox SET attempts = 0, next_at = ? WHERE next_at IS NOT NULL`, at); err != nil {
		return fmt.Errorf("queue ratings: %w", err)
	}
	return nil
}
