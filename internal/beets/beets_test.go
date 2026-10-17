package beets

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"

	"gopkg.in/yaml.v3"
)

func TestRender(t *testing.T) {
	c := Config{
		DataDir:     "/data/wn-server",
		SortedDir:   "/data/wn-server/sorted",
		PluginDir:   "/opt/wn-server/beets-plugins",
		AcoustIDKey: "k3y",

		ForgetFolders: []string{"/music/a", "/music/b"},
	}
	body, err := c.Render()
	if err != nil {
		t.Fatal(err)
	}
	var got map[string]any
	if err := yaml.Unmarshal(body, &got); err != nil {
		t.Fatalf("parse rendered yaml: %v\n%s", err, body)
	}
	want := map[string]any{
		"library":   "/data/wn-server/beets.db",
		"directory": "/data/wn-server/sorted",
		"statefile": "/data/wn-server/beets-state.pickle",
		"paths":     map[string]any{"default": "$albumartist/$album%aunique{}/$track $title"},
		"import": map[string]any{
			"copy": true, "move": false, "link": false, "hardlink": false, "reflink": false,
			"delete": false, "write": true, "quiet": true, "quiet_fallback": "skip",
			"duplicate_action": "skip", "autotag": true, "resume": false,
			"incremental": true, "incremental_skip_later": true,
			"fix_ext_inplace": false, "remux_mp3_in_wav": false,
			"log": "/data/wn-server/beets-import.log",
		},
		"plugins":    []any{"musicbrainz", "chroma", "wn_candidates"},
		"pluginpath": []any{"/opt/wn-server/beets-plugins"},
		"chroma":     map[string]any{"auto": true},
		"wn_candidates": map[string]any{
			"output": "/data/wn-server/beets-candidates.jsonl", "acoustid_app_key": "k3y",
			"forget_folders": []any{"/music/a", "/music/b"},
		},
		"musicbrainz": map[string]any{"host": "musicbrainz.org", "ratelimit": 1},
		"ui":          map[string]any{"color": false},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("rendered config:\n%s\nparsed %#v\nwant %#v", body, got, want)
	}

	c.AcoustIDKey, c.ForgetFolders = "", nil
	body, err = c.Render()
	if err != nil {
		t.Fatal(err)
	}
	got = nil
	if err := yaml.Unmarshal(body, &got); err != nil {
		t.Fatal(err)
	}
	if _, ok := got["wn_candidates"].(map[string]any)["acoustid_app_key"]; ok {
		t.Fatalf("acoustid_app_key present without a key:\n%s", body)
	}
	if _, ok := got["wn_candidates"].(map[string]any)["forget_folders"]; ok {
		t.Fatalf("forget_folders present without folders:\n%s", body)
	}
}

