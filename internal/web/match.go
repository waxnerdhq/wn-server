package web

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"strconv"
	"time"

	"github.com/waxnerdhq/wn-server/internal/match"
	"github.com/waxnerdhq/wn-server/internal/store"
)

// matchRow is one queue entry on the match page.
type matchRow struct {
	store.MatchEntry
	// CanDismiss shows the dismiss button.
	CanDismiss bool
	// Busy disables the buttons while a job runs.
	Busy bool
	// All is true on the view of every entry. The forms carry it so the
	// redirect after a POST returns to the same view.
	All bool
}

// jobView is a job as the match page shows it.
type jobView struct {
	Label    string
	Time     time.Time
	Summary  string
	Error    string
	Finished bool
}

type matchData struct {
	page
	All  bool
	Busy bool
	// Message is an error notice and Notice a confirmation.
	Message string
	Notice  string
	Running *jobView
	Last    *jobView

	Pending, Dismissed, Applied, Duplicates []matchRow
}

// jobLabel names a job for the page, for example "apply of entry 7".
func jobLabel(kind match.Kind, entry int64) string {
	switch kind {
	case match.KindRun:
		return "import run"
	case match.KindApply:
		return fmt.Sprintf("apply of entry %d", entry)
	case match.KindRevert:
		return fmt.Sprintf("revert of entry %d", entry)
	}
	return string(kind)
}

// viewAll reports whether the request asks for the view of every entry.
// The request's form must be parsed.
func viewAll(r *http.Request) bool { return r.FormValue("all") == "1" }

// matchURL is the match page's path for a view.
func matchURL(all bool, query string) string {
	u := "/ui/match"
	sep := "?"
	if all {
		u += "?all=1"
		sep = "&"
	}
	if query != "" {
		u += sep + query
	}
	return u
}

// matchPage serves GET /ui/match.
func (s *server) matchPage(w http.ResponseWriter, r *http.Request, _ store.Session) {
	var notice string
	if r.URL.Query().Get("done") == "dismissed" {
		notice = "The entry is dismissed."
	}
	s.renderMatch(w, r, http.StatusOK, viewAll(r), "", notice)
}

// renderMatch renders the match page with status. It shows the running job,
// or the last finished job once.
func (s *server) renderMatch(w http.ResponseWriter, r *http.Request, status int, all bool, message, notice string) {
	ctx := r.Context()
	jobs := s.Jobs.Status()
	data := matchData{page: s.signedIn("Match review", "match"), All: all, Busy: jobs.Running != nil, Message: message, Notice: notice}
	data.Wide = true
	if jobs.Running != nil {
		data.Running = &jobView{Label: jobLabel(jobs.Running.Kind, jobs.Running.EntryID), Time: jobs.Running.StartedAt}
		data.RefreshURL = matchURL(all, "")
	}
	var entries []store.MatchEntry
	var err error
	if all {
		entries, err = s.Store.MatchEntries(ctx)
	} else {
		entries, err = s.Store.MatchEntries(ctx, store.MatchPending)
	}
	if err != nil {
		s.internalError(w, r, "list match entries", err)
		return
	}
	// The result is taken only when the page is about to be sent. A HEAD
	// request does not take it.
	last := jobs.Last
	if r.Method != http.MethodHead {
		last = s.Jobs.Take().Last
	}
	if l := last; l != nil {
		data.Last = &jobView{Label: jobLabel(l.Kind, l.EntryID), Time: l.FinishedAt, Summary: l.Summary, Error: l.Error, Finished: true}
	}
	for _, e := range entries {
		row := matchRow{MatchEntry: e, Busy: data.Busy, All: all}
		switch e.State {
		case store.MatchPending:
			row.CanDismiss = true
			data.Pending = append(data.Pending, row)
		case store.MatchDismissed:
			data.Dismissed = append(data.Dismissed, row)
		case store.MatchApplied:
			data.Applied = append(data.Applied, row)
		case store.MatchDuplicate:
			data.Duplicates = append(data.Duplicates, row)
		}
	}
	s.render(w, r, status, "match", data)
}

// matchForm parses the form of a POST. It answers 400 and returns false when
// the form is bad.
func (s *server) matchForm(w http.ResponseWriter, r *http.Request) bool {
	r.Body = http.MaxBytesReader(w, r.Body, maxFormBytes)
	if err := r.ParseForm(); err != nil {
		http.Error(w, "bad form", http.StatusBadRequest)
		return false
	}
	return true
}

// entryID reads the entry id from the path. It answers 404 and returns
// false when the id is not a positive number.
func entryID(w http.ResponseWriter, r *http.Request) (int64, bool) {
	id, err := strconv.ParseInt(r.PathValue("id"), 10, 64)
	if err != nil || id <= 0 {
		http.NotFound(w, r)
		return 0, false
	}
	return id, true
}

