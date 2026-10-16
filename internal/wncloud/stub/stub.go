// Package stub serves the wn-cloud routes described in docs/wn-cloud-api.md
// from memory, for tests. It is the executable form of that document, not a
// second wn-cloud: it keeps no history, has no rate limits, and checks only
// what the document says each route checks.
//
// Use it with httptest:
//
//	s := stub.New()
//	s.AddUser(ownerID, "owner@example.com", "adnan")
//	s.AddToken("wn_...", ownerID)
//	srv := httptest.NewServer(s)
//
// The approval page of the browser sign-in (GET /cli/auth) has no user
// interface here. It redirects at once with a code for the user set by
// SignInAs, or with error=access_denied when no user is set.
package stub

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
	"net/netip"
	"net/url"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"
)

// MaxListeners is the number of listeners a server may have.
const MaxListeners = 12

const codeTTL = 120 * time.Second

// cursorLayout is the time part of the GET /ratings cursor and of the rating
// timestamps: RFC 3339 in UTC with microseconds, as Postgres stores them.
const cursorLayout = "2006-01-02T15:04:05.000000Z07:00"

var (
	stateRE     = regexp.MustCompile(`^[A-Za-z0-9_-]{16,128}$`)
	challengeRE = regexp.MustCompile(`^[A-Za-z0-9_-]{43}$`)
	codeRE      = regexp.MustCompile(`^[A-Za-z0-9_-]{43}$`)
	verifierRE  = regexp.MustCompile(`^[A-Za-z0-9._~-]{43,128}$`)
	portRE      = regexp.MustCompile(`^[0-9]{4,5}$`)
	uuidRE      = regexp.MustCompile(`^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$`)
	kinds       = map[string]bool{"release_group": true, "release": true, "track": true, "artist": true}
)

type user struct {
	id, email, username string
	createdAt           time.Time
}

type authCode struct {
	userID, challenge, redirectURI, label string
	port                                  int
	expires                               time.Time
}

type server struct {
	id, ownerID, name string
	redirectURIs      []string
	createdAt         time.Time
	members           map[string]time.Time // listener user id to the time of the invite
}

type ratingKey struct{ userID, kind, mbid string }

type rating struct {
	id, kind, mbid       string
	halfStars            *int
	likeLevel            *string
	createdAt, updatedAt time.Time
	deleted              bool
}

// Stub is an in-memory wn-cloud. It implements http.Handler. The zero value
// is not usable; call New.
type Stub struct {
	mu           sync.Mutex
	mux          *http.ServeMux
	users        map[string]*user
	tokens       map[string]string // bearer to user id
	codes        map[string]*authCode
	servers      map[string]*server
	ratings      map[ratingKey]*rating
	appRedirects map[string]bool
	signedIn     string
	last         time.Time
}

// New returns an empty stub.
func New() *Stub {
	s := &Stub{
		mux:          http.NewServeMux(),
		users:        map[string]*user{},
		tokens:       map[string]string{},
		codes:        map[string]*authCode{},
		servers:      map[string]*server{},
		ratings:      map[ratingKey]*rating{},
		appRedirects: map[string]bool{},
	}
	s.mux.HandleFunc("GET /me", s.me)
	s.mux.HandleFunc("GET /cli/auth", s.cliAuth)
	s.mux.HandleFunc("POST /cli/token", s.cliToken)
	s.mux.HandleFunc("POST /cli/logout", s.cliLogout)
	s.mux.HandleFunc("POST /servers", s.createServer)
	s.mux.HandleFunc("PUT /servers/{id}/redirect-uris", s.putRedirectURIs)
	s.mux.HandleFunc("GET /servers/{id}/members", s.listMembers)
	s.mux.HandleFunc("POST /servers/{id}/invites", s.invite)
	s.mux.HandleFunc("DELETE /servers/{id}/members/{userId}", s.removeMember)
	s.mux.HandleFunc("PUT /ratings/{kind}/{mbid}", s.putRating)
	s.mux.HandleFunc("DELETE /ratings/{kind}/{mbid}", s.deleteRating)
	s.mux.HandleFunc("GET /ratings/{kind}/{mbid}", s.getRating)
	s.mux.HandleFunc("GET /ratings", s.listRatings)
	return s
}

