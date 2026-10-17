package store

import (
	"context"
	"errors"
	"path/filepath"
	"testing"
	"time"
)

func TestMatchQueue(t *testing.T) {
	ctx := context.Background()
	path := filepath.Join(t.TempDir(), FileName)
	s, err := Open(ctx, path)
	if err != nil {
		t.Fatal(err)
	}
	t0 := time.Date(2026, 10, 16, 12, 0, 0, 0, time.UTC)
	t1 := t0.Add(time.Hour)
	candA := MatchCandidate{ReleaseID: "a", Artist: "Ar", Album: "Al", Year: 2001, Label: "L", Distance: 0.2}
	candB := MatchCandidate{ReleaseID: "b", Distance: 0.3}

	id, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/x", Decision: LineSkipped, Artist: "Ar", TrackCount: 9, Candidates: []MatchCandidate{candA, candB}}, t0)
	if err != nil {
		t.Fatal(err)
	}
	// A second skipped line replaces the candidates and keeps first_seen.
	if id2, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/x", Decision: LineSkipped, Artist: "Ar", TrackCount: 9, Candidates: []MatchCandidate{candB}}, t1); err != nil || id2 != id {
		t.Fatalf("second RecordMatch = %d, %v; want %d", id2, err, id)
	}
	e, err := s.MatchEntry(ctx, id)
	if err != nil {
		t.Fatal(err)
	}
	if e.State != MatchPending || !e.FirstSeen.Equal(t0) || !e.LastSeen.Equal(t1) || e.TrackCount != 9 ||
		len(e.Candidates) != 1 || e.Candidates[0] != candB {
		t.Fatalf("entry after re-skip = %+v", e)
	}

	// An error line keeps the state and candidates and stores the message.
	if _, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/x", Decision: LineError, Error: "boom"}, t1); err != nil {
		t.Fatal(err)
	}
	// Dismissed stays dismissed on a later skip, with fresh candidates.
	if err := s.SetMatchState(ctx, id, MatchDismissed); err != nil {
		t.Fatal(err)
	}
	if _, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/x", Decision: LineSkipped, Candidates: []MatchCandidate{candA}}, t1); err != nil {
		t.Fatal(err)
	}
	if e, _ := s.MatchEntry(ctx, id); e.State != MatchDismissed || len(e.Candidates) != 1 || e.Candidates[0] != candA || e.LastError != "" {
		t.Fatalf("dismissed entry after skip = %+v", e)
	}

	// Applied, then a duplicate line: the entry stays applied.
	if _, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/x", Decision: LineApplied, ReleaseID: "a", CopiedTo: "/s/x", Candidates: []MatchCandidate{candA}}, t1); err != nil {
		t.Fatal(err)
	}
	if _, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/x", Decision: LineDuplicate, Candidates: []MatchCandidate{candA}}, t1); err != nil {
		t.Fatal(err)
	}
	e, _ = s.MatchEntry(ctx, id)
	if e.State != MatchApplied || e.AppliedReleaseID != "a" || e.CopiedTo != "/s/x" || !e.AppliedAt.Equal(t1) {
		t.Fatalf("applied entry = %+v", e)
	}
	if err := s.SetMatchState(ctx, id, MatchApplied); err == nil {
		t.Fatal("SetMatchState(applied) succeeded")
	}

	// A duplicate line on a new folder, and an error line on a new folder.
	dup, _ := s.RecordMatch(ctx, MatchLine{Folder: "/m/dup", Decision: LineDuplicate}, t0)
	errID, _ := s.RecordMatch(ctx, MatchLine{Folder: "/m/err", Decision: LineError, Error: "bad"}, t0)
	if n, _ := s.CountMatches(ctx, MatchPending); n != 1 {
		t.Fatalf("pending count = %d, want 1", n)
	}
	s.Close()

	// Entries survive reopening the store.
	s, err = Open(ctx, path)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	all, err := s.MatchEntries(ctx)
	if err != nil || len(all) != 3 {
		t.Fatalf("MatchEntries = %d entries, %v", len(all), err)
	}
	if all[0].ID != id || all[1].ID != dup || all[1].State != MatchDuplicate || all[2].ID != errID || all[2].LastError != "bad" {
		t.Fatalf("entries after reopen = %+v", all)
	}
	if len(all[0].Candidates) != 1 || all[0].Candidates[0] != candA {
		t.Fatalf("candidates after reopen = %+v", all[0].Candidates)
	}

	if err := s.RevertMatch(ctx, id); err != nil {
		t.Fatal(err)
	}
	pending, err := s.MatchEntries(ctx, MatchPending)
	if err != nil || len(pending) != 2 || pending[0].ID != id || pending[0].CopiedTo != "" || !pending[0].AppliedAt.IsZero() {
		t.Fatalf("pending after revert = %+v, %v", pending, err)
	}
	if err := s.SetMatchState(ctx, 999, MatchDismissed); !errors.Is(err, ErrMatchNotFound) {
		t.Fatalf("SetMatchState(999) = %v, want ErrMatchNotFound", err)
	}
	if _, err := s.MatchEntry(ctx, 999); !errors.Is(err, ErrMatchNotFound) {
		t.Fatalf("MatchEntry(999) = %v, want ErrMatchNotFound", err)
	}
}

