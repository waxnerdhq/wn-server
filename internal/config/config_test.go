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
	if c.Listen != DefaultListen || c.WNCloudURL != DefaultWNCloudURL {
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

// TestRenderNavidrome also loads the legacy navidrome_port key, which older
// config files carry and which is ignored.
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
		"Address = 'unix:" + filepath.Join(dir, "navidrome.sock") + "'",
		"UnixSocketPerm = '0600'",
		"MusicFolder = '" + c.SortedDir + "'",
		"DataFolder = '" + filepath.Join(dir, "navidrome") + "'",
		"[ExtAuth]",
		"UserHeader = 'Remote-User'",
		"TrustedSources = '@'",
		"EnableInsightsCollector = false",
		"EnableExternalServices = true",
	} {
		if !strings.Contains(string(got), want) {
			t.Errorf("navidrome.toml lacks %q:\n%s", want, got)
		}
	}
	if strings.Contains(string(got), "Port") {
		t.Errorf("navidrome.toml has a port:\n%s", got)
	}
}

func TestRenderNavidromeExternalServicesOff(t *testing.T) {
	dir := t.TempDir()
	music := t.TempDir()
	c, err := Load(writeConfig(t, dir, "music_folders = ['"+music+"']\nnavidrome_external_services = false\n"))
	if err != nil {
		t.Fatal(err)
	}
	got, err := c.RenderNavidrome()
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"EnableExternalServices = false", "EnableInsightsCollector = false"} {
		if !strings.Contains(string(got), want) {
			t.Errorf("navidrome.toml lacks %q:\n%s", want, got)
		}
	}
}

func TestInitOmitsExternalServicesKey(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "config.toml")
	if _, err := Init(p, []string{t.TempDir()}); err != nil {
		t.Fatal(err)
	}
	body, err := os.ReadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(body), "navidrome_external_services") || strings.Contains(string(body), "navidrome_port") {
		t.Fatalf("init wrote the key:\n%s", body)
	}
}

func TestLoadRejectsSortedDirOverlap(t *testing.T) {
	dir := t.TempDir()
	music := t.TempDir()
	// sorted_dir inside the music folder, through a symlink to it.
	link := filepath.Join(dir, "link")
	if err := os.Symlink(music, link); err != nil {
		t.Fatal(err)
	}
	inside := filepath.Join(link, "sorted")
	_, err := Load(writeConfig(t, dir, "music_folders = ['"+music+"']\nsorted_dir = '"+inside+"'\n"))
	if err == nil || !strings.Contains(err.Error(), "is inside music folder") || !strings.Contains(err.Error(), inside) || !strings.Contains(err.Error(), music) {
		t.Fatalf("got %v, want a sorted_dir inside error", err)
	}
	// sorted_dir that contains the music folder.
	sorted := t.TempDir()
	nested := filepath.Join(sorted, "music")
	if err := os.Mkdir(nested, 0o755); err != nil {
		t.Fatal(err)
	}
	_, err = Load(writeConfig(t, dir, "music_folders = ['"+nested+"']\nsorted_dir = '"+sorted+"'\n"))
	if err == nil || !strings.Contains(err.Error(), "contains music folder") || !strings.Contains(err.Error(), nested) {
		t.Fatalf("got %v, want a sorted_dir contains error", err)
	}
}

func TestRelayMaxBitrate(t *testing.T) {
	music := t.TempDir()
	c, err := Load(writeConfig(t, t.TempDir(), "music_folders = ['"+music+"']\n"))
	if err != nil {
		t.Fatal(err)
	}
	if c.RelayMaxBitrate != 0 {
		t.Errorf("default %d, want 0", c.RelayMaxBitrate)
	}
	c, err = Load(writeConfig(t, t.TempDir(), "music_folders = ['"+music+"']\nrelay_max_bitrate = 128\n"))
	if err != nil {
		t.Fatal(err)
	}
	if c.RelayMaxBitrate != 128 {
		t.Errorf("got %d, want 128", c.RelayMaxBitrate)
	}
	_, err = Load(writeConfig(t, t.TempDir(), "music_folders = ['"+music+"']\nrelay_max_bitrate = -1\n"))
	if err == nil || !strings.Contains(err.Error(), "relay_max_bitrate") {
		t.Errorf("negative value: err %v", err)
	}
}

func TestRelayKeys(t *testing.T) {
	music := t.TempDir()
	c, err := Load(writeConfig(t, t.TempDir(), "music_folders = ['"+music+"']\n"))
	if err != nil {
		t.Fatal(err)
	}
	if c.Relay || c.RelayEdgeURL != "" {
		t.Errorf("defaults relay=%v relay_edge_url=%q, want false and empty", c.Relay, c.RelayEdgeURL)
	}
	c, err = Load(writeConfig(t, t.TempDir(), "music_folders = ['"+music+"']\nrelay = true\nrelay_edge_url = 'https://edge.test:8443/v1/tunnel'\n"))
	if err != nil {
		t.Fatal(err)
	}
	if !c.Relay || c.RelayEdgeURL != "https://edge.test:8443/v1/tunnel" {
		t.Errorf("got relay=%v relay_edge_url=%q", c.Relay, c.RelayEdgeURL)
	}
	_, err = Load(writeConfig(t, t.TempDir(), "music_folders = ['"+music+"']\nrelay_edge_url = 'http://edge.test/v1/tunnel'\n"))
	if err == nil || !strings.Contains(err.Error(), "relay_edge_url") {
		t.Errorf("http URL: err %v", err)
	}
}
