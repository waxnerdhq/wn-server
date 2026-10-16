package store

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestStore(t *testing.T) {
	ctx := context.Background()
	path := filepath.Join(t.TempDir(), FileName)
	s, err := Open(ctx, path)
	if err != nil {
		t.Fatal(err)
	}
	if info, err := os.Stat(path); err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("stat = %v, %v; want mode 0600", info, err)
	}
	if _, err := s.Owner(ctx); !errors.Is(err, ErrNotFound) {
		t.Fatalf("Owner on empty store = %v, want ErrNotFound", err)
	}
	owner, err := s.CreateOwner(ctx, "owner")
	if err != nil {
		t.Fatal(err)
	}
	if len(owner.ID) != 16 || len(owner.AppPassword) != AppPasswordLength {
		t.Fatalf("owner = %+v", owner)
	}
	if _, err := s.CreateOwner(ctx, "other"); !errors.Is(err, ErrOwnerExists) {
		t.Fatalf("second CreateOwner = %v, want ErrOwnerExists", err)
	}
	// The database is the file at path, in WAL mode.
	if _, err := os.Stat(path + "-wal"); err != nil {
		t.Fatalf("no WAL file next to %s: %v", path, err)
	}
	s.Close()

	// Reopen: migrations do not run twice and the owner persists.
	s, err = Open(ctx, path)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	got, err := s.Owner(ctx)
	if err != nil || got != owner {
		t.Fatalf("Owner = %+v, %v; want %+v", got, err, owner)
	}
	if got, err := s.AccountByName(ctx, "OWNER"); err != nil || got.ID != owner.ID {
		t.Fatalf("AccountByName = %+v, %v", got, err)
	}
	if _, err := s.AccountByName(ctx, "nobody"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("AccountByName(nobody) = %v, want ErrNotFound", err)
	}

	pw, err := NewAppPassword()
	if err != nil {
		t.Fatal(err)
	}
	if err := s.SetAppPassword(ctx, owner.ID, pw); err != nil {
		t.Fatal(err)
	}
	if got, _ := s.Owner(ctx); got.AppPassword != pw {
		t.Fatalf("password after reset = %q, want %q", got.AppPassword, pw)
	}
}

func TestNewAppPassword(t *testing.T) {
	pw, err := NewAppPassword()
	if err != nil {
		t.Fatal(err)
	}
	if len(pw) != AppPasswordLength || strings.ContainsAny(pw, "0O1lI") {
		t.Fatalf("password %q", pw)
	}
}