func TestParseCandidates(t *testing.T) {
	f, err := os.Open("testdata/candidates.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	albums, forgotten, malformed, err := parseCandidates(f)
	if err != nil {
		t.Fatal(err)
	}
	if malformed != 1 {
		t.Errorf("malformed = %d, want 1", malformed)
	}
	if want := []string{"/music/Reverted"}; !reflect.DeepEqual(forgotten, want) {
		t.Errorf("forgotten = %v, want %v", forgotten, want)
	}
	var decisions []string
	for _, a := range albums {
		decisions = append(decisions, a.Decision)
	}
	if want := []string{"applied", "skipped", "duplicate", "skipped", "error"}; !reflect.DeepEqual(decisions, want) {
		t.Fatalf("decisions = %v, want %v", decisions, want)
	}
	a := albums[0]
	if a.Folder != "/music/Boards of Canada/Geogaddi" || a.CopiedTo != "/data/sorted/Boards of Canada/Geogaddi" ||
		a.Tags.TrackCount != 23 || len(a.Candidates) != 2 ||
		a.Candidates[0].ReleaseID != "4b8a8d08-8d2d-4c8f-9a3e-1c2b3d4e5f60" || a.Candidates[0].Distance != 0.012 ||
		a.Candidates[0].Label != "Warp Records" {
		t.Errorf("applied line parsed as %+v", a)
	}
	if albums[2].Tags.Artist != "Sigur Rós" {
		t.Errorf("non-ASCII artist = %q", albums[2].Tags.Artist)
	}
	if albums[4].Error == "" {
		t.Errorf("error line has no message: %+v", albums[4])
	}
	sum := summarize(albums, malformed)
	if sum.Applied != 1 || sum.Skipped != 2 || sum.Duplicate != 1 || sum.Errors != 2 || sum.Malformed != 1 {
		t.Errorf("summary = %+v", sum)
	}
}

type fakeScanner struct {
	calls int
	err   error
}

func (s *fakeScanner) StartScan(context.Context) error {
	s.calls++
	return s.err
}

// TestImportWithFakeBeets runs Import with a shell script in place of beets.
// The script writes the candidates file, so the test covers the subprocess
// call, the parse, the .ndignore markers, and the scan trigger.
func TestImportWithFakeBeets(t *testing.T) {
	tmp := t.TempDir()
	music := filepath.Join(tmp, "music")
	applied := filepath.Join(music, "A", "Album")
	skipped := filepath.Join(music, "B", "Other")
	for _, d := range []string{applied, skipped} {
		if err := os.MkdirAll(d, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	data := filepath.Join(tmp, "data")
	out := filepath.Join(data, "beets-candidates.jsonl")
	lines := `{"folder":"` + applied + `","tags":{},"decision":"applied","candidates":[],"copied_to":"/x"}
{"folder":"` + music + `","tags":{},"decision":"applied","candidates":[]}
{"folder":"` + skipped + `","tags":{},"decision":"skipped","candidates":[]}
`
	script := filepath.Join(tmp, "beets")
	body := "#!/bin/sh\n" +
		"test \"$1\" = --config || exit 3\n" +
		"test \"$3\" = import && test \"$4\" = -q || exit 4\n" +
		"case \"$PATH\" in /opt/fp:*) ;; *) exit 5;; esac\n" +
		"cat >> '" + out + "' <<'EOF'\n" + lines + "EOF\n"
	if err := os.WriteFile(script, []byte(body), 0o755); err != nil {
		t.Fatal(err)
	}
	// A stale line from an earlier run must be dropped.
	if err := os.MkdirAll(data, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, []byte(`{"folder":"/old","decision":"applied"}`+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	scan := &fakeScanner{err: errors.New("connection refused")}
	r := &Runner{
		Config:    Config{DataDir: data, SortedDir: filepath.Join(data, "sorted"), PluginDir: "../../bundle/beets-plugins"},
		BeetsBin:  script,
		FpcalcBin: "/opt/fp/fpcalc",
		Scanner:   scan,
		Log:       slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	sum, err := r.Import(context.Background(), []string{music}, nil)
	if err != nil {
		t.Fatalf("Import: %v", err)
	}
	if sum.Applied != 2 || sum.Skipped != 1 || len(sum.Albums) != 3 || sum.Malformed != 0 {
		t.Fatalf("summary = %+v", sum)
	}
	if scan.calls != 1 {
		t.Errorf("scan calls = %d, want 1", scan.calls)
	}
	if info, err := os.Stat(filepath.Join(applied, ".ndignore")); err != nil || info.Size() != 0 {
		t.Errorf("marker in applied folder: %v", err)
	}
	for _, d := range []string{music, skipped} {
		if _, err := os.Stat(filepath.Join(d, ".ndignore")); !errors.Is(err, os.ErrNotExist) {
			t.Errorf("unexpected marker in %s: %v", d, err)
		}
	}
	info, err := os.Stat(filepath.Join(data, "beets.yaml"))
	if err != nil || info.Mode().Perm() != 0o600 {
		t.Errorf("beets.yaml mode: %v %v", info, err)
	}
	cfg, _ := os.ReadFile(filepath.Join(data, "beets.yaml"))
	if !strings.Contains(string(cfg), "wn_candidates") {
		t.Errorf("beets.yaml has no wn_candidates:\n%s", cfg)
	}
}

// TestWriteMarkersNested checks that an applied folder gets no marker when an
// album below it was not applied, because the marker would hide that album.
func TestWriteMarkersNested(t *testing.T) {
	music := t.TempDir()
	parent := filepath.Join(music, "Artist", "Album")
	child := filepath.Join(parent, "Bonus")
	if err := os.MkdirAll(child, 0o755); err != nil {
		t.Fatal(err)
	}
	r := &Runner{Log: slog.New(slog.NewTextHandler(io.Discard, nil))}
	r.writeMarkers([]Album{
		{Folder: parent, Decision: DecisionApplied},
		{Folder: child, Decision: DecisionSkipped},
	}, []string{music})
	for _, d := range []string{parent, child} {
		if _, err := os.Stat(filepath.Join(d, markerName)); !errors.Is(err, os.ErrNotExist) {
			t.Errorf("unexpected marker in %s: %v", d, err)
		}
	}
}

// TestImportCancelled checks that a cancelled import still writes the marker
// for an album that beets applied before the cancel.
func TestImportCancelled(t *testing.T) {
	tmp := t.TempDir()
	music := filepath.Join(tmp, "music")
	applied := filepath.Join(music, "A", "Album")
	if err := os.MkdirAll(applied, 0o755); err != nil {
		t.Fatal(err)
	}
	data := filepath.Join(tmp, "data")
	out := filepath.Join(data, "beets-candidates.jsonl")
	line := `{"folder":"` + applied + `","tags":{},"decision":"applied","candidates":[],"copied_to":"/x"}`
	script := filepath.Join(tmp, "beets")
	body := "#!/bin/sh\necho '" + line + "' >> '" + out + "'\nexec sleep 30\n"
	if err := os.WriteFile(script, []byte(body), 0o755); err != nil {
		t.Fatal(err)
	}
	scan := &fakeScanner{}
	r := &Runner{
		Config:    Config{DataDir: data, SortedDir: filepath.Join(data, "sorted"), PluginDir: "../../bundle/beets-plugins"},
		BeetsBin:  script,
		FpcalcBin: "fpcalc",
		Scanner:   scan,
		Log:       slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	ctx, cancel := context.WithTimeout(context.Background(), 500*time.Millisecond)
	defer cancel()
	sum, err := r.Import(ctx, []string{music}, nil)
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("Import error = %v, want deadline exceeded", err)
	}
	if sum.Applied != 1 {
		t.Errorf("summary = %+v", sum)
	}
	if _, err := os.Stat(filepath.Join(applied, markerName)); err != nil {
		t.Errorf("marker in applied folder: %v", err)
	}
	if scan.calls != 0 {
		t.Errorf("scan calls = %d, want 0", scan.calls)
	}
}
