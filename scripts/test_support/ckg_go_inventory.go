// Standalone syntax inventory for native Go exporters; not execution coverage.
package main

import (
	"encoding/json"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"strings"
)

type Point struct {
	Function string `json:"function"`
	Line     int    `json:"line"`
	Kind     string `json:"kind"`
	Code     string `json:"code"`
}
type File struct {
	Path   string  `json:"file"`
	Points []Point `json:"points"`
}

func main() {
	result := []File{}
	for _, path := range os.Args[1:] {
		if path == "--" {
			continue
		}
		source, err := os.ReadFile(path)
		if err != nil {
			panic(err)
		}
		set := token.NewFileSet()
		file, err := parser.ParseFile(set, path, source, 0)
		if err != nil {
			panic(err)
		}
		out := File{Path: path, Points: []Point{}}
		for _, decl := range file.Decls {
			fn, ok := decl.(*ast.FuncDecl)
			if !ok {
				continue
			}
			ast.Inspect(fn, func(node ast.Node) bool {
				if node == nil {
					return true
				}
				kind := ""
				switch n := node.(type) {
				case *ast.IfStmt:
					kind = "if"
				case *ast.CaseClause:
					kind = "case"
				case *ast.ForStmt, *ast.RangeStmt:
					kind = "loop"
				case *ast.BinaryExpr:
					if n.Op == token.LAND || n.Op == token.LOR {
						kind = "short-circuit"
					}
				case *ast.CallExpr:
					if s, ok := n.Fun.(*ast.SelectorExpr); ok {
						if x, ok := s.X.(*ast.Ident); ok && x.Name == "t" && (strings.HasPrefix(s.Sel.Name, "Fatal") || strings.HasPrefix(s.Sel.Name, "Error") || s.Sel.Name == "Run") {
							kind = "test-assertion"
						}
					}
				}
				if kind != "" {
					a, b := set.Position(node.Pos()).Offset, set.Position(node.End()).Offset
					if b-a > 500 {
						b = a + 500
					}
					out.Points = append(out.Points, Point{fn.Name.Name, set.Position(node.Pos()).Line, kind, string(source[a:b])})
				}
				return true
			})
		}
		result = append(result, out)
	}
	if err := json.NewEncoder(os.Stdout).Encode(result); err != nil {
		panic(err)
	}
}
