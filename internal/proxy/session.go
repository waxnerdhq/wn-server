package proxy

import (
	"context"
	"encoding/json"
	"errors"
	"log/slog"
	"net/http"
	"strings"
	"sync"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/waxnerdhq/wn-server/internal/relayreq"
	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

// Limits of the local rate limit on failed POST /api/v1/session requests.
const (
	exchangeWindow      = 10 * time.Minute
	exchangeMaxPerAddr  = 5
	exchangeMaxTotal    = 10
	exchangeReadTimeout = 10 * time.Second
	maxDeviceLength     = 100
	maxSessionBodyBytes = 4 << 10
	// touchInterval is the least time between two writes of a session's
	// last seen time.
	touchInterval = time.Minute
	// unreachableLogInterval is the least time between two log lines about
	// wn-cloud being unreachable.
	unreachableLogInterval = time.Minute
)

// deviceCodeAlphabet is the alphabet of wn-cloud device codes.
const deviceCodeAlphabet = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"

// Verifier verifies a device code at wn-cloud. *wncloud.Client implements it.
type Verifier interface {
	VerifyDeviceCode(ctx context.Context, token, serverID, code string) (userID, email string, err error)
}

type sessionKey struct{}

// sessions issues, checks, and revokes wn-server sessions.
type sessions struct {
	store   Store
	cloud   Verifier
	logger  *slog.Logger
	limiter *limiter
	now     func() time.Time
	// cloudStatus records whether wn-cloud answered. It may be nil.
	cloudStatus *wncloud.Status

	mu              sync.Mutex
	lastUnreachable time.Time
}

func newSessions(st Store, cloud Verifier, logger *slog.Logger) *sessions {
	return &sessions{store: st, cloud: cloud, logger: logger, limiter: newLimiter(time.Now), now: time.Now}
}

// check returns the session for token and records its use. It returns
// store.ErrSessionNotFound for an unknown or revoked token.
func (s *sessions) check(ctx context.Context, token string) (store.Session, error) {
	sess, err := s.store.SessionByToken(ctx, token)
	if err != nil {
		return store.Session{}, err
	}
	if now := s.now(); now.Sub(sess.LastSeenAt) >= touchInterval {
		if err := s.store.TouchSession(ctx, sess.ID, now); err != nil && ctx.Err() == nil {
			s.logger.Warn("record session use", "session", sess.ID, "error", err)
		}
	}
	return sess, nil
}

// bearerToken returns the token of an Authorization: Bearer header.
func bearerToken(r *http.Request) (string, bool) {
	scheme, token, ok := strings.Cut(r.Header.Get("Authorization"), " ")
	if !ok || !strings.EqualFold(scheme, "Bearer") {
		return "", false
	}
	token = strings.TrimSpace(token)
	return token, token != ""
}

// apiHandler serves wn-server's own /api/v1/* routes. POST /api/v1/session
// needs no session. Every other route needs Authorization: Bearer with a
// valid session. The rating routes are added when rt is not nil. They reach
// Navidrome, so they answer 503 until ready is closed.
func (s *sessions) apiHandler(rt Ratings, ready <-chan struct{}) http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("POST /api/v1/session", s.exchange)
	mux.HandleFunc("DELETE /api/v1/session", s.revoke)
	if rt != nil {
		gate := gateReadyJSON(ready)
		(&ratingAPI{ratings: rt, logger: s.logger}).mount(mux, func(h http.Handler) http.Handler {
			return s.requireSession(gate(h))
		})
	}
	mux.Handle("/api/v1/", s.requireSession(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		writeAPIError(w, http.StatusNotFound, "not_found", "no such route")
	})))
	return mux
}

// requireSession answers 401 unless the request carries a valid session.
// It puts the session and its account id in the request context.
func (s *sessions) requireSession(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		token, ok := bearerToken(r)
		if !ok {
			writeUnauthorized(w)
			return
		}
		sess, err := s.check(r.Context(), token)
		if err != nil {
			if r.Context().Err() != nil {
				return
			}
			if !errors.Is(err, store.ErrSessionNotFound) {
				s.logger.Error("look up session", "error", err)
			}
			writeUnauthorized(w)
			return
		}
		ctx := context.WithValue(r.Context(), sessionKey{}, sess)
		ctx = context.WithValue(ctx, accountIDKey{}, sess.AccountID)
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}

