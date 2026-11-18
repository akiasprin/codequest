package main

import (
	"go/ast"
	"go/importer"
	"go/parser"
	"go/token"
	"go/types"
	"strings"
	"testing"
)

// These are compiler type-checked inputs, independent of the exporter's raw
// JSON. NativeProject tests separately exercise real go test variant loading.
func TestRunnableTestSignaturesMatchCompilerGraph(t *testing.T) {
	for _, tc := range []struct {
		name, file, source string
		want               bool
	}{
		{"test", "x_test.go", "func TestRun(t *testing.T) {}", true},
		{"main", "x_test.go", "func TestMain(m *testing.M) {}", true},
		{"benchmark", "x_test.go", "func BenchmarkRun(b *testing.B) {}", true},
		{"fuzz", "x_test.go", "func FuzzRun(f *testing.F) {}", true},
		{"alias", "x_test.go", "type T = testing.T; func TestAlias(t *T) {}", true},
		{"unicode_upper", "x_test.go", "func TestÄ(t *testing.T) {}", true},
		{"unicode_lower", "x_test.go", "func Testä(t *testing.T) {}", false},
		{"lower_suffix", "x_test.go", "func Testhelper(t *testing.T) {}", false},
		{"production_file", "x.go", "func TestRun(t *testing.T) {}", false},
		{"receiver", "x_test.go", "type X struct{}; func (X) TestMethod(t *testing.T) {}", false},
		{"result", "x_test.go", "func TestResult(t *testing.T) int { return 1 }", false},
		{"type_params", "x_test.go", "func TestGeneric[T any](t *testing.T) {}", false},
		{"not_pointer", "x_test.go", "func TestValue(t testing.T) {}", false},
		{"wrong_type", "x_test.go", "func TestWrong(b *testing.B) {}", false},
		{"wrong_main", "x_test.go", "func TestMain(t *testing.T) {}", false},
		{"extra_arg", "x_test.go", "func TestExtra(t *testing.T, x int) {}", false},
		{"no_arg", "x_test.go", "func TestEmpty() {}", false},
		{"example_output", "x_test.go", "func ExampleRun() {\n// Output: value\n}", true},
		{"example_empty_output", "x_test.go", "func ExampleEmpty() {\n// Output:\n}", true},
		{"example_no_output", "x_test.go", "func ExampleNoOutput() {}", false},
		{"example_params", "x_test.go", "func ExampleArg(t *testing.T) {}", false},
		{"ordinary", "x_test.go", "func Ordinary() {}", false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			fs := token.NewFileSet()
			imports := ""
			if strings.Contains(tc.source, "testing.") {
				imports = "import \"testing\"\n"
			}
			file, err := parser.ParseFile(fs, tc.file, "package fixture\n"+imports+tc.source, parser.ParseComments)
			if err != nil {
				t.Fatal(err)
			}
			info := &types.Info{Defs: map[*ast.Ident]types.Object{}}
			config := types.Config{Importer: importer.Default()}
			if _, err := config.Check("example.test/fixture", fs, []*ast.File{file}, info); err != nil {
				t.Fatal(err)
			}
			var found bool
			for _, decl := range file.Decls {
				if f, ok := decl.(*ast.FuncDecl); ok {
					found = true
					if got := isTestFunction(tc.file, file, f, info); got != tc.want {
						t.Fatalf("%s: got %v, want %v", f.Name, got, tc.want)
					}
				}
			}
			if !found {
				t.Fatal("fixture has no function")
			}
		})
	}
}
