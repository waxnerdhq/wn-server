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
// address and in total. LAN requests and relayed requests have separate
// counts and totals, so failures through the relay never lock the LAN form
// and the other way round. An IPv6 address counts by its /64. A sign-in
// counts as a failure from begin until forget, so that parallel attempts
// cannot pass the limit. Each pool holds at most signInMaxTotal live
// failures.
type limiter struct {
	now func() time.Time

	mu    sync.Mutex
	lan   pool
	relay pool
}

// pool is the failures of one class of requests.
type pool struct {
	failures map[string][]time.Time
	total    int
}

func newLimiter(now func() time.Time) *limiter {
	return &limiter{
		now:   now,
		lan:   pool{failures: map[string][]time.Time{}},
		relay: pool{failures: map[string][]time.Time{}},
	}
}

// pool returns the pool of relayed or LAN requests. The caller holds l.mu.
func (l *limiter) pool(relayed bool) *pool {
	if relayed {
		return &l.relay
	}
	return &l.lan
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

// begin reports whether a sign-in from addr may proceed. relayed says
// whether the request arrived through the relay. When begin returns true,
// the attempt is recorded as a failure.
func (l *limiter) begin(addr string, relayed bool) bool {
	key := limitKey(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	p := l.pool(relayed)
	now := l.now()
	cutoff := now.Add(-signInWindow)
	for a, list := range p.failures {
		i := 0
		for i < len(list) && !list[i].After(cutoff) {
			i++
		}
		p.total -= i
		if i == len(list) {
			delete(p.failures, a)
		} else {
			p.failures[a] = list[i:]
		}
	}
	if len(p.failures[key]) >= signInMaxPerAddr || p.total >= signInMaxTotal {
		return false
	}
	p.failures[key] = append(p.failures[key], now)
	p.total++
	return true
}

// forget removes the failure that begin recorded, for a sign-in that
// succeeded or failed through no fault of the client.
func (l *limiter) forget(addr string, relayed bool) {
	key := limitKey(addr)
	l.mu.Lock()
	defer l.mu.Unlock()
	p := l.pool(relayed)
	list := p.failures[key]
	switch len(list) {
	case 0:
		return
	case 1:
		delete(p.failures, key)
	default:
		p.failures[key] = list[:len(list)-1]
	}
	p.total--
}
