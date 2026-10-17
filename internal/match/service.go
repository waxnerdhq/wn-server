package match

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"strings"
	"time"

	"github.com/waxnerdhq/wn-server/internal/beets"
	"github.com/waxnerdhq/wn-server/internal/config"
	"github.com/waxnerdhq/wn-server/internal/store"
)

// ErrNoEntry matches the error for an id that is not in the match queue.
var ErrNoEntry = errors.New("no match queue entry")

type noEntryError struct{ id int64 }

func (e noEntryError) Error() string        { return fmt.Sprintf("no match queue entry %d", e.id) }
func (e noEntryError) Is(target error) bool { return target == ErrNoEntry }

// RefusedError is returned when the state of an entry or the request does
// not allow the action. Nothing was changed.
type RefusedError struct{ Msg string }

func (e *RefusedError) Error() string { return e.Msg }

func refuse(format string, args ...any) error {
	return &RefusedError{Msg: fmt.Sprintf(format, args...)}
}

// Service runs the match actions against the store and beets.
type Service struct {
	cfg    *config.Config
	st     *store.Store
	runner *beets.Runner
	log    *slog.Logger
}

// NewService returns a service for the config c and the store st. scanner is
// asked to start a Navidrome scan after an album changed; it may be nil. A
// nil logger selects slog.Default.
func NewService(c *config.Config, st *store.Store, scanner beets.Scanner, logger *slog.Logger) *Service {
	if logger == nil {
		logger = slog.Default()
	}
	return &Service{cfg: c, st: st, runner: beets.NewRunner(c, scanner, logger), log: logger}
}

// RunResult is the outcome of a full run.
type RunResult struct {
	Summary beets.Summary
	// Reported is false for a run that did not start, such as one the beets
	// lock refused. Such a run has nothing to report.
	Reported bool
	// Pending is the number of pending entries after the run, or -1 when
	// it is unknown.
	Pending int
}

// SummaryLine returns the counts of a run on one line.
func SummaryLine(sum beets.Summary) string {
	return fmt.Sprintf("applied %d, skipped %d, duplicates %d, errors %d", sum.Applied, sum.Skipped, sum.Duplicate, sum.Errors)
}

// Line returns the counts of the run and the pending count on one line.
func (r RunResult) Line() string {
	if !r.Reported {
		return ""
	}
	line := SummaryLine(r.Summary)
	if r.Pending >= 0 {
		line += fmt.Sprintf("; %d pending", r.Pending)
	}
	return line
}

// Run imports every music folder with beets and records every album beets
// saw in the match queue. After a cancel it still records the albums that
// beets wrote before it was killed. The result is valid also when err is
// not nil.
//
// The folders of reverted entries (forget_pending) go to the plugin as
// forget_folders, which removes them from beets' incremental history so this
// run offers them again. The plugin confirms each folder with a `forgotten`
// line in the candidates file. The flag is cleared only for those folders,
// also when the run fails afterwards. A folder without the line keeps it.
func (s *Service) Run(ctx context.Context) (RunResult, error) {
	bg := context.WithoutCancel(ctx)
	var forget []string
	forgetIDs := map[string]int64{}
	pending, ferr := s.st.ForgetPending(bg)
	if ferr != nil {
		s.log.Warn("could not read the reverted folders; beets keeps them in its history on this run", "error", ferr)
	}
	for _, f := range pending {
		forget = append(forget, f.Folder)
		forgetIDs[f.Folder] = f.ID
	}
	sum, err := s.runner.Import(ctx, s.cfg.MusicFolders, forget)
	var cleared []int64
	for _, folder := range sum.Forgotten {
		if id, ok := forgetIDs[folder]; ok {
			cleared = append(cleared, id)
		}
	}
	if len(cleared) > 0 {
		if cerr := s.st.ClearForgetPending(bg, cleared); cerr != nil {
			s.log.Warn("could not clear forget_pending; the next run forgets the folders again", "error", cerr)
		}
	}
	recErr := recordAlbums(bg, s.st, sum.Albums, sum.MarkersWritten, s.log)
	if recErr == nil {
		// A cancelled run still marks the duplicates of the lines it
		// recorded, but starts no scan.
		// Import scanned when it applied an album and did not fail.
		scanned := err == nil && sum.Applied > 0
		recErr = s.markDuplicates(bg, sum.Albums, ctx.Err() == nil && !scanned)
	}
	res := RunResult{Summary: sum, Pending: -1}
	if err == nil || len(sum.Albums) > 0 {
		res.Reported = true
		if n, cerr := s.st.CountMatches(bg, store.MatchPending); cerr == nil {
			res.Pending = n
		}
	}
	return res, errors.Join(err, recErr)
}

