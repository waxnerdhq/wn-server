package web

import (
	"bytes"
	"context"
	"embed"
	"errors"
	"html/template"
	"io/fs"
	"log/slog"
	"mime"
	"net"
	"net/http"
	"net/url"
	"path"
	"strconv"
	"strings"
	"time"

	"github.com/waxnerdhq/wn-server/internal/match"
	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

// CookieName is the cookie that holds the browser's wn-server session.
const CookieName = "wn_session"

const (
	// touchInterval is the least time between two writes of a session's last
	// seen time, as on the proxy.
	touchInterval = time.Minute
	// maxUserAgent is the number of user agent characters kept in the
	// device label of a web session.
	maxUserAgent = 60
	// scanTimeout bounds the call to Navidrome for the scan state.
	scanTimeout  = 3 * time.Second
	maxFormBytes = 4 << 10
)

// contentSecurityPolicy allows the embedded stylesheet and script only.
const contentSecurityPolicy = "default-src 'none'; style-src 'self'; script-src 'self'; img-src 'self'; " +
	"form-action 'self'; frame-ancestors 'none'; base-uri 'none'"

// Store is the part of the store that the web UI uses. *store.Store
// implements it.
type Store interface {
	Owner(ctx context.Context) (store.Account, error)
	// AccountByName returns store.ErrNotFound for an unknown name.
	AccountByName(ctx context.Context, name string) (store.Account, error)
	SetAppPassword(ctx context.Context, id, password string) error
	ServerCredential(ctx context.Context) (store.ServerCredential, error)
	CreateSession(ctx context.Context, accountID, device string) (string, store.Session, error)
	// SessionByToken returns store.ErrSessionNotFound for an unknown token.
	SessionByToken(ctx context.Context, token string) (store.Session, error)
	TouchSession(ctx context.Context, id string, at time.Time) error
	Sessions(ctx context.Context, accountID string) ([]store.Session, error)
	RevokeSession(ctx context.Context, id string) error
	RevokeSessionByToken(ctx context.Context, token string) (store.Session, error)
	// MatchEntries returns the entries in the given states, or every entry
	// when there are none.
	MatchEntries(ctx context.Context, states ...store.MatchState) ([]store.MatchEntry, error)
	// MatchEntry returns store.ErrMatchNotFound for an unknown id.
	MatchEntry(ctx context.Context, id int64) (store.MatchEntry, error)
	CountMatches(ctx context.Context, state store.MatchState) (int, error)
}

// Matcher runs the match actions. *match.Service implements it.
type Matcher interface {
	Run(ctx context.Context) (match.RunResult, error)
	Apply(ctx context.Context, id int64, release string) (match.ApplyResult, error)
	Dismiss(ctx context.Context, id int64) (folder string, err error)
	Revert(ctx context.Context, id int64) (match.RevertResult, error)
}

// Scanner reports Navidrome's scan state. *navidrome.Client implements it.
type Scanner interface {
	ScanStatus(ctx context.Context) (navidrome.ScanStatus, error)
}

// Options configures New.
type Options struct {
	Store Store
	// Navidrome is asked for the scan state once Ready is closed.
	Navidrome Scanner
	// Match runs the match actions. The match page needs it.
	Match Matcher
	// Jobs runs the actions of the match page in the background. New
	// creates one when it is nil.
	Jobs *match.Jobs
	// Ready is closed when the supervisor reports ready. Until then the
	// status page shows that Navidrome is starting.
	Ready <-chan struct{}
	// CloudStatus is read for the wn-cloud outage banner. It may be nil.
	CloudStatus *wncloud.Status
	// ServerName is shown on the status page.
	ServerName string
	// NavidromeVersion is the version that serve checked at start.
	NavidromeVersion string
	// Logger defaults to slog.Default.
	Logger *slog.Logger
}

//go:embed templates static
var files embed.FS

var funcs = template.FuncMap{"when": when, "distance": distance, "tags": tags}

// when formats t in the server's time zone.
func when(t time.Time) string { return t.Local().Format("2006-01-02 15:04") }

// distance formats a candidate's distance from the existing tags.
func distance(d float64) string { return strconv.FormatFloat(d, 'f', 3, 64) }

// tags describes an entry's existing tags on one line.
func tags(e store.MatchEntry) string {
	or := func(s string) string {
		if s == "" {
			return "?"
		}
		return s
	}
	s := or(e.Artist) + " - " + or(e.Album)
	if e.Year > 0 {
		s += " (" + strconv.Itoa(e.Year) + ")"
	}
	return s + ", " + strconv.Itoa(e.TrackCount) + " tracks"
}

// pageNames are the templates in templates/, besides layout.html.
var pageNames = []string{"login", "status", "account", "match"}

type server struct {
	Options
	pages   map[string]*template.Template
	static  fs.FS
	limiter *limiter
	now     func() time.Time
}

// New returns the handler for / and /ui/*. GET / redirects to /ui/. Every
// /ui/* page except the sign-in page needs a session cookie.
func New(o Options) http.Handler {
	if o.Logger == nil {
		o.Logger = slog.Default()
	}
	if o.Jobs == nil {
		o.Jobs = match.NewJobs(context.Background(), o.Logger)
	}
	s := &server{Options: o, pages: map[string]*template.Template{}, limiter: newLimiter(time.Now), now: time.Now}
	for _, name := range pageNames {
		s.pages[name] = template.Must(template.New(name).Funcs(funcs).
			ParseFS(files, "templates/layout.html", "templates/"+name+".html"))
	}
	static, err := fs.Sub(files, "static")
	if err != nil {
		panic(err)
	}
	s.static = static

	mux := http.NewServeMux()
	mux.HandleFunc("GET /{$}", redirectTo("/ui/"))
	mux.HandleFunc("GET /ui", redirectTo("/ui/"))
	mux.HandleFunc("GET /ui/static/{name}", s.staticFile)
	mux.HandleFunc("GET /ui/login", s.loginPage)
	mux.HandleFunc("POST /ui/login", s.login)
	mux.HandleFunc("POST /ui/logout", s.logout)
	mux.Handle("GET /ui/{$}", s.guard(s.statusPage))
	mux.Handle("GET /ui/account", s.guard(s.accountPage))
	mux.Handle("GET /ui/match", s.guard(s.matchPage))
	mux.Handle("POST /ui/match/run", s.guard(s.matchRun))
	mux.Handle("POST /ui/match/{id}/apply", s.guard(s.matchApply))
	mux.Handle("POST /ui/match/{id}/dismiss", s.guard(s.matchDismiss))
	mux.Handle("POST /ui/match/{id}/revert", s.guard(s.matchRevert))
	mux.Handle("POST /ui/account/password/reset", s.guard(s.resetPassword))
	mux.Handle("POST /ui/sessions/{id}/revoke", s.guard(s.revokeSession))
	return securityHeaders(checkOrigin(mux))
}

func redirectTo(target string) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, target, http.StatusSeeOther)
	}
}

