package proxy

import (
	"context"
	"log/slog"
	"net/http"
	"net/http/httputil"
	"path"
	"strings"
	"time"

	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/relayreq"
	"github.com/waxnerdhq/wn-server/internal/store"
)

// relayClientHeader is the header the relay edge sets to the client's
// address. The relay package moves it into the request context.
const relayClientHeader = "X-Relay-Client"

// userHeader is the header Navidrome trusts to name the signed-in user.
const userHeader = "Remote-User"

// Store is the part of the store that the proxy uses. *store.Store
// implements it.
type Store interface {
	// AccountByName returns store.ErrNotFound for an unknown name.
	AccountByName(ctx context.Context, name string) (store.Account, error)
	// AccountByID returns store.ErrNotFound for an unknown id.
	AccountByID(ctx context.Context, id string) (store.Account, error)
	Owner(ctx context.Context) (store.Account, error)
	ServerCredential(ctx context.Context) (store.ServerCredential, error)
	CreateSession(ctx context.Context, accountID, device string) (string, store.Session, error)
	// CreateListenerSession returns store.ErrAccountDisabled when the
	// account is disabled or its user is not in the member list.
	CreateListenerSession(ctx context.Context, accountID, device string) (string, store.Session, error)
	// SessionByToken returns store.ErrSessionNotFound for an unknown token.
	SessionByToken(ctx context.Context, token string) (store.Session, error)
	TouchSession(ctx context.Context, id string, at time.Time) error
	RevokeSessionByToken(ctx context.Context, token string) (store.Session, error)
	// ListenerByUserID returns store.ErrNotFound when the waxnerd user has
	// no listener account.
	ListenerByUserID(ctx context.Context, userID string) (store.Account, error)
	CreateListener(ctx context.Context, userID, email string) (store.Account, error)
	IsMember(ctx context.Context, userID string) (bool, error)
	MembersRefreshedAt(ctx context.Context) (at time.Time, ok bool, err error)
}

type accountIDKey struct{}

// New returns a handler that forwards /rest/* with authentication and
// /share/* without it to Navidrome through rt, the transport from
// navidrome.NewTransport, and serves wn-server's own /api/v1/* routes.
// WithUI adds the web UI on / and /ui/*. Every other path answers 404. Until
// ready is closed, /rest/* and /share/* answer 503 and nothing is forwarded,
// so that no client can become Navidrome's first user. cloud verifies device
// codes for POST /api/v1/session. WithNavidromeUsers lets listeners sign
// in. WithRatings adds the rating intercept on
// setRating, star, and unstar. WithRelayMaxBitrate caps relayed streams. A nil logger selects slog.Default.
func New(rt http.RoundTripper, ready <-chan struct{}, st Store, cloud Verifier, logger *slog.Logger, opts ...Option) http.Handler {
	o := newOptions(opts)
	if logger == nil {
		logger = slog.Default()
	}
	target := navidrome.Target()
	rp := &httputil.ReverseProxy{
		Transport: rt,
		Rewrite: func(r *httputil.ProxyRequest) {
			r.SetURL(target)
			// Navidrome builds share links and public image URLs from
			// X-Forwarded-Host and X-Forwarded-Proto, else from Host, which
			// is localhost on the socket. Both come from this request, not
			// from the client's headers. X-Forwarded-For is never set:
			// Navidrome checks it against its trusted sources for the
			// Remote-User sign-in.
			for _, h := range []string{"Forwarded", "X-Forwarded-For", "X-Forwarded-Host", "X-Forwarded-Proto", "X-Forwarded-Scheme"} {
				r.Out.Header.Del(h)
			}
			r.Out.Header.Set("X-Forwarded-Host", r.In.Host)
			if r.In.TLS != nil || relayreq.IsRelayed(r.In.Context()) {
				r.Out.Header.Set("X-Forwarded-Proto", "https")
			} else {
				r.Out.Header.Set("X-Forwarded-Proto", "http")
			}
			// A client must not choose its Navidrome identity. A session in
			// the Authorization header or in a cookie stays with wn-server.
			r.Out.Header.Del(userHeader)
			r.Out.Header.Del("Authorization")
			r.Out.Header.Del("Cookie")
			requestIdentity(r)
			if id, ok := r.In.Context().Value(accountIDKey{}).(string); ok {
				r.Out.Header.Set(userHeader, id)
			}
		},
		ModifyResponse: func(resp *http.Response) error {
			if err := rewriteUserResponse(resp); err != nil {
				return err
			}
			return recordRatings(resp)
		},
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
	sess := newSessions(st, cloud, logger, o.now)
	sess.cloudStatus = o.cloudStatus
	sess.users = o.navidrome
	sess.ready = ready
	rest := gateReady(ready, subsonicAuth(st, sess, newLoginLimiter(time.Now), logger, capBitrate(o.maxBitrate, interceptRatings(o.ratings, logger, rp))))
	share := gateReady(ready, rp)
	api := sess.apiHandler(o.ratings, ready)
	return logRequests(logger, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// A relayed request carries its client address in the context. A
		// header of this name from any client means nothing, and it must
		// reach neither the logs nor Navidrome.
		r.Header.Del(relayClientHeader)
		switch p := r.URL.Path; {
		case !isClean(p):
			http.NotFound(w, r)
		case strings.HasPrefix(p, "/rest/"):
			rest.ServeHTTP(w, r)
		case strings.HasPrefix(p, "/share/"):
			share.ServeHTTP(w, r)
		case strings.HasPrefix(p, "/api/v1/"):
			api.ServeHTTP(w, r)
		case isUIPath(p):
			o.ui.ServeHTTP(w, r)
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

// requestLogger returns logger with relay=true when r is a relayed request.
func requestLogger(logger *slog.Logger, r *http.Request) *slog.Logger {
	if relayreq.IsRelayed(r.Context()) {
		return logger.With("relay", true)
	}
	return logger
}

// logRequests logs one debug line per request with the client address, and
// relay=true for a relayed request. It logs the path only, never the query
// string, which carries Subsonic credentials.
func logRequests(logger *slog.Logger, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		sw := &statusWriter{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(sw, r)
		requestLogger(logger, r).Debug("request", "remote", relayreq.ClientAddr(r), "method", r.Method, "path", r.URL.Path, "status", sw.status, "duration", time.Since(start))
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
