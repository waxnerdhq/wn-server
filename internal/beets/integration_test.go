package beets

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"io/fs"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
)

// TestImportIntegration runs a real beets import against a temporary copy of
// music. It is skipped unless WN_BEETS_BIN names a beets executable. It
// needs network access to MusicBrainz.
//
//	WN_BEETS_BIN    beets executable (required)
//	WN_FPCALC_BIN   fpcalc executable (optional; default "fpcalc" on PATH)
//	WN_BEETS_ALBUM  folder with one real album (optional). It is copied into
//	                the temporary music folder, so the source is not touched.
//	                With it the test expects at least one applied album.
//
// Without WN_BEETS_ALBUM the test generates one album with ffmpeg and is
// skipped when ffmpeg is not on PATH. The album has a tagged MP3, an MP3
// without an extension, and an MP3 stream in a WAV container. beets rewrites
// the last two in place unless wn-server turns that off. The album is
// unlikely to match MusicBrainz, so the test then checks the skip path only.
//
// The test checks that the original files keep their bytes and mtimes, that
// every applied copy in sorted_dir carries a MusicBrainz album id that is
// one of the candidates (read with ffprobe), and that the original folder of
// every applied album holds .ndignore and no other album folder does.
func TestImportIntegration(t *testing.T) {
	bin := os.Getenv("WN_BEETS_BIN")
	if bin == "" {
		t.Skip("WN_BEETS_BIN not set")
	}
	tmp := t.TempDir()
	music := filepath.Join(tmp, "music")
	if src := os.Getenv("WN_BEETS_ALBUM"); src != "" {
		if err := os.CopyFS(filepath.Join(music, filepath.Base(src)), os.DirFS(src)); err != nil {
			t.Fatalf("copy %s: %v", src, err)
		}
	} else {
		ffmpeg, err := exec.LookPath("ffmpeg")
		if err != nil {
			t.Skip("ffmpeg not on PATH and WN_BEETS_ALBUM not set")
		}
		dir := filepath.Join(music, "Test Artist", "Test Album")
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		// Track 2 is an MP3 without an extension. beets' fix_extension would
		// copy it to "02 Test Track.mp3" next to the original. Track 3 is an
		// MP3 stream in a WAV container. beets' remux_mp3_in_wav would write
		// "03 Test Track.mp3" and delete the WAV.
		tracks := []struct {
			name   string
			format []string
		}{
			{"01 Test Track.mp3", nil},
			{"02 Test Track", []string{"-f", "mp3"}},
			{"03 Test Track.wav", []string{"-c:a", "libmp3lame", "-f", "wav"}},
		}
		for i, tr := range tracks {
			args := []string{"-v", "error", "-f", "lavfi", "-i", "sine=frequency=440:duration=5",
				"-metadata", "artist=Test Artist", "-metadata", "album=Test Album",
				"-metadata", "title=Test Track " + strconv.Itoa(i+1), "-metadata", "track=" + strconv.Itoa(i+1),
				"-metadata", "date=2001"}
			args = append(args, tr.format...)
			out, err := exec.Command(ffmpeg, append(args, filepath.Join(dir, tr.name))...).CombinedOutput()
			if err != nil {
				t.Fatalf("ffmpeg %s: %v\n%s", tr.name, err, out)
			}
		}
	}
	// Give the originals an mtime in the past, so a rewrite would show.
	old := time.Date(2020, 1, 2, 3, 4, 5, 0, time.UTC)
	_ = filepath.WalkDir(music, func(p string, d fs.DirEntry, err error) error {
		if err == nil && !d.IsDir() {
			_ = os.Chtimes(p, old, old)
		}
		return nil
	})
	before := snapshot(t, music)

	pluginDir, err := filepath.Abs(filepath.Join("..", "..", "bundle", "beets-plugins"))
	if err != nil {
		t.Fatal(err)
	}
	fpcalc := os.Getenv("WN_FPCALC_BIN")
	if fpcalc == "" {
		fpcalc = "fpcalc"
	}
	data := filepath.Join(tmp, "data")
	sorted := filepath.Join(data, "sorted")
	r := &Runner{
		Config:    Config{DataDir: data, SortedDir: sorted, PluginDir: pluginDir, AcoustIDKey: DefaultAcoustIDKey},
		BeetsBin:  bin,
		FpcalcBin: fpcalc,
		Log:       slog.New(slog.NewTextHandler(testWriter{t}, &slog.HandlerOptions{Level: slog.LevelDebug})),
	}
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Minute)
	defer cancel()
	sum, err := r.Import(ctx, []string{music})
	if err != nil {
		t.Fatalf("Import: %v", err)
	}
	t.Logf("applied %d, skipped %d, duplicates %d, errors %d", sum.Applied, sum.Skipped, sum.Duplicate, sum.Errors)
	if len(sum.Albums) == 0 {
		t.Fatal("candidates file has no lines")
	}
	if sum.Errors > 0 {
		for _, a := range sum.Albums {
			if a.Decision == DecisionError {
				t.Errorf("error line: %s: %s", a.Folder, a.Error)
			}
		}
	}

	after := snapshot(t, music)
	for p, b := range before {
		if a, ok := after[p]; !ok || a != b {
			t.Errorf("original %s changed: before %+v, after %+v", p, b, a)
		}
	}
	for p := range after {
		if _, ok := before[p]; !ok && filepath.Base(p) != ".ndignore" {
			t.Errorf("new file in the originals tree: %s", p)
		}
	}

	for _, a := range sum.Albums {
		_, markErr := os.Stat(filepath.Join(a.Folder, ".ndignore"))
		if a.Decision != DecisionApplied {
			if markErr == nil {
				t.Errorf("%s is %s but has .ndignore", a.Folder, a.Decision)
			}
			continue
		}
		if markErr != nil {
			t.Errorf("applied %s has no .ndignore: %v", a.Folder, markErr)
		}
		if !strings.HasPrefix(a.CopiedTo, sorted+string(filepath.Separator)) {
			t.Errorf("copied_to %q is not in sorted_dir %q", a.CopiedTo, sorted)
			continue
		}
		checkMBIDs(t, a)
	}
	if os.Getenv("WN_BEETS_ALBUM") != "" && sum.Applied == 0 {
		t.Errorf("WN_BEETS_ALBUM was not applied: %+v", sum.Albums)
	}
}

