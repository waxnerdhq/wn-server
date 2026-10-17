// Package cli defines the wn-server command line.
package cli

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
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
	"github.com/waxnerdhq/wn-server/internal/match"
	"github.com/waxnerdhq/wn-server/internal/navidrome"
	"github.com/waxnerdhq/wn-server/internal/proxy"
	"github.com/waxnerdhq/wn-server/internal/ratings"
	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/web"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

// Version is set at build time with -ldflags "-X .../internal/cli.Version=...".
var Version = "dev"

func newRootCmd() *cobra.Command {
	root := &cobra.Command{
		Use:           "wn-server",
		Short:         "waxnerd music server, powered by Navidrome",
		SilenceUsage:  true,
		SilenceErrors: true,
	}
	root.PersistentFlags().String("config", "", "path to config.toml (default: <data dir>/config.toml)")
	root.AddCommand(newInitCmd(), newServeCmd(), newPasswordCmd(), newLoginCmd(), newLogoutCmd(), newStatusCmd(), newMatchCmd(), newVersionCmd())
	return root
}

func newInitCmd() *cobra.Command {
	var (
		folders []string
		name    string
	)
	cmd := &cobra.Command{
		Use:   "init",
		Short: "Create the data directory, a first config.toml, and the owner account",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			if err := store.ValidateName(name); err != nil {
				return err
			}
			flag, _ := cmd.Flags().GetString("config")
			dataDir, path, err := config.Paths(flag)
			if err != nil {
				return err
			}
			ctx := cmd.Context()
			dbPath := store.Path(dataDir)
			// Refuse before writing anything when the owner already exists.
			if _, err := os.Stat(dbPath); err == nil {
				st, err := store.Open(ctx, dbPath)
				if err != nil {
					return err
				}
				_, err = st.Owner(ctx)
				st.Close()
				if err == nil {
					return fmt.Errorf("%s already has an owner account; use `wn-server password` instead of running init again", dbPath)
				}
				if !errors.Is(err, store.ErrNotFound) {
					return err
				}
			}
			c, err := config.Init(path, folders)
			if err != nil {
				return err
			}
			owner, err := createOwner(ctx, store.Path(c.DataDir), name)
			if err != nil {
				// Leave nothing behind, so the next init can run.
				removeInitFiles(path, store.Path(c.DataDir))
				return err
			}
			out := cmd.OutOrStdout()
			fmt.Fprintf(out, "wrote %s\n", path)
			fmt.Fprintf(out, "data directory: %s\n", c.DataDir)
			fmt.Fprintf(out, "owner name: %s\n", owner.Name)
			fmt.Fprintf(out, "app password: %s\n", owner.AppPassword)
			fmt.Fprintln(out, "Use this name and app password to sign in from Subsonic clients. `wn-server password` prints the password again.")
			return nil
		},
	}
	cmd.Flags().StringArrayVar(&folders, "folder", nil, "music folder (repeatable, at least one)")
	cmd.Flags().StringVar(&name, "name", "owner", "owner account name, used as the Subsonic user name")
	_ = cmd.MarkFlagRequired("folder")
	return cmd
}

// createOwner opens the store at dbPath, creates the owner, and closes it.
func createOwner(ctx context.Context, dbPath, name string) (store.Account, error) {
	st, err := store.Open(ctx, dbPath)
	if err != nil {
		return store.Account{}, err
	}
	defer st.Close()
	return st.CreateOwner(ctx, name)
}

// removeInitFiles removes the config file and the database files that a
// failed init wrote.
func removeInitFiles(configPath, dbPath string) {
	for _, p := range []string{configPath, dbPath, dbPath + "-wal", dbPath + "-shm"} {
		_ = os.Remove(p)
	}
}

// openStore opens the store in dataDir. It does not create one: a missing
// store means init has not run.
func openStore(ctx context.Context, dataDir string) (*store.Store, error) {
	path := store.Path(dataDir)
	if _, err := os.Stat(path); errors.Is(err, fs.ErrNotExist) {
		return nil, fmt.Errorf("%s not found; run `wn-server init --folder <music folder>`", path)
	}
	return store.Open(ctx, path)
}

// openOwner opens the store and returns it with the owner account.
func openOwner(ctx context.Context, dataDir string) (*store.Store, store.Account, error) {
	st, err := openStore(ctx, dataDir)
	if err != nil {
		return nil, store.Account{}, err
	}
	owner, err := st.Owner(ctx)
	if err != nil {
		st.Close()
		if errors.Is(err, store.ErrNotFound) {
			err = fmt.Errorf("%s has no owner account; move config.toml aside and run `wn-server init` again", store.Path(dataDir))
		}
		return nil, store.Account{}, err
	}
	return st, owner, nil
}