// ServeHTTP serves one request. Requests are handled one at a time.
func (s *Stub) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.mux.ServeHTTP(w, r)
}

// Seed helpers.

// AddUser adds a waxnerd user with a row in users. An empty username means
// the user has no profile.
func (s *Stub) AddUser(id, email, username string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.users[id] = &user{id: id, email: email, username: username, createdAt: s.now()}
}

// AddToken makes token a valid bearer for the user. A token that starts with
// wn_ acts as a CLI token; any other string acts as a GoTrue access token.
// The user does not need to exist: a token of an unknown user gets 403
// user_not_created, as a GoTrue account without a users row does.
func (s *Stub) AddToken(token, userID string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.tokens[token] = userID
}

// AddServer registers a server under ownerID.
func (s *Stub) AddServer(id, ownerID, name string, redirectURIs ...string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.servers[id] = &server{
		id: id, ownerID: ownerID, name: name,
		redirectURIs: append([]string{}, redirectURIs...),
		createdAt:    s.now(),
		members:      map[string]time.Time{},
	}
}

// AddMember makes userID a listener of the server. It panics when the server
// does not exist.
func (s *Stub) AddMember(serverID, userID string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.servers[serverID].members[userID] = s.now()
}

// AddRating sets a user's rating. halfStars 0 and likeLevel "" mean null.
func (s *Stub) AddRating(userID, kind, mbid string, halfStars int, likeLevel string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	var hs *int
	var ll *string
	if halfStars != 0 {
		hs = &halfStars
	}
	if likeLevel != "" {
		ll = &likeLevel
	}
	s.setRating(userID, kind, strings.ToLower(mbid), hs, ll)
}

// AddAppRedirectURI adds a first-party app redirect URI that the browser
// sign-in accepts without a server_id.
func (s *Stub) AddAppRedirectURI(uri string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.appRedirects[uri] = true
}

// SignInAs sets the user that the approval page approves as. An empty id
// makes the page cancel.
func (s *Stub) SignInAs(userID string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.signedIn = userID
}

// Helpers.

// now returns the current time in UTC with microseconds. Each call returns a
// later time than the one before, so rating cursors are strictly ordered.
func (s *Stub) now() time.Time {
	t := time.Now().UTC().Truncate(time.Microsecond)
	if !t.After(s.last) {
		t = s.last.Add(time.Microsecond)
	}
	s.last = t
	return t
}

func randomURLSafe() string {
	b := make([]byte, 32)
	_, _ = rand.Read(b)
	return base64.RawURLEncoding.EncodeToString(b)
}

func newUUID() string {
	b := make([]byte, 16)
	_, _ = rand.Read(b)
	b[6] = b[6]&0x0f | 0x40
	b[8] = b[8]&0x3f | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:])
}

type errorBody struct {
	Error string `json:"error"`
	Code  string `json:"code,omitempty"`
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func writeError(w http.ResponseWriter, status int, code, msg string) {
	writeJSON(w, status, errorBody{Error: msg, Code: code})
}

func decode(r *http.Request, v any) error {
	return json.NewDecoder(r.Body).Decode(v)
}

// authUser applies v2's requireUser check. It writes the error and returns nil
// when the request is not authenticated.
func (s *Stub) authUser(w http.ResponseWriter, r *http.Request) *user {
	header := r.Header.Get("Authorization")
	if !strings.HasPrefix(header, "Bearer ") {
		writeError(w, http.StatusUnauthorized, "", "Missing bearer token")
		return nil
	}
	id, ok := s.tokens[strings.TrimPrefix(header, "Bearer ")]
	if !ok {
		writeError(w, http.StatusUnauthorized, "", "Invalid or expired token")
		return nil
	}
	u := s.users[id]
	if u == nil {
		writeError(w, http.StatusForbidden, "user_not_created", "User not created")
		return nil
	}
	return u
}

// ownedServer returns the server in the path when u owns it.
func (s *Stub) ownedServer(w http.ResponseWriter, r *http.Request, u *user) *server {
	srv := s.servers[r.PathValue("id")]
	if srv == nil {
		writeError(w, http.StatusNotFound, "server_not_found", "No server has this id")
		return nil
	}
	if srv.ownerID != u.id {
		writeError(w, http.StatusForbidden, "not_owner", "The caller is not the owner of this server")
		return nil
	}
	return srv
}

func validLabel(label string) bool {
	if len([]rune(label)) > 100 {
		return false
	}
	for _, c := range label {
		if unicode.IsControl(c) {
			return false
		}
	}
	return true
}

func validRedirectURI(raw string) bool {
	u, err := url.Parse(raw)
	if err != nil || !u.IsAbs() || u.User != nil || u.RawQuery != "" || u.ForceQuery || u.Fragment != "" || u.Hostname() == "" {
		return false
	}
	switch u.Scheme {
	case "https":
		return true
	case "http":
		host := u.Hostname()
		if addr, err := netip.ParseAddr(host); err == nil {
			return addr.IsLoopback() || addr.IsPrivate()
		}
		return strings.HasSuffix(host, ".local")
	}
	return false
}

// Existing routes.

func (s *Stub) me(w http.ResponseWriter, r *http.Request) {
	u := s.authUser(w, r)
	if u == nil {
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"id": u.id, "email": u.email, "createdAt": u.createdAt})
}

