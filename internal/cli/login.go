package cli

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/spf13/cobra"

	"github.com/waxnerdhq/wn-server/internal/config"
	"github.com/waxnerdhq/wn-server/internal/store"
	"github.com/waxnerdhq/wn-server/internal/wncloud"
)

// maxServerNameLength is the longest server name wn-cloud accepts, in
// characters, after trimming.
const maxServerNameLength = 100

// errWrongState is returned when the pasted code carries another state.
var errWrongState = errors.New("the code does not belong to this login attempt")

// loadConfig loads config.toml from the path that --config names.
func loadConfig(cmd *cobra.Command) (*config.Config, error) {
	flag, _ := cmd.Flags().GetString("config")
	_, path, err := config.Paths(flag)
	if err != nil {
		return nil, err
	}
	return config.Load(path)
}

func newClient(c *config.Config) *wncloud.Client {
	return wncloud.New(c.WNCloudURL, "wn-server/"+Version)
}

// serverName returns the trimmed name, or the host name when name is empty,
// and checks it against the rules of POST /servers.
func serverName(name string) (string, error) {
	name = strings.TrimSpace(name)
	if name == "" {
		h, err := os.Hostname()
		if err != nil || strings.TrimSpace(h) == "" {
			return "", errors.New("cannot determine the host name; pass --name")
		}
		name = strings.TrimSpace(h)
	}
	if utf8.RuneCountInString(name) > maxServerNameLength {
		return "", fmt.Errorf("server name is longer than %d characters", maxServerNameLength)
	}
	for _, r := range name {
		if unicode.IsControl(r) {
			return "", errors.New("server name contains a control character")
		}
	}
	return name, nil
}

func newLoginCmd() *cobra.Command {
	var name string
	cmd := &cobra.Command{
		Use:   "login",
		Short: "Link the owner account to a waxnerd account and register this server at wn-cloud",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			name, err := serverName(name)
			if err != nil {
				return err
			}
			c, err := loadConfig(cmd)
			if err != nil {
				return err
			}
			ctx := cmd.Context()
			st, owner, err := openOwner(ctx, c.DataDir)
			if err != nil {
				return err
			}
			defer st.Close()
			return login(ctx, cmd.InOrStdin(), cmd.OutOrStdout(), cmd.ErrOrStderr(), st, owner, newClient(c), name)
		},
	}
	cmd.Flags().StringVar(&name, "name", "", "server name shown at wn-cloud (default: the host name)")
	return cmd
}

// login runs the code display sign-in (docs/wn-cloud-api.md, Sign-in with a
// displayed code), registers the server, and links the owner account.
func login(ctx context.Context, in io.Reader, out, errOut io.Writer, st *store.Store, owner store.Account, client *wncloud.Client, name string) error {
	if cred, err := st.ServerCredential(ctx); err == nil {
		return fmt.Errorf("already linked to server %s; run wn-server logout first", cred.ServerID)
	} else if !errors.Is(err, store.ErrNoServerCredential) {
		return err
	}
	state, err := wncloud.NewState()
	if err != nil {
		return err
	}
	verifier, err := wncloud.NewVerifier()
	if err != nil {
		return err
	}

	fmt.Fprintln(out, "Open this URL in a browser on any device:")
	fmt.Fprintln(out)
	fmt.Fprintln(out, "  "+client.AuthURL(state, wncloud.Challenge(verifier), name))
	fmt.Fprintln(out)
	fmt.Fprintf(out, "Sign in with your waxnerd account and approve the request for %q.\n", name)
	fmt.Fprintln(out, "wn-cloud then shows a sign-in code. Paste the code here.")
	fmt.Fprint(out, "code: ")

	line, err := bufio.NewReader(in).ReadString('\n')
	if err != nil && !errors.Is(err, io.EOF) {
		return err
	}
	pasted := strings.TrimSpace(line)
	if pasted == "" {
		return errors.New("no code entered")
	}
	code, gotState, _ := strings.Cut(pasted, ".")
	if gotState != state {
		return errWrongState
	}

	token, userID, err := client.ExchangeCode(ctx, code, verifier)
	if err != nil {
		return explain("wn-cloud did not accept the code", err)
	}
	// A registration kept by logout belongs to the same server. It is reused
	// when the same waxnerd user signs in again (docs/wn-cloud-api.md,
	// POST /servers).
	serverID, regUserID, registered, err := st.ServerRegistration(ctx)
	if err != nil {
		revoke(ctx, client, token, errOut)
		return err
	}
	if !registered || regUserID != userID {
		serverID, err = client.RegisterServer(ctx, token, name)
		if err != nil {
			revoke(ctx, client, token, errOut)
			return explain("wn-cloud did not register the server", err)
		}
	}
	cred := store.ServerCredential{Token: token, ServerID: serverID, WaxnerdUserID: userID, CreatedAt: time.Now()}
	if err := st.Link(ctx, owner.ID, cred); err != nil {
		revoke(ctx, client, token, errOut)
		return fmt.Errorf("store the server credential: %w", err)
	}
	// Link also queued every local rating in the outbox. The drain in serve
	// sends them with the new credential (README, Authentication, step 8).

	fmt.Fprintf(out, "linked owner %q to waxnerd user %s\n", owner.Name, userID)
	fmt.Fprintf(out, "server id: %s\n", serverID)
	fmt.Fprintf(out, "app password: %s\n", owner.AppPassword)
	fmt.Fprintln(out, "This app password keeps working when wn-cloud is unreachable.")
	return nil
}