// entry reads an entry and maps a missing one to ErrNoEntry.
func (s *Service) entry(ctx context.Context, id int64) (store.MatchEntry, error) {
	e, err := s.st.MatchEntry(ctx, id)
	if errors.Is(err, store.ErrMatchNotFound) {
		return store.MatchEntry{}, noEntryError{id}
	}
	return e, err
}

// CheckApply checks that e can be applied and returns the release to apply.
// release is the MusicBrainz release id the owner chose, or empty for the
// entry's first candidate. It returns a *RefusedError when it cannot.
func CheckApply(e store.MatchEntry, release string) (string, error) {
	if e.State != store.MatchPending && e.State != store.MatchDismissed {
		return "", refuse("entry %d is %s; only a pending or dismissed entry can be applied", e.ID, e.State)
	}
	release = strings.ToLower(strings.TrimSpace(release))
	if release == "" {
		if len(e.Candidates) == 0 {
			return "", refuse("entry %d has no candidates; pass --release <MusicBrainz release id>", e.ID)
		}
		release = e.Candidates[0].ReleaseID
	}
	if !beets.ValidReleaseID(release) {
		return "", refuse("release id %q is not a MusicBrainz id (a UUID)", release)
	}
	return release, nil
}

// CheckRevert checks that e can be reverted. It returns a *RefusedError when
// it cannot.
func CheckRevert(e store.MatchEntry) error {
	if e.State != store.MatchApplied {
		return refuse("entry %d is %s; only an applied entry can be reverted", e.ID, e.State)
	}
	if e.CopiedTo == "" {
		return refuse("entry %d has no copied_to; nothing to revert", e.ID)
	}
	return nil
}

// ApplyResult describes an applied entry.
type ApplyResult struct {
	ID        int64
	Folder    string
	ReleaseID string
	CopiedTo  string
}

// Line describes the result on one line.
func (r ApplyResult) Line() string {
	return fmt.Sprintf("applied entry %d: %s copied to %s", r.ID, r.Folder, r.CopiedTo)
}

// Apply tags and copies the album of a pending or dismissed entry with a
// MusicBrainz release. An empty release selects the entry's first candidate.
func (s *Service) Apply(ctx context.Context, id int64, release string) (ApplyResult, error) {
	e, err := s.entry(ctx, id)
	if err != nil {
		return ApplyResult{}, err
	}
	release, err = CheckApply(e, release)
	if err != nil {
		return ApplyResult{}, err
	}
	s.log.Info("applying match", "id", e.ID, "folder", e.Folder, "release_id", release)
	sum, runErr := s.runner.Apply(ctx, e.Folder, release, s.cfg.MusicFolders)
	if runErr != nil && len(sum.Albums) == 0 {
		// beets did not run, or wrote nothing.
		return ApplyResult{}, runErr
	}
	// Only the entry's own applied line is used. With -S, any other line for
	// the folder holds the requested release or no candidate. Other albums
	// below the folder were skipped by the plugin for the same reason.
	var applied []beets.Album
	reason := "beets wrote no line for the folder"
	for _, a := range sum.Albums {
		if a.Folder != e.Folder {
			continue
		}
		if a.Decision == beets.DecisionApplied {
			applied = append(applied, a)
			continue
		}
		reason = "decision " + a.Decision
		if a.Error != "" {
			reason += ": " + a.Error
		}
	}
	bg := context.WithoutCancel(ctx)
	if len(applied) == 0 {
		msg := fmt.Sprintf("match apply: beets did not apply release %s (%s)", release, reason)
		noteErr := s.st.NoteMatchError(bg, e.ID, msg, time.Now())
		if runErr == nil {
			runErr = fmt.Errorf("beets did not apply release %s to %s (%s); see the log for beets' output", release, e.Folder, reason)
		}
		return ApplyResult{}, errors.Join(runErr, noteErr)
	}
	// MarkApplied leaves the tags and candidates alone, because this run
	// looked up only the requested release.
	a := applied[len(applied)-1]
	if a.ReleaseID != "" {
		release = a.ReleaseID
	}
	markerWritten := slices.Contains(sum.MarkersWritten, e.Folder)
	if err := s.st.MarkApplied(bg, e.ID, release, a.CopiedTo, markerWritten, time.Now()); err != nil || runErr != nil {
		return ApplyResult{}, errors.Join(runErr, err)
	}
	got, err := s.st.MatchEntry(bg, e.ID)
	if err != nil {
		return ApplyResult{}, err
	}
	s.log.Info("applied match", "id", e.ID, "folder", e.Folder, "release_id", got.AppliedReleaseID, "copied_to", got.CopiedTo)
	// The entry is applied. A failure to mark earlier duplicates of the
	// release does not undo that.
	// The apply run scanned already.
	if err := s.markDuplicates(bg, sum.Albums, false); err != nil {
		s.log.Warn("could not mark the duplicate folders of the applied release", "id", e.ID, "release_id", release, "error", err)
	}
	return ApplyResult{ID: e.ID, Folder: e.Folder, ReleaseID: got.AppliedReleaseID, CopiedTo: got.CopiedTo}, nil
}

