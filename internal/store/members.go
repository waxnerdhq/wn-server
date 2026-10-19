package store

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"time"
	"unicode"
)

// Member is one entry of the cached member list (GET /servers/:id/members).
type Member struct {
	UserID    string
	Name      string
	Role      string // owner or listener
	CreatedAt time.Time
	SeenAt    time.Time
}

// MembersRefresh is the result of ReplaceMembers: the listener accounts it
// disabled and the ones it enabled again.
type MembersRefresh struct {
	Disabled []Account
	Enabled  []Account
	// SessionsEnded is the number of sessions of the disabled accounts that
	// were deleted.
	SessionsEnded int
}

// ReplaceMembers stores list as the cached member list with seen_at and the
// time of the last successful refresh set to at. In the same transaction it
// disables every listener account whose waxnerd user id is not in list and
// deletes its sessions, and enables every disabled listener account whose
// user id is in list again. The accounts are kept.
func (s *Store) ReplaceMembers(ctx context.Context, list []Member, at time.Time) (MembersRefresh, error) {
	ts := at.UTC().Format(time.RFC3339)
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return MembersRefresh{}, err
	}
	defer tx.Rollback() //nolint:errcheck
	if _, err := tx.ExecContext(ctx, `DELETE FROM members`); err != nil {
		return MembersRefresh{}, fmt.Errorf("replace members: %w", err)
	}
	for _, m := range list {
		_, err := tx.ExecContext(ctx,
			`INSERT OR REPLACE INTO members (user_id, name, role, created_at, seen_at) VALUES (?, ?, ?, ?, ?)`,
			m.UserID, m.Name, m.Role, m.CreatedAt.UTC().Format(time.RFC3339Nano), ts)
		if err != nil {
			return MembersRefresh{}, fmt.Errorf("replace members: %w", err)
		}
	}
	if _, err := tx.ExecContext(ctx,
		`INSERT OR REPLACE INTO members_refresh (id, refreshed_at) VALUES (1, ?)`, ts); err != nil {
		return MembersRefresh{}, fmt.Errorf("replace members: %w", err)
	}

	var out MembersRefresh
	if out.Disabled, err = accountsTx(ctx, tx,
		`WHERE kind = 'listener' AND disabled = 0 AND waxnerd_user_id NOT IN (SELECT user_id FROM members)`); err != nil {
		return MembersRefresh{}, err
	}
	if out.Enabled, err = accountsTx(ctx, tx,
		`WHERE kind = 'listener' AND disabled = 1 AND waxnerd_user_id IN (SELECT user_id FROM members)`); err != nil {
		return MembersRefresh{}, err
	}
	for _, a := range out.Disabled {
		if _, err := tx.ExecContext(ctx, `UPDATE accounts SET disabled = 1 WHERE id = ?`, a.ID); err != nil {
			return MembersRefresh{}, err
		}
		res, err := tx.ExecContext(ctx, `DELETE FROM sessions WHERE account_id = ?`, a.ID)
		if err != nil {
			return MembersRefresh{}, err
		}
		n, _ := res.RowsAffected()
		out.SessionsEnded += int(n)
	}
	for _, a := range out.Enabled {
		if _, err := tx.ExecContext(ctx, `UPDATE accounts SET disabled = 0 WHERE id = ?`, a.ID); err != nil {
			return MembersRefresh{}, err
		}
	}
	return out, tx.Commit()
}