func newPasswordCmd() *cobra.Command {
	var reset bool
	cmd := &cobra.Command{
		Use:   "password",
		Short: "Print the owner's app password",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			flag, _ := cmd.Flags().GetString("config")
			dataDir, _, err := config.Paths(flag)
			if err != nil {
				return err
			}
			ctx := cmd.Context()
			st, owner, err := openOwner(ctx, dataDir)
			if err != nil {
				return err
			}
			defer st.Close()
			pw := owner.AppPassword
			if reset {
				if pw, err = store.NewAppPassword(); err != nil {
					return err
				}
				if err := st.SetAppPassword(ctx, owner.ID, pw); err != nil {
					return err
				}
			}
			fmt.Fprintln(cmd.OutOrStdout(), pw)
			return nil
		},
	}
	cmd.Flags().BoolVar(&reset, "reset", false, "generate a new app password; the old one stops working")
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
			st, owner, err := openOwner(ctx, c.DataDir)
			if err != nil {
				return err
			}
			defer st.Close()
			return serve(ctx, c, st, owner, slog.Default())
		},
	}
}

// shutdownGrace is how long the HTTP server gets to finish in-flight requests.
const shutdownGrace = 5 * time.Second

// jobShutdownWait is how long serve waits for a canceled match job to record
// what beets did before the process exits.
const jobShutdownWait = 30 * time.Second

// serve runs the Navidrome supervisor and the proxy until ctx is cancelled
// or one of them fails. The supervisor outlives the HTTP server: on a signal
// the server drains first, then Navidrome is stopped, so in-flight streams
// are not cut by an early SIGTERM to Navidrome.
func serve(ctx context.Context, c *config.Config, st *store.Store, owner store.Account, logger *slog.Logger) error {
	// Check the binary before binding, so a mismatch logs no listening line.
	if err := navidrome.VerifyBinary(ctx, c.NavidromeBin); err != nil {
		return err
	}
	ln, err := net.Listen("tcp", c.Listen)
	if err != nil {
		return err
	}
	sup := navidrome.NewSupervisor(c, navidrome.Account{ID: owner.ID, Name: owner.Name}, logger)
	target := &url.URL{Scheme: "http", Host: net.JoinHostPort("127.0.0.1", strconv.Itoa(c.NavidromePort))}
	cloudStatus := &wncloud.Status{}
	// The server name at wn-cloud is not stored. The UI shows the host name,
	// which is the default of login --name.
	hostName, _ := os.Hostname()
	// Match jobs from the web UI stop with ctx, and serve waits for one to
	// finish before it returns.
	jobsCtx, cancelJobs := context.WithCancel(ctx)
	jobs := match.NewJobs(jobsCtx, logger)
	defer func() {
		cancelJobs()
		if !jobs.Wait(jobShutdownWait) {
			logger.Warn("a match job did not stop in time", "wait", jobShutdownWait.String())
		}
	}()
	ndClient := navidrome.NewClient(c.NavidromePort)
	ui := web.New(web.Options{
		Store:            st,
		Navidrome:        ndClient,
		Match:            match.NewService(c, st, ndClient, logger),
		Jobs:             jobs,
		Ready:            sup.Ready(),
		CloudStatus:      cloudStatus,
		ServerName:       hostName,
		NavidromeVersion: navidrome.ServerVersion(),
		Logger:           logger,
	})
	cloud := newClient(c)
	rt := ratings.New(st, cloud, ndClient, cloudStatus, logger)
	srv := &http.Server{
		Handler: proxy.New(target, sup.Ready(), st, cloud, logger,
			proxy.WithUI(ui), proxy.WithCloudStatus(cloudStatus), proxy.WithRatings(rt)),
		ReadHeaderTimeout: 10 * time.Second,
	}

	supCtx, cancelSup := context.WithCancel(context.WithoutCancel(ctx))
	defer cancelSup()
	supDone := make(chan error, 1)
	go func() { supDone <- sup.Run(supCtx) }()
	srvDone := make(chan error, 1)
	go func() { srvDone <- srv.Serve(ln) }()
	go rt.Run(ctx)
	go rt.RunReconcile(ctx, sup.Ready())
	logger.Info("listening", "addr", ln.Addr().String())

	shutdownServer := func() {
		shutCtx, stop := context.WithTimeout(context.Background(), shutdownGrace)
		defer stop()
		if err := srv.Shutdown(shutCtx); err != nil {
			_ = srv.Close()
		}
	}

	select {
	case <-ctx.Done():
		logger.Info("signal received; shutting down")
		shutdownServer()
		cancelSup()
		err := <-supDone
		logger.Info("shutdown complete")
		return err
	case err := <-supDone:
		shutdownServer()
		if err == nil {
			err = errors.New("navidrome supervisor stopped")
		}
		return err
	case err := <-srvDone:
		cancelSup()
		<-supDone
		if errors.Is(err, http.ErrServerClosed) {
			err = errors.New("http server stopped")
		}
		return err
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
