package store

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"strings"
	"time"
)

// ErrMatchNotFound is returned when a match queue entry does not exist.
var ErrMatchNotFound = errors.New("match queue entry not found")

// MatchState is the state of a match queue entry.
type MatchState string

// Match queue states.
const (
	// MatchPending is an album that beets did not apply and that waits for
	// review.
	MatchPending MatchState = "pending"
	// MatchApplied is an album that beets copied into sorted_dir.
	MatchApplied MatchState = "applied"
	// MatchDismissed is an album that the owner chose not to match.
	MatchDismissed MatchState = "dismissed"
	// MatchDuplicate is an album whose match is already in the beets library.
	MatchDuplicate MatchState = "duplicate"
)

// Valid reports whether s is one of the four states.
func (s MatchState) Valid() bool {
	switch s {
	case MatchPending, MatchApplied, MatchDismissed, MatchDuplicate:
		return true
	}
	return false
}

// Decisions of a candidates line, as the wn_candidates plugin writes them.
const (
	LineApplied   = "applied"
	LineSkipped   = "skipped"
	LineDuplicate = "duplicate"
	LineError     = "error"
)

// MatchCandidate is one MusicBrainz release that beets considered.
type MatchCandidate struct {
	ReleaseID string  `json:"release_id"`
	Artist    string  `json:"artist"`
	Album     string  `json:"album"`
	Year      int     `json:"year"`
	Label     string  `json:"label"`
	Distance  float64 `json:"distance"`
}

// MatchEntry is one album folder in the match queue.
type MatchEntry struct {
	ID         int64      `json:"id"`
	Folder     string     `json:"folder"`
	FirstSeen  time.Time  `json:"first_seen"`
	LastSeen   time.Time  `json:"last_seen"`
	State      MatchState `json:"state"`
	Artist     string     `json:"artist"`
	Album      string     `json:"album"`
	Year       int        `json:"year"`
	TrackCount int        `json:"track_count"`
	// The applied fields are set while State is MatchApplied.
	AppliedReleaseID string    `json:"applied_release_id,omitempty"`
	CopiedTo         string    `json:"copied_to,omitempty"`
	AppliedAt        time.Time `json:"applied_at,omitzero"`
	// DuplicateOfReleaseID is the release that a duplicate entry duplicates.
	DuplicateOfReleaseID string `json:"duplicate_of_release_id,omitempty"`
	// MarkerWritten is true when wn-server created the .ndignore in Folder,
	// for an applied entry or for a duplicate of an applied release.
	MarkerWritten bool   `json:"marker_written"`
	LastError     string `json:"last_error,omitempty"`
	// Candidates are in beets' order, best first.
	Candidates []MatchCandidate `json:"candidates"`
}

// MatchLine is one parsed line of the candidates file.
type MatchLine struct {
	Folder     string
	Decision   string // LineApplied, LineSkipped, LineDuplicate, or LineError
	Artist     string
	Album      string
	Year       int
	TrackCount int
	Candidates []MatchCandidate
	// ReleaseID is set on applied lines and on duplicate lines. CopiedTo is
	// set on applied lines.
	ReleaseID string
	CopiedTo  string
	// MarkerWritten is true on an applied line when wn-server created the
	// .ndignore in Folder.
	MarkerWritten bool
	// Error is set on error lines.
	Error string
}