func (s *Stub) cliAuth(w http.ResponseWriter, r *http.Request) {
	bad := func() {
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		w.WriteHeader(http.StatusBadRequest)
		_, _ = w.Write([]byte("<p>This sign-in request is not valid.</p>"))
	}
	q := r.URL.Query()
	state, challenge, label := q.Get("state"), q.Get("challenge"), strings.TrimSpace(q.Get("label"))
	if !stateRE.MatchString(state) || !challengeRE.MatchString(challenge) || !validLabel(label) {
		bad()
		return
	}
	if q.Has("port") == q.Has("redirect_uri") {
		bad()
		return
	}
	code := &authCode{challenge: challenge, label: label}
	var callback string
	if q.Has("port") {
		p := q.Get("port")
		port, _ := strconv.Atoi(p)
		if !portRE.MatchString(p) || port < 1024 || port > 65535 {
			bad()
			return
		}
		code.port = port
		callback = fmt.Sprintf("http://127.0.0.1:%d/callback", port)
	} else {
		uri := q.Get("redirect_uri")
		if id := q.Get("server_id"); id != "" {
			srv := s.servers[id]
			if srv == nil || !contains(srv.redirectURIs, uri) {
				bad()
				return
			}
			if code.label == "" {
				code.label = srv.name
			}
		} else if !s.appRedirects[uri] {
			bad()
			return
		}
		code.redirectURI = uri
		callback = uri
	}

	result := url.Values{"state": {state}}
	if s.signedIn == "" {
		result.Set("error", "access_denied")
	} else {
		value := randomURLSafe()
		code.userID = s.signedIn
		code.expires = s.now().Add(codeTTL)
		s.codes[value] = code
		result.Set("code", value)
	}
	http.Redirect(w, r, callback+"?"+result.Encode(), http.StatusSeeOther)
}

func contains(list []string, v string) bool {
	for _, x := range list {
		if x == v {
			return true
		}
	}
	return false
}

func (s *Stub) cliToken(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Cache-Control", "no-store")
	var body map[string]any
	if err := decode(r, &body); err != nil {
		http.Error(w, "Bad Request", http.StatusBadRequest)
		return
	}
	invalid := func(msg string) { writeError(w, http.StatusBadRequest, "invalid_request", msg) }
	codeValue, _ := body["code"].(string)
	if !codeRE.MatchString(codeValue) {
		invalid("code must be 43 base64url characters")
		return
	}
	verifier, _ := body["code_verifier"].(string)
	if !verifierRE.MatchString(verifier) {
		invalid("code_verifier must be 43 to 128 unreserved characters")
		return
	}
	rawPort, hasPort := body["redirect_port"]
	rawURI, hasURI := body["redirect_uri"]
	if hasPort == hasURI {
		invalid("send exactly one of redirect_port and redirect_uri")
		return
	}
	port, uri := 0, ""
	if hasPort {
		f, ok := rawPort.(float64)
		if !ok || f != float64(int(f)) || f < 1024 || f > 65535 {
			invalid("redirect_port must be an integer from 1024 to 65535")
			return
		}
		port = int(f)
	} else {
		var ok bool
		if uri, ok = rawURI.(string); !ok || uri == "" {
			invalid("redirect_uri must be a URL")
			return
		}
	}

	code := s.codes[codeValue]
	delete(s.codes, codeValue) // a live code is consumed even when the exchange fails
	sum := sha256.Sum256([]byte(verifier))
	if code == nil || s.now().After(code.expires) ||
		base64.RawURLEncoding.EncodeToString(sum[:]) != code.challenge ||
		code.port != port || code.redirectURI != uri {
		writeError(w, http.StatusBadRequest, "invalid_grant", "The code is invalid, expired or already used")
		return
	}
	u := s.users[code.userID]
	if u == nil {
		writeError(w, http.StatusBadRequest, "invalid_grant", "The code is invalid, expired or already used")
		return
	}
	token := "wn_" + randomURLSafe()
	s.tokens[token] = u.id
	writeJSON(w, http.StatusOK, map[string]any{
		"token":      token,
		"token_type": "Bearer",
		"user":       map[string]string{"id": u.id, "email": u.email},
	})
}