func TestMatchMarkerAndNoteError(t *testing.T) {
	ctx := context.Background()
	s, err := Open(ctx, filepath.Join(t.TempDir(), FileName))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	t0 := time.Date(2026, 10, 16, 12, 0, 0, 0, time.UTC)
	cand := MatchCandidate{ReleaseID: "a", Distance: 0.2}
	id, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/x", Decision: LineSkipped, Artist: "Ar", Candidates: []MatchCandidate{cand}}, t0)
	if err != nil {
		t.Fatal(err)
	}
	// NoteMatchError keeps state, tags, and candidates.
	if err := s.NoteMatchError(ctx, id, "beets skipped it", t0.Add(time.Hour)); err != nil {
		t.Fatal(err)
	}
	e, _ := s.MatchEntry(ctx, id)
	if e.State != MatchPending || e.Artist != "Ar" || len(e.Candidates) != 1 || e.LastError != "beets skipped it" || !e.LastSeen.Equal(t0.Add(time.Hour)) {
		t.Fatalf("entry after NoteMatchError = %+v", e)
	}
	if err := s.NoteMatchError(ctx, 999, "x", t0); !errors.Is(err, ErrMatchNotFound) {
		t.Fatalf("NoteMatchError(999) = %v", err)
	}

	if _, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/x", Decision: LineApplied, ReleaseID: "a", CopiedTo: "/s/x", MarkerWritten: true}, t0); err != nil {
		t.Fatal(err)
	}
	if e, _ := s.MatchEntry(ctx, id); !e.MarkerWritten {
		t.Fatal("marker_written not set by the applied line")
	}
	// A later applied line without a new marker keeps marker_written.
	if _, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/x", Decision: LineApplied, ReleaseID: "a", CopiedTo: "/s/x"}, t0); err != nil {
		t.Fatal(err)
	}
	if e, _ := s.MatchEntry(ctx, id); !e.MarkerWritten {
		t.Fatal("marker_written cleared by a second applied line")
	}
	if err := s.RevertMatch(ctx, id); err != nil {
		t.Fatal(err)
	}
	if e, _ := s.MatchEntry(ctx, id); e.MarkerWritten {
		t.Fatal("marker_written still set after revert")
	}
}

func TestMarkAppliedKeepsCandidates(t *testing.T) {
	ctx := context.Background()
	s, err := Open(ctx, filepath.Join(t.TempDir(), FileName))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	t0 := time.Date(2026, 10, 16, 12, 0, 0, 0, time.UTC)
	t1 := t0.Add(time.Hour)
	cands := []MatchCandidate{{ReleaseID: "a", Distance: 0.2}, {ReleaseID: "b", Distance: 0.3}}
	id, err := s.RecordMatch(ctx, MatchLine{Folder: "/m/x", Decision: LineSkipped, Artist: "Ar", Album: "Al", Year: 2001, TrackCount: 9, Candidates: cands}, t0)
	if err != nil {
		t.Fatal(err)
	}
	if err := s.NoteMatchError(ctx, id, "earlier failure", t0); err != nil {
		t.Fatal(err)
	}
	check := func(when string, e MatchEntry) {
		t.Helper()
		if e.Artist != "Ar" || e.Album != "Al" || e.Year != 2001 || e.TrackCount != 9 || len(e.Candidates) != 2 || e.Candidates[0] != cands[0] || e.Candidates[1] != cands[1] {
			t.Fatalf("tags or candidates changed %s: %+v", when, e)
		}
	}

	if err := s.MarkApplied(ctx, id, "b", "/s/x", true, t1); err != nil {
		t.Fatal(err)
	}
	e, _ := s.MatchEntry(ctx, id)
	if e.State != MatchApplied || e.AppliedReleaseID != "b" || e.CopiedTo != "/s/x" || !e.AppliedAt.Equal(t1) || !e.LastSeen.Equal(t1) || !e.MarkerWritten || e.LastError != "" {
		t.Fatalf("entry after MarkApplied = %+v", e)
	}
	check("by MarkApplied", e)

	if err := s.RevertMatch(ctx, id); err != nil {
		t.Fatal(err)
	}
	e, _ = s.MatchEntry(ctx, id)
	if e.State != MatchPending {
		t.Fatalf("state after revert = %s", e.State)
	}
	check("by the revert", e)

	if err := s.MarkApplied(ctx, 999, "a", "/s/y", false, t1); !errors.Is(err, ErrMatchNotFound) {
		t.Fatalf("MarkApplied(999) = %v, want ErrMatchNotFound", err)
	}
}
