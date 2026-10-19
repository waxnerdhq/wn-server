package web

import (
	"context"
	"crypto/subtle"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"unicode"

	"github.com/waxnerdhq/wn-server/internal/relay"
	"github.com/waxnerdhq/wn-server/internal/relayreq"
	"github.com/waxnerdhq/wn-server/internal/store"
)

type loginData struct {
	page
	Name    string
	Message string
}

// loginPage serves GET /ui/login. A browser that is signed in goes to the
// status page.
func (s *server) loginPage(w http.ResponseWriter, r *http.Request) {
	if _, ok, err := s.session(r); err == nil && ok {
		http.Redirect(w, r, "/ui/", http.StatusSeeOther)
		return
	}
	s.render(w, r, http.StatusOK, "login", loginData{page: page{Title: "Sign in"}})
}

// login serves POST /ui/login: it checks the account name and the app
// password, creates a session, and sets the cookie.
func (s *server) login(w http.ResponseWriter, r *http.Request) {
	r.Body = http.MaxBytesReader(w, r.Body, maxFormBytes)
	if err := r.ParseForm(); err != nil {
		http.Error(w, "bad form", http.StatusBadRequest)
		return
	}
	name, password := r.PostForm.Get("name"), r.PostForm.Get("password")
	addr := relayreq.ClientAddr(r)
	relayed := relayreq.IsRelayed(r.Context())
	logger := s.requestLogger(r)
	data := loginData{page: page{Title: "Sign in"}, Name: name}

	if !s.limiter.begin(addr, relayed) {
		logger.Warn("web sign-in failed", "remote", addr, "reason", "rate_limited")
		data.Message = "Too many failed sign-ins. Try again in a few minutes."
		s.render(w, r, http.StatusTooManyRequests, "login", data)
		return
	}
	acct, err := s.Store.AccountByName(r.Context(), name)
	if err != nil && !errors.Is(err, store.ErrNotFound) {
		s.limiter.forget(addr, relayed)
		s.internalError(w, r, "look up account", err)
		return
	}
	// Only the owner signs in to the web UI. A listener's stored password is
	// a hash, which must not work as a password either.
	if err != nil || acct.Kind != store.KindOwner || subtle.ConstantTimeCompare([]byte(password), []byte(acct.AppPassword)) != 1 {
		logger.Warn("web sign-in failed", "remote", addr, "reason", "bad_credentials")
		data.Message = "Wrong account name or app password."
		s.render(w, r, http.StatusUnauthorized, "login", data)
		return
	}
	s.limiter.forget(addr, relayed)

	token, sess, err := s.Store.CreateSession(r.Context(), acct.ID, deviceLabel(r.UserAgent()))
	if err != nil {
		s.internalError(w, r, "create session", err)
		return
	}
	logger.Info("web sign-in", "account", acct.ID, "session", sess.ID, "remote", addr)
	// A session that the browser already holds would be left without a cookie.
	if c, err := r.Cookie(CookieName); err == nil && c.Value != "" {
		_, _ = s.Store.RevokeSessionByToken(r.Context(), c.Value)
	}
	setCookie(w, r, token)
	http.Redirect(w, r, "/ui/", http.StatusSeeOther)
}

// logout serves POST /ui/logout: it revokes the cookie's session and clears
// the cookie.
func (s *server) logout(w http.ResponseWriter, r *http.Request) {
	if c, err := r.Cookie(CookieName); err == nil && c.Value != "" {
		sess, err := s.Store.RevokeSessionByToken(r.Context(), c.Value)
		switch {
		case err == nil:
			s.Logger.Info("web sign-out", "account", sess.AccountID, "session", sess.ID)
		case !errors.Is(err, store.ErrSessionNotFound):
			s.internalError(w, r, "revoke session", err)
			return
		}
	}
	clearCookie(w, r)
	http.Redirect(w, r, "/ui/login", http.StatusSeeOther)
}

// deviceLabel returns "web" followed by the user agent, without control
// characters and cut to maxUserAgent characters.
func deviceLabel(userAgent string) string {
	ua := strings.TrimSpace(strings.Map(func(c rune) rune {
		if unicode.IsControl(c) {
			return -1
		}
		return c
	}, userAgent))
	if r := []rune(ua); len(r) > maxUserAgent {
		ua = strings.TrimSpace(string(r[:maxUserAgent]))
	}
	if ua == "" {
		return "web"
	}
	return "web " + ua
}

