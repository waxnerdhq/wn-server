package proxy

import (
	"net/http"
	"net/url"
	"strconv"
	"strings"

	"github.com/waxnerdhq/wn-server/internal/relayreq"
)

// capBitrate limits relayed streams to maxKbps. For a relayed request to
// /rest/stream or /rest/stream.view it sets maxBitRate to maxKbps when the
// parameter is absent, is not a positive integer, or is higher than maxKbps,
// and removes format=raw, so that Navidrome transcodes to the ceiling. Every
// other route, including download, and every request without the relayed mark
// pass unchanged, and so does every request when maxKbps is 0. It runs after
// the credential check, on the merged query that subsonicAuth forwards, so a
// parameter from a POSTed form is covered.
func capBitrate(maxKbps int, next http.Handler) http.Handler {
	if maxKbps <= 0 {
		return next
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if (r.URL.Path != "/rest/stream" && r.URL.Path != "/rest/stream.view") || !relayreq.IsRelayed(r.Context()) {
			next.ServeHTTP(w, r)
			return
		}
		q := r.URL.Query()
		if n, err := strconv.Atoi(q.Get("maxBitRate")); err != nil || n <= 0 || n > maxKbps {
			q.Set("maxBitRate", strconv.Itoa(maxKbps))
		}
		if strings.EqualFold(q.Get("format"), "raw") {
			q.Del("format")
		}
		out := r.Clone(r.Context())
		out.URL = new(url.URL)
		*out.URL = *r.URL
		out.URL.RawQuery = q.Encode()
		next.ServeHTTP(w, out)
	})
}
