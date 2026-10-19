package relayreq

import (
	"context"
	"net"
	"net/http"
)

type key struct{}

// WithRelay returns a context that marks a request as relayed and carries
// the client's address, as the relay edge reported it.
func WithRelay(ctx context.Context, clientAddr string) context.Context {
	return context.WithValue(ctx, key{}, clientAddr)
}

// IsRelayed reports whether ctx carries the relayed mark.
func IsRelayed(ctx context.Context) bool {
	_, ok := ctx.Value(key{}).(string)
	return ok
}

// ClientAddr returns the client address of r: the address given to
// WithRelay when the request is relayed, else r.RemoteAddr without the port.
// Forwarded headers are not read.
func ClientAddr(r *http.Request) string {
	if addr, ok := r.Context().Value(key{}).(string); ok {
		return addr
	}
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return r.RemoteAddr
	}
	return host
}