// Dismiss sets an entry to dismissed and returns its folder. Nothing on disk
// changes. It refuses an applied entry.
func (s *Service) Dismiss(ctx context.Context, id int64) (folder string, err error) {
	e, err := s.entry(ctx, id)
	if err != nil {
		return "", err
	}
	if e.State == store.MatchApplied {
		return "", refuse("entry %d is applied; run `wn-server match revert %d` first", e.ID, e.ID)
	}
	if err := s.st.SetMatchState(ctx, e.ID, store.MatchDismissed); err != nil {
		return "", err
	}
	return e.Folder, nil
}

// RevertResult describes a reverted entry.
type RevertResult struct {
	ID     int64
	Folder string
	// Reverted is true once the entry is pending again. It is also true
	// when err is not nil because the marker could not be removed.
	Reverted bool
}

// Line describes the result on one line.
func (r RevertResult) Line() string {
	return fmt.Sprintf("reverted entry %d; it is pending again.", r.ID)
}

// Revert removes an applied entry's copy from sorted_dir and the beets
// library, removes the .ndignore that wn-server wrote, and sets the entry to
// pending. It also removes the .ndignore that wn-server wrote into duplicate
// folders of the same release. Those entries stay duplicates. When only a
// marker could not be removed, it returns the result with Reverted set
// together with the error.
func (s *Service) Revert(ctx context.Context, id int64) (RevertResult, error) {
	e, err := s.entry(ctx, id)
	if err != nil {
		return RevertResult{}, err
	}
	if err := CheckRevert(e); err != nil {
		return RevertResult{}, err
	}
	s.log.Info("reverting match", "id", e.ID, "folder", e.Folder, "release_id", e.AppliedReleaseID, "copied_to", e.CopiedTo)
	if err := s.runner.Remove(ctx, e.CopiedTo); err != nil {
		return RevertResult{}, err
	}
	// The copy is gone. The remaining steps run to the end.
	bg := context.WithoutCancel(ctx)
	// Only a marker that wn-server created is removed.
	var markerErr error
	if e.MarkerWritten {
		if _, err := s.runner.RemoveMarker(e.Folder, s.cfg.MusicFolders); err != nil {
			markerErr = fmt.Errorf("remove .ndignore from %s: %w", e.Folder, err)
		}
	}
	if e.AppliedReleaseID != "" {
		// Another applied entry of the same release still hides them.
		others, err := s.st.CountAppliedRelease(bg, e.AppliedReleaseID, e.ID)
		if err == nil && others == 0 {
			err = s.unmarkDuplicates(bg, e.AppliedReleaseID)
		}
		if err != nil {
			markerErr = errors.Join(markerErr, err)
		}
	}
	if err := s.st.RevertMatch(bg, e.ID); err != nil {
		return RevertResult{}, errors.Join(err, markerErr)
	}
	s.runner.Scan(bg)
	s.log.Info("reverted match", "id", e.ID, "folder", e.Folder, "release_id", e.AppliedReleaseID)
	return RevertResult{ID: e.ID, Folder: e.Folder, Reverted: true}, markerErr
}