// revoke revokes a token that login obtained but cannot use, so that no
// unused token stays at wn-cloud. A failure is printed as a warning.
func revoke(ctx context.Context, client *wncloud.Client, token string, errOut io.Writer) {
	if err := client.Logout(ctx, token); err != nil {
		fmt.Fprintf(errOut, "warning: could not revoke the new CLI token at wn-cloud: %v\n", err)
	}
}

// explain adds a sentence on what to do next to a wn-cloud error.
func explain(what string, err error) error {
	switch {
	case errors.Is(err, wncloud.ErrInvalidGrant):
		return fmt.Errorf("%s: the code is invalid, expired, or already used; run wn-server login again (%w)", what, err)
	case errors.Is(err, wncloud.ErrUnreachable):
		return fmt.Errorf("%s: wn-cloud is unreachable; run wn-server login again later (%w)", what, err)
	default:
		return fmt.Errorf("%s: %w", what, err)
	}
}

func newLogoutCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "logout",
		Short: "Revoke the server credential and unlink the owner account from waxnerd",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			c, err := loadConfig(cmd)
			if err != nil {
				return err
			}
			ctx := cmd.Context()
			st, owner, err := openOwner(ctx, c.DataDir)
			if err != nil {
				return err
			}
			defer st.Close()
			return logout(ctx, cmd.OutOrStdout(), cmd.ErrOrStderr(), st, owner, newClient(c))
		},
	}
}

// logout revokes the server credential at wn-cloud, removes the token, and
// clears the owner's waxnerd user id. The store keeps the server id. Every
// error from wn-cloud is a warning, and the credential is removed anyway.
func logout(ctx context.Context, out, errOut io.Writer, st *store.Store, owner store.Account, client *wncloud.Client) error {
	cred, err := st.ServerCredential(ctx)
	if errors.Is(err, store.ErrNoServerCredential) {
		if owner.WaxnerdUserID != "" {
			if err := st.LinkOwner(ctx, owner.ID, ""); err != nil {
				return err
			}
		}
		fmt.Fprintln(out, "not linked; nothing to do")
		return nil
	}
	if err != nil {
		return err
	}
	if err := client.Logout(ctx, cred.Token); err != nil {
		fmt.Fprintf(errOut, "warning: wn-cloud did not revoke the server credential; it is removed here (%v)\n", err)
	} else {
		fmt.Fprintln(out, "revoked the server credential at wn-cloud")
	}
	if err := st.Unlink(ctx, owner.ID); err != nil {
		return err
	}
	fmt.Fprintln(out, "removed the server credential")
	fmt.Fprintf(out, "unlinked owner %q from waxnerd user %s\n", owner.Name, cred.WaxnerdUserID)
	fmt.Fprintf(out, "wn-cloud keeps the record of server %s\n", cred.ServerID)
	return nil
}

func newStatusCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "status",
		Short: "Print the owner account and whether it is linked to waxnerd",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			c, err := loadConfig(cmd)
			if err != nil {
				return err
			}
			ctx := cmd.Context()
			st, owner, err := openOwner(ctx, c.DataDir)
			if err != nil {
				return err
			}
			defer st.Close()
			out := cmd.OutOrStdout()
			fmt.Fprintf(out, "owner name: %s\n", owner.Name)
			fmt.Fprintf(out, "account id: %s\n", owner.ID)
			cred, err := st.ServerCredential(ctx)
			switch {
			case errors.Is(err, store.ErrNoServerCredential):
				fmt.Fprintln(out, "not linked; run wn-server login")
			case err != nil:
				return err
			default:
				fmt.Fprintf(out, "waxnerd user id: %s\n", cred.WaxnerdUserID)
				fmt.Fprintf(out, "server id: %s\n", cred.ServerID)
				fmt.Fprintf(out, "linked at: %s\n", cred.CreatedAt.Format(time.RFC3339))
			}
			fmt.Fprintf(out, "wn-cloud: %s\n", c.WNCloudURL)
			return nil
		},
	}
}
