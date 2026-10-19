package proxy

import (
	"net/http"
	"strings"
	"time"

	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

// Option configures New.
type Option func(*options)

type options struct {
	ui          http.Handler
	cloudStatus *wncloud.Status
	ratings     Ratings
	maxBitrate  int
	navidrome   navidrome.ListenerAPI
	now         func() time.Time
}

func newOptions(opts []Option) options {
	o := options{ui: http.NotFoundHandler(), now: time.Now}
	for _, opt := range opts {
		opt(&o)
	}
	return o
}

// WithUI serves h on /, /ui, and /ui/*. h does not wait for the supervisor
// to report ready. Without this option those paths answer 404.
func WithUI(h http.Handler) Option {
	return func(o *options) { o.ui = h }
}

// WithCloudStatus makes the session exchange record in st whether wn-cloud
// answered.
func WithCloudStatus(st *wncloud.Status) Option {
	return func(o *options) { o.cloudStatus = st }
}

// WithRatings records the ratings and favorites of the Subsonic calls
// setRating, star, and unstar in rt after Navidrome accepted them.
func WithRatings(rt Ratings) Option {
	return func(o *options) { o.ratings = rt }
}

// WithRelayMaxBitrate caps the bitrate of relayed streams at kbps. On a relayed
// stream call the proxy sets maxBitRate to kbps when the client sent none or
// a higher one, and removes format=raw. A value of 0, the default, sets no
// cap.
func WithRelayMaxBitrate(kbps int) Option {
	return func(o *options) { o.maxBitrate = kbps }
}

// WithNavidromeUsers lets members of the server sign in as listeners with a
// device code. nd creates each listener's Navidrome user at the first
// sign-in. Without this option a listener's sign-in answers 500 internal.
func WithNavidromeUsers(nd navidrome.ListenerAPI) Option {
	return func(o *options) { o.navidrome = nd }
}

// withNow sets the clock of the session exchange, for tests.
func withNow(now func() time.Time) Option {
	return func(o *options) { o.now = now }
}

// isUIPath reports whether p belongs to the handler given with WithUI.
func isUIPath(p string) bool {
	return p == "/" || p == "/ui" || strings.HasPrefix(p, "/ui/")
}