// matchBusy answers 409 with the match page and a message.
func (s *server) matchBusy(w http.ResponseWriter, r *http.Request) {
	s.renderMatch(w, r, http.StatusConflict, viewAll(r),
		"Another match job is running. Wait for it to finish, then try again.", "")
}

// matchFailure answers a failed check or lookup before a job started.
func (s *server) matchFailure(w http.ResponseWriter, r *http.Request, what string, err error) {
	var refused *match.RefusedError
	switch {
	case errors.Is(err, match.ErrNoEntry), errors.Is(err, store.ErrMatchNotFound):
		http.NotFound(w, r)
	case errors.As(err, &refused):
		s.renderMatch(w, r, http.StatusBadRequest, viewAll(r), refused.Msg, "")
	default:
		s.internalError(w, r, what, err)
	}
}

// startMatchJob starts the job and redirects to the match page. While
// another job runs it answers 409.
func (s *server) startMatchJob(w http.ResponseWriter, r *http.Request, sess store.Session, kind match.Kind, entry int64, fn func(ctx context.Context) (string, error)) {
	if s.Match == nil {
		http.Error(w, "matching is not available", http.StatusServiceUnavailable)
		return
	}
	if err := s.Jobs.Start(kind, entry, fn); err != nil {
		if errors.Is(err, match.ErrJobRunning) {
			s.matchBusy(w, r)
			return
		}
		s.internalError(w, r, "start match job", err)
		return
	}
	if entry > 0 {
		s.Logger.Info("web match "+string(kind)+" started", "account", sess.AccountID, "entry", entry)
	} else {
		s.Logger.Info("web match "+string(kind)+" started", "account", sess.AccountID)
	}
	http.Redirect(w, r, matchURL(viewAll(r), ""), http.StatusSeeOther)
}

// matchRun serves POST /ui/match/run: a full import run as a background job.
func (s *server) matchRun(w http.ResponseWriter, r *http.Request, sess store.Session) {
	if !s.matchForm(w, r) {
		return
	}
	s.startMatchJob(w, r, sess, match.KindRun, 0, func(ctx context.Context) (string, error) {
		res, err := s.Match.Run(ctx)
		return res.Line(), err
	})
}

// matchApply serves POST /ui/match/{id}/apply. The form field release names
// the MusicBrainz release; without it the entry's first candidate is used.
func (s *server) matchApply(w http.ResponseWriter, r *http.Request, sess store.Session) {
	id, ok := entryID(w, r)
	if !ok || !s.matchForm(w, r) {
		return
	}
	e, err := s.Store.MatchEntry(r.Context(), id)
	if err != nil {
		s.matchFailure(w, r, "read match entry", err)
		return
	}
	release, err := match.CheckApply(e, r.PostForm.Get("release"))
	if err != nil {
		s.matchFailure(w, r, "check apply", err)
		return
	}
	s.startMatchJob(w, r, sess, match.KindApply, id, func(ctx context.Context) (string, error) {
		res, err := s.Match.Apply(ctx, id, release)
		if err != nil {
			return "", err
		}
		return res.Line(), nil
	})
}

// matchRevert serves POST /ui/match/{id}/revert.
func (s *server) matchRevert(w http.ResponseWriter, r *http.Request, sess store.Session) {
	id, ok := entryID(w, r)
	if !ok || !s.matchForm(w, r) {
		return
	}
	e, err := s.Store.MatchEntry(r.Context(), id)
	if err != nil {
		s.matchFailure(w, r, "read match entry", err)
		return
	}
	if err := match.CheckRevert(e); err != nil {
		s.matchFailure(w, r, "check revert", err)
		return
	}
	s.startMatchJob(w, r, sess, match.KindRevert, id, func(ctx context.Context) (string, error) {
		res, err := s.Match.Revert(ctx, id)
		if res.Reverted {
			return res.Line(), err
		}
		return "", err
	})
}

// matchDismiss serves POST /ui/match/{id}/dismiss. It changes only the
// queue, so it runs in the request. It answers 409 while a job runs.
func (s *server) matchDismiss(w http.ResponseWriter, r *http.Request, sess store.Session) {
	id, ok := entryID(w, r)
	if !ok || !s.matchForm(w, r) {
		return
	}
	if s.Match == nil {
		http.Error(w, "matching is not available", http.StatusServiceUnavailable)
		return
	}
	if s.Jobs.Status().Running != nil {
		s.matchBusy(w, r)
		return
	}
	folder, err := s.Match.Dismiss(r.Context(), id)
	if err != nil {
		s.matchFailure(w, r, "dismiss match entry", err)
		return
	}
	s.Logger.Info("web match dismissed", "account", sess.AccountID, "entry", id, "folder", folder)
	http.Redirect(w, r, matchURL(viewAll(r), "done=dismissed"), http.StatusSeeOther)
}
