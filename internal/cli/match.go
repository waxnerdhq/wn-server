package cli

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"text/tabwriter"
	"time"

	"github.com/spf13/cobra"

	"github.com/waxnerdhq/wn-server/internal/beets"
	"github.com/waxnerdhq/wn-server/internal/config"
	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/store"
)

// newMatchCmd is the `match` command and its queue subcommands.
func newMatchCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "match",
		Short: "Match albums to MusicBrainz with beets and copy confident matches into sorted_dir",
		Long: "Match albums to MusicBrainz with beets and copy confident matches into sorted_dir.\n" +
			"Every album beets saw is recorded in the match queue. The subcommands list, apply, dismiss, and revert entries.",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			c, st, err := openMatch(cmd)
			if err != nil {
				return err
			}
			defer st.Close()
			ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
			defer stop()
			r := beets.NewRunner(c, navidrome.NewClient(c.NavidromePort), slog.Default())
			sum, err := r.Import(ctx, c.MusicFolders)
			// Record what beets wrote, also after a cancel.
			recErr := recordAlbums(context.WithoutCancel(ctx), st, sum.Albums, slog.Default())
			out := cmd.OutOrStdout()
			if err == nil || len(sum.Albums) > 0 {
				printSummary(out, sum)
			}
			if n, cerr := st.CountMatches(context.WithoutCancel(ctx), store.MatchPending); cerr == nil {
				if n == 0 {
					fmt.Fprintln(out, "match queue: 0 pending")
				} else {
					fmt.Fprintf(out, "match queue: %d pending; `wn-server match list` shows them\n", n)
				}
			}
			return errors.Join(err, recErr)
		},
	}
	cmd.AddCommand(newMatchListCmd(), newMatchApplyCmd(), newMatchDismissCmd(), newMatchRevertCmd())
	return cmd
}

// openMatch loads the config and opens the store.
func openMatch(cmd *cobra.Command) (*config.Config, *store.Store, error) {
	flag, _ := cmd.Flags().GetString("config")
	_, path, err := config.Paths(flag)
	if err != nil {
		return nil, nil, err
	}
	c, err := config.Load(path)
	if err != nil {
		return nil, nil, err
	}
	st, err := openStore(cmd.Context(), c.DataDir)
	if err != nil {
		return nil, nil, err
	}
	return c, st, nil
}

// openEntry loads the config, opens the store, and reads the entry whose id
// is arg.
func openEntry(cmd *cobra.Command, arg string) (*config.Config, *store.Store, store.MatchEntry, error) {
	id, err := strconv.ParseInt(arg, 10, 64)
	if err != nil || id <= 0 {
		return nil, nil, store.MatchEntry{}, fmt.Errorf("entry id %q is not a positive number", arg)
	}
	c, st, err := openMatch(cmd)
	if err != nil {
		return nil, nil, store.MatchEntry{}, err
	}
	e, err := st.MatchEntry(cmd.Context(), id)
	if err != nil {
		st.Close()
		if errors.Is(err, store.ErrMatchNotFound) {
			err = fmt.Errorf("no match queue entry %d", id)
		}
		return nil, nil, store.MatchEntry{}, err
	}
	return c, st, e, nil
}

