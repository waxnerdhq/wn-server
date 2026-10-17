package proxy

import (
	"bytes"
	"context"
	"encoding/json"
	"encoding/xml"
	"io"
	"log/slog"
	"net/http"
	"net/http/httputil"
	"net/url"
	"regexp"
	"strconv"
	"strings"
)

// maxUserBody is the largest getUser or getUsers response that is rewritten.
// The calls return one user, so a larger body is passed on unchanged.
const maxUserBody = 1 << 20

type renameKey struct{}

// userRename names the account whose Navidrome user name, the account id,
// is replaced in the response.
type userRename struct{ id, name string }

// userCall returns "getUser" or "getUsers" when p is that call on /rest/,
// with or without the .view suffix, and "" otherwise.
func userCall(p string) string {
	switch c := strings.TrimSuffix(strings.TrimPrefix(p, "/rest/"), ".view"); c {
	case "getUser", "getUsers":
		return c
	}
	return ""
}

// forwardAccount forwards an authenticated request. For getUser and getUsers
// it maps the account name to the account id in the username parameter and
// marks the response for rewriting, so the client sees the account name where
// Navidrome has the account id. name is the account name, or empty when the
// caller does not have it.
func forwardAccount(w http.ResponseWriter, r *http.Request, params url.Values, st Store, logger *slog.Logger, accountID, name string, next http.Handler) {
	call := userCall(r.URL.Path)
	if call == "" {
		forward(w, r, params, accountID, next)
		return
	}
	if name == "" {
		owner, err := st.Owner(r.Context())
		if err != nil || owner.ID != accountID {
			if r.Context().Err() != nil {
				return
			}
			logger.Error("look up account name", "account", accountID, "error", err)
			writeSubsonicError(w, params, errGeneric, "internal error")
			return
		}
		name = owner.Name
	}
	if call == "getUser" && params.Has("username") {
		// The owner's Navidrome user is an admin. Only the account itself may
		// be asked for, so that the wn-server user is not exposed.
		switch u := params.Get("username"); {
		case strings.EqualFold(u, name), strings.EqualFold(u, accountID):
			params.Set("username", accountID)
		default:
			writeSubsonicError(w, params, errNotAuthorized, "User is not authorized for the given operation")
			return
		}
	}
	ctx := context.WithValue(r.Context(), renameKey{}, userRename{id: accountID, name: name})
	forward(w, r.WithContext(ctx), params, accountID, next)
}

// requestIdentity asks Navidrome for an uncompressed body when the response
// will be rewritten.
func requestIdentity(r *httputil.ProxyRequest) {
	if _, ok := r.In.Context().Value(renameKey{}).(userRename); ok {
		r.Out.Header.Set("Accept-Encoding", "identity")
	}
}

// rewriteUserResponse replaces the account id with the account name in the
// username field of a getUser or getUsers response, in JSON, JSONP, and XML.
// Other responses pass unchanged.
func rewriteUserResponse(resp *http.Response) error {
	rn, ok := resp.Request.Context().Value(renameKey{}).(userRename)
	if !ok || resp.Header.Get("Content-Encoding") != "" {
		return nil
	}
	ct := resp.Header.Get("Content-Type")
	var re *regexp.Regexp
	var repl string
	switch {
	case strings.Contains(ct, "json"), strings.Contains(ct, "javascript"):
		re = regexp.MustCompile(`"username"\s*:\s*"` + regexp.QuoteMeta(rn.id) + `"`)
		b, _ := json.Marshal(rn.name)
		repl = `"username":` + string(b)
	case strings.Contains(ct, "xml"):
		re = regexp.MustCompile(`\busername\s*=\s*"` + regexp.QuoteMeta(rn.id) + `"`)
		var esc bytes.Buffer
		_ = xml.EscapeText(&esc, []byte(rn.name))
		repl = `username="` + esc.String() + `"`
	default:
		return nil
	}
	orig := resp.Body
	body, err := io.ReadAll(io.LimitReader(orig, maxUserBody+1))
	if err != nil {
		return err
	}
	if len(body) > maxUserBody {
		resp.Body = struct {
			io.Reader
			io.Closer
		}{io.MultiReader(bytes.NewReader(body), orig), orig}
		return nil
	}
	_ = orig.Close()
	body = re.ReplaceAllLiteral(body, []byte(repl))
	resp.Body = io.NopCloser(bytes.NewReader(body))
	resp.ContentLength = int64(len(body))
	resp.Header.Set("Content-Length", strconv.Itoa(len(body)))
	return nil
}
