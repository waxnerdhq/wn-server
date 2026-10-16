package navidrome

import "time"

// RestartPolicy sets the delay before Navidrome is restarted after an
// unexpected exit.
type RestartPolicy struct {
	// Initial is the delay after a first failure.
	Initial time.Duration
	// Max caps the delay.
	Max time.Duration
	// ResetAfter is the uptime after which the delay starts over at Initial.
	ResetAfter time.Duration
}

// DefaultRestartPolicy starts at 1 s, caps at 30 s, and resets after 60 s of
// uptime.
func DefaultRestartPolicy() RestartPolicy {
	return RestartPolicy{Initial: time.Second, Max: 30 * time.Second, ResetAfter: 60 * time.Second}
}

// Next returns the delay before the next restart. prev is the delay used
// before the run that just ended (zero when that run was the first), and
// uptime is how long that run stayed up.
func (p RestartPolicy) Next(prev, uptime time.Duration) time.Duration {
	if prev <= 0 || uptime >= p.ResetAfter {
		return p.Initial
	}
	return min(prev*2, p.Max)
}