// recordAlbums records candidates lines in the match queue. A line without
// a folder is logged and skipped.
func recordAlbums(ctx context.Context, st *store.Store, albums []beets.Album, logger *slog.Logger) error {
	now := time.Now()
	var errs []error
	for _, a := range albums {
		if a.Folder == "" {
			logger.Warn("candidates line has no folder; not recorded in the match queue", "decision", a.Decision, "error", a.Error)
			continue
		}
		if _, err := st.RecordMatch(ctx, matchLine(a), now); err != nil {
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

// printSummary prints the counts and one line per skipped album with its
// best candidate.
func printSummary(w io.Writer, sum beets.Summary) {
	fmt.Fprintf(w, "applied %d, skipped %d, duplicates %d, errors %d\n", sum.Applied, sum.Skipped, sum.Duplicate, sum.Errors)
	for _, a := range sum.Albums {
		if a.Decision != beets.DecisionSkipped {
			continue
		}
		if len(a.Candidates) == 0 {
			fmt.Fprintf(w, "skipped %s: no candidates\n", a.Folder)
			continue
		}
		c := a.Candidates[0]
		fmt.Fprintf(w, "skipped %s: best candidate %s - %s (%d) %s, distance %.3f\n", a.Folder, c.Artist, c.Album, c.Year, c.ReleaseID, c.Distance)
	}
}

func newMatchListCmd() *cobra.Command {
	var all, asJSON bool
	cmd := &cobra.Command{
		Use:   "list",
		Short: "List pending match queue entries",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			_, st, err := openMatch(cmd)
			if err != nil {
				return err
			}
			defer st.Close()
			var states []store.MatchState
			if !all {
				states = []store.MatchState{store.MatchPending}
			}
			entries, err := st.MatchEntries(cmd.Context(), states...)
			if err != nil {
				return err
			}
			if asJSON {
				return printEntriesJSON(cmd.OutOrStdout(), entries)
			}
			printEntries(cmd.OutOrStdout(), entries, all)
			return nil
		},
	}
	cmd.Flags().BoolVar(&all, "all", false, "list entries in every state")
	cmd.Flags().BoolVar(&asJSON, "json", false, "print the entries as JSON")
	return cmd
}

// printEntries prints one row per entry with its id, state, folder, tags,
// and top candidate.
func printEntries(w io.Writer, entries []store.MatchEntry, all bool) {
	if len(entries) == 0 {
		if all {
			fmt.Fprintln(w, "the match queue is empty")
		} else {
			fmt.Fprintln(w, "no pending entries")
		}
		return
	}
	tw := tabwriter.NewWriter(w, 0, 0, 2, ' ', 0)
	fmt.Fprintln(tw, "ID\tSTATE\tFOLDER\tTAGS\tTOP CANDIDATE")
	for _, e := range entries {
		fmt.Fprintf(tw, "%d\t%s\t%s\t%s\t%s\n", e.ID, e.State, e.Folder, entryTags(e), topCandidate(e))
	}
	tw.Flush()
}

func entryTags(e store.MatchEntry) string {
	s := orUnknown(e.Artist) + " - " + orUnknown(e.Album)
	if e.Year > 0 {
		s += fmt.Sprintf(" (%d)", e.Year)
	}
	return s + fmt.Sprintf(", %d tracks", e.TrackCount)
}

func topCandidate(e store.MatchEntry) string {
	if len(e.Candidates) == 0 {
		return "no candidates"
	}
	c := e.Candidates[0]
	s := orUnknown(c.Artist) + " - " + orUnknown(c.Album)
	if c.Year > 0 {
		s += fmt.Sprintf(" (%d)", c.Year)
	}
	return s + fmt.Sprintf(" %s, distance %.3f", c.ReleaseID, c.Distance)
}

func orUnknown(s string) string {
	if s == "" {
		return "?"
	}
	return s
}

// printEntriesJSON prints the entries as a JSON array.
func printEntriesJSON(w io.Writer, entries []store.MatchEntry) error {
	if entries == nil {
		entries = []store.MatchEntry{}
	}
	for i := range entries {
		if entries[i].Candidates == nil {
			entries[i].Candidates = []store.MatchCandidate{}
		}
	}
	enc := json.NewEncoder(w)
	enc.SetIndent("", "  ")
	return enc.Encode(entries)
}

func newMatchApplyCmd() *cobra.Command {
	var release string
	cmd := &cobra.Command{
		Use:   "apply <id>",
		Short: "Tag and copy an album with a chosen MusicBrainz release",
		Long: "Tag and copy the album of a pending or dismissed match queue entry with a MusicBrainz release.\n" +
			"The release is the entry's first candidate unless --release names another.",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			c, st, e, err := openEntry(cmd, args[0])
			if err != nil {
				return err
			}
			defer st.Close()
			if e.State != store.MatchPending && e.State != store.MatchDismissed {
				return fmt.Errorf("entry %d is %s; only a pending or dismissed entry can be applied", e.ID, e.State)
			}
			release = strings.ToLower(strings.TrimSpace(release))
			if release == "" {
				if len(e.Candidates) == 0 {
					return fmt.Errorf("entry %d has no candidates; pass --release <MusicBrainz release id>", e.ID)
				}
				release = e.Candidates[0].ReleaseID
			}
			if !beets.ValidReleaseID(release) {
				return fmt.Errorf("release id %q is not a MusicBrainz id (a UUID)", release)
			}
			ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
			defer stop()
			logger := slog.Default()
			logger.Info("applying match", "id", e.ID, "folder", e.Folder, "release_id", release)
			r := beets.NewRunner(c, navidrome.NewClient(c.NavidromePort), logger)
			sum, runErr := r.Apply(ctx, e.Folder, release, c.MusicFolders)
			// Only the entry's own line is recorded. Other albums below the
			// folder were skipped by the plugin and looked up with the
			// requested release only, so their candidates are not useful.
			var own []beets.Album
			for _, a := range sum.Albums {
				if a.Folder == e.Folder {
					own = append(own, a)
				}
			}
			bg := context.WithoutCancel(ctx)
			if err := recordAlbums(bg, st, own, logger); err != nil || runErr != nil {
				return errors.Join(runErr, err)
			}
			got, err := st.MatchEntry(bg, e.ID)
			if err != nil {
				return err
			}
			if got.State != store.MatchApplied {
				reason := "beets wrote no line for the folder"
				if len(own) > 0 {
					reason = "decision " + own[len(own)-1].Decision
					if own[len(own)-1].Error != "" {
						reason += ": " + own[len(own)-1].Error
					}
				}
				return fmt.Errorf("beets did not apply release %s to %s (%s); see the log for beets' output", release, e.Folder, reason)
			}
			logger.Info("applied match", "id", e.ID, "folder", e.Folder, "release_id", got.AppliedReleaseID, "copied_to", got.CopiedTo)
			fmt.Fprintf(cmd.OutOrStdout(), "applied entry %d: %s copied to %s\n", e.ID, e.Folder, got.CopiedTo)
			return nil
		},
	}
	cmd.Flags().StringVar(&release, "release", "", "MusicBrainz release id to apply (default: the first candidate)")
	return cmd
}

func newMatchDismissCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "dismiss <id>",
		Short: "Mark a match queue entry as dismissed; nothing on disk changes",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			_, st, e, err := openEntry(cmd, args[0])
			if err != nil {
				return err
			}
			defer st.Close()
			if e.State == store.MatchApplied {
				return fmt.Errorf("entry %d is applied; run `wn-server match revert %d` first", e.ID, e.ID)
			}
			if err := st.SetMatchState(cmd.Context(), e.ID, store.MatchDismissed); err != nil {
				return err
			}
			fmt.Fprintf(cmd.OutOrStdout(), "dismissed entry %d: %s\n", e.ID, e.Folder)
			return nil
		},
	}
}

func newMatchRevertCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "revert <id>",
		Short: "Remove an applied album's copy from sorted_dir and the beets library",
		Long: "Remove an applied album from the beets library, delete its copy in sorted_dir, delete the empty\n" +
			".ndignore in the original folder, and set the entry to pending. beets' incremental history still\n" +
			"lists the folder, so `wn-server match` does not offer it again. `wn-server match apply` tags it again.",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			c, st, e, err := openEntry(cmd, args[0])
			if err != nil {
				return err
			}
			defer st.Close()
			if e.State != store.MatchApplied {
				return fmt.Errorf("entry %d is %s; only an applied entry can be reverted", e.ID, e.State)
			}
			if e.CopiedTo == "" {
				return fmt.Errorf("entry %d has no copied_to; nothing to revert", e.ID)
			}
			ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
			defer stop()
			logger := slog.Default()
			logger.Info("reverting match", "id", e.ID, "folder", e.Folder, "release_id", e.AppliedReleaseID, "copied_to", e.CopiedTo)
			r := beets.NewRunner(c, navidrome.NewClient(c.NavidromePort), logger)
			if err := r.Remove(ctx, e.CopiedTo); err != nil {
				return err
			}
			// The copy is gone. The remaining steps run to the end.
			bg := context.WithoutCancel(ctx)
			_, markerErr := r.RemoveMarker(e.Folder, c.MusicFolders)
			if markerErr != nil {
				markerErr = fmt.Errorf("remove .ndignore from %s: %w", e.Folder, markerErr)
			}
			if err := st.RevertMatch(bg, e.ID); err != nil {
				return errors.Join(err, markerErr)
			}
			r.Scan(bg)
			logger.Info("reverted match", "id", e.ID, "folder", e.Folder, "release_id", e.AppliedReleaseID)
			fmt.Fprintf(cmd.OutOrStdout(), "reverted entry %d; it is pending again. `wn-server match apply %d` tags it again.\n", e.ID, e.ID)
			return markerErr
		},
	}
}
