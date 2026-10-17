package store

import (
	"context"
	"path/filepath"
	"testing"
	"time"
)

func TestForgetPending(t *testing.T) {
	ctx := context.Background()
	s, err := Open(ctx, filepath.Join(t.TempDir(), FileName))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	now := time.Date(2026, 10, 17, 12, 0, 0, 0, time.UTC)
	a, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/a", Decision: LineApplied, ReleaseID: "r1", CopiedTo: "/s/a"}, now)
	if err != nil {
		t.Fatal(err)
	}
	b, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/b", Decision: LineApplied, ReleaseID: "r2", CopiedTo: "/s/b"}, now)
	if err != nil {
		t.Fatal(err)
	}
	if got, err := s.ForgetPending(ctx); err != nil || len(got) != 0 {
		t.Fatalf("ForgetPending before a revert = %v, %v", got, err)
	}
	for _, id := range []int64{a, b} {
		if err := s.RevertMatch(ctx, id); err != nil {
			t.Fatal(err)
		}
	}
	got, err := s.ForgetPending(ctx)
	if err != nil || len(got) != 2 || got[0] != (ForgetFolder{a, "/m/a"}) || got[1] != (ForgetFolder{b, "/m/b"}) {
		t.Fatalf("ForgetPending after reverts = %v, %v", got, err)
	}
	if err := s.ClearForgetPending(ctx, []int64{a, 999}); err != nil {
		t.Fatal(err)
	}
	if err := s.ClearForgetPending(ctx, nil); err != nil {
		t.Fatal(err)
	}
	got, err = s.ForgetPending(ctx)
	if err != nil || len(got) != 1 || got[0].ID != b {
		t.Fatalf("ForgetPending after clearing %d = %v, %v", a, got, err)
	}
}