func (s *Stub) cliLogout(w http.ResponseWriter, r *http.Request) {
	header := r.Header.Get("Authorization")
	if !strings.HasPrefix(header, "Bearer ") {
		writeError(w, http.StatusUnauthorized, "", "Missing bearer token")
		return
	}
	token := strings.TrimPrefix(header, "Bearer ")
	if _, ok := s.tokens[token]; !ok {
		writeError(w, http.StatusUnauthorized, "", "Invalid or expired token")
		return
	}
	if !strings.HasPrefix(token, "wn_") {
		writeError(w, http.StatusBadRequest, "invalid_request", "Only a CLI token can be revoked here")
		return
	}
	delete(s.tokens, token)
	w.WriteHeader(http.StatusNoContent)
}

// Servers and members.

func serverJSON(srv *server) map[string]any {
	return map[string]any{
		"id":            srv.id,
		"owner_id":      srv.ownerID,
		"name":          srv.name,
		"redirect_uris": append([]string{}, srv.redirectURIs...),
		"created_at":    srv.createdAt,
	}
}

func (s *Stub) memberJSON(userID, role string, created time.Time) map[string]any {
	m := map[string]any{"user_id": userID, "email": nil, "username": nil, "role": role, "created_at": created}
	if u := s.users[userID]; u != nil {
		m["email"] = u.email
		if u.username != "" {
			m["username"] = u.username
		}
	}
	return m
}

func (s *Stub) createServer(w http.ResponseWriter, r *http.Request) {
	u := s.authUser(w, r)
	if u == nil {
		return
	}
	var body struct {
		Name string `json:"name"`
	}
	if err := decode(r, &body); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "The body is not valid JSON")
		return
	}
	name := strings.TrimSpace(body.Name)
	if name == "" || !validLabel(name) {
		writeError(w, http.StatusBadRequest, "invalid_request", "name must be 1 to 100 characters")
		return
	}
	srv := &server{id: newUUID(), ownerID: u.id, name: name, redirectURIs: []string{}, createdAt: s.now(), members: map[string]time.Time{}}
	s.servers[srv.id] = srv
	writeJSON(w, http.StatusCreated, serverJSON(srv))
}

func (s *Stub) putRedirectURIs(w http.ResponseWriter, r *http.Request) {
	u := s.authUser(w, r)
	if u == nil {
		return
	}
	srv := s.ownedServer(w, r, u)
	if srv == nil {
		return
	}
	var body struct {
		RedirectURIs *[]string `json:"redirect_uris"`
	}
	if err := decode(r, &body); err != nil || body.RedirectURIs == nil || len(*body.RedirectURIs) > 10 {
		writeError(w, http.StatusBadRequest, "invalid_request", "redirect_uris must be a list of at most 10 URLs")
		return
	}
	for _, uri := range *body.RedirectURIs {
		if !validRedirectURI(uri) {
			writeError(w, http.StatusBadRequest, "invalid_request", "redirect URI not allowed: "+uri)
			return
		}
	}
	srv.redirectURIs = append([]string{}, *body.RedirectURIs...)
	writeJSON(w, http.StatusOK, serverJSON(srv))
}