// accountsTx returns the accounts that match clause, read in tx.
func accountsTx(ctx context.Context, tx *sql.Tx, clause string) ([]Account, error) {
	rows, err := tx.QueryContext(ctx, `SELECT id, name, waxnerd_user_id FROM accounts `+clause+` ORDER BY created_at, rowid`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var list []Account
	for rows.Next() {
		var (
			a       Account
			waxnerd sql.NullString
		)
		if err := rows.Scan(&a.ID, &a.Name, &waxnerd); err != nil {
			return nil, err
		}
		a.WaxnerdUserID, a.Kind = waxnerd.String, KindListener
		list = append(list, a)
	}
	return list, rows.Err()
}

// IsMember reports whether the cached member list holds the user id.
func (s *Store) IsMember(ctx context.Context, userID string) (bool, error) {
	var n int
	err := s.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM members WHERE user_id = ?`, userID).Scan(&n)
	return n > 0, err
}

// MembersRefreshedAt returns the time of the last successful refresh of the
// member list. ok is false when no refresh has succeeded yet.
func (s *Store) MembersRefreshedAt(ctx context.Context) (at time.Time, ok bool, err error) {
	var ts string
	err = s.db.QueryRowContext(ctx, `SELECT refreshed_at FROM members_refresh WHERE id = 1`).Scan(&ts)
	if errors.Is(err, sql.ErrNoRows) {
		return time.Time{}, false, nil
	}
	if err != nil {
		return time.Time{}, false, err
	}
	at, err = time.Parse(time.RFC3339, ts)
	if err != nil {
		return time.Time{}, false, fmt.Errorf("members_refresh: bad refreshed_at: %w", err)
	}
	return at, true, nil
}

// ListenerByUserID returns the listener account of the waxnerd user, or
// ErrNotFound.
func (s *Store) ListenerByUserID(ctx context.Context, userID string) (Account, error) {
	return s.one(ctx, `WHERE kind = 'listener' AND waxnerd_user_id = ?`, userID)
}

// CreateListener creates the listener account of the waxnerd user. The name
// is the local part of email, cleaned to a valid account name, or
// "listener" when nothing is left. A name that is taken gets the suffix -2,
// -3, and so on. The app password is random, is never returned, and is
// stored as its SHA-256 hash. A user who has a listener account already gets
// that account.
func (s *Store) CreateListener(ctx context.Context, userID, email string) (Account, error) {
	id, err := newID()
	if err != nil {
		return Account{}, err
	}
	pw, err := NewAppPassword()
	if err != nil {
		return Account{}, err
	}
	sum := sha256.Sum256([]byte(pw))
	base := listenerName(email)
	created := time.Now().UTC().Truncate(time.Second)
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return Account{}, err
	}
	defer tx.Rollback() //nolint:errcheck
	var existing string
	err = tx.QueryRowContext(ctx, `SELECT id FROM accounts WHERE kind = 'listener' AND waxnerd_user_id = ?`, userID).Scan(&existing)
	switch {
	case err == nil:
		_ = tx.Rollback()
		return s.AccountByID(ctx, existing)
	case !errors.Is(err, sql.ErrNoRows):
		return Account{}, err
	}
	name := base
	for i := 2; ; i++ {
		var n int
		if err := tx.QueryRowContext(ctx, `SELECT COUNT(*) FROM accounts WHERE name = ?`, name).Scan(&n); err != nil {
			return Account{}, err
		}
		if n == 0 {
			break
		}
		suffix := "-" + strconv.Itoa(i)
		name = truncateRunes(base, MaxNameLength-len(suffix)) + suffix
	}
	_, err = tx.ExecContext(ctx,
		`INSERT INTO accounts (id, name, app_password, waxnerd_user_id, kind, created_at) VALUES (?, ?, ?, ?, 'listener', ?)`,
		id, name, hex.EncodeToString(sum[:]), userID, created.Format(time.RFC3339))
	if err != nil {
		return Account{}, fmt.Errorf("create listener: %w", err)
	}
	if err := tx.Commit(); err != nil {
		return Account{}, err
	}
	return Account{ID: id, Name: name, AppPassword: hex.EncodeToString(sum[:]), WaxnerdUserID: userID, Kind: KindListener, CreatedAt: created}, nil
}

// listenerName returns the local part of email without control characters
// and surrounding space, cut to MaxNameLength characters, or "listener".
func listenerName(email string) string {
	local, _, _ := strings.Cut(email, "@")
	local = strings.TrimSpace(strings.Map(func(r rune) rune {
		if unicode.IsControl(r) {
			return -1
		}
		return r
	}, local))
	local = truncateRunes(local, MaxNameLength)
	if ValidateName(local) != nil {
		return "listener"
	}
	return local
}

// truncateRunes returns s cut to at most n characters.
func truncateRunes(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	return string(r[:n])
}
