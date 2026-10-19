package store

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
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

func TestServerCredential(t *testing.T) {
	ctx := context.Background()
	s, err := Open(ctx, filepath.Join(t.TempDir(), FileName))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	owner, err := s.CreateOwner(ctx, "owner")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := s.ServerCredential(ctx); !errors.Is(err, ErrNoServerCredential) {
		t.Fatalf("ServerCredential on empty store = %v, want ErrNoServerCredential", err)
	}
	if _, _, found, err := s.ServerRegistration(ctx); err != nil || found {
		t.Fatalf("ServerRegistration on empty store = %v, %v; want not found", found, err)
	}
	want := ServerCredential{Token: "wn_t", ServerID: "srv", WaxnerdUserID: "user", CreatedAt: time.Date(2026, 10, 16, 12, 0, 0, 0, time.UTC)}
	if err := s.Link(ctx, owner.ID, want); err != nil {
		t.Fatal(err)
	}
	if got, err := s.ServerCredential(ctx); err != nil || got != want {
		t.Fatalf("ServerCredential = %+v, %v; want %+v", got, err, want)
	}
	if got, _ := s.Owner(ctx); got.WaxnerdUserID != "user" {
		t.Fatalf("owner waxnerd user id = %q", got.WaxnerdUserID)
	}
	if err := s.Unlink(ctx, owner.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := s.ServerCredential(ctx); !errors.Is(err, ErrNoServerCredential) {
		t.Fatalf("ServerCredential after Unlink = %v", err)
	}
	if id, user, found, err := s.ServerRegistration(ctx); err != nil || !found || id != "srv" || user != "user" {
		t.Fatalf("ServerRegistration after Unlink = %q, %q, %v, %v", id, user, found, err)
	}
	if got, _ := s.Owner(ctx); got.WaxnerdUserID != "" {
		t.Fatalf("owner waxnerd user id after Unlink = %q", got.WaxnerdUserID)
	}
	if err := s.LinkOwner(ctx, "nobody", "user"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("LinkOwner(nobody) = %v, want ErrNotFound", err)
	}
}

func TestSessions(t *testing.T) {
	ctx := context.Background()
	s, err := Open(ctx, filepath.Join(t.TempDir(), FileName))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	owner, err := s.CreateOwner(ctx, "owner")
	if err != nil {
		t.Fatal(err)
	}
	token, created, err := s.CreateSession(ctx, owner.ID, "phone")
	if err != nil {
		t.Fatal(err)
	}
	// The prefix and 43 base64url characters for 32 bytes.
	if !strings.HasPrefix(token, SessionTokenPrefix) || len(token) != len(SessionTokenPrefix)+43 {
		t.Fatalf("token %q", token)
	}
	got, err := s.SessionByToken(ctx, token)
	created.AccountKind = KindOwner
	if err != nil || got != created || got.AccountID != owner.ID || got.Device != "phone" {
		t.Fatalf("SessionByToken = %+v, %v; want %+v", got, err, created)
	}
	for _, unknown := range []string{"", "wns_unknown", token + "x", strings.TrimPrefix(token, SessionTokenPrefix)} {
		if _, err := s.SessionByToken(ctx, unknown); !errors.Is(err, ErrSessionNotFound) {
			t.Fatalf("SessionByToken(%q) = %v, want ErrSessionNotFound", unknown, err)
		}
	}
	later := created.LastSeenAt.Add(time.Hour)
	if err := s.TouchSession(ctx, created.ID, later); err != nil {
		t.Fatal(err)
	}
	if got, _ := s.SessionByToken(ctx, token); !got.LastSeenAt.Equal(later) {
		t.Fatalf("last seen %v, want %v", got.LastSeenAt, later)
	}

	token2, second, err := s.CreateSession(ctx, owner.ID, "")
	if err != nil {
		t.Fatal(err)
	}
	if list, err := s.Sessions(ctx, owner.ID); err != nil || len(list) != 2 || list[0].ID != created.ID || list[1].ID != second.ID {
		t.Fatalf("Sessions = %+v, %v", list, err)
	}
	if err := s.RevokeSession(ctx, created.ID); err != nil {
		t.Fatal(err)
	}
	if err := s.RevokeSession(ctx, created.ID); !errors.Is(err, ErrSessionNotFound) {
		t.Fatalf("second RevokeSession = %v, want ErrSessionNotFound", err)
	}
	if _, err := s.SessionByToken(ctx, token); !errors.Is(err, ErrSessionNotFound) {
		t.Fatalf("SessionByToken after revoke = %v", err)
	}
	if got, err := s.RevokeSessionByToken(ctx, token2); err != nil || got.ID != second.ID {
		t.Fatalf("RevokeSessionByToken = %+v, %v", got, err)
	}
	if _, err := s.RevokeSessionByToken(ctx, token2); !errors.Is(err, ErrSessionNotFound) {
		t.Fatalf("second RevokeSessionByToken = %v, want ErrSessionNotFound", err)
	}
	if list, err := s.Sessions(ctx, owner.ID); err != nil || len(list) != 0 {
		t.Fatalf("Sessions after revoke = %+v, %v", list, err)
	}
}
