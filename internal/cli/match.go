package cli

import (
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/signal"
	"strconv"
	"syscall"
	"text/tabwriter"

	"github.com/spf13/cobra"

	"github.com/waxnerdhq/wn-server/internal/beets"
	"github.com/waxnerdhq/wn-server/internal/config"
	"github.com/waxnerdhq/wn-server/internal/match"
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
			svc, closeStore, err := openService(cmd)
			if err != nil {
				return err
			}
			defer closeStore()
			ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
			defer stop()
			res, err := svc.Run(ctx)
			out := cmd.OutOrStdout()
			// A run that did not start, such as one the beets lock refused,
			// prints neither the summary nor the queue count.
			if res.Reported {
				printSummary(out, res.Summary)
				switch {
				case res.Pending == 0:
					fmt.Fprintln(out, "match queue: 0 pending")
				case res.Pending > 0:
					fmt.Fprintf(out, "match queue: %d pending; `wn-server match list` shows them\n", res.Pending)
				}
			}
			return err
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

// openService loads the config, opens the store, and returns the match
// service with the function that closes the store.
func openService(cmd *cobra.Command) (*match.Service, func(), error) {
	c, st, err := openMatch(cmd)
	if err != nil {
		return nil, nil, err
	}
	nd := navidrome.NewClient(navidrome.NewTransport(c.NavidromeSocketPath()))
	return match.NewService(c, st, nd, slog.Default()), func() { st.Close() }, nil
}

// parseEntryID parses the id argument of an entry command.
func parseEntryID(arg string) (int64, error) {
	id, err := strconv.ParseInt(arg, 10, 64)
	if err != nil || id <= 0 {
		return 0, fmt.Errorf("entry id %q is not a positive number", arg)
	}
	return id, nil
}

// printSummary prints the counts and one line per skipped album with its
// best candidate.
func printSummary(w io.Writer, sum beets.Summary) {
	fmt.Fprintln(w, match.SummaryLine(sum))
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
		fmt.Fprintf(tw, "%d\t%s\t%s\t%s\t%s\n", e.ID, entryState(e), e.Folder, entryTags(e), topCandidate(e))
	}
	tw.Flush()
}

// entryState returns the state of e. A duplicate with a release id also
// names the release it duplicates.
func entryState(e store.MatchEntry) string {
	if e.State == store.MatchDuplicate && e.DuplicateOfReleaseID != "" {
		return fmt.Sprintf("%s of %s", e.State, e.DuplicateOfReleaseID)
	}
	return string(e.State)
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
			id, err := parseEntryID(args[0])
			if err != nil {
				return err
			}
			svc, closeStore, err := openService(cmd)
			if err != nil {
				return err
			}
			defer closeStore()
			ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
			defer stop()
			res, err := svc.Apply(ctx, id, release)
			if err != nil {
				return err
			}
			fmt.Fprintln(cmd.OutOrStdout(), res.Line())
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
			id, err := parseEntryID(args[0])
			if err != nil {
				return err
			}
			svc, closeStore, err := openService(cmd)
			if err != nil {
				return err
			}
			defer closeStore()
			folder, err := svc.Dismiss(cmd.Context(), id)
			if err != nil {
				return err
			}
			fmt.Fprintf(cmd.OutOrStdout(), "dismissed entry %d: %s\n", id, folder)
			return nil
		},
	}
}

func newMatchRevertCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "revert <id>",
		Short: "Remove an applied album's copy from sorted_dir and the beets library",
		Long: "Remove an applied album from the beets library, delete its copy in sorted_dir, delete the empty\n" +
			".ndignore in the original folder and in the duplicate folders of the same release, and set the entry to\n" +
			"pending. beets' incremental history still\n" +
			"lists the folder, so `wn-server match` does not offer it again. `wn-server match apply` tags it again.",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			id, err := parseEntryID(args[0])
			if err != nil {
				return err
			}
			svc, closeStore, err := openService(cmd)
			if err != nil {
				return err
			}
			defer closeStore()
			ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
			defer stop()
			res, err := svc.Revert(ctx, id)
			if res.Reverted {
				fmt.Fprintf(cmd.OutOrStdout(), "%s `wn-server match apply %d` tags it again.\n", res.Line(), res.ID)
			}
			return err
		},
	}
}