// RecordMatch records one candidates line in the match queue and returns
// the entry id. now is the time stored as last_seen, and as first_seen and
// applied_at where they are set.
//
// A skipped line creates a pending entry or sets an existing one to pending.
// A dismissed or applied entry keeps its state. An applied line sets the
// entry to applied with the release id, copied_to, and marker_written. On an
// entry that is already applied or a duplicate, marker_written stays 1. A
// duplicate line sets duplicate and duplicate_of_release_id, except on an
// applied entry, which keeps its state. An error
// line creates a pending entry or keeps the state of an existing one, and
// stores the message in last_error. Every line except an error line replaces
// the tags and the candidates and clears last_error.
func (s *Store) RecordMatch(ctx context.Context, l MatchLine, now time.Time) (int64, error) {
	if l.Folder == "" {
		return 0, errors.New("record match: empty folder")
	}
	switch l.Decision {
	case LineApplied, LineSkipped, LineDuplicate, LineError:
	default:
		return 0, fmt.Errorf("record match: unknown decision %q", l.Decision)
	}
	ts := formatTime(now)
	var id int64
	err := s.inTx(ctx, func(tx *sql.Tx) error {
		var cur MatchState
		err := tx.QueryRowContext(ctx, `SELECT id, state FROM match_queue WHERE folder = ?`, l.Folder).Scan(&id, &cur)
		if errors.Is(err, sql.ErrNoRows) {
			res, err := tx.ExecContext(ctx,
				`INSERT INTO match_queue (folder, first_seen, last_seen, state) VALUES (?, ?, ?, ?)`,
				l.Folder, ts, ts, MatchPending)
			if err != nil {
				return err
			}
			if id, err = res.LastInsertId(); err != nil {
				return err
			}
			cur = MatchPending
		} else if err != nil {
			return err
		}

		if l.Decision == LineError {
			_, err := tx.ExecContext(ctx, `UPDATE match_queue SET last_seen = ?, last_error = ? WHERE id = ?`, ts, l.Error, id)
			return err
		}

		next := cur
		switch l.Decision {
		case LineApplied:
			next = MatchApplied
		case LineSkipped:
			if cur != MatchDismissed && cur != MatchApplied {
				next = MatchPending
			}
		case LineDuplicate:
			if cur != MatchApplied {
				next = MatchDuplicate
			}
		}
		_, err = tx.ExecContext(ctx,
			`UPDATE match_queue SET last_seen = ?, state = ?, artist = ?, album = ?, year = ?, track_count = ?, last_error = NULL WHERE id = ?`,
			ts, next, l.Artist, l.Album, l.Year, l.TrackCount, id)
		if err != nil {
			return err
		}
		switch {
		case l.Decision == LineApplied:
			// A duplicate folder that wn-server marked keeps its flag, so a
			// later revert still removes the marker.
			keep := cur == MatchApplied || cur == MatchDuplicate
			if err := setApplied(ctx, tx, id, l.ReleaseID, l.CopiedTo, now, l.MarkerWritten, keep); err != nil {
				return err
			}
			if _, err := tx.ExecContext(ctx, `UPDATE match_queue SET duplicate_of_release_id = NULL WHERE id = ?`, id); err != nil {
				return err
			}
		case l.Decision == LineDuplicate && next == MatchDuplicate && l.ReleaseID != "":
			if _, err := tx.ExecContext(ctx, `UPDATE match_queue SET duplicate_of_release_id = ? WHERE id = ?`, l.ReleaseID, id); err != nil {
				return err
			}
		}
		if _, err := tx.ExecContext(ctx, `DELETE FROM match_candidates WHERE queue_id = ?`, id); err != nil {
			return err
		}
		for i, c := range l.Candidates {
			_, err := tx.ExecContext(ctx,
				`INSERT INTO match_candidates (queue_id, position, release_id, artist, album, year, label, distance) VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
				id, i, c.ReleaseID, c.Artist, c.Album, c.Year, c.Label, c.Distance)
			if err != nil {
				return err
			}
		}
		return nil
	})
	if err != nil {
		return 0, fmt.Errorf("record match %s: %w", l.Folder, err)
	}
	return id, nil
}

const matchColumns = `id, folder, first_seen, last_seen, state, artist, album, year, track_count, applied_release_id, copied_to, applied_at, marker_written, last_error, duplicate_of_release_id`

// MatchEntries returns the entries in the given states, ordered by id, with
// their candidates. No states returns every entry.
func (s *Store) MatchEntries(ctx context.Context, states ...MatchState) ([]MatchEntry, error) {
	where, args := "", []any{}
	if len(states) > 0 {
		where = ` WHERE state IN (?` + strings.Repeat(`, ?`, len(states)-1) + `)`
		for _, st := range states {
			args = append(args, st)
		}
	}
	rows, err := s.db.QueryContext(ctx, `SELECT `+matchColumns+` FROM match_queue`+where+` ORDER BY id`, args...)
	if err != nil {
		return nil, err
	}
	var entries []MatchEntry
	index := map[int64]int{}
	for rows.Next() {
		e, err := scanMatch(rows)
		if err != nil {
			rows.Close()
			return nil, err
		}
		index[e.ID] = len(entries)
		entries = append(entries, e)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, err
	}
	if len(entries) == 0 {
		return entries, nil
	}
	crows, err := s.db.QueryContext(ctx,
		`SELECT queue_id, release_id, artist, album, year, label, distance FROM match_candidates
		 WHERE queue_id IN (SELECT id FROM match_queue`+where+`) ORDER BY queue_id, position`, args...)
	if err != nil {
		return nil, err
	}
	defer crows.Close()
	for crows.Next() {
		var (
			qid int64
			c   MatchCandidate
		)
		if err := crows.Scan(&qid, &c.ReleaseID, &c.Artist, &c.Album, &c.Year, &c.Label, &c.Distance); err != nil {
			return nil, err
		}
		if i, ok := index[qid]; ok {
			entries[i].Candidates = append(entries[i].Candidates, c)
		}
	}
	return entries, crows.Err()
}

// MatchEntry returns the entry with the given id and its candidates.
func (s *Store) MatchEntry(ctx context.Context, id int64) (MatchEntry, error) {
	e, err := scanMatch(s.db.QueryRowContext(ctx, `SELECT `+matchColumns+` FROM match_queue WHERE id = ?`, id))
	if errors.Is(err, sql.ErrNoRows) {
		return MatchEntry{}, ErrMatchNotFound
	}
	if err != nil {
		return MatchEntry{}, err
	}
	rows, err := s.db.QueryContext(ctx,
		`SELECT release_id, artist, album, year, label, distance FROM match_candidates WHERE queue_id = ? ORDER BY position`, id)
	if err != nil {
		return MatchEntry{}, err
	}
	defer rows.Close()
	for rows.Next() {
		var c MatchCandidate
		if err := rows.Scan(&c.ReleaseID, &c.Artist, &c.Album, &c.Year, &c.Label, &c.Distance); err != nil {
			return MatchEntry{}, err
		}
		e.Candidates = append(e.Candidates, c)
	}
	return e, rows.Err()
}

// CountMatches returns the number of entries in state.
func (s *Store) CountMatches(ctx context.Context, state MatchState) (int, error) {
	var n int
	err := s.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM match_queue WHERE state = ?`, state).Scan(&n)
	return n, err
}

// SetMatchState sets the state of an entry. It refuses MatchApplied, which
// SetMatchApplied sets together with the applied fields.
func (s *Store) SetMatchState(ctx context.Context, id int64, state MatchState) error {
	if !state.Valid() || state == MatchApplied {
		return fmt.Errorf("set match state: invalid state %q", state)
	}
	return checkOne(s.db.ExecContext(ctx, `UPDATE match_queue SET state = ? WHERE id = ?`, state, id))
}

// SetMatchApplied sets an entry to applied with the release id, the album
// folder in sorted_dir, the time of the apply, and whether wn-server created
// the .ndignore in the entry's folder.
func (s *Store) SetMatchApplied(ctx context.Context, id int64, releaseID, copiedTo string, at time.Time, markerWritten bool) error {
	return setApplied(ctx, s.db, id, releaseID, copiedTo, at, markerWritten, false)
}

// setApplied sets the applied fields. With keepMarker, a marker_written of 1
// stays 1.
func setApplied(ctx context.Context, db execer, id int64, releaseID, copiedTo string, at time.Time, markerWritten, keepMarker bool) error {
	return checkOne(db.ExecContext(ctx,
		`UPDATE match_queue SET state = ?, applied_release_id = ?, copied_to = ?, applied_at = ?,
		 marker_written = CASE WHEN ? THEN MAX(marker_written, ?) ELSE ? END WHERE id = ?`,
		MatchApplied, releaseID, copiedTo, formatTime(at), keepMarker, markerWritten, markerWritten, id))
}

// MarkApplied records the applied line of a match apply run. It sets the
// entry to applied with the release id, copied_to, applied_at, and
// marker_written, sets last_seen to now, and clears last_error. The tags and
// the candidates stay, because that run looked up only the requested release.
func (s *Store) MarkApplied(ctx context.Context, id int64, releaseID, copiedTo string, markerWritten bool, now time.Time) error {
	err := checkOne(s.db.ExecContext(ctx,
		`UPDATE match_queue SET state = ?, applied_release_id = ?, copied_to = ?, applied_at = ?, marker_written = ?, last_seen = ?, last_error = NULL WHERE id = ?`,
		MatchApplied, releaseID, copiedTo, formatTime(now), markerWritten, formatTime(now), id))
	if err != nil {
		return fmt.Errorf("mark match %d applied: %w", id, err)
	}
	return nil
}

// RevertMatch sets an entry to pending and clears the applied fields.
func (s *Store) RevertMatch(ctx context.Context, id int64) error {
	return checkOne(s.db.ExecContext(ctx,
		`UPDATE match_queue SET state = ?, applied_release_id = NULL, copied_to = NULL, applied_at = NULL, marker_written = 0 WHERE id = ?`,
		MatchPending, id))
}

// UnmarkedDuplicates returns the duplicate entries without a marker from
// wn-server whose duplicate_of_release_id is the applied_release_id of an
// applied entry. The entries have no candidates.
func (s *Store) UnmarkedDuplicates(ctx context.Context) ([]MatchEntry, error) {
	return s.queryEntries(ctx,
		`WHERE state = ? AND marker_written = 0 AND duplicate_of_release_id IN
		 (SELECT applied_release_id FROM match_queue WHERE state = ?) ORDER BY id`,
		MatchDuplicate, MatchApplied)
}

// MarkedDuplicates returns the entries that are not applied and whose
// duplicate_of_release_id is releaseID, for which wn-server created the
// marker. The entries have no candidates.
func (s *Store) MarkedDuplicates(ctx context.Context, releaseID string) ([]MatchEntry, error) {
	return s.queryEntries(ctx,
		`WHERE state != ? AND marker_written = 1 AND duplicate_of_release_id = ? ORDER BY id`,
		MatchApplied, releaseID)
}

// SetMarkerWritten sets marker_written of an entry.
func (s *Store) SetMarkerWritten(ctx context.Context, id int64, written bool) error {
	return checkOne(s.db.ExecContext(ctx, `UPDATE match_queue SET marker_written = ? WHERE id = ?`, written, id))
}

// queryEntries returns the entries that the WHERE clause selects, without
// their candidates.
func (s *Store) queryEntries(ctx context.Context, where string, args ...any) ([]MatchEntry, error) {
	rows, err := s.db.QueryContext(ctx, `SELECT `+matchColumns+` FROM match_queue `+where, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var entries []MatchEntry
	for rows.Next() {
		e, err := scanMatch(rows)
		if err != nil {
			return nil, err
		}
		entries = append(entries, e)
	}
	return entries, rows.Err()
}

// NoteMatchError stores msg in last_error and now in last_seen. The state,
// the tags, and the candidates stay.
func (s *Store) NoteMatchError(ctx context.Context, id int64, msg string, now time.Time) error {
	return checkOne(s.db.ExecContext(ctx,
		`UPDATE match_queue SET last_seen = ?, last_error = ? WHERE id = ?`, formatTime(now), msg, id))
}

// checkOne returns ErrMatchNotFound when an update changed no row.
func checkOne(res sql.Result, err error) error {
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return ErrMatchNotFound
	}
	return nil
}

type rowScanner interface {
	Scan(dest ...any) error
}

func scanMatch(r rowScanner) (MatchEntry, error) {
	var (
		e                                MatchEntry
		first, last                      string
		releaseID, copiedTo, at, lastErr sql.NullString
		duplicateOf                      sql.NullString
	)
	err := r.Scan(&e.ID, &e.Folder, &first, &last, &e.State, &e.Artist, &e.Album, &e.Year, &e.TrackCount,
		&releaseID, &copiedTo, &at, &e.MarkerWritten, &lastErr, &duplicateOf)
	if err != nil {
		return MatchEntry{}, err
	}
	e.AppliedReleaseID, e.CopiedTo, e.LastError = releaseID.String, copiedTo.String, lastErr.String
	e.DuplicateOfReleaseID = duplicateOf.String
	if e.FirstSeen, err = time.Parse(time.RFC3339, first); err != nil {
		return MatchEntry{}, fmt.Errorf("match entry %d: bad first_seen: %w", e.ID, err)
	}
	if e.LastSeen, err = time.Parse(time.RFC3339, last); err != nil {
		return MatchEntry{}, fmt.Errorf("match entry %d: bad last_seen: %w", e.ID, err)
	}
	if at.Valid {
		if e.AppliedAt, err = time.Parse(time.RFC3339, at.String); err != nil {
			return MatchEntry{}, fmt.Errorf("match entry %d: bad applied_at: %w", e.ID, err)
		}
	}
	return e, nil
}

func formatTime(t time.Time) string { return t.UTC().Format(time.RFC3339) }
