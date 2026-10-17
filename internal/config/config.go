package config

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/pelletier/go-toml/v2"
)

// Defaults for config.toml keys.
const (
	DefaultListen     = "0.0.0.0:4533"
	DefaultWNCloudURL = "https://waxnerd.com"

	configFileName = "config.toml"
	dataDirName    = "wn-server"
)

// Config is the parsed config.toml with defaults applied.
type Config struct {
	MusicFolders   []string `toml:"music_folders"`
	Listen         string   `toml:"listen,omitempty"`
	WNCloudURL     string   `toml:"wn_cloud_url,omitempty"`
	NavidromeBin   string   `toml:"navidrome_bin,omitempty"`
	BeetsBin       string   `toml:"beets_bin,omitempty"`
	FpcalcBin      string   `toml:"fpcalc_bin,omitempty"`
	SortedDir      string   `toml:"sorted_dir,omitempty"`
	AcoustIDAPIKey string   `toml:"acoustid_api_key,omitempty"`

	// LegacyNavidromePort holds navidrome_port, which init wrote before
	// Navidrome moved to a Unix socket. It is read so that older config
	// files still load, and it is ignored.
	LegacyNavidromePort int `toml:"navidrome_port,omitempty"`

	// NavidromeExternalServices sets Navidrome's EnableExternalServices.
	// Nil means true. Read it through ExternalServices.
	NavidromeExternalServices *bool `toml:"navidrome_external_services,omitempty"`

	// DataDir is the directory that holds config.toml. It is not a config key.
	DataDir string `toml:"-"`
}

// Paths resolves the data directory and config file path. configFlag is the
// value of --config; when it is empty the data directory is
// $XDG_DATA_HOME/wn-server, or ~/.local/share/wn-server, and the config file
// is <data dir>/config.toml. When it is set, the data directory is the
// directory that contains the file.
func Paths(configFlag string) (dataDir, configPath string, err error) {
	if configFlag != "" {
		configPath, err = filepath.Abs(configFlag)
		if err != nil {
			return "", "", err
		}
		return filepath.Dir(configPath), configPath, nil
	}
	base := os.Getenv("XDG_DATA_HOME")
	if base == "" || !filepath.IsAbs(base) {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", "", fmt.Errorf("find home directory: %w", err)
		}
		base = filepath.Join(home, ".local", "share")
	}
	dataDir = filepath.Join(base, dataDirName)
	return dataDir, filepath.Join(dataDir, configFileName), nil
}

// Load reads the config file at path, applies defaults, validates it, and
// creates the sorted directory if it is absent.
func Load(path string) (*Config, error) {
	path, err := filepath.Abs(path)
	if err != nil {
		return nil, err
	}
	data, err := os.ReadFile(path)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return nil, fmt.Errorf("config file %s not found; run `wn-server init --folder <music folder>`", path)
		}
		return nil, fmt.Errorf("read config: %w", err)
	}
	var c Config
	dec := toml.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&c); err != nil {
		var strict *toml.StrictMissingError
		if errors.As(err, &strict) {
			return nil, fmt.Errorf("parse %s: %s", path, strict.String())
		}
		return nil, fmt.Errorf("parse %s: %w", path, err)
	}
	c.DataDir = filepath.Dir(path)
	if err := c.applyDefaults(); err != nil {
		return nil, err
	}
	if err := c.validate(); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	if err := os.MkdirAll(c.SortedDir, 0o755); err != nil {
		return nil, fmt.Errorf("create sorted_dir: %w", err)
	}
	return &c, nil
}

// ExternalServices reports whether Navidrome may call external services. It is
// true when navidrome_external_services is absent.
func (c *Config) ExternalServices() bool {
	return c.NavidromeExternalServices == nil || *c.NavidromeExternalServices
}

func (c *Config) applyDefaults() error {
	if c.Listen == "" {
		c.Listen = DefaultListen
	}
	if c.WNCloudURL == "" {
		c.WNCloudURL = DefaultWNCloudURL
	}
	if c.SortedDir == "" {
		c.SortedDir = filepath.Join(c.DataDir, "sorted")
	}
	// Bundled binaries sit next to the wn-server executable. When the
	// executable path is unknown, the bare names resolve through PATH.
	dir := ""
	if exe, err := os.Executable(); err == nil {
		dir = filepath.Dir(exe)
	}
	if c.NavidromeBin == "" {
		c.NavidromeBin = defaultBin(dir, "navidrome")
	}
	if c.BeetsBin == "" {
		c.BeetsBin = defaultBin(dir, "beets")
	}
	if c.FpcalcBin == "" {
		c.FpcalcBin = defaultBin(dir, "fpcalc")
	}
	return nil
}

func defaultBin(dir, name string) string {
	if dir == "" {
		return name
	}
	return filepath.Join(dir, name)
}

