package proxy

import (
	"net/netip"
	"sync"
	"time"
)

// Limit on failed Subsonic credential checks at /rest/*.
const (
	loginWindow     = time.Minute
	loginMaxPerAddr = 10
	// loginWarnInterval is the least time between two log lines about one
	// address being limited.
	loginWarnInterval = time.Minute
)

// loginLimiter counts failed Subsonic credential checks in a sliding
// window, per client address. An IPv6 address counts by its /64. A check
// counts as a failure from begin until reset or forget, so that parallel
// attempts cannot pass the limit. Entries with no live failure are evicted
// once per window.
type loginLimiter struct {
	now func() time.Time

	mu        sync.Mutex
	entries   map[string]*loginEntry
	lastSweep time.Time
}

type loginEntry struct {
	failures []time.Time
	lastWarn time.Time
}

func newLoginLimiter(now func() time.Time) *loginLimiter {
	return &loginLimiter{now: now, entries: map[string]*loginEntry{}}
}

// loginKey returns the /64 prefix of an IPv6 address and addr otherwise.
func loginKey(addr string) string {
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

// begin reports whether a check from addr may proceed. When it returns true,
// the check is recorded as a failure. When it returns false, warn reports
// whether the caller should log the limit, which is true at most once per
// loginWarnInterval for an address.
func (l *loginLimiter) begin(addr string) (ok, warn bool) {
	key := loginKey(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	now := l.now()
	l.sweep(now)
	e := l.entries[key]
	if e == nil {
		e = &loginEntry{}
		l.entries[key] = e
	}
	e.failures = dropBefore(e.failures, now.Add(-loginWindow))
	if len(e.failures) >= loginMaxPerAddr {
		if e.lastWarn.IsZero() || now.Sub(e.lastWarn) >= loginWarnInterval {
			e.lastWarn = now
			warn = true
		}
		return false, warn
	}
	e.failures = append(e.failures, now)
	return true, false
}

// reset clears the failures of addr, after a successful check.
func (l *loginLimiter) reset(addr string) {
	key := loginKey(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	delete(l.entries, key)
}

// forget removes the failure that begin recorded, for a check that failed
// through no fault of the client.
func (l *loginLimiter) forget(addr string) {
	key := loginKey(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	if e := l.entries[key]; e != nil && len(e.failures) > 0 {
		e.failures = e.failures[:len(e.failures)-1]
	}
}

// sweep evicts the entries with no live failure and no recent log line. It
// runs at most once per window.
func (l *loginLimiter) sweep(now time.Time) {
	if now.Sub(l.lastSweep) < loginWindow {
		return
	}
	l.lastSweep = now
	cutoff := now.Add(-loginWindow)
	for key, e := range l.entries {
		e.failures = dropBefore(e.failures, cutoff)
		if len(e.failures) == 0 && !e.lastWarn.After(cutoff) {
			delete(l.entries, key)
		}
	}
}
