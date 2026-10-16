package navidrome

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"net/http"
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
	users, err := api.ListUsers(ctx)
	if err != nil {
		return false, fmt.Errorf("list users: %w", err)
	}
	for _, u := range users {
		// Navidrome compares user names without regard to case.
		if strings.EqualFold(u.UserName, accountID) {
			return false, nil
		}
	}
	err = api.CreateUser(ctx, User{
		UserName: accountID,
		Name:     displayName,
		IsAdmin:  true,
		Password: rand.Text(),
	})
	if err != nil {
		return false, fmt.Errorf("create user %s: %w", accountID, err)
	}
	return true, nil
}
