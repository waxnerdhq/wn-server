package beets

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	"gopkg.in/yaml.v3"

	"github.com/waxnerdhq/wn-server/internal/config"
)

// DefaultAcoustIDKey is waxnerd's AcoustID application key. It is set at
// build time with
// -ldflags "-X github.com/waxnerdhq/wn-server/internal/beets.DefaultAcoustIDKey=...".
// acoustid_api_key in config.toml overrides it.
var DefaultAcoustIDKey string

// File names in the data directory.
const (
	configFileName     = "beets.yaml"
	libraryFileName    = "beets.db"
	importLogFileName  = "beets-import.log"
	candidatesFileName = "beets-candidates.jsonl"
	stateFileName      = "beets-state.pickle"
	pluginDirName      = "beets-plugins"
	pluginFileName     = "wn_candidates.py"
)

// PathFormat is the beets path format for albums in the sorted directory.
const PathFormat = "$albumartist/$album%aunique{}/$track $title"

// Config holds the values of the generated beets config.
type Config struct {
	DataDir   string
	SortedDir string
	// PluginDir is the directory that holds wn_candidates.py. It defaults to
	// beets-plugins next to the beets executable.
	PluginDir string
	// AcoustIDKey is empty when neither the config nor the build sets a key.
	// The chroma plugin then looks up with its own key.
	AcoustIDKey string
}

// NewConfig builds the beets config from wn-server's config.
func NewConfig(c *config.Config) Config {
	key := c.AcoustIDAPIKey
	if key == "" {
		key = DefaultAcoustIDKey
	}
	return Config{
		DataDir:     c.DataDir,
		SortedDir:   c.SortedDir,
		PluginDir:   filepath.Join(binDir(c.BeetsBin), pluginDirName),
		AcoustIDKey: key,
	}
}

// binDir returns the directory of the executable bin. A bare name is looked
// up on PATH, and symlinks are resolved, so the plugin directory is found
// next to the real file.
func binDir(bin string) string {
	path := bin
	if !strings.ContainsRune(bin, filepath.Separator) {
		if p, err := exec.LookPath(bin); err == nil {
			path = p
		}
	}
	if abs, err := filepath.Abs(path); err == nil {
		path = abs
	}
	if real, err := filepath.EvalSymlinks(path); err == nil {
		path = real
	}
	return filepath.Dir(path)
}

// Path is <data dir>/beets.yaml.
func (c Config) Path() string { return filepath.Join(c.DataDir, configFileName) }

// LibraryPath is the beets database, <data dir>/beets.db.
func (c Config) LibraryPath() string { return filepath.Join(c.DataDir, libraryFileName) }

// ImportLogPath is beets' import log, <data dir>/beets-import.log.
func (c Config) ImportLogPath() string { return filepath.Join(c.DataDir, importLogFileName) }

// CandidatesPath is the wn_candidates output, <data dir>/beets-candidates.jsonl.
func (c Config) CandidatesPath() string { return filepath.Join(c.DataDir, candidatesFileName) }

// StatePath is beets' state file, which holds the incremental import history.
func (c Config) StatePath() string { return filepath.Join(c.DataDir, stateFileName) }

// The structs below mirror the beets options wn-server sets. Field order is
// the order in the file.
type beetsYAML struct {
	Library      string            `yaml:"library"`
	Directory    string            `yaml:"directory"`
	Statefile    string            `yaml:"statefile"`
	Paths        map[string]string `yaml:"paths"`
	Import       importYAML        `yaml:"import"`
	Plugins      []string          `yaml:"plugins"`
	Pluginpath   []string          `yaml:"pluginpath"`
	Chroma       chromaYAML        `yaml:"chroma"`
	WnCandidates wnCandidatesYAML  `yaml:"wn_candidates"`
	MusicBrainz  musicBrainzYAML   `yaml:"musicbrainz"`
	UI           uiYAML            `yaml:"ui"`
}

type uiYAML struct {
	Color bool `yaml:"color"`
}

