package proxy

import (
	"log/slog"
	"net/http"
	"net/http/httputil"
	"net/url"
	"time"
)

// userHeader is the header Navidrome trusts to name the signed-in user.
const userHeader = "Remote-User"

// New returns a handler that forwards requests to target. Until ready is
// closed it answers 503 and forwards nothing, so that no client can become
// Navidrome's first user. A nil logger selects slog.Default.
func New(target *url.URL, ready <-chan struct{}, logger *slog.Logger) http.Handler {
	if logger == nil {
		logger = slog.Default()
	}
	rp := &httputil.ReverseProxy{
		Rewrite: func(r *httputil.ProxyRequest) {
			r.SetURL(target)
			// A client must not choose its Navidrome identity.
			r.Out.Header.Del(userHeader)
		},
		// Flush after every write so audio and large bodies stream.
		FlushInterval: -1,
		ErrorHandler: func(w http.ResponseWriter, r *http.Request, err error) {
			logger.Warn("proxy error", "method", r.Method, "path", r.URL.Path, "error", err)
			w.WriteHeader(http.StatusBadGateway)
		},
	}
	return logRequests(logger, gateReady(ready, rp))
}

// gateReady answers 503 until ready is closed.
func gateReady(ready <-chan struct{}, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		select {
		case <-ready:
			next.ServeHTTP(w, r)
		default:
			w.Header().Set("Content-Type", "text/plain; charset=utf-8")
			w.Header().Set("Retry-After", "5")
			w.WriteHeader(http.StatusServiceUnavailable)
			_, _ = w.Write([]byte("wn-server is starting; try again shortly\n"))
		}
	})
}

// logRequests logs one debug line per request.
func logRequests(logger *slog.Logger, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		sw := &statusWriter{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(sw, r)
		logger.Debug("request", "method", r.Method, "path", r.URL.Path, "status", sw.status, "duration", time.Since(start))
	})
}

// statusWriter records the response status. Unwrap lets http.ResponseController
// reach the underlying writer for flushing.
type statusWriter struct {
	http.ResponseWriter
	status int
}

func (s *statusWriter) WriteHeader(code int) {
	s.status = code
	s.ResponseWriter.WriteHeader(code)
}

func (s *statusWriter) Unwrap() http.ResponseWriter { return s.ResponseWriter }
