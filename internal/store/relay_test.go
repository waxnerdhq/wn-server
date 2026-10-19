package store

import (
	"context"
	"errors"
	"path/filepath"
	"reflect"
	"testing"
	"time"
)

func TestRelayRows(t *testing.T) {
	ctx := context.Background()
	s, err := Open(ctx, filepath.Join(t.TempDir(), FileName))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if _, err := s.RelaySettings(ctx); !errors.Is(err, ErrNoRelaySettings) {
		t.Fatalf("RelaySettings on empty store = %v", err)
	}
	if _, err := s.RelayState(ctx); !errors.Is(err, ErrNoRelayState) {
		t.Fatalf("RelayState on empty store = %v", err)
	}
	if en, set, err := s.RelayEnabled(ctx); err != nil || en || set {
		t.Fatalf("RelayEnabled on empty store = %v, %v, %v", en, set, err)
	}
	// A switch before the first settings makes a row that counts as no settings.
	if err := s.SetRelayEnabled(ctx, true); err != nil {
		t.Fatal(err)
	}
	if _, err := s.RelaySettings(ctx); !errors.Is(err, ErrNoRelaySettings) {
		t.Fatalf("RelaySettings with only the switch = %v", err)
	}
	now := time.Now().UTC().Truncate(time.Second)
	set := RelaySettings{
		ConnectURL: "https://connect.relay.test/v1/tunnel",
		Host:       "id.relay.test",
		Addresses:  []RelayAddress{{Kind: "relay", URL: "https://id.relay.test"}},
		FetchedAt:  now,
	}
	for range 2 {
		if err := s.SetRelaySettings(ctx, set); err != nil {
			t.Fatal(err)
		}
	}
	if got, err := s.RelaySettings(ctx); err != nil || !reflect.DeepEqual(got, set) {
		t.Fatalf("RelaySettings = %+v, %v; want %+v", got, err, set)
	}
	// Storing settings keeps the switch, and the switch keeps the settings.
	if en, isSet, err := s.RelayEnabled(ctx); err != nil || !en || !isSet {
		t.Fatalf("RelayEnabled after SetRelaySettings = %v, %v, %v", en, isSet, err)
	}
	if err := s.SetRelayEnabled(ctx, false); err != nil {
		t.Fatal(err)
	}
	if en, isSet, err := s.RelayEnabled(ctx); err != nil || en || !isSet {
		t.Fatalf("RelayEnabled after off = %v, %v, %v", en, isSet, err)
	}
	if got, err := s.RelaySettings(ctx); err != nil || !reflect.DeepEqual(got, set) {
		t.Fatalf("RelaySettings after the switch = %+v, %v", got, err)
	}
	st := RelayState{State: "unreachable", Since: now.Add(-time.Minute), LastError: "EOF", WrittenAt: now}
	if err := s.SetRelayState(ctx, RelayState{State: "waiting", Since: now, WrittenAt: now}); err != nil {
		t.Fatal(err)
	}
	if err := s.SetRelayState(ctx, st); err != nil {
		t.Fatal(err)
	}
	if got, err := s.RelayState(ctx); err != nil || got != st {
		t.Fatalf("RelayState = %+v, %v; want %+v", got, err, st)
	}
}
