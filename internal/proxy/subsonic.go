package proxy

import (
	"context"
	"crypto/md5" //nolint:gosec // the Subsonic token scheme is MD5
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"encoding/xml"
	"errors"
	"fmt"
	"log/slog"
	"mime"
	"net/http"
	"net/url"
	"regexp"
	"strings"

	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/store"
)

// Subsonic error codes, as in Navidrome's server/subsonic/responses.
const (
	errGeneric         = 0
	errMissingParam    = 10
	errWrongCredential = 40
	errMultipleAuth    = 43
	errInvalidAPIKey   = 44
)

// subsonicAPIVersion is the Subsonic API version Navidrome 0.64.2 reports.
const subsonicAPIVersion = "1.16.1"

// credentialParams are removed before forwarding. Navidrome ignores them when
// Remote-User comes from a trusted source, and removing them keeps the app
// password and sessions out of Navidrome's logs.
var credentialParams = []string{"u", "p", "t", "s", "jwt", "apiKey"}

// apiKeyConflicts may not be sent together with apiKey, as in Navidrome.
var apiKeyConflicts = []string{"u", "p", "t", "s", "jwt"}

// maxFormBytes matches Navidrome's limit on a POSTed Subsonic form.
const maxFormBytes = 10 << 20

// subsonicAuth checks a wn-server session, sent as the OpenSubsonic apiKey
// parameter or as Authorization: Bearer, or else the Subsonic credentials in
// u plus t and s, or u plus p, against the store. On success it forwards the
// request with the account id as Remote-User and without the credential
// parameters. On failure it answers as Navidrome does: HTTP 200 with a
// Subsonic error body.
func subsonicAuth(st Store, sess *sessions, logger *slog.Logger, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		params, err := subsonicParams(w, r)
		if err != nil {
			writeSubsonicError(w, r.URL.Query(), errGeneric, err.Error())
			return
		}
		apiKey := params.Get("apiKey")
		bearer, hasBearer := bearerToken(r)
		if apiKey != "" || hasBearer {
			if conflicting(params, apiKey != "", hasBearer) {
				logger.Warn("subsonic session sent with other credentials", "remote", r.RemoteAddr)
				writeSubsonicError(w, params, errMultipleAuth, "Multiple conflicting authentication mechanisms provided")
				return
			}
			token := apiKey
			if hasBearer {
				token = bearer
			}
			s, err := sess.check(r.Context(), token)
			if err != nil {
				if r.Context().Err() != nil {
					return
				}
				if !errors.Is(err, store.ErrSessionNotFound) {
					logger.Error("look up session", "error", err)
					writeSubsonicError(w, params, errGeneric, "internal error")
					return
				}
				logger.Warn("subsonic session unknown", "remote", r.RemoteAddr)
				writeSubsonicError(w, params, errInvalidAPIKey, "Invalid API key")
				return
			}
			forward(w, r, params, s.AccountID, next)
			return
		}

		user := params.Get("u")
		pass, token, salt := params.Get("p"), params.Get("t"), params.Get("s")
		switch {
		case user == "":
			writeSubsonicError(w, params, errMissingParam, "missing parameter: 'u'")
			return
		case pass == "" && (token == "" || salt == ""):
			writeSubsonicError(w, params, errMissingParam, "missing parameter: 'p', or 't' and 's'")
			return
		}
		acct, err := st.AccountByName(r.Context(), user)
		if err != nil {
			if errors.Is(err, context.Canceled) {
				return
			}
			if !errors.Is(err, store.ErrNotFound) {
				logger.Error("look up account", "error", err)
			}
			writeSubsonicError(w, params, errWrongCredential, "Wrong username or password")
			return
		}
		if !checkCredential(acct.AppPassword, pass, token, salt) {
			logger.Warn("subsonic sign-in failed", "remote", r.RemoteAddr)
			writeSubsonicError(w, params, errWrongCredential, "Wrong username or password")
			return
		}
		forward(w, r, params, acct.ID, next)
	})
}

// conflicting reports whether a session comes with a second credential: an
// apiKey parameter together with a Bearer header, or either one together
// with a Subsonic password or token parameter.
func conflicting(params url.Values, hasKey, hasBearer bool) bool {
	if hasKey && hasBearer {
		return true
	}
	for _, k := range apiKeyConflicts {
		if params.Has(k) {
			return true
		}
	}
	return false
}

