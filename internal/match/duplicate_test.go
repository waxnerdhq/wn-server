package match

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/waxnerdhq/wn-server/internal/config"
	"github.com/waxnerdhq/wn-server/internal/store"
)

// dupEnv runs the match service with a shell script in place of beets. On
// `import` the script copies next.jsonl to the candidates file. On `remove`
// it deletes the album folder.
type dupEnv struct {
	t     *testing.T
	svc   *Service
	st    *store.Store
	music string
	data  string
}

func newDupEnv(t *testing.T) *dupEnv {
	t.Helper()
	tmp := t.TempDir()
	e := &dupEnv{t: t, music: filepath.Join(tmp, "music"), data: filepath.Join(tmp, "data")}
	bin := filepath.Join(tmp, "bin")
	for _, d := range []string{e.music, e.data, bin, filepath.Join(bin, "beets-plugins"), filepath.Join(e.data, "sorted")} {
		if err := os.MkdirAll(d, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	// beets-plugins next to the executable must hold the plugin file.
	if err := os.WriteFile(filepath.Join(bin, "beets-plugins", "wn_candidates.py"), nil, 0o644); err != nil {
		t.Fatal(err)
	}
	script := "#!/bin/sh\n" +
		"case \"$3\" in\n" +
		"import) cp '" + filepath.Join(e.data, "next.jsonl") + "' '" + filepath.Join(e.data, "beets-candidates.jsonl") + "' ;;\n" +
		"remove) rm -r \"${7#path:}\" ;;\n" +
		"*) exit 3 ;;\nesac\n"
	beetsBin := filepath.Join(bin, "beets")
	if err := os.WriteFile(beetsBin, []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	st, err := store.Open(context.Background(), filepath.Join(tmp, store.FileName))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { st.Close() })
	cfg := &config.Config{MusicFolders: []string{e.music}, SortedDir: filepath.Join(e.data, "sorted"), DataDir: e.data, BeetsBin: beetsBin}
	e.svc = NewService(cfg, st, nil, slog.New(slog.NewTextHandler(io.Discard, nil)))
	e.st = st
	return e
}

// folder creates an album folder below the music folder.
func (e *dupEnv) folder(name string) string {
	e.t.Helper()
	dir := filepath.Join(e.music, name)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		e.t.Fatal(err)
	}
	return dir
}

// run makes the fake beets write lines and runs a full import.
func (e *dupEnv) run(lines ...map[string]any) {
	e.t.Helper()
	var b strings.Builder
	for _, l := range lines {
		l["tags"], l["candidates"] = map[string]any{}, []any{}
		data, err := json.Marshal(l)
		if err != nil {
			e.t.Fatal(err)
		}
		b.Write(data)
		b.WriteByte('\n')
	}
	if err := os.WriteFile(filepath.Join(e.data, "next.jsonl"), []byte(b.String()), 0o644); err != nil {
		e.t.Fatal(err)
	}
	if _, err := e.svc.Run(context.Background()); err != nil {
		e.t.Fatalf("Run: %v", err)
	}
}

func (e *dupEnv) entry(folder string) store.MatchEntry {
	e.t.Helper()
	entries, err := e.st.MatchEntries(context.Background())
	if err != nil {
		e.t.Fatal(err)
	}
	for _, m := range entries {
		if m.Folder == folder {
			return m
		}
	}
	e.t.Fatalf("no entry for %s", folder)
	return store.MatchEntry{}
}

func hasMarker(folder string) bool {
	_, err := os.Lstat(filepath.Join(folder, ".ndignore"))
	return !errors.Is(err, os.ErrNotExist)
}

func TestDuplicateFoldersGetMarkers(t *testing.T) {
	const r1, r2 = "11111111-1111-1111-1111-111111111111", "22222222-2222-2222-2222-222222222222"
	e := newDupEnv(t)
	a, d1, owned, d3, b := e.folder("a/album"), e.folder("b/album"), e.folder("c/album"), e.folder("d/album"), e.folder("e/album")
	copyA := filepath.Join(e.data, "sorted", "Ar", "Al")
	if err := os.MkdirAll(copyA, 0o755); err != nil {
		t.Fatal(err)
	}
	// The owner made this marker. wn-server must not claim it or remove it.
	if err := os.WriteFile(filepath.Join(owned, ".ndignore"), nil, 0o644); err != nil {
		t.Fatal(err)
	}

	// d3 duplicates r2, which no entry has applied yet, so it gets no marker.
	e.run(
		map[string]any{"folder": a, "decision": "applied", "release_id": r1, "copied_to": copyA},
		map[string]any{"folder": d1, "decision": "duplicate", "release_id": r1},
		map[string]any{"folder": owned, "decision": "duplicate", "release_id": r1},
		map[string]any{"folder": d3, "decision": "duplicate", "release_id": r2},
	)
	if !hasMarker(a) || !hasMarker(d1) || hasMarker(d3) {
		t.Fatalf("markers after the run: applied %v, duplicate %v, unlinked duplicate %v", hasMarker(a), hasMarker(d1), hasMarker(d3))
	}
	if got := e.entry(d1); got.State != store.MatchDuplicate || got.DuplicateOfReleaseID != r1 || !got.MarkerWritten {
		t.Fatalf("duplicate entry = %+v", got)
	}
	if got := e.entry(owned); got.MarkerWritten {
		t.Fatalf("an owner-made marker was claimed: %+v", got)
	}

	// A later run applies r2. The earlier duplicate d3 gets its marker.
	copyB := filepath.Join(e.data, "sorted", "Br", "Bl")
	if err := os.MkdirAll(copyB, 0o755); err != nil {
		t.Fatal(err)
	}
	e.run(map[string]any{"folder": b, "decision": "applied", "release_id": r2, "copied_to": copyB})
	if !hasMarker(d3) || !e.entry(d3).MarkerWritten {
		t.Fatalf("earlier duplicate not marked after its release was applied: %+v", e.entry(d3))
	}

	// Reverting a removes the markers of its duplicates and nothing else.
	if _, err := e.svc.Revert(context.Background(), e.entry(a).ID); err != nil {
		t.Fatalf("Revert: %v", err)
	}
	if hasMarker(a) || hasMarker(d1) {
		t.Fatalf("markers left after the revert: applied %v, duplicate %v", hasMarker(a), hasMarker(d1))
	}
	if got := e.entry(d1); got.State != store.MatchDuplicate || got.MarkerWritten {
		t.Fatalf("duplicate entry after the revert = %+v", got)
	}
	if !hasMarker(owned) {
		t.Fatal("the revert removed an owner-made marker")
	}
	if !hasMarker(d3) || !hasMarker(b) {
		t.Fatal("the revert removed a marker of another release")
	}
}
