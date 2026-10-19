package cli

import (
	"context"
	"log/slog"
	"net/http/httptest"
	"path/filepath"
	"testing"
	"time"

	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
	"github.com/waxnerdhq/wn-server/internal/wncloud/stub"
)

func TestMembersRefreshEndsRemovedListenerSessions(t *testing.T) {
	const (
		ownerID    = "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a70"
		listenerID = "0192f1c4-3b7e-7d2a-9a10-5f4e2c1b8a71"
		serverID   = "7f3c9a2e-1b4d-4e8f-9a6b-2c5d8e1f0a3b"
		token      = "wn_servercredential"
	)
	ctx := context.Background()
	st, err := store.Open(ctx, filepath.Join(t.TempDir(), store.FileName))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	owner, err := st.CreateOwner(ctx, "owner")
	if err != nil {
		t.Fatal(err)
	}
	cred := store.ServerCredential{Token: token, ServerID: serverID, WaxnerdUserID: ownerID}
	if err := st.Link(ctx, owner.ID, cred); err != nil {
		t.Fatal(err)
	}
	cloud := stub.New()
	cloud.AddUser(ownerID, "owner@example.com")
	cloud.AddUser(listenerID, "ada@example.com")
	cloud.AddToken(token, ownerID)
	cloud.AddServer(serverID, ownerID, "test")
	cloud.AddMember(serverID, listenerID, "listener")
	srv := httptest.NewServer(cloud)
	defer srv.Close()
	now := time.Date(2026, 11, 2, 10, 0, 0, 0, time.UTC)
	m := &membersRefresher{st: st, cloud: wncloud.New(srv.URL, "test"), cloudStatus: &wncloud.Status{},
		logger: slog.New(slog.DiscardHandler), now: func() time.Time { return now }}

	if !m.refresh(ctx, cred) {
		t.Fatal("first refresh failed")
	}
	if ok, err := st.IsMember(ctx, listenerID); err != nil || !ok {
		t.Fatalf("IsMember = %v, %v", ok, err)
	}
	acct, err := st.CreateListener(ctx, listenerID, "ada@example.com")
	if err != nil {
		t.Fatal(err)
	}
	sessToken, _, err := st.CreateSession(ctx, acct.ID, "phone")
	if err != nil {
		t.Fatal(err)
	}

	// A failed refresh changes nothing.
	srv.Close()
	now = now.Add(time.Hour)
	if m.refresh(ctx, cred) {
		t.Fatal("refresh with wn-cloud down succeeded")
	}
	if at, ok, err := st.MembersRefreshedAt(ctx); err != nil || !ok || !at.Equal(now.Add(-time.Hour)) {
		t.Fatalf("MembersRefreshedAt = %v, %v, %v", at, ok, err)
	}
	if _, err := st.SessionByToken(ctx, sessToken); err != nil {
		t.Fatalf("session after a failed refresh: %v", err)
	}

	// The refresh after the removal ends the session and disables the
	// account; re-adding enables it.
	srv = httptest.NewServer(cloud)
	defer srv.Close()
	m.cloud = wncloud.New(srv.URL, "test")
	cloud.RemoveMember(serverID, listenerID)
	if !m.refresh(ctx, cred) {
		t.Fatal("refresh after the removal failed")
	}
	if _, err := st.SessionByToken(ctx, sessToken); err == nil {
		t.Fatal("the removed listener's session still works")
	}
	if a, err := st.AccountByID(ctx, acct.ID); err != nil || !a.Disabled {
		t.Fatalf("account after the removal = %+v, %v", a, err)
	}
	cloud.AddMember(serverID, listenerID, "listener")
	if !m.refresh(ctx, cred) {
		t.Fatal("refresh after re-adding failed")
	}
	if a, err := st.AccountByID(ctx, acct.ID); err != nil || a.Disabled {
		t.Fatalf("account after re-adding = %+v, %v", a, err)
	}
}
