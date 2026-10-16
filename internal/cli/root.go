// Package cli defines the wn-server command line.
package cli

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"strconv"
	"syscall"
	"time"

	"github.com/spf13/cobra"

	"github.com/waxnerdhq/wn-server/internal/config"
	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/proxy"
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
	root.AddCommand(newInitCmd(), newServeCmd(), newLoginCmd(), newMatchCmd(), newVersionCmd())
	return root
}

func newInitCmd() *cobra.Command {
	var folders []string
	cmd := &cobra.Command{
		Use:   "init",
		Short: "Create the data directory and a first config.toml",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			flag, _ := cmd.Flags().GetString("config")
			_, path, err := config.Paths(flag)
			if err != nil {
				return err
			}
			c, err := config.Init(path, folders)
			if err != nil {
				return err
			}
			fmt.Fprintf(cmd.OutOrStdout(), "wrote %s\n", path)
			fmt.Fprintf(cmd.OutOrStdout(), "data directory: %s\n", c.DataDir)
			return nil
		},
	}
	cmd.Flags().StringArrayVar(&folders, "folder", nil, "music folder (repeatable, at least one)")
	_ = cmd.MarkFlagRequired("folder")
	return cmd
}

func newServeCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "serve",
		Short: "Start Navidrome and the wn-server proxy",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			flag, _ := cmd.Flags().GetString("config")
			_, path, err := config.Paths(flag)
			if err != nil {
				return err
			}
			c, err := config.Load(path)
			if err != nil {
				return err
			}
			ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
			defer stop()
			return serve(ctx, c, slog.Default())
		},
	}
}

// shutdownGrace is how long the HTTP server gets to finish in-flight requests.
const shutdownGrace = 5 * time.Second

// serve runs the Navidrome supervisor and the proxy until ctx is cancelled
// or one of them fails.
func serve(ctx context.Context, c *config.Config, logger *slog.Logger) error {
	ln, err := net.Listen("tcp", c.Listen)
	if err != nil {
		return err
	}
	sup := navidrome.NewSupervisor(c, logger)
	target := &url.URL{Scheme: "http", Host: net.JoinHostPort("127.0.0.1", strconv.Itoa(c.NavidromePort))}
	srv := &http.Server{
		Handler:           proxy.New(target, sup.Ready(), logger),
		ReadHeaderTimeout: 10 * time.Second,
	}

	runCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	supDone := make(chan error, 1)
	go func() { supDone <- sup.Run(runCtx) }()
	srvDone := make(chan error, 1)
	go func() { srvDone <- srv.Serve(ln) }()
	logger.Info("listening", "addr", ln.Addr().String())

	var runErr error
	supStopped := false
	select {
	case <-runCtx.Done():
	case runErr = <-supDone:
		supStopped = true
	case runErr = <-srvDone:
	}
	cancel()

	shutCtx, stop := context.WithTimeout(context.Background(), shutdownGrace)
	defer stop()
	if err := srv.Shutdown(shutCtx); err != nil {
		_ = srv.Close()
	}
	if !supStopped {
		if err := <-supDone; runErr == nil {
			runErr = err
		}
	}
	if errors.Is(runErr, http.ErrServerClosed) {
		runErr = nil
	}
	return runErr
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