// securityHeaders sets the headers that every response of the web UI
// carries.
func securityHeaders(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		h := w.Header()
		h.Set("Content-Security-Policy", contentSecurityPolicy)
		h.Set("X-Content-Type-Options", "nosniff")
		h.Set("Referrer-Policy", "same-origin")
		next.ServeHTTP(w, r)
	})
}

// checkOrigin answers 403 to a request other than GET or HEAD whose Origin
// header names another host than the request. A request without Origin
// passes.
func checkOrigin(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			if origin := r.Header.Get("Origin"); origin != "" && !sameHost(origin, r.Host) {
				http.Error(w, "cross-origin request refused", http.StatusForbidden)
				return
			}
		}
		next.ServeHTTP(w, r)
	})
}

// sameHost reports whether the origin's host and port equal host.
func sameHost(origin, host string) bool {
	u, err := url.Parse(origin)
	if err != nil || u.Host == "" {
		return false
	}
	return strings.EqualFold(u.Host, host)
}

type sessionKey struct{}

// guard redirects to the sign-in page unless the request carries the cookie
// of a valid session. It records the session's use at most once per
// touchInterval.
func (s *server) guard(next func(http.ResponseWriter, *http.Request, store.Session)) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		sess, ok, err := s.session(r)
		if err != nil {
			s.internalError(w, r, "look up session", err)
			return
		}
		if !ok {
			if _, err := r.Cookie(CookieName); err == nil {
				clearCookie(w, r)
			}
			http.Redirect(w, r, "/ui/login", http.StatusSeeOther)
			return
		}
		if now := s.now(); now.Sub(sess.LastSeenAt) >= touchInterval {
			if err := s.Store.TouchSession(r.Context(), sess.ID, now); err != nil && r.Context().Err() == nil {
				s.Logger.Warn("record session use", "session", sess.ID, "error", err)
			}
		}
		next(w, r, sess)
	})
}

