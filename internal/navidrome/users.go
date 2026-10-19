package navidrome

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"strings"
)

// User is a Navidrome user as returned by /api/user. Field names match
// model.User in Navidrome.
type User struct {
	ID       string `json:"id,omitempty"`
	UserName string `json:"userName"`
	Name     string `json:"name"`
	IsAdmin  bool   `json:"isAdmin"`
	Password string `json:"password,omitempty"`
}

// UserAPI is the part of Client that EnsureAccountUser uses.
type UserAPI interface {
	ListUsers(ctx context.Context) ([]User, error)
	CreateUser(ctx context.Context, u User) error
}

// ListUsers returns all Navidrome users. The route is admin only.
func (c *Client) ListUsers(ctx context.Context) ([]User, error) {
	data, err := c.do(ctx, http.MethodGet, "/api/user", nil)
	if err != nil {
		return nil, err
	}
	var users []User
	if err := json.Unmarshal(data, &users); err != nil {
		return nil, fmt.Errorf("decode users: %w", err)
	}
	return users, nil
}

// CreateUser adds a Navidrome user. The route is admin only.
func (c *Client) CreateUser(ctx context.Context, u User) error {
	_, err := c.do(ctx, http.MethodPost, "/api/user", u)
	return err
}

// EnsureAccountUser creates the Navidrome user for a wn-server account when
// it does not exist. The Navidrome user name is the account id, which is the
// Remote-User value the proxy sends. The user is admin and gets a random
// password that is not stored: nobody signs in to Navidrome with it, because
// every request arrives through the proxy with Remote-User. It reports
// whether it created the user.
func EnsureAccountUser(ctx context.Context, api UserAPI, accountID, displayName string) (bool, error) {
	u, err := findUser(ctx, api, accountID)
	if err != nil || u != nil {
		return false, err
	}
	return true, createUser(ctx, api, accountID, displayName, true)
}

// ListenerAPI is the part of Client that EnsureListenerUser uses.
type ListenerAPI interface {
	UserAPI
	ListLibraries(ctx context.Context) ([]Library, error)
	SetUserLibraries(ctx context.Context, userID string, libraryIDs []int) error
}

// SetUserLibraries sets the libraries of a non-admin user
// (PUT /api/user/:id/library). userID is Navidrome's user id. The route is
// admin only, and Navidrome refuses an empty list.
func (c *Client) SetUserLibraries(ctx context.Context, userID string, libraryIDs []int) error {
	_, err := c.do(ctx, http.MethodPut, "/api/user/"+url.PathEscape(userID)+"/library", map[string][]int{"libraryIds": libraryIDs})
	return err
}

// EnsureListenerUser creates the Navidrome user for a listener account when
// it does not exist, as EnsureAccountUser does for the owner, but without
// admin. It then gives the user every library, because Navidrome gives a new
// non-admin user only the libraries marked for new users. It reports whether
// it created the user.
func EnsureListenerUser(ctx context.Context, api ListenerAPI, accountID, displayName string) (bool, error) {
	u, err := findUser(ctx, api, accountID)
	if err != nil {
		return false, err
	}
	created := false
	if u == nil {
		if err := createUser(ctx, api, accountID, displayName, false); err != nil {
			return false, err
		}
		created = true
		if u, err = findUser(ctx, api, accountID); err != nil {
			return true, err
		}
		if u == nil {
			return true, fmt.Errorf("user %s not listed after it was created", accountID)
		}
	}
	if u.IsAdmin {
		return created, nil
	}
	libs, err := api.ListLibraries(ctx)
	if err != nil {
		return created, fmt.Errorf("list libraries: %w", err)
	}
	if len(libs) == 0 {
		return created, nil
	}
	ids := make([]int, len(libs))
	for i, l := range libs {
		ids[i] = l.ID
	}
	if err := api.SetUserLibraries(ctx, u.ID, ids); err != nil {
		return created, fmt.Errorf("set libraries of user %s: %w", accountID, err)
	}
	return created, nil
}

// findUser returns the Navidrome user named accountID, or nil.
func findUser(ctx context.Context, api UserAPI, accountID string) (*User, error) {
	users, err := api.ListUsers(ctx)
	if err != nil {
		return nil, fmt.Errorf("list users: %w", err)
	}
	for _, u := range users {
		// Navidrome compares user names without regard to case.
		if strings.EqualFold(u.UserName, accountID) {
			return &u, nil
		}
	}
	return nil, nil
}

func createUser(ctx context.Context, api UserAPI, accountID, displayName string, admin bool) error {
	err := api.CreateUser(ctx, User{
		UserName: accountID,
		Name:     displayName,
		IsAdmin:  admin,
		Password: rand.Text(),
	})
	if err != nil {
		return fmt.Errorf("create user %s: %w", accountID, err)
	}
	return nil
}
