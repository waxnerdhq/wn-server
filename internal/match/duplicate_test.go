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
	"syscall"
	"testing"

	"github.com/waxnerdhq/wn-server/internal/config"
	"github.com/waxnerdhq/wn-server/internal/store"
)

// dupEnv runs the match service with a shell script in place of beets. On
// `import` the script copies next.jsonl to the candidates file, appends
// forgotten.jsonl when that file exists (the plugin's `forgotten` lines), and
// keeps a copy of beets.yaml as seen.yaml. On `remove` it deletes the album
// folder.
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
		"import) cp \"$2\" '" + filepath.Join(e.data, "seen.yaml") + "'; cp '" + filepath.Join(e.data, "next.jsonl") + "' '" + filepath.Join(e.data, "beets-candidates.jsonl") + "'\n" +
		"if [ -f '" + filepath.Join(e.data, "forgotten.jsonl") + "' ]; then cat '" + filepath.Join(e.data, "forgotten.jsonl") + "' >> '" + filepath.Join(e.data, "beets-candidates.jsonl") + "'; fi ;;\n" +
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

// holdLock takes the beets.lock flock, as a running beets run does, and
// returns the function that releases it.
func (e *dupEnv) holdLock() func() {
	e.t.Helper()
	f, err := os.OpenFile(filepath.Join(e.data, "beets.lock"), os.O_RDWR|os.O_CREATE, 0o600)
	if err != nil {
		e.t.Fatal(err)
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		e.t.Fatal(err)
	}
	return func() { f.Close() }
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

// copyDir creates an album copy in sorted_dir.
func (e *dupEnv) copyDir(name string) string {
	e.t.Helper()
	dir := filepath.Join(e.data, "sorted", name)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		e.t.Fatal(err)
	}
	return dir
}

func TestRevertKeepsMarkersWhileAnotherEntryIsApplied(t *testing.T) {
	const r1 = "11111111-1111-1111-1111-111111111111"
	e := newDupEnv(t)
	a, b, d := e.folder("a/album"), e.folder("b/album"), e.folder("d/album")
	e.run(
		map[string]any{"folder": a, "decision": "applied", "release_id": r1, "copied_to": e.copyDir("A")},
		map[string]any{"folder": b, "decision": "applied", "release_id": r1, "copied_to": e.copyDir("B")},
		map[string]any{"folder": d, "decision": "duplicate", "release_id": r1},
	)
	if !hasMarker(d) {
		t.Fatal("the duplicate folder has no marker")
	}
	if _, err := e.svc.Revert(context.Background(), e.entry(a).ID); err != nil {
		t.Fatal(err)
	}
	if !hasMarker(d) || !e.entry(d).MarkerWritten {
		t.Fatal("the revert removed the duplicate marker while another entry of the release is applied")
	}
	if _, err := e.svc.Revert(context.Background(), e.entry(b).ID); err != nil {
		t.Fatal(err)
	}
	if hasMarker(d) || e.entry(d).MarkerWritten {
		t.Fatal("the duplicate marker stayed after the last applied entry was reverted")
	}
}

