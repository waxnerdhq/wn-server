package proxy

import (
	"net/http"
	"strings"

	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

// Option configures New.
type Option func(*options)

type options struct {
	ui          http.Handler
	cloudStatus *wncloud.Status
	ratings     Ratings
	maxBitrate  int
}

func newOptions(opts []Option) options {
	o := options{ui: http.NotFoundHandler()}
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

// isUIPath reports whether p belongs to the handler given with WithUI.
func isUIPath(p string) bool {
	return p == "/" || p == "/ui" || strings.HasPrefix(p, "/ui/")
}
