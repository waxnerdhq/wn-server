package relay

import (
	"go/parser"
	"go/token"
	"os"
	"strconv"
	"strings"
	"testing"
)

// TestImports checks that the package imports the standard library, yamux,
// and internal/relayreq only (docs/relay.md, "The relay package").
func TestImports(t *testing.T) {
	allowed := map[string]bool{
		"github.com/hashicorp/yamux":                       true,
		"github.com/waxnerdhq/wn-server/internal/relayreq": true,
	}
	entries, err := os.ReadDir(".")
	if err != nil {
		t.Fatal(err)
	}
	fset := token.NewFileSet()
	for _, e := range entries {
		name := e.Name()
		if !strings.HasSuffix(name, ".go") || strings.HasSuffix(name, "_test.go") {
			continue
		}
		f, err := parser.ParseFile(fset, name, nil, parser.ImportsOnly)
		if err != nil {
			t.Fatal(err)
		}
		for _, imp := range f.Imports {
			path, _ := strconv.Unquote(imp.Path.Value)
			std := !strings.Contains(strings.SplitN(path, "/", 2)[0], ".")
			if !std && !allowed[path] {
				t.Errorf("%s imports %s", name, path)
			}
		}
	}
}
