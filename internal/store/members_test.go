package store

import (
	"context"
	"errors"
	"path/filepath"
	"testing"
)

func TestCreateListener(t *testing.T) {
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
	// A name taken by the owner gets a suffix, without regard to case.
	a, err := s.CreateListener(ctx, "u1", "Owner@example.com")
	if err != nil || a.Name != "Owner-2" || a.Kind != KindListener || a.AppPassword == "" || len(a.AppPassword) != 64 {
		t.Fatalf("CreateListener = %+v, %v", a, err)
	}
	// The same user gets the same account.
	if b, err := s.CreateListener(ctx, "u1", "other@example.com"); err != nil || b.ID != a.ID {
		t.Fatalf("second CreateListener = %+v, %v", b, err)
	}
	if b, err := s.CreateListener(ctx, "u2", "@example.com"); err != nil || b.Name != "listener" {
		t.Fatalf("CreateListener without a local part = %+v, %v", b, err)
	}
	if got, err := s.Owner(ctx); err != nil || got.ID != owner.ID {
		t.Fatalf("Owner = %+v, %v", got, err)
	}
	if got, err := s.ListenerByUserID(ctx, "u1"); err != nil || got.ID != a.ID {
		t.Fatalf("ListenerByUserID = %+v, %v", got, err)
	}
	if _, err := s.ListenerByUserID(ctx, "u3"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("ListenerByUserID of a stranger = %v", err)
	}
}