// TestRevertForgetsFolderOnNextRun checks that the run after a revert lists
// the folder in forget_folders, that the run after that does not, and that
// only a `forgotten` line for the folder clears the flag: a run that cannot
// start, or that stops before the plugin forgets, leaves it.
func TestRevertForgetsFolderOnNextRun(t *testing.T) {
	const r1 = "11111111-1111-1111-1111-111111111111"
	e := newDupEnv(t)
	a := e.folder("a/album")
	e.run(map[string]any{"folder": a, "decision": "applied", "release_id": r1, "copied_to": e.copyDir("A")})
	if _, err := e.svc.Revert(context.Background(), e.entry(a).ID); err != nil {
		t.Fatal(err)
	}
	pending := func() int {
		got, err := e.st.ForgetPending(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		return len(got)
	}
	if pending() != 1 {
		t.Fatalf("forget_pending entries after revert = %d, want 1", pending())
	}

	// beets.lock held elsewhere: the run does not start and the flag stays.
	unlock := e.holdLock()
	if _, err := e.svc.Run(context.Background()); err == nil {
		t.Fatal("Run succeeded while another run held the lock")
	}
	unlock()
	if pending() != 1 {
		t.Fatal("a run that did not start cleared forget_pending")
	}

	// beets runs but writes no forgotten line, as when it stops before
	// import_begin.
	e.run(map[string]any{"folder": a, "decision": "skipped"})
	seen, err := os.ReadFile(filepath.Join(e.data, "seen.yaml"))
	if err != nil || !strings.Contains(string(seen), "forget_folders:") || !strings.Contains(string(seen), a) {
		t.Fatalf("beets.yaml of the run after the revert: %v\n%s", err, seen)
	}
	if pending() != 1 {
		t.Fatal("a run without a forgotten line cleared forget_pending")
	}

	// A forgotten line for another folder does not clear it either.
	forgotten := filepath.Join(e.data, "forgotten.jsonl")
	if err := os.WriteFile(forgotten, []byte(`{"decision":"forgotten","folder":"/elsewhere"}`+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	e.run(map[string]any{"folder": a, "decision": "skipped"})
	if pending() != 1 {
		t.Fatal("a forgotten line for another folder cleared forget_pending")
	}

	if err := os.WriteFile(forgotten, []byte(`{"decision":"forgotten","folder":"`+a+`"}`+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	e.run(map[string]any{"folder": a, "decision": "skipped"})
	if pending() != 0 {
		t.Fatal("forget_pending stayed after the forgotten line")
	}
	os.Remove(forgotten)
	if e.entry(a).State != store.MatchPending {
		t.Fatalf("entry state = %s, want pending", e.entry(a).State)
	}
	e.run(map[string]any{"folder": a, "decision": "skipped"})
	seen, _ = os.ReadFile(filepath.Join(e.data, "seen.yaml"))
	if strings.Contains(string(seen), "forget_folders") {
		t.Fatalf("the second run still lists forget_folders:\n%s", seen)
	}
}

// TestApplyAfterRevertDoesNotForget checks that an entry reverted and then
// applied again is not forgotten by the next full run.
func TestApplyAfterRevertDoesNotForget(t *testing.T) {
	const r1 = "11111111-1111-1111-1111-111111111111"
	e := newDupEnv(t)
	a := e.folder("a/album")
	copied := e.copyDir("A")
	line := map[string]any{"folder": a, "decision": "applied", "release_id": r1, "copied_to": copied}
	e.run(line)
	id := e.entry(a).ID
	if _, err := e.svc.Revert(context.Background(), id); err != nil {
		t.Fatal(err)
	}
	e.run(map[string]any{"folder": a, "decision": "skipped", "candidates": []any{map[string]any{"release_id": r1}}})
	if err := os.MkdirAll(copied, 0o755); err != nil {
		t.Fatal(err)
	}
	// The fake beets answers the apply run with an applied line.
	line["tags"], line["candidates"] = map[string]any{}, []any{}
	data, _ := json.Marshal(line)
	if err := os.WriteFile(filepath.Join(e.data, "next.jsonl"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := e.svc.Apply(context.Background(), id, r1); err != nil {
		t.Fatal(err)
	}
	got, err := e.st.ForgetPending(context.Background())
	if err != nil || len(got) != 0 {
		t.Fatalf("ForgetPending after revert and apply = %v, %v", got, err)
	}
	e.run(map[string]any{"folder": a, "decision": "skipped"})
	seen, _ := os.ReadFile(filepath.Join(e.data, "seen.yaml"))
	if strings.Contains(string(seen), "forget_folders") {
		t.Fatalf("the run after the apply lists forget_folders:\n%s", seen)
	}
}

func TestOldDuplicateWaitsForPendingEntryBelow(t *testing.T) {
	const r2 = "22222222-2222-2222-2222-222222222222"
	e := newDupEnv(t)
	d, below, b := e.folder("d/album"), e.folder("d/album/cd2"), e.folder("e/album")
	e.run(
		map[string]any{"folder": d, "decision": "duplicate", "release_id": r2},
		map[string]any{"folder": below, "decision": "skipped"},
	)
	// This run's lines do not mention the pending entry below d.
	e.run(map[string]any{"folder": b, "decision": "applied", "release_id": r2, "copied_to": e.copyDir("B")})
	if hasMarker(d) || e.entry(d).MarkerWritten {
		t.Fatal("a duplicate folder got a marker that would hide a pending entry below it")
	}
}