func (c *Config) validate() error {
	if err := ValidateMusicFolders(c.MusicFolders); err != nil {
		return err
	}
	_, portStr, err := net.SplitHostPort(c.Listen)
	if err != nil {
		return fmt.Errorf("listen %q is not a host:port address: %w", c.Listen, err)
	}
	port, err := strconv.Atoi(portStr)
	if err != nil || port < 1 || port > 65535 {
		return fmt.Errorf("listen %q has an invalid port", c.Listen)
	}
	if u, err := url.Parse(c.WNCloudURL); err != nil || u.Scheme == "" || u.Host == "" {
		return fmt.Errorf("wn_cloud_url %q is not a URL", c.WNCloudURL)
	}
	if !filepath.IsAbs(c.SortedDir) {
		return fmt.Errorf("sorted_dir %q must be an absolute path", c.SortedDir)
	}
	// beets writes into sorted_dir and never into the music folders. The two
	// must not overlap.
	sorted := resolvePath(c.SortedDir)
	for _, f := range c.MusicFolders {
		music := resolvePath(f)
		if within(sorted, music) {
			return fmt.Errorf("sorted_dir %q is inside music folder %q", c.SortedDir, f)
		}
		if within(music, sorted) {
			return fmt.Errorf("sorted_dir %q contains music folder %q", c.SortedDir, f)
		}
	}
	return nil
}

// resolvePath returns the cleaned absolute path with symlinks resolved. A
// path that does not exist yet is resolved through its nearest existing
// parent.
func resolvePath(path string) string {
	path = filepath.Clean(path)
	if real, err := filepath.EvalSymlinks(path); err == nil {
		return real
	}
	parent := filepath.Dir(path)
	if parent == path {
		return path
	}
	return filepath.Join(resolvePath(parent), filepath.Base(path))
}

// within reports whether path is dir or below it.
func within(path, dir string) bool {
	rel, err := filepath.Rel(dir, path)
	return err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator))
}

// ValidateMusicFolders checks that there is at least one folder and that each
// is an absolute path to a readable directory.
func ValidateMusicFolders(folders []string) error {
	if len(folders) == 0 {
		return errors.New("music_folders needs at least one folder")
	}
	for _, f := range folders {
		if !filepath.IsAbs(f) {
			return fmt.Errorf("music folder %q must be an absolute path", f)
		}
		info, err := os.Stat(f)
		if err != nil {
			if errors.Is(err, fs.ErrNotExist) {
				return fmt.Errorf("music folder %q does not exist", f)
			}
			return fmt.Errorf("music folder %q is not accessible: %w", f, err)
		}
		if !info.IsDir() {
			return fmt.Errorf("music folder %q is not a directory", f)
		}
		d, err := os.Open(f)
		if err != nil {
			return fmt.Errorf("music folder %q is not readable: %w", f, err)
		}
		_, err = d.ReadDir(1)
		d.Close()
		if err != nil && !errors.Is(err, io.EOF) {
			return fmt.Errorf("music folder %q is not readable: %w", f, err)
		}
	}
	return nil
}

// Init creates the data directory and writes a first config file at
// configPath with the given music folders. It does not overwrite an existing
// file. The returned config is the loaded result, with the sorted directory
// created.
func Init(configPath string, folders []string) (*Config, error) {
	configPath, err := filepath.Abs(configPath)
	if err != nil {
		return nil, err
	}
	abs := make([]string, len(folders))
	for i, f := range folders {
		if abs[i], err = filepath.Abs(f); err != nil {
			return nil, err
		}
	}
	if err := ValidateMusicFolders(abs); err != nil {
		return nil, err
	}
	first := Config{
		MusicFolders: abs,
		Listen:       DefaultListen,
		WNCloudURL:   DefaultWNCloudURL,
	}
	// Validate the in-memory config before writing, so a bad config does not
	// leave a config.toml behind.
	check := first
	check.DataDir = filepath.Dir(configPath)
	if err := check.applyDefaults(); err != nil {
		return nil, err
	}
	if err := check.validate(); err != nil {
		return nil, err
	}
	body, err := toml.Marshal(first)
	if err != nil {
		return nil, err
	}
	if err := os.MkdirAll(filepath.Dir(configPath), 0o700); err != nil {
		return nil, fmt.Errorf("create data directory: %w", err)
	}
	f, err := os.OpenFile(configPath, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		if errors.Is(err, fs.ErrExist) {
			return nil, fmt.Errorf("%s already exists; edit it instead of running init again", configPath)
		}
		return nil, fmt.Errorf("write config: %w", err)
	}
	_, werr := f.Write(body)
	if cerr := f.Close(); werr == nil {
		werr = cerr
	}
	if werr != nil {
		return nil, fmt.Errorf("write config: %w", werr)
	}
	c, err := Load(configPath)
	if err != nil {
		os.Remove(configPath)
		return nil, err
	}
	return c, nil
}