// revoke serves DELETE /api/v1/session: it revokes the caller's session.
func (s *sessions) revoke(w http.ResponseWriter, r *http.Request) {
	token, ok := bearerToken(r)
	if !ok {
		writeUnauthorized(w)
		return
	}
	sess, err := s.store.RevokeSessionByToken(r.Context(), token)
	if err != nil {
		if r.Context().Err() != nil {
			return
		}
		if !errors.Is(err, store.ErrSessionNotFound) {
			s.logger.Error("revoke session", "error", err)
			writeAPIError(w, http.StatusInternalServerError, "internal", "the session could not be revoked")
			return
		}
		writeUnauthorized(w)
		return
	}
	s.logger.Info("session revoked", "session", sess.ID, "account", sess.AccountID, "device", sess.Device)
	w.WriteHeader(http.StatusNoContent)
}

// exchangeError is a refused POST /api/v1/session.
type exchangeError struct {
	status  int
	code    string
	message string
	// counted failures count toward the local rate limit.
	counted bool
}

var (
	errNotLinked = &exchangeError{http.StatusConflict, "not_linked", "this server is not linked to waxnerd; run `wn-server login` on the server", false}
	errBadCode   = &exchangeError{http.StatusUnauthorized, "invalid_code", "the code is invalid, expired, or already used; get a new code", true}
	errNotOwner  = &exchangeError{http.StatusForbidden, "not_owner", "the code was issued by a waxnerd user who does not own this server", true}
	errLimited   = &exchangeError{http.StatusTooManyRequests, "rate_limited", "too many failed attempts; try again later", false}
	errCloud     = &exchangeError{http.StatusBadGateway, "cloud_error", "wn-cloud refused this server's credential; the owner must run `wn-server logout`, then `wn-server login`", false}
	errCloudDown = &exchangeError{http.StatusServiceUnavailable, "cloud_unreachable", "wn-cloud is unreachable; try again later or use the app password", false}
	// errCanceled is a verification that ended because the client went
	// away. It counts, because wn-cloud may have seen the code.
	errCanceled   = &exchangeError{http.StatusServiceUnavailable, "cloud_unreachable", "the request was canceled", true}
	errInternal   = &exchangeError{http.StatusInternalServerError, "internal", "internal error", false}
	errBadRequest = func(msg string) *exchangeError {
		return &exchangeError{http.StatusBadRequest, "invalid_request", msg, true}
	}
)

type exchangeRequest struct {
	Code   string `json:"code"`
	Device string `json:"device"`
}

type exchangeResponse struct {
	Token   string          `json:"token"`
	Account exchangeAccount `json:"account"`
}

type exchangeAccount struct {
	ID   string `json:"id"`
	Name string `json:"name"`
}

// exchange serves POST /api/v1/session: it verifies a device code at
// wn-cloud and issues a session for the owner.
func (s *sessions) exchange(w http.ResponseWriter, r *http.Request) {
	// The server sets only ReadHeaderTimeout. A slow body must not hold the
	// handler.
	_ = http.NewResponseController(w).SetReadDeadline(time.Now().Add(exchangeReadTimeout))
	addr := relayreq.ClientAddr(r)
	code, device, xerr := readExchange(w, r)
	if xerr != nil {
		s.limiter.fail(addr)
		s.refuse(w, r, addr, xerr)
		return
	}
	if !s.limiter.acquire(addr) {
		s.refuse(w, r, addr, errLimited)
		return
	}
	failed := false
	defer func() { s.limiter.release(addr, failed) }()

	token, owner, xerr := s.exchangeCode(r.Context(), addr, code, device)
	if r.Context().Err() != nil {
		failed = true
		return
	}
	if xerr != nil {
		failed = xerr.counted
		s.refuse(w, r, addr, xerr)
		return
	}
	w.Header().Set("Cache-Control", "no-store")
	writeJSON(w, http.StatusOK, exchangeResponse{Token: token, Account: exchangeAccount{ID: owner.ID, Name: owner.Name}})
}

