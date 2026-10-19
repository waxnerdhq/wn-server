package cli

import (
	"context"
	"errors"
	"log/slog"
	"time"

	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

const (
	// membersRefreshInterval is the time between two calls of
	// GET /servers/:id/members, after a success or a failure.
	membersRefreshInterval = 5 * time.Minute
	// membersCredentialPoll is how often the loop reads the server
	// credential, so that the list is read within this time of a login.
	membersCredentialPoll = 10 * time.Second
)

// memberLister is the part of *wncloud.Client that the refresh uses.
type memberLister interface {
	ListMembers(ctx context.Context, token, serverID string) ([]wncloud.Member, error)
}

// membersRefresher keeps the cached member list (docs/relay.md, section 5).
type membersRefresher struct {
	st          *store.Store
	cloud       memberLister
	cloudStatus *wncloud.Status
	logger      *slog.Logger
	now         func() time.Time
}

// run reads the member list at start, every 5 minutes, and within
// membersCredentialPoll of a new server credential, until ctx ends. Without
// a server credential it reads nothing.
func (m *membersRefresher) run(ctx context.Context) {
	t := time.NewTicker(membersCredentialPoll)
	defer t.Stop()
	var (
		token string
		next  time.Time
	)
	for {
		cred, err := m.st.ServerCredential(ctx)
		switch {
		case err == nil:
			if cred.Token != token || !m.now().Before(next) {
				token = cred.Token
				m.refresh(ctx, cred)
				next = m.now().Add(membersRefreshInterval)
			}
		case errors.Is(err, store.ErrNoServerCredential):
			token = ""
		case ctx.Err() == nil:
			m.logger.Error("read the server credential for the member list", "error", err)
		}
		select {
		case <-ctx.Done():
			return
		case <-t.C:
		}
	}
}

// refresh calls GET /servers/:id/members once and stores the list. A
// listener who left the list loses their sessions and their account is
// disabled; one who is back is enabled again. A failed call changes
// nothing. It reports whether the list was stored.
func (m *membersRefresher) refresh(ctx context.Context, cred store.ServerCredential) bool {
	list, err := m.cloud.ListMembers(ctx, cred.Token, cred.ServerID)
	if ctx.Err() != nil {
		return false
	}
	switch {
	case errors.Is(err, wncloud.ErrUnreachable):
		m.cloudStatus.MarkUnreachable(m.now())
		m.logger.Warn("wn-cloud is unreachable; listeners are served from the cached member list", "error", err)
		return false
	case errors.Is(err, wncloud.ErrUnauthorized):
		m.cloudStatus.MarkReachable()
		m.cloudStatus.SetCredentialRejected(true)
		m.logger.Error("wn-cloud refused the server credential for the member list; run `wn-server logout`, then `wn-server login`", "error", err)
		return false
	case err != nil:
		m.cloudStatus.MarkReachable()
		m.logger.Warn("read the member list from wn-cloud", "error", err)
		return false
	}
	m.cloudStatus.MarkReachable()
	m.cloudStatus.SetCredentialRejected(false)
	members := make([]store.Member, 0, len(list))
	for _, x := range list {
		members = append(members, store.Member{UserID: x.UserID, Name: x.Name, Role: x.Role, CreatedAt: x.CreatedAt})
	}
	res, err := m.st.ReplaceMembers(ctx, members, m.now())
	if err != nil {
		if ctx.Err() == nil {
			m.logger.Error("store the member list", "error", err)
		}
		return false
	}
	for _, a := range res.Disabled {
		m.logger.Info("listener removed from the member list; sessions ended and account disabled", "account", a.ID, "user", a.WaxnerdUserID)
	}
	for _, a := range res.Enabled {
		m.logger.Info("listener back in the member list; account enabled", "account", a.ID, "user", a.WaxnerdUserID)
	}
	return true
}
