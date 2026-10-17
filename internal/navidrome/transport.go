package navidrome

import (
	"context"
	"net"
	"net/http"
	"net/url"
	"time"
)

// Host is the URL host of every request to Navidrome. Navidrome listens on a
// Unix socket, so the host only fills the Host header. Navidrome does not
// check it.
const Host = "localhost"

// maxSocketPath is the longest socket path Linux accepts. sun_path holds
// 108 bytes including the terminating NUL.
const maxSocketPath = 107

// Target returns the base URL of Navidrome for a transport from NewTransport.
func Target() *url.URL { return &url.URL{Scheme: "http", Host: Host} }

// NewTransport returns a transport that dials Navidrome's Unix socket at
// socket for every request, whatever the URL host. serve builds one and
// shares it between the proxy, the client, and the supervisor.
func NewTransport(socket string) *http.Transport {
	d := &net.Dialer{Timeout: 5 * time.Second}
	return &http.Transport{
		DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
			return d.DialContext(ctx, "unix", socket)
		},
		MaxIdleConns:        100,
		MaxIdleConnsPerHost: 100,
		IdleConnTimeout:     90 * time.Second,
	}
}
