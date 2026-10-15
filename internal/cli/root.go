// Package cli defines the wn-server command line.
package cli

import (
	"errors"
	"fmt"
	"os"

	"github.com/spf13/cobra"
)

// Version is set at build time with -ldflags "-X .../internal/cli.Version=...".
var Version = "dev"

var errNotImplemented = errors.New("not implemented")

func newRootCmd() *cobra.Command {
	root := &cobra.Command{
		Use:           "wn-server",
		Short:         "waxnerd music server, powered by Navidrome",
		SilenceUsage:  true,
		SilenceErrors: true,
	}
	root.PersistentFlags().String("config", "", "path to config.toml (default: <data dir>/config.toml)")
	root.AddCommand(newServeCmd(), newLoginCmd(), newMatchCmd(), newVersionCmd())
	return root
}

func newServeCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "serve",
		Short: "Start Navidrome and the wn-server proxy",
		RunE: func(cmd *cobra.Command, args []string) error {
			return errNotImplemented
		},
	}
}

func newLoginCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "login",
		Short: "Sign in with a waxnerd account and register this server",
		RunE: func(cmd *cobra.Command, args []string) error {
			return errNotImplemented
		},
	}
}

func newMatchCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "match",
		Short: "Match albums to MusicBrainz with beets",
		RunE: func(cmd *cobra.Command, args []string) error {
			return errNotImplemented
		},
	}
}

func newVersionCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "version",
		Short: "Print the version",
		Run: func(cmd *cobra.Command, args []string) {
			fmt.Fprintln(cmd.OutOrStdout(), "wn-server", Version)
		},
	}
}

// Execute runs the root command and prints any error to stderr.
func Execute() error {
	err := newRootCmd().Execute()
	if err != nil {
		fmt.Fprintln(os.Stderr, "wn-server:", err)
	}
	return err
}
