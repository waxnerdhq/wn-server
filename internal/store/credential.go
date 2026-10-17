package store

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"time"
)

// ErrNoServerCredential is returned when the store holds no server token,
// which means wn-server login has not run or wn-server logout has.
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

// setServerCredential stores c as the server credential, replacing any
// stored one. A zero CreatedAt is set to the current time.
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
// ErrNoServerCredential when no row exists or its token is empty.
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
	if c.Token == "" {
		return ServerCredential{}, ErrNoServerCredential
	}
	if c.CreatedAt, err = time.Parse(time.RFC3339, created); err != nil {
		return ServerCredential{}, fmt.Errorf("server credential: bad created_at: %w", err)
	}
	return c, nil
}

// ServerRegistration returns the server id and the waxnerd user id that the
// store holds, also after logout has cleared the token. found is false when
// wn-server login has never stored them.
func (s *Store) ServerRegistration(ctx context.Context) (serverID, waxnerdUserID string, found bool, err error) {
	err = s.db.QueryRowContext(ctx,
		`SELECT server_id, waxnerd_user_id FROM server_credential WHERE id = 1`).
		Scan(&serverID, &waxnerdUserID)
	if errors.Is(err, sql.ErrNoRows) {
		return "", "", false, nil
	}
	if err != nil {
		return "", "", false, err
	}
	return serverID, waxnerdUserID, true, nil
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
// to c.WaxnerdUserID, in one transaction. When the stored registration
// belongs to another waxnerd user, or there is none, it also queues every
// rating of the account in the outbox, so that they become the new user's
// ratings at wn-cloud. Either way every outbox entry that waits for a retry
// becomes due at once (README, Authentication, step 8).
func (s *Store) Link(ctx context.Context, accountID string, c ServerCredential) error {
	return s.inTx(ctx, func(tx *sql.Tx) error {
		var prev string
		err := tx.QueryRowContext(ctx, `SELECT waxnerd_user_id FROM server_credential WHERE id = 1`).Scan(&prev)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return err
		}
		if err := setServerCredential(ctx, tx, c); err != nil {
			return err
		}
		if err := linkOwner(ctx, tx, accountID, c.WaxnerdUserID); err != nil {
			return err
		}
		if prev != c.WaxnerdUserID {
			return queueAllRatings(ctx, tx, accountID, time.Now())
		}
		return retryOutboxNow(ctx, tx, time.Now())
	})
}

// Unlink clears the server token and the account's waxnerd user id, in one
// transaction. The server id and the waxnerd user id of the registration
// stay, so that a later login by the same user reuses the server id.
func (s *Store) Unlink(ctx context.Context, accountID string) error {
	return s.inTx(ctx, func(tx *sql.Tx) error {
		if _, err := tx.ExecContext(ctx, `UPDATE server_credential SET token = ''`); err != nil {
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
