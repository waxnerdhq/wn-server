package web

import (
	"sync"
	"time"
)

// Limits on failed web sign-ins.
const (
	signInWindow     = 10 * time.Minute
	signInMaxPerAddr = 10
)

// limiter counts failed web sign-ins per remote address in a sliding
// window. A sign-in counts as a failure from begin until forget, so that
// parallel attempts cannot pass the limit.
type limiter struct {
	now func() time.Time

	mu       sync.Mutex
	failures map[string][]time.Time
}

func newLimiter(now func() time.Time) *limiter {
	return &limiter{now: now, failures: map[string][]time.Time{}}
}

// begin reports whether a sign-in from addr may proceed. When it returns
// true, the attempt is recorded as a failure.
func (l *limiter) begin(addr string) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	now := l.now()
	cutoff := now.Add(-signInWindow)
	for a, list := range l.failures {
		i := 0
		for i < len(list) && !list[i].After(cutoff) {
			i++
		}
		if i == len(list) {
			delete(l.failures, a)
		} else {
			l.failures[a] = list[i:]
		}
	}
	if len(l.failures[addr]) >= signInMaxPerAddr {
		return false
	}
	l.failures[addr] = append(l.failures[addr], now)
	return true
}

// forget removes the failure that begin recorded, for a sign-in that
// succeeded or failed through no fault of the client.
func (l *limiter) forget(addr string) {
	l.mu.Lock()
	defer l.mu.Unlock()
	list := l.failures[addr]
	switch len(list) {
	case 0:
	case 1:
		delete(l.failures, addr)
	default:
		l.failures[addr] = list[:len(list)-1]
	}
}