// markDuplicates writes .ndignore into the folder of every duplicate entry
// whose release is the applied release of an applied entry, and sets
// marker_written on the entry when it created the file. It considers the
// duplicates among albums, the lines of the run just recorded, and the
// duplicates of earlier runs whose release one of those lines applied. It
// asks Navidrome to scan when it created a marker and scan is true. A caller
// that already scanned after its own change passes false.
func (s *Service) markDuplicates(ctx context.Context, albums []beets.Album, scan bool) error {
	duplicate, applied := map[string]bool{}, map[string]bool{}
	for _, a := range albums {
		switch a.Decision {
		case beets.DecisionDuplicate:
			duplicate[a.Folder] = true
		case beets.DecisionApplied:
			applied[matchLine(a).ReleaseID] = true
		}
	}
	if len(duplicate) == 0 && len(applied) == 0 {
		return nil
	}
	linked, err := s.st.UnmarkedDuplicates(ctx)
	if err != nil {
		return err
	}
	var folders []string
	byFolder := map[string]store.MatchEntry{}
	for _, e := range linked {
		if duplicate[e.Folder] || applied[e.DuplicateOfReleaseID] {
			folders = append(folders, e.Folder)
			byFolder[e.Folder] = e
		}
	}
	if len(folders) == 0 {
		return nil
	}
	// Albums of earlier runs that are not applied must stay visible too.
	notApplied, err := s.st.NotAppliedFolders(ctx)
	if err != nil {
		return err
	}
	written, err := s.runner.WriteDuplicateMarkers(folders, albums, notApplied, s.cfg.MusicFolders)
	if err != nil {
		return err
	}
	var errs []error
	for _, f := range written {
		e := byFolder[f]
		if err := s.st.SetMarkerWritten(ctx, e.ID, true); err != nil {
			errs = append(errs, err)
			continue
		}
		s.log.Info("marked duplicate folder", "id", e.ID, "folder", e.Folder, "release_id", e.DuplicateOfReleaseID)
	}
	if len(written) > 0 && scan {
		s.runner.Scan(ctx)
	}
	return errors.Join(errs...)
}

// unmarkDuplicates removes the .ndignore that wn-server wrote into the
// duplicate folders of releaseID and clears marker_written on their entries.
// The entries keep their state. It continues after an error and returns all
// of them.
func (s *Service) unmarkDuplicates(ctx context.Context, releaseID string) error {
	dups, err := s.st.MarkedDuplicates(ctx, releaseID)
	if err != nil {
		return err
	}
	var errs []error
	for _, d := range dups {
		removed, err := s.runner.RemoveMarker(d.Folder, s.cfg.MusicFolders)
		if err != nil {
			errs = append(errs, fmt.Errorf("remove .ndignore from duplicate folder %s: %w", d.Folder, err))
			continue
		}
		if err := s.st.SetMarkerWritten(ctx, d.ID, false); err != nil {
			errs = append(errs, err)
			continue
		}
		if removed {
			s.log.Info("removed .ndignore from duplicate folder", "id", d.ID, "folder", d.Folder, "release_id", releaseID)
		}
	}
	return errors.Join(errs...)
}

// recordAlbums records candidates lines in the match queue. markers holds
// the folders whose .ndignore the run created. A line without a folder is
// logged and skipped.
func recordAlbums(ctx context.Context, st *store.Store, albums []beets.Album, markers []string, logger *slog.Logger) error {
	now := time.Now()
	var errs []error
	for _, a := range albums {
		if a.Folder == "" {
			logger.Warn("candidates line has no folder; not recorded in the match queue", "decision", a.Decision, "error", a.Error)
			continue
		}
		l := matchLine(a)
		l.MarkerWritten = a.Decision == beets.DecisionApplied && slices.Contains(markers, a.Folder)
		if _, err := st.RecordMatch(ctx, l, now); err != nil {
			errs = append(errs, err)
		}
	}
	return errors.Join(errs...)
}

// matchLine converts a candidates line for the store. An applied line from
// an older plugin without release_id takes the first candidate, which is
// the one beets applies in quiet mode.
func matchLine(a beets.Album) store.MatchLine {
	l := store.MatchLine{
		Folder:     a.Folder,
		Decision:   a.Decision,
		Artist:     a.Tags.Artist,
		Album:      a.Tags.Album,
		Year:       a.Tags.Year,
		TrackCount: a.Tags.TrackCount,
		ReleaseID:  a.ReleaseID,
		CopiedTo:   a.CopiedTo,
		Error:      a.Error,
	}
	for _, c := range a.Candidates {
		l.Candidates = append(l.Candidates, store.MatchCandidate(c))
	}
	if l.Decision == beets.DecisionApplied && l.ReleaseID == "" && len(l.Candidates) > 0 {
		l.ReleaseID = l.Candidates[0].ReleaseID
	}
	return l
}