type importYAML struct {
	Copy                 bool   `yaml:"copy"`
	Move                 bool   `yaml:"move"`
	Link                 bool   `yaml:"link"`
	Hardlink             bool   `yaml:"hardlink"`
	Reflink              bool   `yaml:"reflink"`
	Delete               bool   `yaml:"delete"`
	Write                bool   `yaml:"write"`
	Quiet                bool   `yaml:"quiet"`
	QuietFallback        string `yaml:"quiet_fallback"`
	DuplicateAction      string `yaml:"duplicate_action"`
	Autotag              bool   `yaml:"autotag"`
	Resume               bool   `yaml:"resume"`
	Incremental          bool   `yaml:"incremental"`
	IncrementalSkipLater bool   `yaml:"incremental_skip_later"`
	FixExtInplace        bool   `yaml:"fix_ext_inplace"`
	RemuxMP3InWAV        bool   `yaml:"remux_mp3_in_wav"`
	Log                  string `yaml:"log"`
}

type chromaYAML struct {
	Auto bool `yaml:"auto"`
}

type wnCandidatesYAML struct {
	Output string `yaml:"output"`
	// AcoustIDAppKey replaces the chroma plugin's built-in application key.
	// beets has no option for it: acoustid.apikey is the user key for
	// `beet submit`.
	AcoustIDAppKey string `yaml:"acoustid_app_key,omitempty"`
}

type musicBrainzYAML struct {
	Host      string `yaml:"host"`
	Ratelimit int    `yaml:"ratelimit"`
}

// Render returns the contents of beets.yaml.
//
// beets 2.14 loads MusicBrainz as a plugin. Its default plugin list is
// [musicbrainz], and a plugins key replaces that list, so musicbrainz is
// listed explicitly.
//
// The import options keep beets from writing to the original files. Copy is
// the only file operation. remux_mp3_in_wav is off, because beets otherwise
// writes an .mp3 next to an MP3-in-WAV original and deletes the WAV.
// fix_ext_inplace is off. The wn_candidates plugin also turns off
// fix_extension, which has no option and copies an extensionless original to
// <name>.<ext> next to it.
func (c Config) Render() ([]byte, error) {
	doc := beetsYAML{
		Library:   c.LibraryPath(),
		Directory: c.SortedDir,
		Statefile: c.StatePath(),
		Paths:     map[string]string{"default": PathFormat},
		Import: importYAML{
			Copy:                 true,
			Move:                 false,
			Link:                 false,
			Hardlink:             false,
			Reflink:              false,
			Delete:               false,
			Write:                true,
			Quiet:                true,
			QuietFallback:        "skip",
			DuplicateAction:      "skip",
			Autotag:              true,
			Resume:               false,
			Incremental:          true,
			IncrementalSkipLater: true,
			FixExtInplace:        false,
			RemuxMP3InWAV:        false,
			Log:                  c.ImportLogPath(),
		},
		Plugins:      []string{"musicbrainz", "chroma", "wn_candidates"},
		Pluginpath:   []string{c.PluginDir},
		Chroma:       chromaYAML{Auto: true},
		WnCandidates: wnCandidatesYAML{Output: c.CandidatesPath(), AcoustIDAppKey: c.AcoustIDKey},
		MusicBrainz:  musicBrainzYAML{Host: "musicbrainz.org", Ratelimit: 1},
		// beets colours its output even when it is not a terminal, and the
		// output goes to wn-server's log.
		UI: uiYAML{Color: false},
	}
	body, err := yaml.Marshal(doc)
	if err != nil {
		return nil, err
	}
	header := "# Generated by wn-server. Changes are overwritten.\n"
	return append([]byte(header), body...), nil
}

// Write writes beets.yaml with mode 0600. The file can hold the AcoustID key.
func (c Config) Write() error {
	body, err := c.Render()
	if err != nil {
		return err
	}
	if err := os.MkdirAll(c.DataDir, 0o700); err != nil {
		return fmt.Errorf("create data directory: %w", err)
	}
	path := c.Path()
	if err := os.WriteFile(path, body, 0o600); err != nil {
		return fmt.Errorf("write %s: %w", path, err)
	}
	// WriteFile keeps the mode of an existing file.
	if err := os.Chmod(path, 0o600); err != nil {
		return fmt.Errorf("chmod %s: %w", path, err)
	}
	return nil
}
