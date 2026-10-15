package cli

import (
	"bytes"
	"testing"
)

func TestVersionCommand(t *testing.T) {
	var out bytes.Buffer
	root := newRootCmd()
	root.SetOut(&out)
	root.SetArgs([]string{"version"})
	if err := root.Execute(); err != nil {
		t.Fatal(err)
	}
	if got, want := out.String(), "wn-server dev\n"; got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}
