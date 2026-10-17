package beets

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// TestApplyWithFakeBeets checks the apply arguments, that beets.yaml carries
// apply_release_id during the run and not after it, and the marker.
func TestApplyWithFakeBeets(t *testing.T) {
	tmp := t.TempDir()
	music := filepath.Join(tmp, "music")
	album := filepath.Join(music, "A", "Album")
	if err := os.MkdirAll(album, 0o755); err != nil {
		t.Fatal(err)
	}
	data := filepath.Join(tmp, "data")
	out := filepath.Join(data, "beets-candidates.jsonl")
	const mbid = "4b8a8d08-8d2d-4c8f-9a3e-1c2b3d4e5f60"
	line := `{"folder":"` + album + `","tags":{},"decision":"applied","candidates":[],"copied_to":"/x","release_id":"` + mbid + `","reason":"requested_release_id"}`
	script := filepath.Join(tmp, "beets")
	body := "#!/bin/sh\n" +
		"test \"$3 $4 $5 $6 $7\" = 'import -q -I -S " + mbid + "' || exit 3\n" +
		"test \"$8\" = '" + album + "' || exit 4\n" +
		"grep -q 'apply_release_id: " + mbid + "' \"$2\" || exit 5\n" +
		"echo '" + line + "' >> '" + out + "'\n"
	if err := os.WriteFile(script, []byte(body), 0o755); err != nil {
		t.Fatal(err)
	}
	scan := &fakeScanner{}
	r := &Runner{
		Config:   Config{DataDir: data, SortedDir: filepath.Join(data, "sorted"), PluginDir: "../../bundle/beets-plugins"},
		BeetsBin: script,
		Scanner:  scan,
		Log:      slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	if _, err := r.Apply(context.Background(), album, "not-a-uuid", []string{music}); err == nil {
		t.Fatal("Apply accepted a release id that is not a UUID")
	}
	sum, err := r.Apply(context.Background(), album, mbid, []string{music})
	if err != nil {
		t.Fatalf("Apply: %v", err)
	}
	if sum.Applied != 1 || sum.Albums[0].Reason != ReasonRequestedRelease || sum.Albums[0].ReleaseID != mbid || scan.calls != 1 {
		t.Fatalf("summary = %+v, scans %d", sum, scan.calls)
	}
	if _, err := os.Stat(filepath.Join(album, markerName)); err != nil {
		t.Errorf("marker in applied folder: %v", err)
	}
	if cfg, _ := os.ReadFile(filepath.Join(data, "beets.yaml")); strings.Contains(string(cfg), "apply_") {
		t.Errorf("beets.yaml keeps the apply options after the run:\n%s", cfg)
	}
}

// TestRemoveAndMarker checks the sorted_dir check of Remove, the remove
// arguments, and that RemoveMarker deletes only an empty marker.
func TestRemoveAndMarker(t *testing.T) {
	tmp := t.TempDir()
	music := filepath.Join(tmp, "music")
	sorted := filepath.Join(tmp, "data", "sorted")
	albumCopy := filepath.Join(sorted, "Ar", "Al")
	for _, d := range []string{albumCopy, music} {
		if err := os.MkdirAll(d, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	script := filepath.Join(tmp, "beets")
	body := "#!/bin/sh\n" +
		"test \"$3 $4 $5 $6\" = 'remove -a -d -f' || exit 3\n" +
		"rm -r \"${7#path:}\"\n"
	if err := os.WriteFile(script, []byte(body), 0o755); err != nil {
		t.Fatal(err)
	}
	r := &Runner{
		Config:   Config{DataDir: filepath.Join(tmp, "data"), SortedDir: sorted},
		BeetsBin: script,
		Log:      slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	for _, p := range []string{sorted, music, filepath.Join(sorted, "..", "x")} {
		if err := r.Remove(context.Background(), p); err == nil {
			t.Errorf("Remove(%s) succeeded", p)
		}
	}
	// A symlink in sorted_dir that points outside it is refused.
	if err := os.Symlink(music, filepath.Join(sorted, "link")); err != nil {
		t.Fatal(err)
	}
	if err := r.Remove(context.Background(), filepath.Join(sorted, "link", "A")); err == nil {
		t.Error("Remove through a symlink out of sorted_dir succeeded")
	}
	if err := r.Remove(context.Background(), albumCopy); err != nil {
		t.Fatalf("Remove: %v", err)
	}
	if _, err := os.Stat(albumCopy); !errors.Is(err, os.ErrNotExist) {
		t.Errorf("copy still exists: %v", err)
	}

	empty := filepath.Join(music, "A", "Empty")
	full := filepath.Join(music, "A", "Full")
	for _, d := range []string{empty, full} {
		if err := os.MkdirAll(d, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	for path, content := range map[string]string{
		filepath.Join(empty, markerName): "",
		filepath.Join(full, markerName):  "*.flac\n",
		filepath.Join(music, markerName): "",
	} {
		if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	for _, tc := range []struct {
		folder  string
		removed bool
	}{{empty, true}, {full, false}, {music, false}} {
		removed, err := r.RemoveMarker(tc.folder, []string{music})
		if err != nil || removed != tc.removed {
			t.Errorf("RemoveMarker(%s) = %v, %v; want %v", tc.folder, removed, err, tc.removed)
		}
		if _, err := os.Stat(filepath.Join(tc.folder, markerName)); errors.Is(err, os.ErrNotExist) != tc.removed {
			t.Errorf("marker in %s: %v", tc.folder, err)
		}
	}
}
