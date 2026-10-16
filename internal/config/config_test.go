package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeConfig(t *testing.T, dir, body string) string {
	t.Helper()
	p := filepath.Join(dir, "config.toml")
	if err := os.WriteFile(p, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	return p
}

func TestLoadMissingMusicFolder(t *testing.T) {
	dir := t.TempDir()
	missing := filepath.Join(dir, "nope")
	p := writeConfig(t, dir, "music_folders = ['"+missing+"']\n")
	_, err := Load(p)
	if err == nil || !strings.Contains(err.Error(), "does not exist") {
		t.Fatalf("got %v, want a missing-folder error", err)
	}
}

func TestLoadDefaultsAndSortedDir(t *testing.T) {
	dir := t.TempDir()
	music := t.TempDir()
	c, err := Load(writeConfig(t, dir, "music_folders = ['"+music+"']\n"))
	if err != nil {
		t.Fatal(err)
	}
	if c.Listen != DefaultListen || c.NavidromePort != DefaultNavidromePort || c.WNCloudURL != DefaultWNCloudURL {
		t.Fatalf("defaults not applied: %+v", c)
	}
	if want := filepath.Join(dir, "sorted"); c.SortedDir != want {
		t.Fatalf("sorted_dir = %q, want %q", c.SortedDir, want)
	}
	if fi, err := os.Stat(c.SortedDir); err != nil || !fi.IsDir() {
		t.Fatalf("sorted dir not created: %v", err)
	}
}

func TestPaths(t *testing.T) {
	xdg := t.TempDir()
	t.Setenv("XDG_DATA_HOME", xdg)
	dir, p, err := Paths("")
	if err != nil {
		t.Fatal(err)
	}
	if dir != filepath.Join(xdg, "wn-server") || p != filepath.Join(dir, "config.toml") {
		t.Fatalf("got %q, %q", dir, p)
	}
	other := filepath.Join(t.TempDir(), "x.toml")
	dir, p, err = Paths(other)
	if err != nil {
		t.Fatal(err)
	}
	if dir != filepath.Dir(other) || p != other {
		t.Fatalf("got %q, %q", dir, p)
	}
}

func TestRenderNavidrome(t *testing.T) {
	dir := t.TempDir()
	music := t.TempDir()
	c, err := Load(writeConfig(t, dir, "music_folders = ['"+music+"', '"+t.TempDir()+"']\nnavidrome_port = 5000\n"))
	if err != nil {
		t.Fatal(err)
	}
	if err := c.WriteNavidrome(); err != nil {
		t.Fatal(err)
	}
	got, err := os.ReadFile(filepath.Join(dir, "navidrome.toml"))
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{
		"Address = '127.0.0.1'",
		"Port = 5000",
		"MusicFolder = '" + c.SortedDir + "'",
		"DataFolder = '" + filepath.Join(dir, "navidrome") + "'",
		"[ExtAuth]",
		"UserHeader = 'Remote-User'",
		"TrustedSources = '127.0.0.1/32'",
	} {
		if !strings.Contains(string(got), want) {
			t.Errorf("navidrome.toml lacks %q:\n%s", want, got)
		}
	}
}

func TestLoadRejectsListenPortEqualToNavidromePort(t *testing.T) {
	dir := t.TempDir()
	music := t.TempDir()
	_, err := Load(writeConfig(t, dir, "music_folders = ['"+music+"']\nlisten = '0.0.0.0:5000'\nnavidrome_port = 5000\n"))
	if err == nil || !strings.Contains(err.Error(), "navidrome_port") {
		t.Fatalf("got %v, want a port-clash error", err)
	}
}
