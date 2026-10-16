package beets

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/waxnerdhq/wn-server/internal/config"
)

// Decisions in the candidates file.
const (
	DecisionApplied   = "applied"
	DecisionSkipped   = "skipped"
	DecisionDuplicate = "duplicate"
	DecisionError     = "error"
)

// markerName is the file that makes Navidrome's scanner skip a folder. An
// empty file ignores everything in the folder and below it.
const markerName = ".ndignore"

// killWait is how long the runner waits for output after it kills beets.
const killWait = 5 * time.Second

// Tags are an album's existing tags before the import.
type Tags struct {
	Artist     string `json:"artist"`
	Album      string `json:"album"`
	Year       int    `json:"year"`
	TrackCount int    `json:"track_count"`
}

// Candidate is one MusicBrainz release that beets considered.
type Candidate struct {
	ReleaseID string  `json:"release_id"`
	Artist    string  `json:"artist"`
	Album     string  `json:"album"`
	Year      int     `json:"year"`
	Label     string  `json:"label"`
	Distance  float64 `json:"distance"`
}

// Album is one line of the candidates file.
type Album struct {
	Folder     string      `json:"folder"`
	Tags       Tags        `json:"tags"`
	Decision   string      `json:"decision"`
	Candidates []Candidate `json:"candidates"`
	// CopiedTo is the album directory in sorted_dir. Only on applied lines.
	CopiedTo string `json:"copied_to,omitempty"`
	// Error is set on error lines.
	Error string `json:"error,omitempty"`
}

// Summary is the result of one import run.
type Summary struct {
	Applied   int
	Skipped   int
	Duplicate int
	// Errors counts error lines and malformed lines.
	Errors int
	// Malformed counts lines of the candidates file that did not parse.
	Malformed int
	Albums    []Album
}

// Scanner starts a Navidrome library scan.
type Scanner interface {
	StartScan(ctx context.Context) error
}

// Runner runs beet import over music folders.
type Runner struct {
	Config    Config
	BeetsBin  string
	FpcalcBin string
	// Scanner is told to scan after albums were applied. Nil skips the scan.
	Scanner Scanner
	Log     *slog.Logger
}

// NewRunner returns a runner for the beets described by c. A nil logger
// selects slog.Default.
func NewRunner(c *config.Config, scanner Scanner, logger *slog.Logger) *Runner {
	if logger == nil {
		logger = slog.Default()
	}
	return &Runner{
		Config:    NewConfig(c),
		BeetsBin:  c.BeetsBin,
		FpcalcBin: c.FpcalcBin,
		Scanner:   scanner,
		Log:       logger,
	}
}

// Import writes beets.yaml, runs `beet import -q` over folders, reads the
// candidates file, writes .ndignore into the original folder of every
// applied album, and starts a Navidrome scan when an album was applied.
//
// A failed scan is logged as a warning and does not fail the import. When
// beets exits non-zero, the summary of what it did write is returned with
// the error.
func (r *Runner) Import(ctx context.Context, folders []string) (Summary, error) {
	if len(folders) == 0 {
		return Summary{}, errors.New("no folders to import")
	}
	roots := make([]string, len(folders))
	for i, f := range folders {
		abs, err := filepath.Abs(f)
		if err != nil {
			return Summary{}, err
		}
		roots[i] = filepath.Clean(abs)
	}
	// beets only warns about a missing plugin and imports anyway, which
	// would leave the candidates file empty.
	plugin := filepath.Join(r.Config.PluginDir, pluginFileName)
	if _, err := os.Stat(plugin); err != nil {
		return Summary{}, fmt.Errorf("beets plugin %s not found; it ships in beets-plugins next to the beets executable: %w", plugin, err)
	}
	if err := r.Config.Write(); err != nil {
		return Summary{}, err
	}
	if err := os.WriteFile(r.Config.CandidatesPath(), nil, 0o600); err != nil {
		return Summary{}, fmt.Errorf("truncate candidates file: %w", err)
	}

	runErr := r.run(ctx, roots)
	if ctx.Err() != nil {
		return Summary{}, ctx.Err()
	}

	sum, err := r.readCandidates()
	if err != nil {
		return sum, errors.Join(runErr, err)
	}
	for _, a := range sum.Albums {
		if a.Decision == DecisionApplied {
			r.writeMarker(a.Folder, roots)
		}
	}
	if sum.Applied > 0 && r.Scanner != nil {
		if err := r.Scanner.StartScan(ctx); err != nil {
			r.Log.Warn("could not start a Navidrome scan; it runs at the next scheduled scan or `wn-server serve` start", "error", err)
		}
	}
	return sum, runErr
}

// run starts beets and waits for it.
func (r *Runner) run(ctx context.Context, folders []string) error {
	args := append([]string{"--config", r.Config.Path(), "import", "-q"}, folders...)
	cmd := exec.CommandContext(ctx, r.BeetsBin, args...)
	cmd.Env = r.env()
	stdout := &lineWriter{log: r.Log}
	stderr := &lineWriter{log: r.Log}
	cmd.Stdout, cmd.Stderr = stdout, stderr
	// beets leads its own process group, so a cancel also stops fpcalc.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error { return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL) }
	cmd.WaitDelay = killWait
	r.Log.Info("running beets import", "bin", r.BeetsBin, "folders", len(folders))
	err := cmd.Run()
	stdout.flush()
	stderr.flush()
	if err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			if last := stderr.lastLine(); last != "" {
				return fmt.Errorf("beets import: %w: %s", err, last)
			}
		}
		return fmt.Errorf("beets import: %w", err)
	}
	return nil
}

