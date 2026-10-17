package cli

import (
	"bytes"
	"encoding/json"
	"strings"
	"testing"

	"github.com/waxnerdhq/wn-server/internal/store"
)

func TestPrintEntries(t *testing.T) {
	entries := []store.MatchEntry{
		{ID: 3, State: store.MatchPending, Folder: "/music/Unknown/Rip 2004", Artist: "Various", Album: "Rip 2004", TrackCount: 11,
			Candidates: []store.MatchCandidate{{ReleaseID: "1a2b3c4d-5e6f-7081-92a3-b4c5d6e7f809", Artist: "Various Artists", Album: "Rip It Up 2004", Year: 2004, Distance: 0.41}}},
		{ID: 12, State: store.MatchDismissed, Folder: "/music/Empty", TrackCount: 2},
	}
	var out bytes.Buffer
	printEntries(&out, entries, true)
	want := "" +
		"ID  STATE      FOLDER                   TAGS                           TOP CANDIDATE\n" +
		"3   pending    /music/Unknown/Rip 2004  Various - Rip 2004, 11 tracks  Various Artists - Rip It Up 2004 (2004) 1a2b3c4d-5e6f-7081-92a3-b4c5d6e7f809, distance 0.410\n" +
		"12  dismissed  /music/Empty             ? - ?, 2 tracks                no candidates\n"
	if got := out.String(); got != want {
		t.Fatalf("printEntries:\n%s\nwant:\n%s", got, want)
	}

	out.Reset()
	printEntries(&out, nil, false)
	if got := out.String(); got != "no pending entries\n" {
		t.Fatalf("empty list = %q", got)
	}

	out.Reset()
	if err := printEntriesJSON(&out, entries); err != nil {
		t.Fatal(err)
	}
	var got []map[string]any
	if err := json.Unmarshal(out.Bytes(), &got); err != nil {
		t.Fatalf("JSON output does not parse: %v\n%s", err, out.String())
	}
	if len(got) != 2 || got[0]["state"] != "pending" || got[1]["candidates"] == nil || strings.Contains(out.String(), "applied_at") {
		t.Fatalf("JSON output:\n%s", out.String())
	}
}
