package store

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"database/sql"
	"encoding/base64"
	"encoding/hex"
	"errors"
	"fmt"
	"strings"
	"time"
)

// ErrSessionNotFound is returned when no session has the given token or id.
var ErrSessionNotFound = errors.New("session not found")

// SessionTokenPrefix starts every session token.
const SessionTokenPrefix = "wns_"

// Session is a wn-server session of a first-party client.
type Session struct {
	ID         string // 16 hex characters
	AccountID  string
	Device     string // the client's label, or empty
	CreatedAt  time.Time
	LastSeenAt time.Time
}

// newSessionToken returns SessionTokenPrefix followed by 32 bytes from
// crypto/rand in base64url without padding.
func newSessionToken() (string, error) {
	b := make([]byte, 32)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return SessionTokenPrefix + base64.RawURLEncoding.EncodeToString(b), nil
}

// hashSessionToken returns the hex SHA-256 of a session token.
func hashSessionToken(token string) string {
	sum := sha256.Sum256([]byte(token))
	return hex.EncodeToString(sum[:])
}

// CreateSession creates a session for the account and returns its token.
// The token is returned only here. The store keeps its hash.
func (s *Store) CreateSession(ctx context.Context, accountID, device string) (string, Session, error) {
	id, err := newID()
	if err != nil {
		return "", Session{}, err
	}
	token, err := newSessionToken()
	if err != nil {
		return "", Session{}, err
	}
	now := time.Now().UTC().Truncate(time.Second)
	sess := Session{ID: id, AccountID: accountID, Device: device, CreatedAt: now, LastSeenAt: now}
	_, err = s.db.ExecContext(ctx,
		`INSERT INTO sessions (id, account_id, token_hash, device, created_at, last_seen_at) VALUES (?, ?, ?, ?, ?, ?)`,
		sess.ID, sess.AccountID, hashSessionToken(token), sess.Device, now.Format(time.RFC3339), now.Format(time.RFC3339))
	if err != nil {
		return "", Session{}, fmt.Errorf("create session: %w", err)
	}
	return token, sess, nil
}

const sessionColumns = `id, account_id, device, created_at, last_seen_at`

// scanner is satisfied by *sql.Row and *sql.Rows.
type scanner interface {
	Scan(dest ...any) error
}

func scanSession(row scanner) (Session, error) {
	var (
		sess              Session
		created, lastSeen string
	)
	if err := row.Scan(&sess.ID, &sess.AccountID, &sess.Device, &created, &lastSeen); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return Session{}, ErrSessionNotFound
		}
		return Session{}, err
	}
	var err error
	if sess.CreatedAt, err = time.Parse(time.RFC3339, created); err != nil {
		return Session{}, fmt.Errorf("session %s: bad created_at: %w", sess.ID, err)
	}
	if sess.LastSeenAt, err = time.Parse(time.RFC3339, lastSeen); err != nil {
		return Session{}, fmt.Errorf("session %s: bad last_seen_at: %w", sess.ID, err)
	}
	return sess, nil
}

// SessionByToken returns the session whose token hash is the hash of token.
// It returns ErrSessionNotFound for an unknown or revoked token.
func (s *Store) SessionByToken(ctx context.Context, token string) (Session, error) {
	if !strings.HasPrefix(token, SessionTokenPrefix) {
		return Session{}, ErrSessionNotFound
	}
	return scanSession(s.db.QueryRowContext(ctx,
		`SELECT `+sessionColumns+` FROM sessions WHERE token_hash = ?`, hashSessionToken(token)))
}

// TouchSession sets the last seen time of the session.
func (s *Store) TouchSession(ctx context.Context, id string, at time.Time) error {
	_, err := s.db.ExecContext(ctx, `UPDATE sessions SET last_seen_at = ? WHERE id = ?`,
		at.UTC().Format(time.RFC3339), id)
	return err
}

// Sessions returns the sessions of the account, oldest first.
func (s *Store) Sessions(ctx context.Context, accountID string) ([]Session, error) {
	rows, err := s.db.QueryContext(ctx,
		`SELECT `+sessionColumns+` FROM sessions WHERE account_id = ? ORDER BY created_at, rowid`, accountID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var list []Session
	for rows.Next() {
		sess, err := scanSession(rows)
		if err != nil {
			return nil, err
		}
		list = append(list, sess)
	}
	return list, rows.Err()
}

// RevokeSession deletes the session with the given id. It returns
// ErrSessionNotFound when there is none.
func (s *Store) RevokeSession(ctx context.Context, id string) error {
	res, err := s.db.ExecContext(ctx, `DELETE FROM sessions WHERE id = ?`, id)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return ErrSessionNotFound
	}
	return nil
}

// RevokeSessionByToken deletes the session whose token hash is the hash of
// token, and returns it. It returns ErrSessionNotFound when there is none.
func (s *Store) RevokeSessionByToken(ctx context.Context, token string) (Session, error) {
	if !strings.HasPrefix(token, SessionTokenPrefix) {
		return Session{}, ErrSessionNotFound
	}
	return scanSession(s.db.QueryRowContext(ctx,
		`DELETE FROM sessions WHERE token_hash = ? RETURNING `+sessionColumns, hashSessionToken(token)))
}