// env is the environment of the beets process. The directory of fpcalc_bin
// goes first on PATH, because the chroma plugin runs fpcalc by name; FPCALC
// names the binary for pyacoustid as well. BEETSDIR is the data directory,
// so beets does not read a personal ~/.config/beets/config.yaml.
func (r *Runner) env() []string {
	fpcalc := r.FpcalcBin
	if abs, err := filepath.Abs(fpcalc); err == nil && strings.ContainsRune(fpcalc, filepath.Separator) {
		fpcalc = abs
	}
	var env []string
	path := ""
	for _, kv := range os.Environ() {
		switch {
		case strings.HasPrefix(kv, "PATH="):
			path = strings.TrimPrefix(kv, "PATH=")
		case strings.HasPrefix(kv, "FPCALC="), strings.HasPrefix(kv, "BEETSDIR="), strings.HasPrefix(kv, "PYTHONDONTWRITEBYTECODE="):
		default:
			env = append(env, kv)
		}
	}
	if strings.ContainsRune(fpcalc, filepath.Separator) {
		dir := filepath.Dir(fpcalc)
		if path == "" {
			path = dir
		} else {
			path = dir + string(os.PathListSeparator) + path
		}
		env = append(env, "FPCALC="+fpcalc)
	}
	// PYTHONDONTWRITEBYTECODE keeps Python from writing __pycache__ into
	// beets-plugins, which sits in the install directory.
	return append(env, "PATH="+path, "BEETSDIR="+r.Config.DataDir, "PYTHONDONTWRITEBYTECODE=1")
}

func (r *Runner) readCandidates() (Summary, error) {
	f, err := os.Open(r.Config.CandidatesPath())
	if err != nil {
		return Summary{}, fmt.Errorf("read candidates file: %w", err)
	}
	defer f.Close()
	albums, malformed, err := parseCandidates(f)
	if malformed > 0 {
		r.Log.Warn("skipped malformed lines in the candidates file", "count", malformed, "file", r.Config.CandidatesPath())
	}
	sum := summarize(albums, malformed)
	if err != nil {
		return sum, fmt.Errorf("read candidates file: %w", err)
	}
	return sum, nil
}

func summarize(albums []Album, malformed int) Summary {
	sum := Summary{Albums: albums, Malformed: malformed, Errors: malformed}
	for _, a := range albums {
		switch a.Decision {
		case DecisionApplied:
			sum.Applied++
		case DecisionSkipped:
			sum.Skipped++
		case DecisionDuplicate:
			sum.Duplicate++
		case DecisionError:
			sum.Errors++
		}
	}
	return sum
}

// ParseCandidates reads the JSON lines that the wn_candidates plugin writes.
// Blank lines are ignored. Lines that are not a JSON object with a known
// decision are skipped.
func ParseCandidates(r io.Reader) ([]Album, error) {
	albums, _, err := parseCandidates(r)
	return albums, err
}

// parseCandidates is ParseCandidates that also counts the skipped lines.
func parseCandidates(r io.Reader) ([]Album, int, error) {
	sc := bufio.NewScanner(r)
	sc.Buffer(make([]byte, 64<<10), 4<<20)
	var albums []Album
	malformed := 0
	for sc.Scan() {
		line := bytes.TrimSpace(sc.Bytes())
		if len(line) == 0 {
			continue
		}
		var a Album
		if err := json.Unmarshal(line, &a); err != nil {
			malformed++
			continue
		}
		switch a.Decision {
		case DecisionApplied, DecisionSkipped, DecisionDuplicate, DecisionError:
			albums = append(albums, a)
		default:
			malformed++
		}
	}
	return albums, malformed, sc.Err()
}

// writeMarker writes an empty .ndignore into folder, the original album
// folder of an applied album. It never overwrites a file, and it refuses a
// folder that is not strictly inside one of the import roots: a marker in a
// root would hide the whole library from Navidrome.
func (r *Runner) writeMarker(folder string, roots []string) {
	if !filepath.IsAbs(folder) {
		r.Log.Warn("not writing .ndignore: folder is not absolute", "folder", folder)
		return
	}
	folder = filepath.Clean(folder)
	if !insideAny(folder, roots) {
		r.Log.Warn("not writing .ndignore: folder is a music folder itself or outside the music folders", "folder", folder)
		return
	}
	path := filepath.Join(folder, markerName)
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o644)
	if err != nil {
		if !errors.Is(err, fs.ErrExist) {
			r.Log.Warn("could not write .ndignore", "path", path, "error", err)
		}
		return
	}
	if err := f.Close(); err != nil {
		r.Log.Warn("could not write .ndignore", "path", path, "error", err)
	}
}

// insideAny reports whether path is strictly below one of roots.
func insideAny(path string, roots []string) bool {
	for _, root := range roots {
		rel, err := filepath.Rel(root, path)
		if err != nil || rel == "." || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
			continue
		}
		return true
	}
	return false
}

// lineWriter logs each line of beets output at debug level and keeps the
// last line for error messages.
type lineWriter struct {
	log  *slog.Logger
	mu   sync.Mutex
	buf  []byte
	last string
}

func (w *lineWriter) Write(p []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	w.buf = append(w.buf, p...)
	for {
		i := bytes.IndexByte(w.buf, '\n')
		if i < 0 {
			break
		}
		w.emit(w.buf[:i])
		w.buf = w.buf[i+1:]
	}
	return len(p), nil
}

func (w *lineWriter) flush() {
	w.mu.Lock()
	defer w.mu.Unlock()
	w.emit(w.buf)
	w.buf = nil
}

func (w *lineWriter) emit(line []byte) {
	line = bytes.TrimRight(line, "\r")
	if len(bytes.TrimSpace(line)) == 0 {
		return
	}
	w.last = string(line)
	w.log.Debug("beets: " + w.last)
}

func (w *lineWriter) lastLine() string {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.last
}
