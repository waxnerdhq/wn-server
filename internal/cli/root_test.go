package cli

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/waxnerdhq/wn-server/internal/config"
)

func TestVersionCommand(t *testing.T) {
	var out bytes.Buffer
	root := newRootCmd()
	root.SetOut(&out)
	root.SetArgs([]string{"version"})
	if err := root.Execute(); err != nil {
		t.Fatal(err)
	}
	if got, want := out.String(), "wn-server dev\n"; got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestInitWritesConfigAndRefusesSecondRun(t *testing.T) {
	data := t.TempDir()
	music := t.TempDir()
	cfg := filepath.Join(data, "wn", "config.toml")
	var out bytes.Buffer
	run := func(args ...string) error {
		out.Reset()
		root := newRootCmd()
		root.SetOut(&out)
		root.SetArgs(append([]string{"--config", cfg}, args...))
		return root.Execute()
	}
	if err := run("init", "--folder", music, "--name", "me"); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out.String(), "owner name: me\n") || !strings.Contains(out.String(), "app password: ") {
		t.Fatalf("init output %q lacks the owner name or password", out.String())
	}
	c, err := config.Load(cfg)
	if err != nil {
		t.Fatal(err)
	}
	if len(c.MusicFolders) != 1 || c.MusicFolders[0] != music {
		t.Fatalf("music_folders = %v", c.MusicFolders)
	}
	if err := run("init", "--folder", music); err == nil || !strings.Contains(err.Error(), "already") {
		t.Fatalf("second init: got %v, want an already-exists error", err)
	}

	if err := run("password"); err != nil {
		t.Fatal(err)
	}
	first := out.String()
	if err := run("password", "--reset"); err != nil {
		t.Fatal(err)
	}
	if out.String() == first || len(out.String()) != len(first) {
		t.Fatalf("password --reset printed %q after %q", out.String(), first)
	}

	// With the config gone but the owner present, init still refuses and
	// writes no config.
	if err := os.Remove(cfg); err != nil {
		t.Fatal(err)
	}
	if err := run("init", "--folder", music); err == nil || !strings.Contains(err.Error(), "owner account") {
		t.Fatalf("init with existing owner: got %v", err)
	}
	if _, err := os.Stat(cfg); err == nil {
		t.Fatal("init wrote config.toml although the owner exists")
	}
}
