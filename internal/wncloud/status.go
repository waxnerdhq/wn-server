package wncloud

import (
	"sync"
	"time"
)

// Status records whether wn-cloud answered the last call that wn-server made
// to it, and whether it accepted the server credential. The session exchange,
// the outbox drain, and the rating reconcile update it, and the web UI reads
// it. The zero value is reachable with an accepted credential. A nil *Status
// ignores updates and reports reachable.
type Status struct {
	mu       sync.Mutex
	since    time.Time
	rejected bool
}

// MarkUnreachable records that wn-cloud did not answer at now. While wn-cloud
// stays unreachable, the first time is kept.
func (s *Status) MarkUnreachable(now time.Time) {
	if s == nil {
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.since.IsZero() {
		s.since = now
	}
}

// MarkReachable records that wn-cloud answered.
func (s *Status) MarkReachable() {
	if s == nil {
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	s.since = time.Time{}
}

// UnreachableSince returns the time of the first unanswered call since the
// last answered one. ok is false while wn-cloud is reachable.
func (s *Status) UnreachableSince() (since time.Time, ok bool) {
	if s == nil {
		return time.Time{}, false
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.since, !s.since.IsZero()
}

// SetCredentialRejected records whether wn-cloud refused the server
// credential in its last answer to a call that needs one.
func (s *Status) SetCredentialRejected(rejected bool) {
	if s == nil {
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	s.rejected = rejected
}

// CredentialRejected reports whether wn-cloud refused the server credential
// in its last answer. The owner then runs `wn-server logout`, then
// `wn-server login`.
func (s *Status) CredentialRejected() bool {
	if s == nil {
		return false
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.rejected
}
