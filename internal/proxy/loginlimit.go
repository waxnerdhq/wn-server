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
// window, per client address. An IPv6 address counts by its /64. Only
// confirmed failures count: a wrong password or an unknown user, recorded
// after the check. Entries whose last failure and last log line are older
// than the window are evicted, in a sweep that runs at most once per window.
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

// limited reports whether addr already has loginMaxPerAddr confirmed
// failures in the window. When it does, warn reports whether the caller
// should log the limit, which is true at most once per loginWarnInterval for
// an address. Checks in flight do not count, so parallel attempts can exceed
// the limit by the number of checks in flight.
func (l *loginLimiter) limited(addr string) (limited, warn bool) {
	key := loginKey(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	now := l.now()
	l.sweep(now)
	e := l.entries[key]
	if e == nil {
		return false, false
	}
	e.failures = dropBefore(e.failures, now.Add(-loginWindow))
	if len(e.failures) < loginMaxPerAddr {
		return false, false
	}
	if e.lastWarn.IsZero() || now.Sub(e.lastWarn) >= loginWarnInterval {
		e.lastWarn = now
		warn = true
	}
	return true, warn
}

// fail records a confirmed failed check from addr.
func (l *loginLimiter) fail(addr string) {
	key := loginKey(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	now := l.now()
	e := l.entries[key]
	if e == nil {
		e = &loginEntry{}
		l.entries[key] = e
	}
	e.failures = append(dropBefore(e.failures, now.Add(-loginWindow)), now)
}

// reset clears the failures of addr, after a successful check.
func (l *loginLimiter) reset(addr string) {
	key := loginKey(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	delete(l.entries, key)
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
