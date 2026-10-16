package config

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/url"
	"os"
	"path/filepath"

	"github.com/pelletier/go-toml/v2"
)

// Defaults for config.toml keys.
const (
	DefaultListen        = "0.0.0.0:4533"
	DefaultWNCloudURL    = "https://waxnerd.com"
	DefaultNavidromePort = 4534

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
	NavidromePort  int      `toml:"navidrome_port,omitempty"`
	SortedDir      string   `toml:"sorted_dir,omitempty"`
	AcoustIDAPIKey string   `toml:"acoustid_api_key,omitempty"`

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

func (c *Config) applyDefaults() error {
	if c.Listen == "" {
		c.Listen = DefaultListen
	}
	if c.WNCloudURL == "" {
		c.WNCloudURL = DefaultWNCloudURL
	}
	if c.NavidromePort == 0 {
		c.NavidromePort = DefaultNavidromePort
	}
	if c.SortedDir == "" {
		c.SortedDir = filepath.Join(c.DataDir, "sorted")
	}
	if c.NavidromeBin == "" || c.BeetsBin == "" || c.FpcalcBin == "" {
		exe, err := os.Executable()
		if err != nil {
			return fmt.Errorf("find wn-server executable: %w", err)
		}
		dir := filepath.Dir(exe)
		if c.NavidromeBin == "" {
			c.NavidromeBin = filepath.Join(dir, "navidrome")
		}
		if c.BeetsBin == "" {
			c.BeetsBin = filepath.Join(dir, "beets")
		}
		if c.FpcalcBin == "" {
			c.FpcalcBin = filepath.Join(dir, "fpcalc")
		}
	}
	return nil
}

func (c *Config) validate() error {
	if err := ValidateMusicFolders(c.MusicFolders); err != nil {
		return err
	}
	if c.NavidromePort < 1 || c.NavidromePort > 65535 {
		return fmt.Errorf("navidrome_port %d is out of range", c.NavidromePort)
	}
	if u, err := url.Parse(c.WNCloudURL); err != nil || u.Scheme == "" || u.Host == "" {
		return fmt.Errorf("wn_cloud_url %q is not a URL", c.WNCloudURL)
	}
	if !filepath.IsAbs(c.SortedDir) {
		return fmt.Errorf("sorted_dir %q must be an absolute path", c.SortedDir)
	}
	return nil
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
		MusicFolders:  abs,
		Listen:        DefaultListen,
		WNCloudURL:    DefaultWNCloudURL,
		NavidromePort: DefaultNavidromePort,
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
	return Load(configPath)
}