type statusData struct {
	page
	ServerName       string
	OwnerName        string
	NavidromeVersion string
	NavidromeReady   bool
	ScanState        string
	Linked           bool
	WaxnerdUserID    string
	ServerID         string
	PendingMatches   int
	MatchJobRunning  bool
	// RelayText is the relay line. When the relay is connected, RelayURL is
	// the relay address and RelayText is the line without it.
	RelayText string
	RelayURL  string
	// RelayAddress is the stored relay address while the relay is not
	// connected and wn-cloud has given one.
	RelayAddress string
	// RelayOn is whether the relay is switched on: the owner's switch, else
	// the config key.
	RelayOn bool
	Notice  string
}

// relayLine returns the relay line, split before the relay address when the
// relay is connected, so the page can show the address as a link.
func (s *server) relayLine() (text, url string) {
	st := relay.State{Name: relay.Off}
	if s.Relay != nil {
		st = s.Relay()
	}
	text = st.Text()
	if st.Name == relay.Connected && st.Host != "" {
		url = "https://" + st.Host
		text = strings.TrimSuffix(text, url)
	}
	return text, url
}

// statusPage serves GET /ui/.
func (s *server) statusPage(w http.ResponseWriter, r *http.Request, _ store.Session) {
	ctx := r.Context()
	owner, err := s.Store.Owner(ctx)
	if err != nil {
		s.internalError(w, r, "read owner account", err)
		return
	}
	data := statusData{
		page:             s.signedIn("Status", "status"),
		ServerName:       s.ServerName,
		OwnerName:        owner.Name,
		NavidromeVersion: s.NavidromeVersion,
		NavidromeReady:   s.ready(),
	}
	data.ScanState = s.scanState(ctx, data.NavidromeReady)
	data.PendingMatches, err = s.Store.CountMatches(ctx, store.MatchPending)
	if err != nil {
		s.internalError(w, r, "count pending matches", err)
		return
	}
	data.MatchJobRunning = s.Jobs.Status().Running != nil
	data.RelayText, data.RelayURL = s.relayLine()
	if data.RelayURL == "" {
		rs, err := s.Store.RelaySettings(ctx)
		switch {
		case err == nil:
			if rs.Host != "" {
				data.RelayAddress = "https://" + rs.Host
			}
		case !errors.Is(err, store.ErrNoRelaySettings):
			s.internalError(w, r, "read relay settings", err)
			return
		}
	}
	data.RelayOn, err = s.relayOn(ctx)
	if err != nil {
		s.internalError(w, r, "read relay switch", err)
		return
	}
	switch r.URL.Query().Get("done") {
	case "relay-on":
		data.Notice = "The relay is switched on. The relay line changes within about 20 seconds."
	case "relay-off":
		data.Notice = "The relay is switched off. The relay line changes within about 15 seconds."
	}
	cred, err := s.Store.ServerCredential(ctx)
	switch {
	case err == nil:
		data.Linked, data.WaxnerdUserID, data.ServerID = true, cred.WaxnerdUserID, cred.ServerID
	case !errors.Is(err, store.ErrNoServerCredential):
		s.internalError(w, r, "read server credential", err)
		return
	}
	s.render(w, r, http.StatusOK, "status", data)
}

// relayOn answers whether the relay is switched on: the owner's switch, else
// the config key.
func (s *server) relayOn(ctx context.Context) (bool, error) {
	enabled, set, err := s.Store.RelayEnabled(ctx)
	if err != nil {
		return false, err
	}
	if !set {
		return s.RelayConfig, nil
	}
	return enabled, nil
}

// relaySwitch serves POST /ui/relay. The form field enabled is 1 or 0. The
// relay package reads the switch within 10 seconds, so no restart is needed.
func (s *server) relaySwitch(w http.ResponseWriter, r *http.Request, sess store.Session) {
	r.Body = http.MaxBytesReader(w, r.Body, maxFormBytes)
	if err := r.ParseForm(); err != nil {
		http.Error(w, "bad form", http.StatusBadRequest)
		return
	}
	var enabled bool
	switch r.PostForm.Get("enabled") {
	case "1":
		enabled = true
	case "0":
	default:
		http.Error(w, "enabled must be 1 or 0", http.StatusBadRequest)
		return
	}
	if err := s.Store.SetRelayEnabled(r.Context(), enabled); err != nil {
		s.internalError(w, r, "set relay switch", err)
		return
	}
	s.Logger.Info("relay switch set", "enabled", enabled, "account", sess.AccountID, "session", sess.ID)
	done := "relay-off"
	if enabled {
		done = "relay-on"
	}
	http.Redirect(w, r, "/ui/?done="+done, http.StatusSeeOther)
}

