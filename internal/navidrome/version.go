package navidrome

import (
	"context"
	"fmt"
	"os/exec"
	"strings"
	"sync/atomic"
	"time"
)

// PinnedVersion is the Navidrome release wn-server supervises. It is the tag
// v0.64.2 without the leading "v". The supervisor refuses to start any other
// version.
const PinnedVersion = "0.64.2"

const versionTimeout = 10 * time.Second

// serverVersion holds the full version string that VerifyBinary read.
var serverVersion atomic.Pointer[string]

// ServerVersion returns the version string Navidrome reports as
// serverVersion in Subsonic responses, for example "0.64.2 (10114574)". It
// is the output of `navidrome --version` that VerifyBinary read. Before
// VerifyBinary has succeeded it returns PinnedVersion.
func ServerVersion() string {
	if v := serverVersion.Load(); v != nil {
		return *v
	}
	return PinnedVersion
}

// fullVersion returns the output of `navidrome --version` without surrounding
// space and without the leading "v" of the tag.
func fullVersion(output string) string {
	return strings.TrimPrefix(strings.Join(strings.Fields(output), " "), "v")
}

// ParseVersion extracts the version from the output of `navidrome --version`.
// Navidrome prints consts.Version, which is the tag without its "v" followed
// by the git sha, for example "0.64.2 (101145742)". A build without version
// information prints "dev".
func ParseVersion(output string) (string, error) {
	fields := strings.Fields(output)
	if len(fields) == 0 {
		return "", fmt.Errorf("empty version output")
	}
	return strings.TrimPrefix(fields[0], "v"), nil
}

// CheckVersion returns an error that names both versions when got is not the
// pinned version.
func CheckVersion(got string) error {
	if got != PinnedVersion {
		return fmt.Errorf("navidrome version %q does not match pinned version %q", got, PinnedVersion)
	}
	return nil
}

// VerifyBinary runs `bin --version` and checks the result against the pinned
// version. It keeps the full version string for ServerVersion.
func VerifyBinary(ctx context.Context, bin string) error {
	ctx, cancel := context.WithTimeout(ctx, versionTimeout)
	defer cancel()
	out, err := exec.CommandContext(ctx, bin, "--version").Output()
	if err != nil {
		return fmt.Errorf("run %s --version: %w", bin, err)
	}
	got, err := ParseVersion(string(out))
	if err != nil {
		return fmt.Errorf("%s --version: %w", bin, err)
	}
	if err := CheckVersion(got); err != nil {
		return fmt.Errorf("%s: %w", bin, err)
	}
	full := fullVersion(string(out))
	serverVersion.Store(&full)
	return nil
}
