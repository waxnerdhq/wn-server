package wncloud

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// Timeout is the limit for one request to wn-cloud (docs/wn-cloud-api.md,
// Conventions).
const Timeout = 10 * time.Second

// Error kinds. A *Error matches exactly one of them with errors.Is.
var (
	// ErrInvalidRequest is a 400 with code invalid_request.
	ErrInvalidRequest = errors.New("invalid_request")
	// ErrInvalidGrant is a 400 with code invalid_grant: the code is unknown,
	// expired, already used, or does not match the verifier.
	ErrInvalidGrant = errors.New("invalid_grant")
	// ErrForbidden is a 403 with code forbidden.
	ErrForbidden = errors.New("forbidden")
	// ErrRateLimited is a 429, code rate_limited.
	ErrRateLimited = errors.New("rate_limited")
	// ErrNotFound is a 404 with code not_found.
	ErrNotFound = errors.New("not_found")
	// ErrUnauthorized is a 401: the bearer is missing, unknown, or revoked.
	ErrUnauthorized = errors.New("unauthorized")
	// ErrUnreachable is a timeout, a connection error, or a 5xx status.
	ErrUnreachable = errors.New("wn-cloud unreachable")
	// ErrUnexpected is any other answer, such as a 403 without code
	// forbidden or a 2xx answer with a malformed body.
	ErrUnexpected = errors.New("unexpected answer from wn-cloud")
)

// Error is a failed call to wn-cloud.
type Error struct {
	Op      string // the route, for example "POST /cli/token"
	Status  int    // 0 when wn-cloud did not answer
	Code    string // the code field of the error body, if any
	Message string // the error field of the error body, if any
	Kind    error  // one of the Err* values above
	Err     error  // the transport error, when wn-cloud did not answer
}

func (e *Error) Error() string {
	var b strings.Builder
	b.WriteString(e.Op)
	b.WriteString(": ")
	switch {
	case e.Err != nil:
		fmt.Fprintf(&b, "%v: %v", e.Kind, e.Err)
	case e.Status != 0:
		fmt.Fprintf(&b, "status %d", e.Status)
		if e.Code != "" {
			fmt.Fprintf(&b, " %s", e.Code)
		}
		if e.Message != "" {
			fmt.Fprintf(&b, ": %s", e.Message)
		}
	default:
		b.WriteString(e.Kind.Error())
	}
	return b.String()
}

// Unwrap returns the kind and the transport error.
func (e *Error) Unwrap() []error {
	if e.Err != nil {
		return []error{e.Kind, e.Err}
	}
	return []error{e.Kind}
}

// Client calls wn-cloud. BaseURL is wn-cloud's site URL, from wn_cloud_url.
type Client struct {
	BaseURL   string
	HTTP      *http.Client
	UserAgent string
}

// New returns a client with a Timeout per request.
func New(baseURL, userAgent string) *Client {
	return &Client{BaseURL: baseURL, HTTP: &http.Client{Timeout: Timeout}, UserAgent: userAgent}
}

func (c *Client) url(path string) string {
	return strings.TrimRight(c.BaseURL, "/") + path
}

// AuthURL returns the URL of the approval page that shows the sign-in code
// (GET /cli/auth with display=code). label is omitted when empty.
func (c *Client) AuthURL(state, challenge, label string) string {
	q := url.Values{"state": {state}, "challenge": {challenge}, "display": {"code"}}
	if label != "" {
		q.Set("label", label)
	}
	return c.url("/cli/auth") + "?" + q.Encode()
}

// ExchangeCode exchanges a code from the code display for a CLI token
// (POST /cli/token without redirect_port). It returns the token and the id
// of the user it belongs to.
func (c *Client) ExchangeCode(ctx context.Context, code, verifier string) (token, userID string, err error) {
	const op = "POST /cli/token"
	var out struct {
		Token string `json:"token"`
		User  struct {
			ID string `json:"id"`
		} `json:"user"`
	}
	body := map[string]string{"code": code, "code_verifier": verifier}
	if err := c.do(ctx, op, http.MethodPost, "/cli/token", "", body, http.StatusOK, &out); err != nil {
		return "", "", err
	}
	if out.Token == "" || out.User.ID == "" {
		return "", "", &Error{Op: op, Status: http.StatusOK, Kind: ErrUnexpected, Message: "token or user.id missing"}
	}
	return out.Token, out.User.ID, nil
}