// session returns the session named by the request's cookie. ok is false
// when there is no cookie or its session is unknown or revoked.
func (s *server) session(r *http.Request) (sess store.Session, ok bool, err error) {
	c, err := r.Cookie(CookieName)
	if err != nil || c.Value == "" {
		return store.Session{}, false, nil
	}
	sess, err = s.Store.SessionByToken(r.Context(), c.Value)
	if errors.Is(err, store.ErrSessionNotFound) {
		return store.Session{}, false, nil
	}
	if err != nil {
		return store.Session{}, false, err
	}
	return sess, true, nil
}

// cookieMaxAge is 400 days, the longest lifetime browsers keep. The session
// row stays until it is revoked.
const cookieMaxAge = 400 * 24 * 60 * 60

// setCookie sets the session cookie. Its path is /ui/, so the browser does
// not send it on /rest/* and /share/*.
func setCookie(w http.ResponseWriter, r *http.Request, token string) {
	http.SetCookie(w, &http.Cookie{
		Name:     CookieName,
		Value:    token,
		Path:     "/ui/",
		MaxAge:   cookieMaxAge,
		HttpOnly: true,
		Secure:   r.TLS != nil,
		SameSite: http.SameSiteLaxMode,
	})
}

func clearCookie(w http.ResponseWriter, r *http.Request) {
	http.SetCookie(w, &http.Cookie{
		Name:     CookieName,
		Value:    "",
		Path:     "/ui/",
		MaxAge:   -1,
		HttpOnly: true,
		Secure:   r.TLS != nil,
		SameSite: http.SameSiteLaxMode,
	})
}

// staticFile serves a file from static/.
func (s *server) staticFile(w http.ResponseWriter, r *http.Request) {
	name := r.PathValue("name")
	data, err := fs.ReadFile(s.static, name)
	if err != nil {
		http.NotFound(w, r)
		return
	}
	if ct := mime.TypeByExtension(path.Ext(name)); ct != "" {
		w.Header().Set("Content-Type", ct)
	}
	w.Header().Set("Cache-Control", "no-cache")
	_, _ = w.Write(data)
}

// page is the data that layout.html reads.
type page struct {
	Title    string
	Nav      string
	SignedIn bool
	// Banner is the wn-cloud outage notice, or empty.
	Banner string
	// Wide gives the page more width.
	Wide bool
	// RefreshURL, when set, makes the browser load that path again after
	// RefreshSeconds.
	RefreshURL string
}

// refreshSeconds is how often the match page reloads while a job runs.
const refreshSeconds = 5

// RefreshSeconds is the refresh interval that the layout reads.
func (page) RefreshSeconds() int { return refreshSeconds }

// signedIn returns the layout data of a page behind the guard.
func (s *server) signedIn(title, nav string) page {
	p := page{Title: title, Nav: nav, SignedIn: true}
	if since, ok := s.CloudStatus.UnreachableSince(); ok {
		p.Banner = "wn-cloud unreachable since " + since.Local().Format("2006-01-02 15:04 MST")
	}
	return p
}

// render executes a page template and writes it with status.
func (s *server) render(w http.ResponseWriter, r *http.Request, status int, name string, data any) {
	var buf bytes.Buffer
	if err := s.pages[name].ExecuteTemplate(&buf, "layout", data); err != nil {
		s.internalError(w, r, "render "+name, err)
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	w.WriteHeader(status)
	_, _ = w.Write(buf.Bytes())
}

func (s *server) internalError(w http.ResponseWriter, r *http.Request, what string, err error) {
	if r.Context().Err() != nil {
		return
	}
	s.Logger.Error("web: "+what, "path", r.URL.Path, "error", err)
	http.Error(w, "internal error", http.StatusInternalServerError)
}

// remoteHost returns the request's remote address without the port.
// Forwarded headers are not read.
func remoteHost(r *http.Request) string {
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return r.RemoteAddr
	}
	return host
}