// readExchange decodes and validates the request body. It returns the
// normalized code and the trimmed device label.
func readExchange(w http.ResponseWriter, r *http.Request) (code, device string, xerr *exchangeError) {
	var req exchangeRequest
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, maxSessionBodyBytes)).Decode(&req); err != nil {
		return "", "", errBadRequest("the body is not valid JSON")
	}
	code, ok := normalizeDeviceCode(req.Code)
	if !ok {
		return "", "", errBadRequest("code must be 8 characters of the device code alphabet")
	}
	device = strings.TrimSpace(req.Device)
	if !validDevice(device) {
		return "", "", errBadRequest("device must be at most 100 characters without control characters")
	}
	return code, device, nil
}

func (s *sessions) exchangeCode(ctx context.Context, addr, code, device string) (string, store.Account, *exchangeError) {
	cred, err := s.store.ServerCredential(ctx)
	if errors.Is(err, store.ErrNoServerCredential) {
		return "", store.Account{}, errNotLinked
	}
	if err != nil {
		s.log(ctx).Error("read server credential", "remote", addr, "error", err)
		return "", store.Account{}, errInternal
	}
	owner, err := s.store.Owner(ctx)
	if err != nil {
		s.log(ctx).Error("read owner account", "remote", addr, "error", err)
		return "", store.Account{}, errInternal
	}
	if owner.WaxnerdUserID == "" {
		return "", store.Account{}, errNotLinked
	}

	userID, _, err := s.cloud.VerifyDeviceCode(ctx, cred.Token, cred.ServerID, code)
	if !errors.Is(err, wncloud.ErrUnreachable) {
		s.cloudStatus.MarkReachable()
	}
	switch {
	case err == nil:
	case errors.Is(err, wncloud.ErrInvalidGrant):
		return "", store.Account{}, errBadCode
	case errors.Is(err, wncloud.ErrRateLimited):
		return "", store.Account{}, errLimited
	case errors.Is(err, wncloud.ErrUnreachable):
		if ctx.Err() != nil {
			return "", store.Account{}, errCanceled
		}
		s.cloudStatus.MarkUnreachable(s.now())
		s.logUnreachable(err)
		return "", store.Account{}, errCloudDown
	default:
		if ctx.Err() == nil {
			s.log(ctx).Error("wn-cloud refused the device code verification; if the server credential no longer works, run `wn-server logout`, then `wn-server login`",
				"server", cred.ServerID, "remote", addr, "error", err)
		}
		return "", store.Account{}, errCloud
	}
	if userID != owner.WaxnerdUserID {
		return "", store.Account{}, errNotOwner
	}

	token, sess, err := s.store.CreateSession(ctx, owner.ID, device)
	if err != nil {
		s.log(ctx).Error("create session", "remote", addr, "error", err)
		return "", store.Account{}, errInternal
	}
	s.log(ctx).Info("session created", "session", sess.ID, "account", owner.ID, "device", device)
	return token, owner, nil
}

// log returns the sessions' logger, with relay=true when ctx carries the
// relayed mark.
func (s *sessions) log(ctx context.Context) *slog.Logger {
	if relayreq.IsRelayed(ctx) {
		return s.logger.With("relay", true)
	}
	return s.logger
}

// refuse writes the error answer of a failed exchange and logs it. The log
// line never carries the code.
func (s *sessions) refuse(w http.ResponseWriter, r *http.Request, addr string, e *exchangeError) {
	switch e {
	case errCloudDown, errCloud, errInternal:
		// Logged where they happen, with their cause.
	default:
		s.log(r.Context()).Warn("session exchange failed", "reason", e.code, "remote", addr)
	}
	writeAPIError(w, e.status, e.code, e.message)
}

