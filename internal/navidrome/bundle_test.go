package navidrome

import (
	"bufio"
	"os"
	"strings"
	"testing"
)

// TestBundleVersionMatchesPin keeps bundle/versions.env and PinnedVersion in
// step, so the release bundle ships the Navidrome version the supervisor
// accepts.
func TestBundleVersionMatchesPin(t *testing.T) {
	f, err := os.Open("../../bundle/versions.env")
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	var got string
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		if v, ok := strings.CutPrefix(strings.TrimSpace(sc.Text()), "NAVIDROME_VERSION="); ok {
			got = strings.Trim(v, `"'`)
		}
	}
	if err := sc.Err(); err != nil {
		t.Fatal(err)
	}
	if got != PinnedVersion {
		t.Fatalf("bundle/versions.env NAVIDROME_VERSION = %q, navidrome.PinnedVersion = %q", got, PinnedVersion)
	}
}
