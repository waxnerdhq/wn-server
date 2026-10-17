package match

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"sync"
	"time"
)

// Kind is the kind of a background job.
type Kind string

// Job kinds.
const (
	KindRun    Kind = "run"
	KindApply  Kind = "apply"
	KindRevert Kind = "revert"
)

// ErrJobRunning is returned by Start while another job runs.
var ErrJobRunning = errors.New("a match job is already running")

// Running describes the job that is running.
type Running struct {
	Kind Kind
	// EntryID is the match queue entry of an apply or revert job, and 0 for
	// a run.
	EntryID   int64
	StartedAt time.Time
}

// Result is the outcome of the last finished job.
type Result struct {
	Kind       Kind
	EntryID    int64
	StartedAt  time.Time
	FinishedAt time.Time
	// Summary is the one-line result. It can be set together with Error,
	// when part of the job succeeded.
	Summary string
	// Error is the failure message, or empty.
	Error string
}

// Status is the state of the job holder.
type Status struct {
	// Running is nil when no job runs.
	Running *Running
	// Last is the last finished job, or nil when there is none or the
	// result was already taken.
	Last *Result
}

// Jobs runs at most one background job at a time and keeps the result of
// the last one.
type Jobs struct {
	ctx context.Context
	log *slog.Logger
	now func() time.Time
	wg  sync.WaitGroup

	mu      sync.Mutex
	running *Running
	last    *Result
}

// NewJobs returns a job holder. ctx is the context that every job receives.
// A nil logger selects slog.Default.
func NewJobs(ctx context.Context, logger *slog.Logger) *Jobs {
	if logger == nil {
		logger = slog.Default()
	}
	return &Jobs{ctx: ctx, log: logger, now: time.Now}
}

// Start runs fn in the background as a job of the given kind. It returns
// ErrJobRunning when a job is running. fn returns the one-line summary and
// an error; both are kept as the job's result. A panic in fn becomes an
// error.
func (j *Jobs) Start(kind Kind, entryID int64, fn func(ctx context.Context) (string, error)) error {
	j.mu.Lock()
	if j.running != nil {
		j.mu.Unlock()
		return ErrJobRunning
	}
	started := j.now()
	j.running = &Running{Kind: kind, EntryID: entryID, StartedAt: started}
	j.wg.Add(1)
	j.mu.Unlock()

	go func() {
		defer j.wg.Done()
		summary, err := j.call(fn)
		res := &Result{Kind: kind, EntryID: entryID, StartedAt: started, FinishedAt: j.now(), Summary: summary}
		if err != nil {
			res.Error = err.Error()
		}
		j.mu.Lock()
		j.running = nil
		j.last = res
		j.mu.Unlock()
		if err != nil {
			j.log.Warn("match job failed", "kind", kind, "entry", entryID, "error", err)
		} else {
			j.log.Info("match job finished", "kind", kind, "entry", entryID, "summary", summary)
		}
	}()
	return nil
}

func (j *Jobs) call(fn func(ctx context.Context) (string, error)) (summary string, err error) {
	defer func() {
		if p := recover(); p != nil {
			err = fmt.Errorf("job panicked: %v", p)
		}
	}()
	return fn(j.ctx)
}

// Status returns the running job and the last result without changing them.
func (j *Jobs) Status() Status {
	j.mu.Lock()
	defer j.mu.Unlock()
	return j.status()
}

// Take returns the status like Status. When no job runs, it also forgets
// the last result, so the next call does not return it again.
func (j *Jobs) Take() Status {
	j.mu.Lock()
	defer j.mu.Unlock()
	s := j.status()
	if j.running == nil {
		j.last = nil
	}
	return s
}

func (j *Jobs) status() Status {
	var s Status
	if j.running != nil {
		r := *j.running
		s.Running = &r
	}
	if j.last != nil {
		l := *j.last
		s.Last = &l
	}
	return s
}

// Wait waits up to timeout for the running job to finish and reports
// whether it did.
func (j *Jobs) Wait(timeout time.Duration) bool {
	done := make(chan struct{})
	go func() {
		j.wg.Wait()
		close(done)
	}()
	select {
	case <-done:
		return true
	case <-time.After(timeout):
		return false
	}
}
