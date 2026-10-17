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
	"regexp"
	"slices"
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

// ReasonRequestedRelease is the reason on the applied line of a `match
// apply` run, where the plugin applied the release that the owner chose.
const ReasonRequestedRelease = "requested_release_id"

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
	// ReleaseID is the MusicBrainz release that beets applied. Only on
	// applied lines.
	ReleaseID string `json:"release_id,omitempty"`
	// Reason is ReasonRequestedRelease on the applied line of an apply run.
	Reason string `json:"reason,omitempty"`
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
	// MarkersWritten holds the Folder of every applied album whose .ndignore
	// this run created. A marker that already existed is not listed.
	MarkersWritten []string
}

// Scanner starts a Navidrome library scan.
type Scanner interface {
	StartScan(ctx context.Context) error
}

// lockFileName is the lock file in the data directory. It serialises the
// beets runs, which share beets.yaml, the candidates file, and beets.db.
const lockFileName = "beets.lock"

// ErrBusy is returned when another beets run holds the lock.
var ErrBusy = errors.New("another beets run is in progress")

// lock takes an exclusive flock on <data dir>/beets.lock without waiting.
// It returns ErrBusy when another process holds it. The lock ends when the
// returned function closes the file or the process exits.
func (c Config) lock() (func(), error) {
	if err := os.MkdirAll(c.DataDir, 0o700); err != nil {
		return nil, fmt.Errorf("create data directory: %w", err)
	}
	f, err := os.OpenFile(filepath.Join(c.DataDir, lockFileName), os.O_RDWR|os.O_CREATE, 0o600)
	if err != nil {
		return nil, fmt.Errorf("open beets lock: %w", err)
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		f.Close()
		if errors.Is(err, syscall.EWOULDBLOCK) {
			return nil, ErrBusy
		}
		return nil, fmt.Errorf("lock %s: %w", f.Name(), err)
	}
	return func() { f.Close() }, nil
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
// the error. When ctx is cancelled, the candidates file is still read and the
// markers are written, because beets has already recorded the applied albums
// in its incremental history and will not offer them again. The summary is
// then returned with ctx.Err() and no scan is started.
func (r *Runner) Import(ctx context.Context, folders []string) (Summary, error) {
	if len(folders) == 0 {
		return Summary{}, errors.New("no folders to import")
	}
	roots, err := absPaths(folders)
	if err != nil {
		return Summary{}, err
	}
	unlock, err := r.Config.lock()
	if err != nil {
		return Summary{}, err
	}
	defer unlock()
	return r.importRun(ctx, r.Config, nil, roots, roots)
}

// Apply imports the album in folder with the MusicBrainz release releaseID.
// It runs `beet import -q -I -S <releaseID> <folder>` with apply_release_id
// and apply_folder set in beets.yaml. The wn_candidates plugin then applies
// that release to the album in folder, also when beets' quiet mode would
// skip it, and skips every other album below folder. -I turns off the
// incremental history check, so a folder that beets already recorded is
// imported again. musicFolders are the import roots for the .ndignore
// checks. The rest is as for Import. After the run, beets.yaml is written
// again without the apply options.
func (r *Runner) Apply(ctx context.Context, folder, releaseID string, musicFolders []string) (Summary, error) {
	if !ValidReleaseID(releaseID) {
		return Summary{}, fmt.Errorf("release id %q is not a MusicBrainz id (a UUID)", releaseID)
	}
	if !filepath.IsAbs(folder) {
		return Summary{}, fmt.Errorf("folder %s is not absolute", folder)
	}
	info, err := os.Stat(folder)
	if err != nil {
		return Summary{}, fmt.Errorf("album folder: %w", err)
	}
	if !info.IsDir() {
		return Summary{}, fmt.Errorf("album folder %s is not a directory", folder)
	}
	roots, err := absPaths(musicFolders)
	if err != nil {
		return Summary{}, err
	}
	unlock, err := r.Config.lock()
	if err != nil {
		return Summary{}, err
	}
	// Deferred calls run in reverse order: beets.yaml is rewritten before
	// the lock is released.
	defer unlock()
	cfg := r.Config
	cfg.ApplyReleaseID, cfg.ApplyFolder = releaseID, folder
	defer func() {
		if err := r.Config.Write(); err != nil {
			r.Log.Warn("could not write beets.yaml without the apply options", "error", err)
		}
	}()
	return r.importRun(ctx, cfg, []string{"-I", "-S", releaseID}, []string{folder}, roots)
}

// releaseIDRe matches a MusicBrainz id, which is a UUID.
var releaseIDRe = regexp.MustCompile(`^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$`)

// ValidReleaseID reports whether id has the form of a MusicBrainz id.
func ValidReleaseID(id string) bool { return releaseIDRe.MatchString(id) }

// absPaths returns the paths made absolute and cleaned.
func absPaths(paths []string) ([]string, error) {
	out := make([]string, len(paths))
	for i, p := range paths {
		abs, err := filepath.Abs(p)
		if err != nil {
			return nil, err
		}
		out[i] = filepath.Clean(abs)
	}
	return out, nil
}

// importRun writes cfg to beets.yaml, empties the candidates file, runs
// `beet import -q <flags> <paths>`, reads the candidates file, writes the
// markers below roots, and starts a scan when an album was applied. The
// caller holds the lock.
func (r *Runner) importRun(ctx context.Context, cfg Config, flags, paths, roots []string) (Summary, error) {
	// beets only warns about a missing plugin and imports anyway, which
	// would leave the candidates file empty.
	plugin := filepath.Join(cfg.PluginDir, pluginFileName)
	if _, err := os.Stat(plugin); err != nil {
		return Summary{}, fmt.Errorf("beets plugin %s not found; it ships in beets-plugins next to the beets executable: %w", plugin, err)
	}
	if err := cfg.Write(); err != nil {
		return Summary{}, err
	}
	if err := os.WriteFile(cfg.CandidatesPath(), nil, 0o600); err != nil {
		return Summary{}, fmt.Errorf("truncate candidates file: %w", err)
	}

	args := append(append([]string{"import", "-q"}, flags...), paths...)
	r.Log.Info("running beets import", "bin", r.BeetsBin, "folders", len(paths))
	runErr := r.run(ctx, args...)
	sum, err := r.readCandidates()
	sum.MarkersWritten = r.writeMarkers(sum.Albums, roots)
	if ctx.Err() != nil {
		return sum, ctx.Err()
	}
	if err != nil {
		return sum, errors.Join(runErr, err)
	}
	if sum.Applied > 0 {
		r.Scan(ctx)
	}
	return sum, runErr
}

// Scan asks Navidrome to scan. A failure is logged as a warning. A nil
// Scanner does nothing.
func (r *Runner) Scan(ctx context.Context) {
	if r.Scanner == nil {
		return
	}
	if err := r.Scanner.StartScan(ctx); err != nil {
		r.Log.Warn("could not start a Navidrome scan; it runs at the next scheduled scan or `wn-server serve` start", "error", err)
	}
}

// Remove removes the album in copiedTo, an album folder in sorted_dir, from
// the beets library and deletes its files with
// `beet remove -a -d -f path:<copiedTo>`. beets also removes the folders
// that become empty. Remove refuses a copiedTo that does not resolve to a
// path strictly inside sorted_dir. It also refuses a copiedTo that ends in
// "," or contains `\:`, because beets reads a trailing "," in a query term
// as an OR separator and `\:` as an escaped colon. When beets fails and
// copiedTo no longer exists, Remove logs a warning and returns nil.
func (r *Runner) Remove(ctx context.Context, copiedTo string) error {
	if !filepath.IsAbs(copiedTo) {
		return fmt.Errorf("copied_to %q is not absolute", copiedTo)
	}
	if strings.HasSuffix(copiedTo, ",") || strings.Contains(copiedTo, `\:`) {
		return fmt.Errorf("refusing to remove %s: beets would not read it as one path query", copiedTo)
	}
	sorted := resolveExisting(r.Config.SortedDir)
	if target := resolveExisting(copiedTo); !inside(target, sorted) {
		return fmt.Errorf("refusing to remove %s: it is not inside sorted_dir %s", copiedTo, r.Config.SortedDir)
	}
	unlock, err := r.Config.lock()
	if err != nil {
		return err
	}
	defer unlock()
	if err := r.Config.Write(); err != nil {
		return err
	}
	r.Log.Info("running beets remove", "bin", r.BeetsBin, "path", copiedTo)
	err = r.run(ctx, "remove", "-a", "-d", "-f", "path:"+copiedTo)
	_, statErr := os.Lstat(copiedTo)
	if err != nil {
		if errors.Is(statErr, fs.ErrNotExist) {
			r.Log.Warn("beets remove failed and the album folder no longer exists; continuing", "path", copiedTo, "error", err)
			return nil
		}
		return err
	}
	if statErr == nil {
		r.Log.Warn("the album folder in sorted_dir still exists after beets remove; it holds files that beets did not import", "path", copiedTo)
	}
	return nil
}

// RemoveMarker deletes folder/.ndignore when it is an empty regular file and
// folder is strictly inside one of musicFolders. It leaves a marker with
// content in place, because wn-server writes only empty markers. It reports
// whether it removed the file.
func (r *Runner) RemoveMarker(folder string, musicFolders []string) (bool, error) {
	if !filepath.IsAbs(folder) {
		return false, fmt.Errorf("folder %q is not absolute", folder)
	}
	roots, err := absPaths(musicFolders)
	if err != nil {
		return false, err
	}
	for i, root := range roots {
		roots[i] = resolvePath(root)
	}
	folder = resolvePath(folder)
	if !insideAny(folder, roots) {
		r.Log.Warn("not removing .ndignore: folder is a music folder itself or outside the music folders", "folder", folder)
		return false, nil
	}
	path := filepath.Join(folder, markerName)
	info, err := os.Lstat(path)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	if !info.Mode().IsRegular() || info.Size() != 0 {
		r.Log.Warn("not removing .ndignore: it is not an empty file", "path", path)
		return false, nil
	}
	if err := os.Remove(path); err != nil {
		return false, err
	}
	return true, nil
}

// resolveExisting resolves symlinks in the longest existing prefix of path
// and appends the rest. It returns path cleaned when no prefix resolves.
func resolveExisting(path string) string {
	path = filepath.Clean(path)
	var rest []string
	for p := path; ; p = filepath.Dir(p) {
		if real, err := filepath.EvalSymlinks(p); err == nil {
			return filepath.Join(append([]string{real}, rest...)...)
		}
		if filepath.Dir(p) == p {
			return path
		}
		rest = append([]string{filepath.Base(p)}, rest...)
	}
}

// run starts `beet --config <beets.yaml> <args>` and waits for it.
func (r *Runner) run(ctx context.Context, args ...string) error {
	cmd := exec.CommandContext(ctx, r.BeetsBin, append([]string{"--config", r.Config.Path()}, args...)...)
	cmd.Env = r.env()
	stdout := &lineWriter{log: r.Log, level: slog.LevelDebug}
	stderr := &lineWriter{log: r.Log, level: slog.LevelInfo}
	cmd.Stdout, cmd.Stderr = stdout, stderr
	// beets leads its own process group, so a cancel also stops fpcalc.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error { return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL) }
	cmd.WaitDelay = killWait
	err := cmd.Run()
	stdout.flush()
	stderr.flush()
	if err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			if last := stderr.lastLine(); last != "" {
				return fmt.Errorf("beets %s: %w: %s", args[0], err, last)
			}
		}
		return fmt.Errorf("beets %s: %w", args[0], err)
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

// writeMarkers writes .ndignore into the original folder of every applied
// album. An empty .ndignore hides its folder and everything below it from
// Navidrome. So an applied folder gets no marker when another album of the
// same run that was not applied has its folder strictly below it. It returns
// the Folder of every album whose marker it created.
func (r *Runner) writeMarkers(albums []Album, roots []string) []string {
	resolved := resolveAll(roots)
	notApplied := notAppliedFolders(albums)
	var written []string
	for _, a := range albums {
		if a.Decision == DecisionApplied && r.markFolder(a.Folder, notApplied, resolved) {
			written = append(written, a.Folder)
		}
	}
	return written
}

// WriteDuplicateMarkers writes .ndignore into each of folders, the original
// folders of albums that beets reported as duplicates of an applied album.
// The rules are those of the applied folders: a folder that is a music
// folder or outside the music folders gets no marker, an existing file is
// not overwritten, and a folder gets no marker when an album in albums that
// was not applied has its folder strictly below it. albums are the lines of
// the run that holds the duplicates. It returns the folders whose marker it
// created.
func (r *Runner) WriteDuplicateMarkers(folders []string, albums []Album, musicFolders []string) ([]string, error) {
	roots, err := absPaths(musicFolders)
	if err != nil {
		return nil, err
	}
	resolved := resolveAll(roots)
	notApplied := notAppliedFolders(albums)
	var written []string
	for _, f := range folders {
		if r.markFolder(f, notApplied, resolved) {
			r.Log.Info("wrote .ndignore into a duplicate folder", "folder", f)
			written = append(written, f)
		}
	}
	return written, nil
}

// resolveAll returns the paths resolved with resolvePath.
func resolveAll(paths []string) []string {
	out := make([]string, len(paths))
	for i, p := range paths {
		out[i] = resolvePath(p)
	}
	return out
}

// notAppliedFolders returns the resolved folders of the albums that were not
// applied.
func notAppliedFolders(albums []Album) []string {
	var out []string
	for _, a := range albums {
		if a.Decision != DecisionApplied && filepath.IsAbs(a.Folder) {
			out = append(out, resolvePath(a.Folder))
		}
	}
	return out
}

// markFolder writes the marker into folder unless a folder in notApplied is
// strictly below it. roots must already be resolved. It reports whether it
// created the file.
func (r *Runner) markFolder(folder string, notApplied, roots []string) bool {
	if filepath.IsAbs(folder) {
		resolved := resolvePath(folder)
		if i := slices.IndexFunc(notApplied, func(p string) bool { return inside(p, resolved) }); i >= 0 {
			r.Log.Warn("not writing .ndignore: it would hide an album below this folder that was not applied", "folder", folder, "below", notApplied[i])
			return false
		}
	}
	return r.writeMarker(folder, roots)
}

// resolvePath returns path cleaned and with symlinks resolved. When the path
// cannot be resolved, it returns path cleaned.
func resolvePath(path string) string {
	path = filepath.Clean(path)
	if real, err := filepath.EvalSymlinks(path); err == nil {
		return real
	}
	return path
}

// writeMarker writes an empty .ndignore into folder, the original album
// folder of an applied album. It never overwrites a file, and it refuses a
// folder that is not strictly inside one of the import roots: a marker in a
// root would hide the whole library from Navidrome. roots must already be
// resolved with resolvePath. folder is resolved here, so a symlink cannot
// point the marker outside the roots. It reports whether it created the
// file.
func (r *Runner) writeMarker(folder string, roots []string) bool {
	if !filepath.IsAbs(folder) {
		r.Log.Warn("not writing .ndignore: folder is not absolute", "folder", folder)
		return false
	}
	folder = resolvePath(folder)
	if !insideAny(folder, roots) {
		r.Log.Warn("not writing .ndignore: folder is a music folder itself or outside the music folders", "folder", folder)
		return false
	}
	path := filepath.Join(folder, markerName)
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o644)
	if err != nil {
		if !errors.Is(err, fs.ErrExist) {
			r.Log.Warn("could not write .ndignore", "path", path, "error", err)
		}
		return false
	}
	if err := f.Close(); err != nil {
		r.Log.Warn("could not write .ndignore", "path", path, "error", err)
	}
	return true
}

// insideAny reports whether path is strictly below one of roots.
func insideAny(path string, roots []string) bool {
	return slices.ContainsFunc(roots, func(root string) bool { return inside(path, root) })
}

// inside reports whether path is strictly below dir.
func inside(path, dir string) bool {
	rel, err := filepath.Rel(dir, path)
	return err == nil && rel != "." && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator))
}

// lineWriter logs each line of beets output at level and keeps the last
// line for error messages. beets writes its warnings and errors to stderr,
// which is logged at info. stdout is logged at debug.
type lineWriter struct {
	log   *slog.Logger
	level slog.Level
	mu    sync.Mutex
	buf   []byte
	last  string
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
	w.log.Log(context.Background(), w.level, "beets: "+w.last)
}

func (w *lineWriter) lastLine() string {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.last
}
