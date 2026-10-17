package web

import (
	"net/netip"
	"sync"
	"time"
)

// Limits on failed web sign-ins.
const (
	signInWindow     = 10 * time.Minute
	signInMaxPerAddr = 10
	signInMaxTotal   = 100
)

// limiter counts failed web sign-ins in a sliding window, per client
// address and in total. An IPv6 address counts by its /64. A sign-in counts
// as a failure from begin until forget, so that parallel attempts cannot
// pass the limit. The map holds at most signInMaxTotal live failures.
type limiter struct {
	now func() time.Time

	mu       sync.Mutex
	failures map[string][]time.Time
	total    int
}

func newLimiter(now func() time.Time) *limiter {
	return &limiter{now: now, failures: map[string][]time.Time{}}
}

// limitKey returns the /64 prefix of an IPv6 address and addr otherwise.
func limitKey(addr string) string {
	ip, err := netip.ParseAddr(addr)
	if err != nil || !ip.Is6() || ip.Is4In6() {
		return addr
	}
	p, err := ip.Prefix(64)
	if err != nil {
		return addr
	}
	return p.String()
}

// begin reports whether a sign-in from addr may proceed. When it returns
// true, the attempt is recorded as a failure.
func (l *limiter) begin(addr string) bool {
	key := limitKey(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	now := l.now()
	cutoff := now.Add(-signInWindow)
	for a, list := range l.failures {
		i := 0
		for i < len(list) && !list[i].After(cutoff) {
			i++
		}
		l.total -= i
		if i == len(list) {
			delete(l.failures, a)
		} else {
			l.failures[a] = list[i:]
		}
	}
	if len(l.failures[key]) >= signInMaxPerAddr || l.total >= signInMaxTotal {
		return false
	}
	l.failures[key] = append(l.failures[key], now)
	l.total++
	return true
}

// forget removes the failure that begin recorded, for a sign-in that
// succeeded or failed through no fault of the client.
func (l *limiter) forget(addr string) {
	key := limitKey(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	list := l.failures[key]
	switch len(list) {
	case 0:
		return
	case 1:
		delete(l.failures, key)
	default:
		l.failures[key] = list[:len(list)-1]
	}
	l.total--
}
