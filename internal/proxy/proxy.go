package proxy

import (
	"context"
	"log/slog"
	"net/http"
	"net/http/httputil"
	"net/url"
	"path"
	"strings"
	"time"

	"github.com/waxnerdhq/wn-server/internal/store"
)

// userHeader is the header Navidrome trusts to name the signed-in user.
const userHeader = "Remote-User"

// Store is the part of the store that the proxy uses. *store.Store
// implements it.
type Store interface {
	// AccountByName returns store.ErrNotFound for an unknown name.
	AccountByName(ctx context.Context, name string) (store.Account, error)
	Owner(ctx context.Context) (store.Account, error)
	ServerCredential(ctx context.Context) (store.ServerCredential, error)
	CreateSession(ctx context.Context, accountID, device string) (string, store.Session, error)
	// SessionByToken returns store.ErrSessionNotFound for an unknown token.
	SessionByToken(ctx context.Context, token string) (store.Session, error)
	TouchSession(ctx context.Context, id string, at time.Time) error
	RevokeSessionByToken(ctx context.Context, token string) (store.Session, error)
}

type accountIDKey struct{}

// New returns a handler that forwards /rest/* with authentication and
// /share/* without it to target, and serves wn-server's own /api/v1/*
// routes. Every other path answers 404. Until ready is closed, /rest/* and
// /share/* answer 503 and nothing is forwarded, so that no client can become
// Navidrome's first user. cloud verifies device codes for
// POST /api/v1/session. A nil logger selects slog.Default.
func New(target *url.URL, ready <-chan struct{}, st Store, cloud Verifier, logger *slog.Logger) http.Handler {
	if logger == nil {
		logger = slog.Default()
	}
	rp := &httputil.ReverseProxy{
		Rewrite: func(r *httputil.ProxyRequest) {
			r.SetURL(target)
			// A client must not choose its Navidrome identity. A session in
			// the Authorization header stays with wn-server.
			r.Out.Header.Del(userHeader)
			r.Out.Header.Del("Authorization")
			requestIdentity(r)
			if id, ok := r.In.Context().Value(accountIDKey{}).(string); ok {
				r.Out.Header.Set(userHeader, id)
			}
		},
		ModifyResponse: rewriteUserResponse,
		// Flush after every write so audio and large bodies stream.
		FlushInterval: -1,
		ErrorHandler: func(w http.ResponseWriter, r *http.Request, err error) {
			if r.Context().Err() != nil {
				// The client went away, for example on an audio seek.
				logger.Debug("proxy request canceled", "method", r.Method, "path", r.URL.Path, "error", err)
				return
			}
			logger.Warn("proxy error", "method", r.Method, "path", r.URL.Path, "error", err)
			w.WriteHeader(http.StatusBadGateway)
		},
	}
	sess := newSessions(st, cloud, logger)
	rest := gateReady(ready, subsonicAuth(st, sess, logger, rp))
	share := gateReady(ready, rp)
	api := sess.apiHandler()
	return logRequests(logger, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch p := r.URL.Path; {
		case !isClean(p):
			http.NotFound(w, r)
		case strings.HasPrefix(p, "/rest/"):
			rest.ServeHTTP(w, r)
		case strings.HasPrefix(p, "/share/"):
			share.ServeHTTP(w, r)
		case strings.HasPrefix(p, "/api/v1/"):
			api.ServeHTTP(w, r)
		default:
			http.NotFound(w, r)
		}
	}))
}

// isClean reports whether p has no dot segments and no empty segments, so
// that a prefix match on p is a match on the path Navidrome routes.
func isClean(p string) bool {
	c := path.Clean(p)
	if strings.HasSuffix(p, "/") && c != "/" {
		c += "/"
	}
	return c == p
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

// logRequests logs one debug line per request. It logs the path only, never
// the query string, which carries Subsonic credentials.
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
