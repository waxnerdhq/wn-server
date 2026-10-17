package match

import (
	"context"
	"errors"
	"log/slog"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/store"
)

func TestJobsOneAtATime(t *testing.T) {
	j := NewJobs(context.Background(), slog.New(slog.DiscardHandler))
	release := make(chan struct{})
	if err := j.Start(KindApply, 7, func(context.Context) (string, error) {
		<-release
		return "done", nil
	}); err != nil {
		t.Fatal(err)
	}
	if err := j.Start(KindRun, 0, func(context.Context) (string, error) { return "", nil }); !errors.Is(err, ErrJobRunning) {
		t.Fatalf("second Start = %v, want ErrJobRunning", err)
	}
	if s := j.Status(); s.Running == nil || s.Running.Kind != KindApply || s.Running.EntryID != 7 || s.Last != nil {
		t.Fatalf("status while running: %+v", s)
	}
	close(release)
	if !j.Wait(5 * time.Second) {
		t.Fatal("job did not finish")
	}
	s := j.Take()
	if s.Running != nil || s.Last == nil || s.Last.Summary != "done" || s.Last.Error != "" {
		t.Fatalf("status after the job: %+v", s)
	}
	if j.Take().Last != nil {
		t.Fatal("Take returned the result twice")
	}
}

func TestJobsFailureAndPanic(t *testing.T) {
	j := NewJobs(context.Background(), slog.New(slog.DiscardHandler))
	for name, fn := range map[string]func(context.Context) (string, error){
		"error": func(context.Context) (string, error) { return "", errors.New("another beets run is in progress") },
		"panic": func(context.Context) (string, error) { panic("boom") },
	} {
		if err := j.Start(KindRun, 0, fn); err != nil {
			t.Fatal(err)
		}
		j.Wait(5 * time.Second)
		if s := j.Take(); s.Last == nil || s.Last.Error == "" {
			t.Fatalf("%s: status %+v", name, s)
		}
	}
}

func TestCheckApply(t *testing.T) {
	const id = "1a2b3c4d-5e6f-7081-92a3-b4c5d6e7f809"
	e := store.MatchEntry{ID: 4, State: store.MatchPending, Candidates: []store.MatchCandidate{{ReleaseID: id}}}
	if got, err := CheckApply(e, ""); err != nil || got != id {
		t.Fatalf("default release = %q, %v", got, err)
	}
	if _, err := CheckApply(e, "not-a-uuid"); err == nil {
		t.Fatal("accepted a release id that is not a UUID")
	}
	e.State = store.MatchApplied
	var refused *RefusedError
	if _, err := CheckApply(e, ""); !errors.As(err, &refused) {
		t.Fatalf("applied entry: %v, want a RefusedError", err)
	}
	e.State = store.MatchDismissed
	e.Candidates = nil
	if _, err := CheckApply(e, ""); err == nil {
		t.Fatal("accepted an entry without candidates and without a release")
	}
}