// logUnreachable logs that wn-cloud is unreachable, at most once per
// unreachableLogInterval.
func (s *sessions) logUnreachable(err error) {
	s.mu.Lock()
	now := s.now()
	quiet := !s.lastUnreachable.IsZero() && now.Sub(s.lastUnreachable) < unreachableLogInterval
	if !quiet {
		s.lastUnreachable = now
	}
	s.mu.Unlock()
	if !quiet {
		s.logger.Warn("session exchange failed: wn-cloud is unreachable", "error", err)
	}
}

// normalizeDeviceCode removes "-" and whitespace, upper-cases the rest, and
// reports whether the result is 8 characters of the device code alphabet.
func normalizeDeviceCode(code string) (string, bool) {
	n := strings.ToUpper(strings.Map(func(c rune) rune {
		if c == '-' || unicode.IsSpace(c) {
			return -1
		}
		return c
	}, code))
	if len(n) != 8 {
		return "", false
	}
	for _, c := range n {
		if !strings.ContainsRune(deviceCodeAlphabet, c) {
			return "", false
		}
	}
	return n, true
}

func validDevice(device string) bool {
	if utf8.RuneCountInString(device) > maxDeviceLength || !utf8.ValidString(device) {
		return false
	}
	for _, c := range device {
		if unicode.IsControl(c) {
			return false
		}
	}
	return true
}

type apiError struct {
	Error string `json:"error"`
	Code  string `json:"code"`
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func writeAPIError(w http.ResponseWriter, status int, code, message string) {
	writeJSON(w, status, apiError{Error: message, Code: code})
}

func writeUnauthorized(w http.ResponseWriter) {
	w.Header().Set("WWW-Authenticate", "Bearer")
	writeAPIError(w, http.StatusUnauthorized, "unauthorized", "a valid session is required")
}

// limiter counts failed session exchanges in a sliding window, per remote
// address and in total. An exchange in progress counts as a failure until it
// ends, so that parallel attempts cannot pass the limit.
type limiter struct {
	now func() time.Time

	mu           sync.Mutex
	failures     map[string][]time.Time
	total        []time.Time
	pending      map[string]int
	pendingTotal int
}

func newLimiter(now func() time.Time) *limiter {
	return &limiter{now: now, failures: map[string][]time.Time{}, pending: map[string]int{}}
}

// acquire reports whether an exchange from addr may proceed. When it
// returns true, the caller must call release once the exchange ends.
func (l *limiter) acquire(addr string) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.prune()
	if len(l.failures[addr])+l.pending[addr] >= exchangeMaxPerAddr ||
		len(l.total)+l.pendingTotal >= exchangeMaxTotal {
		return false
	}
	l.pending[addr]++
	l.pendingTotal++
	return true
}

// fail records a failed exchange that did not acquire a slot, such as a
// malformed request. While a limit is reached it records nothing, so the
// lists stay bounded.
func (l *limiter) fail(addr string) {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.prune()
	if len(l.failures[addr])+l.pending[addr] >= exchangeMaxPerAddr ||
		len(l.total)+l.pendingTotal >= exchangeMaxTotal {
		return
	}
	now := l.now()
	l.failures[addr] = append(l.failures[addr], now)
	l.total = append(l.total, now)
}

// release ends an exchange. A failed exchange is recorded.
func (l *limiter) release(addr string, failed bool) {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.pending[addr]--; l.pending[addr] <= 0 {
		delete(l.pending, addr)
	}
	l.pendingTotal--
	if failed {
		now := l.now()
		l.failures[addr] = append(l.failures[addr], now)
		l.total = append(l.total, now)
	}
}

// prune drops failures older than the window. The map holds at most
// exchangeMaxTotal live failures, so it stays small.
func (l *limiter) prune() {
	cutoff := l.now().Add(-exchangeWindow)
	l.total = dropBefore(l.total, cutoff)
	for addr, list := range l.failures {
		if list = dropBefore(list, cutoff); len(list) == 0 {
			delete(l.failures, addr)
		} else {
			l.failures[addr] = list
		}
	}
}

// dropBefore removes the leading times that are not after cutoff. list is in
// ascending order.
func dropBefore(list []time.Time, cutoff time.Time) []time.Time {
	i := 0
	for i < len(list) && !list[i].After(cutoff) {
		i++
	}
	return list[i:]
}
