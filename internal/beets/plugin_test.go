package beets

import (
	"os/exec"
	"path/filepath"
	"testing"
)

// pluginCheck compiles only the _existing_id_match function from the plugin
// source, because importing the plugin module needs beets.
const pluginCheck = `
import ast, sys
path = sys.argv[1]
tree = ast.parse(open(path, encoding="utf-8").read())
fn = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "_existing_id_match")
ns = {}
exec(compile(ast.Module(body=[fn], type_ignores=[]), path, "exec"), ns)
f = ns["_existing_id_match"]
a, b = "30ec6df1-2b6e-3d93-ab74-aa366b533abf", "11111111-2b6e-3d93-ab74-aa366b533abf"
cases = [
    (f([a, a, a], a, 0.067, 0.25), True),
    (f([a, a, a], a, 0.25, 0.25), True),
    (f([a, "", a], a, 0.067, 0.25), False),
    (f([a, a, b], a, 0.067, 0.25), False),
    (f([b, b, b], a, 0.067, 0.25), False),
    (f([a, a, a], a, 0.26, 0.25), False),
    (f([], a, 0.067, 0.25), False),
]
for i, (got, want) in enumerate(cases):
    if got is not want:
        print("case", i, "got", got, "want", want)
        sys.exit(1)
`

func TestPluginExistingIDMatch(t *testing.T) {
	py, err := exec.LookPath("python3")
	if err != nil {
		t.Skip("python3 is not on PATH")
	}
	plugin, err := filepath.Abs(filepath.Join("..", "..", "bundle", "beets-plugins", "wn_candidates.py"))
	if err != nil {
		t.Fatal(err)
	}
	out, err := exec.Command(py, "-I", "-c", pluginCheck, plugin).CombinedOutput()
	if err != nil {
		t.Fatalf("plugin check failed: %v\n%s", err, out)
	}
}