func (s *server) ready() bool {
	if s.Ready == nil {
		return false
	}
	select {
	case <-s.Ready:
		return true
	default:
		return false
	}
}

// scanState describes Navidrome's scan state in one line.
func (s *server) scanState(ctx context.Context, ready bool) string {
	if !ready {
		return "Navidrome starting"
	}
	if s.Navidrome == nil {
		return "unavailable"
	}
	ctx, cancel := context.WithTimeout(ctx, scanTimeout)
	defer cancel()
	st, err := s.Navidrome.ScanStatus(ctx)
	if err != nil {
		// A canceled context means the browser went away.
		if !errors.Is(ctx.Err(), context.Canceled) {
			s.Logger.Warn("read scan status", "error", err)
		}
		return "unavailable"
	}
	var b strings.Builder
	if st.Scanning {
		fmt.Fprintf(&b, "scanning; %d tracks in %d folders so far", st.Count, st.FolderCount)
	} else {
		fmt.Fprintf(&b, "idle; %d tracks in %d folders", st.Count, st.FolderCount)
	}
	if st.LastScan.IsZero() {
		b.WriteString("; no scan has finished yet")
	} else {
		b.WriteString("; last scan " + when(st.LastScan))
	}
	if st.Error != "" {
		b.WriteString("; last scan error: " + st.Error)
	}
	return b.String()
}

type sessionRow struct {
	store.Session
	Current bool
}

type accountData struct {
	page
	Name        string
	AppPassword string
	Sessions    []sessionRow
	Notice      string
}

// accountPage serves GET /ui/account.
func (s *server) accountPage(w http.ResponseWriter, r *http.Request, sess store.Session) {
	ctx := r.Context()
	owner, err := s.Store.Owner(ctx)
	if err != nil {
		s.internalError(w, r, "read owner account", err)
		return
	}
	list, err := s.Store.Sessions(ctx, sess.AccountID)
	if err != nil {
		s.internalError(w, r, "list sessions", err)
		return
	}
	data := accountData{page: s.signedIn("Account", "account"), Name: owner.Name, AppPassword: owner.AppPassword}
	for _, x := range list {
		data.Sessions = append(data.Sessions, sessionRow{Session: x, Current: x.ID == sess.ID})
	}
	switch r.URL.Query().Get("done") {
	case "reset":
		data.Notice = "A new app password is set. Clients that use the old one must be given the new one."
	case "revoked":
		data.Notice = "The session is revoked."
	}
	s.render(w, r, http.StatusOK, "account", data)
}

// resetPassword serves POST /ui/account/password/reset. It has the effect of
// wn-server password --reset.
func (s *server) resetPassword(w http.ResponseWriter, r *http.Request, sess store.Session) {
	pw, err := store.NewAppPassword()
	if err != nil {
		s.internalError(w, r, "generate app password", err)
		return
	}
	if err := s.Store.SetAppPassword(r.Context(), sess.AccountID, pw); err != nil {
		s.internalError(w, r, "set app password", err)
		return
	}
	s.Logger.Info("app password reset", "account", sess.AccountID, "session", sess.ID)
	http.Redirect(w, r, "/ui/account?done=reset", http.StatusSeeOther)
}

// revokeSession serves POST /ui/sessions/{id}/revoke. Revoking the
// browser's own session also clears the cookie.
func (s *server) revokeSession(w http.ResponseWriter, r *http.Request, sess store.Session) {
	id := r.PathValue("id")
	list, err := s.Store.Sessions(r.Context(), sess.AccountID)
	if err != nil {
		s.internalError(w, r, "list sessions", err)
		return
	}
	found := false
	for _, x := range list {
		found = found || x.ID == id
	}
	if !found {
		http.NotFound(w, r)
		return
	}
	if err := s.Store.RevokeSession(r.Context(), id); err != nil {
		if errors.Is(err, store.ErrSessionNotFound) {
			http.NotFound(w, r)
			return
		}
		s.internalError(w, r, "revoke session", err)
		return
	}
	s.Logger.Info("session revoked", "account", sess.AccountID, "session", id, "by_session", sess.ID)
	if id == sess.ID {
		clearCookie(w, r)
		http.Redirect(w, r, "/ui/login", http.StatusSeeOther)
		return
	}
	http.Redirect(w, r, "/ui/account?done=revoked", http.StatusSeeOther)
}