func (s *Stub) listMembers(w http.ResponseWriter, r *http.Request) {
	u := s.authUser(w, r)
	if u == nil {
		return
	}
	srv := s.ownedServer(w, r, u)
	if srv == nil {
		return
	}
	ids := make([]string, 0, len(srv.members))
	for id := range srv.members {
		ids = append(ids, id)
	}
	sort.Slice(ids, func(i, j int) bool { return srv.members[ids[i]].Before(srv.members[ids[j]]) })
	members := make([]map[string]any, 0, len(ids))
	for _, id := range ids {
		members = append(members, s.memberJSON(id, "listener", srv.members[id]))
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"server_id": srv.id,
		"owner":     s.memberJSON(srv.ownerID, "owner", srv.createdAt),
		"members":   members,
	})
}

func (s *Stub) invite(w http.ResponseWriter, r *http.Request) {
	u := s.authUser(w, r)
	if u == nil {
		return
	}
	srv := s.ownedServer(w, r, u)
	if srv == nil {
		return
	}
	var body struct {
		Email string `json:"email"`
	}
	if err := decode(r, &body); err != nil || !strings.Contains(body.Email, "@") || strings.ContainsAny(body.Email, " \t\r\n") {
		writeError(w, http.StatusBadRequest, "invalid_request", "email must be an email address")
		return
	}
	var invitee *user
	for _, x := range s.users {
		if strings.EqualFold(x.email, body.Email) {
			invitee = x
			break
		}
	}
	if invitee == nil {
		writeError(w, http.StatusNotFound, "user_not_found", "No waxnerd user has this email")
		return
	}
	if _, ok := srv.members[invitee.id]; ok || invitee.id == srv.ownerID {
		writeError(w, http.StatusConflict, "already_member", "The user is already a member of this server")
		return
	}
	if len(srv.members) >= MaxListeners {
		writeError(w, http.StatusConflict, "member_limit", "The server already has 12 listeners")
		return
	}
	srv.members[invitee.id] = s.now()
	writeJSON(w, http.StatusCreated, s.memberJSON(invitee.id, "listener", srv.members[invitee.id]))
}

func (s *Stub) removeMember(w http.ResponseWriter, r *http.Request) {
	u := s.authUser(w, r)
	if u == nil {
		return
	}
	srv := s.ownedServer(w, r, u)
	if srv == nil {
		return
	}
	id := r.PathValue("userId")
	if _, ok := srv.members[id]; !ok {
		writeError(w, http.StatusNotFound, "not_member", "The user is not a listener of this server")
		return
	}
	delete(srv.members, id)
	w.WriteHeader(http.StatusNoContent)
}

// Ratings.

func ratingJSON(rt *rating) map[string]any {
	return map[string]any{
		"id":         rt.id,
		"kind":       rt.kind,
		"mbid":       rt.mbid,
		"half_stars": rt.halfStars,
		"like_level": rt.likeLevel,
		"created_at": rt.createdAt.Format(cursorLayout),
		"updated_at": rt.updatedAt.Format(cursorLayout),
	}
}

// subject validates the path of a rating route. It writes the error and
// returns false when the path is malformed.
func subject(w http.ResponseWriter, r *http.Request) (kind, mbid string, ok bool) {
	kind, mbid = r.PathValue("kind"), r.PathValue("mbid")
	if !kinds[kind] || !uuidRE.MatchString(mbid) {
		writeError(w, http.StatusBadRequest, "invalid_request", "kind or mbid is malformed")
		return "", "", false
	}
	return kind, strings.ToLower(mbid), true
}

// setRating inserts or updates a rating. A cleared rating is replaced by a
// new one with a new id.
func (s *Stub) setRating(userID, kind, mbid string, halfStars *int, likeLevel *string) *rating {
	key := ratingKey{userID, kind, mbid}
	now := s.now()
	rt := s.ratings[key]
	if rt == nil || rt.deleted {
		rt = &rating{id: newUUID(), kind: kind, mbid: mbid, createdAt: now}
		s.ratings[key] = rt
	}
	rt.halfStars, rt.likeLevel, rt.updatedAt = halfStars, likeLevel, now
	return rt
}

