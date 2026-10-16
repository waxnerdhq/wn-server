package store

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"time"
)

// ErrNoServerCredential is returned when the store holds no server
// credential, which means wn-server login has not run.
var ErrNoServerCredential = errors.New("no server credential")

// ServerCredential is the owner's wn-cloud CLI token with the server id and
// the waxnerd user it belongs to.
type ServerCredential struct {
	Token         string
	ServerID      string
	WaxnerdUserID string
	CreatedAt     time.Time
}

// execer is satisfied by *sql.DB and *sql.Tx.
type execer interface {
	ExecContext(ctx context.Context, query string, args ...any) (sql.Result, error)
}

// SetServerCredential stores c as the server credential, replacing any
// stored one. A zero CreatedAt is set to the current time.
func (s *Store) SetServerCredential(ctx context.Context, c ServerCredential) error {
	return setServerCredential(ctx, s.db, c)
}

func setServerCredential(ctx context.Context, db execer, c ServerCredential) error {
	if c.CreatedAt.IsZero() {
		c.CreatedAt = time.Now()
	}
	_, err := db.ExecContext(ctx,
		`INSERT OR REPLACE INTO server_credential (id, token, server_id, waxnerd_user_id, created_at) VALUES (1, ?, ?, ?, ?)`,
		c.Token, c.ServerID, c.WaxnerdUserID, c.CreatedAt.UTC().Format(time.RFC3339))
	if err != nil {
		return fmt.Errorf("set server credential: %w", err)
	}
	return nil
}

// ServerCredential returns the stored server credential, or
// ErrNoServerCredential.
func (s *Store) ServerCredential(ctx context.Context) (ServerCredential, error) {
	var (
		c       ServerCredential
		created string
	)
	err := s.db.QueryRowContext(ctx,
		`SELECT token, server_id, waxnerd_user_id, created_at FROM server_credential WHERE id = 1`).
		Scan(&c.Token, &c.ServerID, &c.WaxnerdUserID, &created)
	if errors.Is(err, sql.ErrNoRows) {
		return ServerCredential{}, ErrNoServerCredential
	}
	if err != nil {
		return ServerCredential{}, err
	}
	if c.CreatedAt, err = time.Parse(time.RFC3339, created); err != nil {
		return ServerCredential{}, fmt.Errorf("server credential: bad created_at: %w", err)
	}
	return c, nil
}

// DeleteServerCredential removes the server credential. It does nothing
// when none is stored.
func (s *Store) DeleteServerCredential(ctx context.Context) error {
	_, err := s.db.ExecContext(ctx, `DELETE FROM server_credential`)
	return err
}

// LinkOwner sets the waxnerd user id of the account. An empty id clears it.
func (s *Store) LinkOwner(ctx context.Context, accountID, waxnerdUserID string) error {
	return linkOwner(ctx, s.db, accountID, waxnerdUserID)
}

func linkOwner(ctx context.Context, db execer, accountID, waxnerdUserID string) error {
	id := sql.NullString{String: waxnerdUserID, Valid: waxnerdUserID != ""}
	res, err := db.ExecContext(ctx, `UPDATE accounts SET waxnerd_user_id = ? WHERE id = ?`, id, accountID)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return ErrNotFound
	}
	return nil
}

// Link stores the server credential and sets the account's waxnerd user id
// to c.WaxnerdUserID, in one transaction.
func (s *Store) Link(ctx context.Context, accountID string, c ServerCredential) error {
	return s.inTx(ctx, func(tx *sql.Tx) error {
		if err := setServerCredential(ctx, tx, c); err != nil {
			return err
		}
		return linkOwner(ctx, tx, accountID, c.WaxnerdUserID)
	})
}

// Unlink removes the server credential and clears the account's waxnerd
// user id, in one transaction.
func (s *Store) Unlink(ctx context.Context, accountID string) error {
	return s.inTx(ctx, func(tx *sql.Tx) error {
		if _, err := tx.ExecContext(ctx, `DELETE FROM server_credential`); err != nil {
			return err
		}
		return linkOwner(ctx, tx, accountID, "")
	})
}

func (s *Store) inTx(ctx context.Context, f func(*sql.Tx) error) error {
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer tx.Rollback() //nolint:errcheck
	if err := f(tx); err != nil {
		return err
	}
	return tx.Commit()
}