// Logout revokes a CLI token (POST /cli/logout). A token that is already
// revoked gives ErrUnauthorized.
func (c *Client) Logout(ctx context.Context, token string) error {
	return c.do(ctx, "POST /cli/logout", http.MethodPost, "/cli/logout", token, nil, http.StatusNoContent, nil)
}

// RegisterServer registers a server under the token's user (POST /servers)
// and returns the new server id.
func (c *Client) RegisterServer(ctx context.Context, token, name string) (serverID string, err error) {
	const op = "POST /servers"
	var out struct {
		ID string `json:"id"`
	}
	if err := c.do(ctx, op, http.MethodPost, "/servers", token, map[string]string{"name": name}, http.StatusCreated, &out); err != nil {
		return "", err
	}
	if out.ID == "" {
		return "", &Error{Op: op, Status: http.StatusCreated, Kind: ErrUnexpected, Message: "id missing"}
	}
	return out.ID, nil
}

// VerifyDeviceCode checks a device code for a server and spends it
// (POST /servers/:id/device-codes/verify). It returns the user who issued
// the code.
func (c *Client) VerifyDeviceCode(ctx context.Context, token, serverID, code string) (userID, email string, err error) {
	const op = "POST /servers/:id/device-codes/verify"
	var out struct {
		UserID string `json:"user_id"`
		Email  string `json:"email"`
	}
	path := "/servers/" + url.PathEscape(serverID) + "/device-codes/verify"
	if err := c.do(ctx, op, http.MethodPost, path, token, map[string]string{"code": code}, http.StatusOK, &out); err != nil {
		return "", "", err
	}
	if out.UserID == "" {
		return "", "", &Error{Op: op, Status: http.StatusOK, Kind: ErrUnexpected, Message: "user_id missing"}
	}
	return out.UserID, out.Email, nil
}

// do sends one request. It returns nil when the status is want and the body
// decodes into out (when out is not nil), and an *Error otherwise.
func (c *Client) do(ctx context.Context, op, method, path, token string, body any, want int, out any) error {
	var r io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		r = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.url(path), r)
	if err != nil {
		return err
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	req.Header.Set("Accept", "application/json")
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	if c.UserAgent != "" {
		req.Header.Set("User-Agent", c.UserAgent)
	}
	hc := c.HTTP
	if hc == nil {
		hc = &http.Client{Timeout: Timeout}
	}
	res, err := hc.Do(req)
	if err != nil {
		return &Error{Op: op, Kind: ErrUnreachable, Err: err}
	}
	defer res.Body.Close()
	data, err := io.ReadAll(io.LimitReader(res.Body, 1<<20))
	if err != nil {
		return &Error{Op: op, Status: res.StatusCode, Kind: ErrUnreachable, Err: err}
	}
	if res.StatusCode == want {
		if out != nil {
			if err := json.Unmarshal(data, out); err != nil {
				return &Error{Op: op, Status: res.StatusCode, Kind: ErrUnexpected, Message: "malformed body"}
			}
		}
		return nil
	}
	var eb struct {
		Error string `json:"error"`
		Code  string `json:"code"`
	}
	_ = json.Unmarshal(data, &eb) // a non-JSON error body leaves both empty
	return &Error{Op: op, Status: res.StatusCode, Code: eb.Code, Message: eb.Error, Kind: kindOf(res.StatusCode, eb.Code)}
}

// kindOf maps a status and an error code to an error kind. wn-server
// branches on the code and the status, never on the message.
func kindOf(status int, code string) error {
	switch {
	case status >= 500:
		return ErrUnreachable
	case status == http.StatusUnauthorized:
		return ErrUnauthorized
	case status == http.StatusTooManyRequests || code == "rate_limited":
		return ErrRateLimited
	case status == http.StatusBadRequest && code == "invalid_grant":
		return ErrInvalidGrant
	case status == http.StatusBadRequest && code == "invalid_request":
		return ErrInvalidRequest
	case status == http.StatusForbidden && code == "forbidden":
		return ErrForbidden
	case status == http.StatusNotFound && code == "not_found":
		return ErrNotFound
	default:
		return ErrUnexpected
	}
}
