package cli

import (
	"bytes"
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
	run := func() error {
		root := newRootCmd()
		root.SetOut(&bytes.Buffer{})
		root.SetArgs([]string{"--config", cfg, "init", "--folder", music})
		return root.Execute()
	}
	if err := run(); err != nil {
		t.Fatal(err)
	}
	c, err := config.Load(cfg)
	if err != nil {
		t.Fatal(err)
	}
	if len(c.MusicFolders) != 1 || c.MusicFolders[0] != music {
		t.Fatalf("music_folders = %v", c.MusicFolders)
	}
	if err := run(); err == nil || !strings.Contains(err.Error(), "already exists") {
		t.Fatalf("second init: got %v, want an already-exists error", err)
	}
}