// forward passes an authenticated request to next with the account id as
// Remote-User and without the credential parameters.
func forward(w http.ResponseWriter, r *http.Request, params url.Values, accountID string, next http.Handler) {
	for _, k := range credentialParams {
		params.Del(k)
	}
	out := r.WithContext(context.WithValue(r.Context(), accountIDKey{}, accountID))
	out.URL = new(url.URL)
	*out.URL = *r.URL
	out.URL.RawQuery = params.Encode()
	if hasFormBody(r) {
		// The form is now in the query. Navidrome merges a POST, PUT, or PATCH form
		// into the query the same way, so an empty body is equivalent.
		out.Body = http.NoBody
		out.ContentLength = 0
		out.Header = r.Header.Clone()
		out.Header.Del("Content-Type")
		out.Header.Del("Content-Length")
	}
	next.ServeHTTP(w, out)
}

// subsonicParams returns the query parameters merged with a POSTed form (also
// PUT and PATCH), as
// Navidrome reads them.
func subsonicParams(w http.ResponseWriter, r *http.Request) (url.Values, error) {
	if !hasFormBody(r) {
		return r.URL.Query(), nil
	}
	r.Body = http.MaxBytesReader(w, r.Body, maxFormBytes)
	if err := r.ParseForm(); err != nil {
		return nil, fmt.Errorf("parse form: %w", err)
	}
	return r.Form, nil
}

// hasFormBody reports whether Navidrome's ParseForm reads a form from the
// request body: POST, PUT, and PATCH with a urlencoded content type.
func hasFormBody(r *http.Request) bool {
	switch r.Method {
	case http.MethodPost, http.MethodPut, http.MethodPatch:
		return isForm(r)
	}
	return false
}

func isForm(r *http.Request) bool {
	ct, _, _ := mime.ParseMediaType(r.Header.Get("Content-Type"))
	return ct == "application/x-www-form-urlencoded"
}

// checkCredential compares a Subsonic credential with the app password in
// constant time. A token is the hex MD5 of the password followed by the salt.
// A plain password may be hex encoded behind "enc:".
func checkCredential(password, pass, token, salt string) bool {
	if pass != "" {
		if rest, ok := strings.CutPrefix(pass, "enc:"); ok {
			dec, err := hex.DecodeString(rest)
			if err != nil {
				return false
			}
			pass = string(dec)
		}
		return subtle.ConstantTimeCompare([]byte(pass), []byte(password)) == 1
	}
	sum := md5.Sum([]byte(password + salt)) //nolint:gosec
	want := hex.EncodeToString(sum[:])
	return subtle.ConstantTimeCompare([]byte(strings.ToLower(token)), []byte(want)) == 1
}

// subsonicResponse is the error envelope Navidrome sends. Field names and
// attributes match responses.Subsonic in Navidrome.
type subsonicResponse struct {
	XMLName       xml.Name      `xml:"http://subsonic.org/restapi subsonic-response" json:"-"`
	Status        string        `xml:"status,attr" json:"status"`
	Version       string        `xml:"version,attr" json:"version"`
	Type          string        `xml:"type,attr" json:"type"`
	ServerVersion string        `xml:"serverVersion,attr" json:"serverVersion"`
	OpenSubsonic  bool          `xml:"openSubsonic,attr,omitempty" json:"openSubsonic,omitempty"`
	Error         subsonicError `xml:"error" json:"error"`
}

type subsonicError struct {
	Code    int    `xml:"code,attr" json:"code"`
	Message string `xml:"message,attr" json:"message"`
}

var validJSIdentifier = regexp.MustCompile(`^[a-zA-Z_$][a-zA-Z0-9_$.]*$`)

// writeSubsonicError writes a failed Subsonic response with HTTP 200 in the
// format named by f: json, jsonp, or XML otherwise.
func writeSubsonicError(w http.ResponseWriter, params url.Values, code int, message string) {
	resp := subsonicResponse{
		Status:        "failed",
		Version:       subsonicAPIVersion,
		Type:          "navidrome",
		ServerVersion: navidrome.PinnedVersion,
		OpenSubsonic:  true,
		Error:         subsonicError{Code: code, Message: message},
	}
	var body []byte
	switch params.Get("f") {
	case "json", "jsonp":
		b, _ := json.Marshal(map[string]subsonicResponse{"subsonic-response": resp})
		callback := params.Get("callback")
		if params.Get("f") == "jsonp" && validJSIdentifier.MatchString(callback) {
			w.Header().Set("Content-Type", "application/javascript")
			body = fmt.Appendf(nil, "%s(%s)", callback, b)
		} else {
			w.Header().Set("Content-Type", "application/json")
			body = b
		}
	default:
		w.Header().Set("Content-Type", "application/xml")
		body, _ = xml.Marshal(resp)
	}
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write(body)
}
