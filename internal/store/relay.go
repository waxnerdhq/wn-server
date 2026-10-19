package store

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"time"
)

var (
	// ErrNoRelaySettings is returned when serve has not stored the relay
	// settings from wn-cloud yet.
	ErrNoRelaySettings = errors.New("no relay settings")
	// ErrNoRelayState is returned when serve has not written a relay state
	// yet.
	ErrNoRelayState = errors.New("no relay state")
)

// RelayAddress is one entry of the addresses list of GET /servers/:id.
type RelayAddress struct {
	Kind string `json:"kind"`
	URL  string `json:"url"`
}

// RelaySettings are the relay fields of GET /servers/:id that serve keeps
// for the times wn-cloud is unreachable.
type RelaySettings struct {
	ConnectURL string
	Host       string
	Addresses  []RelayAddress
	FetchedAt  time.Time
}

// SetRelaySettings stores the relay settings, replacing any stored ones.
func (s *Store) SetRelaySettings(ctx context.Context, r RelaySettings) error {
	addrs := r.Addresses
	if addrs == nil {
		addrs = []RelayAddress{}
	}
	body, err := json.Marshal(addrs)
	if err != nil {
		return err
	}
	_, err = s.db.ExecContext(ctx,
		`INSERT OR REPLACE INTO relay_settings (id, connect_url, host, addresses, fetched_at) VALUES (1, ?, ?, ?, ?)`,
		r.ConnectURL, r.Host, string(body), r.FetchedAt.UTC().Format(time.RFC3339))
	if err != nil {
		return fmt.Errorf("set relay settings: %w", err)
	}
	return nil
}

// RelaySettings returns the stored relay settings, or ErrNoRelaySettings.
func (s *Store) RelaySettings(ctx context.Context) (RelaySettings, error) {
	var (
		r              RelaySettings
		addrs, fetched string
	)
	err := s.db.QueryRowContext(ctx,
		`SELECT connect_url, host, addresses, fetched_at FROM relay_settings WHERE id = 1`).
		Scan(&r.ConnectURL, &r.Host, &addrs, &fetched)
	if errors.Is(err, sql.ErrNoRows) {
		return RelaySettings{}, ErrNoRelaySettings
	}
	if err != nil {
		return RelaySettings{}, err
	}
	if err := json.Unmarshal([]byte(addrs), &r.Addresses); err != nil {
		return RelaySettings{}, fmt.Errorf("relay settings: bad addresses: %w", err)
	}
	if r.FetchedAt, err = time.Parse(time.RFC3339, fetched); err != nil {
		return RelaySettings{}, fmt.Errorf("relay settings: bad fetched_at: %w", err)
	}
	return r, nil
}

// RelayState is the relay package's last state as serve wrote it.
type RelayState struct {
	// State is a relay.StateName, such as "connected".
	State     string
	Since     time.Time
	Host      string
	LastError string
	WrittenAt time.Time
}

// SetRelayState stores the relay state, replacing any stored one.
func (s *Store) SetRelayState(ctx context.Context, r RelayState) error {
	_, err := s.db.ExecContext(ctx,
		`INSERT OR REPLACE INTO relay_state (id, state, since, host, last_error, written_at) VALUES (1, ?, ?, ?, ?, ?)`,
		r.State, r.Since.UTC().Format(time.RFC3339), r.Host, r.LastError, r.WrittenAt.UTC().Format(time.RFC3339))
	if err != nil {
		return fmt.Errorf("set relay state: %w", err)
	}
	return nil
}

// RelayState returns the stored relay state, or ErrNoRelayState.
func (s *Store) RelayState(ctx context.Context) (RelayState, error) {
	var (
		r              RelayState
		since, written string
	)
	err := s.db.QueryRowContext(ctx,
		`SELECT state, since, host, last_error, written_at FROM relay_state WHERE id = 1`).
		Scan(&r.State, &since, &r.Host, &r.LastError, &written)
	if errors.Is(err, sql.ErrNoRows) {
		return RelayState{}, ErrNoRelayState
	}
	if err != nil {
		return RelayState{}, err
	}
	if r.Since, err = time.Parse(time.RFC3339, since); err != nil {
		return RelayState{}, fmt.Errorf("relay state: bad since: %w", err)
	}
	if r.WrittenAt, err = time.Parse(time.RFC3339, written); err != nil {
		return RelayState{}, fmt.Errorf("relay state: bad written_at: %w", err)
	}
	return r, nil
}
