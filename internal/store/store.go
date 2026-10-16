// Package store is wn-server's SQLite database. It holds the owner account.
// Later migrations add sessions, the match queue, ratings, and the outbox.
package store

import (
	"context"
	"crypto/rand"
	"database/sql"
	"embed"
	"encoding/hex"
	"errors"
	"fmt"
	"io/fs"
	"math/big"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"

	_ "modernc.org/sqlite" // registers the "sqlite" driver
)

// FileName is the database file inside the data directory.
const FileName = "wn-server.db"

// Path returns the database path for a data directory.
func Path(dataDir string) string { return filepath.Join(dataDir, FileName) }

var (
	// ErrNotFound is returned when an account does not exist.
	ErrNotFound = errors.New("account not found")
	// ErrOwnerExists is returned by CreateOwner when the store has an owner.
	ErrOwnerExists = errors.New("owner account already exists")
)

//go:embed migrations/*.sql
var migrationFS embed.FS

// Account is a wn-server account. v1 has one, the owner.
type Account struct {
	ID            string // 16 hex characters; Navidrome's Remote-User value
	Name          string // Subsonic user name
	AppPassword   string
	WaxnerdUserID string // empty until wn-server login links the account
	CreatedAt     time.Time
}

// Store is an open wn-server database.
type Store struct {
	db *sql.DB
}

// Open opens or creates the database at path with mode 0600 and applies
// pending migrations.
func Open(ctx context.Context, path string) (*Store, error) {
	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE, 0o600)
	if err != nil {
		return nil, fmt.Errorf("open store: %w", err)
	}
	f.Close()
	if err := os.Chmod(path, 0o600); err != nil {
		return nil, fmt.Errorf("open store: %w", err)
	}
	dsn := (&url.URL{
		Scheme:   "file",
		Path:     path,
		RawQuery: "_pragma=journal_mode(WAL)&_pragma=busy_timeout(5000)",
	}).String()
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, fmt.Errorf("open store: %w", err)
	}
	s := &Store{db: db}
	if err := s.Migrate(ctx); err != nil {
		db.Close()
		return nil, err
	}
	return s, nil
}

// Close closes the database.
func (s *Store) Close() error { return s.db.Close() }

// Migrate applies every embedded migration newer than the recorded schema
// version. Each migration runs in its own transaction. Files are named
// NNN_name.sql and applied in numeric order.
func (s *Store) Migrate(ctx context.Context) error {
	if _, err := s.db.ExecContext(ctx, `CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)`); err != nil {
		return fmt.Errorf("migrate: %w", err)
	}
	var current int
	err := s.db.QueryRowContext(ctx, `SELECT COALESCE(MAX(version), 0) FROM schema_version`).Scan(&current)
	if err != nil {
		return fmt.Errorf("migrate: read version: %w", err)
	}
	names, err := fs.Glob(migrationFS, "migrations/*.sql")
	if err != nil {
		return err
	}
	sort.Strings(names)
	for _, name := range names {
		base := filepath.Base(name)
		n, err := strconv.Atoi(strings.SplitN(base, "_", 2)[0])
		if err != nil {
			return fmt.Errorf("migrate: bad migration name %s", base)
		}
		if n <= current {
			continue
		}
		body, err := migrationFS.ReadFile(name)
		if err != nil {
			return err
		}
		if err := s.apply(ctx, n, string(body)); err != nil {
			return fmt.Errorf("migrate %s: %w", base, err)
		}
	}
	return nil
}

func (s *Store) apply(ctx context.Context, version int, body string) error {
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer tx.Rollback() //nolint:errcheck
	if _, err := tx.ExecContext(ctx, body); err != nil {
		return err
	}
	if _, err := tx.ExecContext(ctx, `DELETE FROM schema_version`); err != nil {
		return err
	}
	if _, err := tx.ExecContext(ctx, `INSERT INTO schema_version (version) VALUES (?)`, version); err != nil {
		return err
	}
	return tx.Commit()
}

// CreateOwner creates the owner account with a new app password. It returns
// ErrOwnerExists when the store already has an account.
func (s *Store) CreateOwner(ctx context.Context, name string) (Account, error) {
	if strings.TrimSpace(name) == "" {
		return Account{}, errors.New("owner name is empty")
	}
	id, err := newID()
	if err != nil {
		return Account{}, err
	}
	pw, err := NewAppPassword()
	if err != nil {
		return Account{}, err
	}
	a := Account{ID: id, Name: name, AppPassword: pw, CreatedAt: time.Now().UTC().Truncate(time.Second)}
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return Account{}, err
	}
	defer tx.Rollback() //nolint:errcheck
	var n int
	if err := tx.QueryRowContext(ctx, `SELECT COUNT(*) FROM accounts`).Scan(&n); err != nil {
		return Account{}, err
	}
	if n > 0 {
		return Account{}, ErrOwnerExists
	}
	_, err = tx.ExecContext(ctx,
		`INSERT INTO accounts (id, name, app_password, waxnerd_user_id, created_at) VALUES (?, ?, ?, NULL, ?)`,
		a.ID, a.Name, a.AppPassword, a.CreatedAt.Format(time.RFC3339))
	if err != nil {
		return Account{}, fmt.Errorf("create owner: %w", err)
	}
	return a, tx.Commit()
}

// Owner returns the owner account, which is the first account created.
func (s *Store) Owner(ctx context.Context) (Account, error) {
	return s.one(ctx, `ORDER BY created_at, rowid LIMIT 1`)
}

// AccountByName returns the account with the given name. Names compare
// without regard to ASCII case, as Navidrome compares user names.
func (s *Store) AccountByName(ctx context.Context, name string) (Account, error) {
	return s.one(ctx, `WHERE name = ?`, name)
}

func (s *Store) one(ctx context.Context, clause string, args ...any) (Account, error) {
	var (
		a       Account
		waxnerd sql.NullString
		created string
	)
	err := s.db.QueryRowContext(ctx,
		`SELECT id, name, app_password, waxnerd_user_id, created_at FROM accounts `+clause, args...).
		Scan(&a.ID, &a.Name, &a.AppPassword, &waxnerd, &created)
	if errors.Is(err, sql.ErrNoRows) {
		return Account{}, ErrNotFound
	}
	if err != nil {
		return Account{}, err
	}
	a.WaxnerdUserID = waxnerd.String
	if a.CreatedAt, err = time.Parse(time.RFC3339, created); err != nil {
		return Account{}, fmt.Errorf("account %s: bad created_at: %w", a.ID, err)
	}
	return a, nil
}

// SetAppPassword replaces the app password of the account with the given id.
func (s *Store) SetAppPassword(ctx context.Context, id, password string) error {
	res, err := s.db.ExecContext(ctx, `UPDATE accounts SET app_password = ? WHERE id = ?`, password, id)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return ErrNotFound
	}
	return nil
}

// appPasswordAlphabet has no 0, O, 1, l, or I.
const appPasswordAlphabet = "23456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

// AppPasswordLength is the number of characters in an app password.
const AppPasswordLength = 20

// NewAppPassword returns a random app password from crypto/rand.
func NewAppPassword() (string, error) {
	limit := big.NewInt(int64(len(appPasswordAlphabet)))
	b := make([]byte, AppPasswordLength)
	for i := range b {
		n, err := rand.Int(rand.Reader, limit)
		if err != nil {
			return "", err
		}
		b[i] = appPasswordAlphabet[n.Int64()]
	}
	return string(b), nil
}

func newID() (string, error) {
	b := make([]byte, 8)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return hex.EncodeToString(b), nil
}