func (s *Stub) putRating(w http.ResponseWriter, r *http.Request) {
	u := s.authUser(w, r)
	if u == nil {
		return
	}
	kind, mbid, ok := subject(w, r)
	if !ok {
		return
	}
	var body struct {
		HalfStars *int    `json:"half_stars"`
		LikeLevel *string `json:"like_level"`
	}
	err := decode(r, &body)
	switch {
	case err != nil:
		writeError(w, http.StatusBadRequest, "invalid_request", "The body is not valid")
	case body.HalfStars != nil && (*body.HalfStars < 1 || *body.HalfStars > 10):
		writeError(w, http.StatusBadRequest, "invalid_request", "half_stars must be from 1 to 10")
	case body.LikeLevel != nil && *body.LikeLevel != "like" && *body.LikeLevel != "love":
		writeError(w, http.StatusBadRequest, "invalid_request", "like_level must be like or love")
	case body.HalfStars == nil && body.LikeLevel == nil:
		writeError(w, http.StatusBadRequest, "invalid_request", "Set half_stars or like_level, or use DELETE")
	default:
		writeJSON(w, http.StatusOK, ratingJSON(s.setRating(u.id, kind, mbid, body.HalfStars, body.LikeLevel)))
	}
}

func (s *Stub) deleteRating(w http.ResponseWriter, r *http.Request) {
	u := s.authUser(w, r)
	if u == nil {
		return
	}
	kind, mbid, ok := subject(w, r)
	if !ok {
		return
	}
	if rt := s.ratings[ratingKey{u.id, kind, mbid}]; rt != nil && !rt.deleted {
		rt.deleted, rt.halfStars, rt.likeLevel, rt.updatedAt = true, nil, nil, s.now()
	}
	w.WriteHeader(http.StatusNoContent)
}

func (s *Stub) getRating(w http.ResponseWriter, r *http.Request) {
	u := s.authUser(w, r)
	if u == nil {
		return
	}
	kind, mbid, ok := subject(w, r)
	if !ok {
		return
	}
	rt := s.ratings[ratingKey{u.id, kind, mbid}]
	if rt == nil || rt.deleted {
		writeError(w, http.StatusNotFound, "not_found", "The caller has no rating of this subject")
		return
	}
	writeJSON(w, http.StatusOK, ratingJSON(rt))
}

func cursorOf(rt *rating) string {
	return rt.updatedAt.Format(cursorLayout) + "," + rt.id
}

// after reports whether rt comes after the cursor (t, id).
func after(rt *rating, t time.Time, id string) bool {
	return rt.updatedAt.After(t) || rt.updatedAt.Equal(t) && rt.id > id
}

func (s *Stub) listRatings(w http.ResponseWriter, r *http.Request) {
	u := s.authUser(w, r)
	if u == nil {
		return
	}
	q := r.URL.Query()
	since := q.Get("since")
	var sinceTime time.Time
	var sinceID string
	if since != "" {
		ts, id, found := strings.Cut(since, ",")
		t, err := time.Parse(time.RFC3339Nano, ts)
		if !found || err != nil || id == "" {
			writeError(w, http.StatusBadRequest, "invalid_request", "since is malformed")
			return
		}
		sinceTime, sinceID = t, id
	}
	limit := 500
	if v := q.Get("limit"); v != "" {
		n, err := strconv.Atoi(v)
		if err != nil || n < 1 || n > 1000 {
			writeError(w, http.StatusBadRequest, "invalid_request", "limit must be from 1 to 1000")
			return
		}
		limit = n
	}

	var list []*rating
	for key, rt := range s.ratings {
		if key.userID != u.id {
			continue
		}
		if since == "" && rt.deleted {
			continue
		}
		if since != "" && !after(rt, sinceTime, sinceID) {
			continue
		}
		list = append(list, rt)
	}
	sort.Slice(list, func(i, j int) bool {
		a, b := list[i], list[j]
		return a.updatedAt.Before(b.updatedAt) || a.updatedAt.Equal(b.updatedAt) && a.id < b.id
	})
	more := len(list) > limit
	if more {
		list = list[:limit]
	}

	changes := make([]map[string]any, 0, len(list))
	for _, rt := range list {
		c := ratingJSON(rt)
		c["deleted"] = rt.deleted
		changes = append(changes, c)
	}
	var next any
	switch {
	case len(list) > 0:
		next = cursorOf(list[len(list)-1])
	case since != "":
		next = since
	}
	writeJSON(w, http.StatusOK, map[string]any{"changes": changes, "next": next, "more": more})
}