type fileState struct {
	sum   [32]byte
	mtime time.Time
}

func snapshot(t *testing.T, root string) map[string]fileState {
	t.Helper()
	out := map[string]fileState{}
	err := filepath.WalkDir(root, func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() {
			return err
		}
		b, err := os.ReadFile(p)
		if err != nil {
			return err
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		out[p] = fileState{sum: sha256.Sum256(b), mtime: info.ModTime()}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	return out
}

// checkMBIDs reads the album id tag of every copied file with ffprobe.
func checkMBIDs(t *testing.T, a Album) {
	t.Helper()
	ffprobe, err := exec.LookPath("ffprobe")
	if err != nil {
		t.Logf("ffprobe not on PATH; not checking the MBIDs in %s", a.CopiedTo)
		return
	}
	ids := map[string]bool{}
	for _, c := range a.Candidates {
		ids[c.ReleaseID] = true
	}
	entries, err := os.ReadDir(a.CopiedTo)
	if err != nil {
		t.Errorf("read %s: %v", a.CopiedTo, err)
		return
	}
	checked := 0
	for _, e := range entries {
		if e.IsDir() || strings.HasPrefix(e.Name(), ".") {
			continue
		}
		p := filepath.Join(a.CopiedTo, e.Name())
		out, err := exec.Command(ffprobe, "-v", "quiet", "-print_format", "json", "-show_format", p).Output()
		if err != nil {
			continue // not an audio file, for example cover art
		}
		var probe struct {
			Format struct {
				Tags map[string]string `json:"tags"`
			} `json:"format"`
		}
		if json.Unmarshal(out, &probe) != nil {
			continue
		}
		id := ""
		for k, v := range probe.Format.Tags {
			norm := strings.NewReplacer(" ", "", "_", "").Replace(strings.ToLower(k))
			if norm == "musicbrainzalbumid" {
				id = v
			}
		}
		if !ids[id] {
			t.Errorf("%s: MusicBrainz album id %q is not one of the candidates", p, id)
		}
		checked++
	}
	if checked == 0 {
		t.Errorf("no audio files found in %s", a.CopiedTo)
	}
}

type testWriter struct{ t *testing.T }

func (w testWriter) Write(p []byte) (int, error) {
	w.t.Log(strings.TrimRight(string(p), "\n"))
	return len(p), nil
}
