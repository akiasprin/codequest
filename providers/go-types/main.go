// CodeQuest native Go semantic exporter. Only standard-library compiler APIs.
package main

import (
	"encoding/json"
	"fmt"
	"go/ast"
	"go/doc"
	"go/importer"
	"go/parser"
	"go/token"
	"go/types"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"unicode"
	"unicode/utf8"
)

type Package struct {
	ImportPath, Name, Dir, Export, ForTest      string
	ImportMap                                   map[string]string
	CompiledGoFiles, GoFiles, CgoFiles, Imports []string
	DepOnly, Standard                           bool
	Error                                       *struct{ Err string }
}
type Input struct {
	GOARCH   string
	Packages []Package
	Root     string
	Files    map[string]string
	Archives map[string]string
	Units    map[string]string
}
type Obj = map[string]any
type Export struct {
	Protocol                                                                string `json:"protocol"`
	Schema                                                                  int    `json:"schema_version"`
	Symbols, References, Parents, Relations, Bodies, Calls, Files, DocLinks []Obj
	Diagnostics                                                             []string
}

type capturedImporter struct {
	packages  map[string]*types.Package
	fallback  types.Importer
	importMap map[string]string
}

func (i *capturedImporter) Import(path string) (*types.Package, error) {
	if mapped := i.importMap[path]; mapped != "" {
		path = mapped
	}
	if p := i.packages[path]; p != nil {
		return p, nil
	}
	return i.fallback.Import(path)
}
func main() {
	if len(os.Args) != 3 {
		panic("cq-go-types INPUT.json OUTPUT.json")
	}
	b, e := os.ReadFile(os.Args[1])
	must(e)
	var in Input
	must(json.Unmarshal(b, &in))
	out := run(in)
	b, e = json.Marshal(out)
	must(e)
	must(os.WriteFile(os.Args[2], b, 0600))
}
func must(e error) {
	if e != nil {
		fmt.Fprintln(os.Stderr, e)
		os.Exit(2)
	}
}
func run(in Input) Obj {
	fs := token.NewFileSet()
	files := map[int]int{}
	rawFiles := []Obj{}
	symbols := []Obj{}
	references := []Obj{}
	parents := []Obj{}
	bodies := []Obj{}
	calls := []Obj{}
	relations := []Obj{}
	diagnostics := []string{}
	tokens := map[types.Object]int{}
	objects := map[types.Object]types.Object{}
	defs := map[types.Object]Obj{}
	fileID := func(pos token.Pos) int {
		if f := fs.File(pos); f != nil {
			return files[f.Base()]
		}
		return 0
	}
	span := func(a, b token.Pos) any {
		pa, pb := fs.PositionFor(a, false), fs.PositionFor(b, false)
		id := fileID(a)
		if id == 0 || fs.File(a) != fs.File(b) {
			return nil
		}
		return Obj{"file": id, "start": pa.Offset, "end": pb.Offset}
	}
	key := func(o types.Object) string {
		pkg := "builtin"
		if o.Pkg() != nil {
			pkg = o.Pkg().Path()
		}
		if f, ok := o.(*types.Func); ok {
			if f.Name() == "init" {
				return fmt.Sprintf("%s::init@%s", pkg, fs.PositionFor(f.Pos(), false))
			}
			sig, _ := f.Type().(*types.Signature)
			if sig != nil && sig.Recv() != nil {
				return pkg + "::" + types.TypeString(sig.Recv().Type(), func(p *types.Package) string { return p.Path() }) + "::" + o.Name()
			}
		}
		if o.Pkg() != nil && o.Parent() != o.Pkg().Scope() {
			return fmt.Sprintf("%s::%s@%s", pkg, o.Name(), fs.PositionFor(o.Pos(), false))
		}
		return pkg + "::" + o.Name()
	}
	canonical := func(o types.Object) types.Object {
		switch object := o.(type) {
		case *types.Func:
			return object.Origin()
		case *types.Var:
			return object.Origin()
		}
		return o
	}
	owners := map[types.Object]types.Object{}
	ensure := func(o types.Object) int {
		o = canonical(o)
		if n := tokens[o]; n != 0 {
			return n
		}
		n := len(tokens) + 1
		tokens[o] = n
		objects[o] = o
		return n
	}
	lookup := func(path string) (io.ReadCloser, error) {
		p := in.Archives[path]
		if p == "" {
			return nil, fmt.Errorf("missing captured export archive for %s", path)
		}
		return os.Open(p)
	}
	imp := &capturedImporter{packages: map[string]*types.Package{}, fallback: importer.ForCompiler(fs, "gc", lookup)}
	allNamed := map[*types.TypeName]*types.Named{}
	comments := []Obj{}
	parameters := []Obj{}
	for _, p := range in.Packages {
		if p.Standard || in.Units[p.ImportPath] == "" {
			continue
		}
		var parsed []*ast.File
		paths := p.CompiledGoFiles
		if len(paths) == 0 {
			paths = p.GoFiles
		}
		for _, path := range paths {
			if !filepath.IsAbs(path) {
				path = filepath.Join(p.Dir, path)
			}
			materialized := in.Files[path]
			if materialized == "" {
				diagnostics = append(diagnostics, "uncaptured Go input: "+path)
				continue
			}
			b, e := os.ReadFile(materialized)
			if e != nil {
				diagnostics = append(diagnostics, e.Error())
				continue
			}
			f, e := parser.ParseFile(fs, path, b, parser.ParseComments|parser.AllErrors)
			if f != nil {
				native := fs.File(f.Pos())
				id := len(rawFiles) + 1
				files[native.Base()] = id
				rawFiles = append(rawFiles, Obj{"id": id, "path": path, "text": string(b), "unit": in.Units[p.ImportPath]})
			}
			if e != nil {
				diagnostics = append(diagnostics, e.Error())
			}
			if f != nil {
				parsed = append(parsed, f)
			}
		}
		info := &types.Info{Types: map[ast.Expr]types.TypeAndValue{}, Defs: map[*ast.Ident]types.Object{}, Uses: map[*ast.Ident]types.Object{}, Selections: map[*ast.SelectorExpr]*types.Selection{}, Implicits: map[ast.Node]types.Object{}, Instances: map[*ast.Ident]types.Instance{}}
		conf := types.Config{Sizes: types.SizesFor("gc", in.GOARCH), Importer: imp, Error: func(e error) { diagnostics = append(diagnostics, e.Error()) }}
		imp.importMap = p.ImportMap
		pkgPath := strings.Split(p.ImportPath, " [")[0]
		checked, checkErr := conf.Check(pkgPath, fs, parsed, info)
		must(checkErr)
		if checked != nil {
			imp.packages[p.ImportPath] = checked
		}
		extents := map[types.Object]ast.Node{}
		docs := map[types.Object]*ast.CommentGroup{}
		for _, f := range parsed {
			if f.Doc != nil {
				comments = append(comments, Obj{"source": span(f.Doc.Pos(), f.Doc.End()), "text": f.Doc.Text(), "kind": "GoDoc"})
			}
			ast.Inspect(f, func(n ast.Node) bool {
				switch n := n.(type) {
				case *ast.FuncDecl:
					slots := []Obj{}
					index := 0
					for _, field := range n.Type.Params.List {
						if len(field.Names) == 0 {
							slots = append(slots, Obj{"index": index, "receiver": false, "bindings": []Obj{}})
							index++
							continue
						}
						for _, name := range field.Names {
							slots = append(slots, Obj{"index": index, "receiver": false, "bindings": []any{span(name.Pos(), name.End())}})
							index++
						}
					}
					parameters = append(parameters, Obj{"owner": span(n.Name.Pos(), n.Name.End()), "slots": slots})
					if o := info.Defs[n.Name]; o != nil {
						extents[o] = n
						docs[o] = n.Doc
					}
					if n.Body != nil {
						bodies = append(bodies, Obj{"kind": "function", "source": span(n.Pos(), n.End()), "block": span(n.Body.Pos(), n.Body.End()), "test": isTestFunction(fs.PositionFor(n.Pos(), false).Filename, f, n, info)})
					}
				case *ast.FuncLit:
					bodies = append(bodies, Obj{"kind": "closure", "source": span(n.Pos(), n.End())})
				case *ast.TypeSpec:
					if o := info.Defs[n.Name]; o != nil {
						extents[o] = n
						docs[o] = n.Doc
					}
				case *ast.CallExpr:
					if tv, ok := info.Types[n.Fun]; ok && tv.IsType() {
						break
					}
					var callee ast.Node = n.Fun
					if s, ok := n.Fun.(*ast.SelectorExpr); ok {
						callee = s.Sel
					}
					args := []any{}
					for _, a := range n.Args {
						args = append(args, span(a.Pos(), a.End()))
					}
					calls = append(calls, Obj{"callee": span(callee.Pos(), callee.End()), "source": span(n.Pos(), n.End()), "arguments": args})
				}
				return true
			})
		}
		for id, o := range info.Defs {
			if o == nil || o.Name() == "_" {
				continue
			}
			o = canonical(o)
			ensure(o)
			v := Obj{"definition": span(id.Pos(), id.End())}
			if n := extents[o]; n != nil {
				v["body"] = span(n.Pos(), n.End())
			}
			if doc := docs[o]; doc != nil {
				v["documentation"] = doc.Text()
				v["documentation_source"] = span(doc.Pos(), doc.End())
			}
			defs[o] = v
		}
		for node, object := range info.Implicits {
			if object == nil || object.Name() == "_" {
				continue
			}
			object = canonical(object)
			if spec, ok := node.(*ast.ImportSpec); ok {
				ensure(object)
				defs[object] = Obj{"definition": span(spec.Path.Pos(), spec.Path.End())}
			}
		}
		callNames := map[*ast.Ident]bool{}
		var callIdent func(ast.Expr) *ast.Ident
		callIdent = func(e ast.Expr) *ast.Ident {
			switch x := e.(type) {
			case *ast.Ident:
				return x
			case *ast.SelectorExpr:
				return x.Sel
			case *ast.ParenExpr:
				return callIdent(x.X)
			case *ast.IndexExpr:
				return callIdent(x.X)
			case *ast.IndexListExpr:
				return callIdent(x.X)
			}
			return nil
		}
		for _, f := range parsed {
			ast.Inspect(f, func(n ast.Node) bool {
				if c, ok := n.(*ast.CallExpr); ok {
					if id := callIdent(c.Fun); id != nil {
						callNames[id] = true
					}
				}
				return true
			})
		}
		for id, o := range info.Uses {
			o = canonical(o)
			role := "Reference"
			if _, typeConversion := o.(*types.TypeName); typeConversion {
				role = "Type"
			} else if callNames[id] {
				role = "Call"
			} else {
				switch o.(type) {
				case *types.TypeName:
					role = "Type"
				case *types.PkgName:
					role = "Import"
				case *types.Var:
					if o.(*types.Var).IsField() {
						role = "Access"
					}
				}
			}
			references = append(references, Obj{"target": ensure(o), "source": span(id.Pos(), id.End()), "kind": role, "is_definition": false})
		}
		for _, o := range info.Defs {
			if t, ok := o.(*types.TypeName); ok {
				if n, ok := t.Type().(*types.Named); ok {
					allNamed[n.Obj()] = n
				}
			}
		}
	}
	for _, t := range allNamed {
		iface, ok := t.Underlying().(*types.Interface)
		if !ok {
			continue
		}
		iface.Complete()
		for _, impl := range allNamed {
			if _, ok := impl.Underlying().(*types.Interface); ok {
				continue
			}
			if types.Implements(impl, iface) || types.Implements(types.NewPointer(impl), iface) {
				from, to := impl.Obj(), t.Obj()
				ensure(from)
				ensure(to)
				relations = append(relations, Obj{"from": key(from), "to": key(to), "file": fileID(from.Pos()), "to_file": fileID(to.Pos()), "kind": "Implements"})
				// The compiler's method set identifies possible implementations,
				// including promoted pointer methods. It does not select the
				// receiver used by an interface call at runtime.
				for i := 0; i < iface.NumMethods(); i++ {
					abstract := iface.Method(i)
					concrete, _, _ := types.LookupFieldOrMethod(types.NewPointer(impl), true, abstract.Pkg(), abstract.Name())
					if concrete == nil {
						continue
					}
					ensure(abstract)
					ensure(concrete)
					relations = append(relations, Obj{"from": key(abstract), "to": key(concrete), "file": fileID(abstract.Pos()), "to_file": fileID(concrete.Pos()), "kind": "Dispatches"})
				}
			}
		}
	}

	// Method objects live on named types, not in package.Scope(). Generic
	// selections are canonicalized to their declared Origin, preserving one
	// declaration while retaining each use occurrence.
	attachMember := func(child types.Object, owner *types.TypeName, kind string) {
		child = canonical(child)
		owners[child] = owner
		parents = append(parents, Obj{"child": ensure(child), "parent_token": ensure(owner), "key": key(owner), "name": owner.Name(), "kind": kind, "source": defs[owner]["definition"]})
	}
	for _, named := range allNamed {
		n := named.Origin()
		owner := n.Obj()
		for i := 0; i < n.NumMethods(); i++ {
			attachMember(n.Method(i), owner, "Struct")
		}
		switch underlying := n.Underlying().(type) {
		case *types.Struct:
			for i := 0; i < underlying.NumFields(); i++ {
				attachMember(underlying.Field(i), owner, "Struct")
			}
		case *types.Interface:
			for i := 0; i < underlying.NumExplicitMethods(); i++ {
				attachMember(underlying.ExplicitMethod(i), owner, "Interface")
			}
		}
	}
	var qualified func(types.Object) string
	qualified = func(o types.Object) string {
		o = canonical(o)
		if parent := owners[o]; parent != nil {
			return qualified(parent) + "::" + o.Name()
		}
		pkg := "builtin"
		if o.Pkg() != nil {
			pkg = o.Pkg().Path()
		}
		return pkg + "::" + o.Name()
	}

	keys := make([]types.Object, 0, len(tokens))
	for k := range tokens {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(i, j int) bool { return key(keys[i]) < key(keys[j]) })
	for _, object := range keys {
		o := objects[object]
		k := key(o)
		kind := "Symbol"
		trait := false
		switch o := o.(type) {
		case *types.Func:
			kind = "Function"
			if sig, ok := o.Type().(*types.Signature); ok && sig.Recv() != nil {
				kind = "Method"
				_, trait = sig.Recv().Type().Underlying().(*types.Interface)
			}
		case *types.TypeName:
			kind = "Type"
			if o.IsAlias() {
				kind = "TypeAlias"
				break
			}
			switch o.Type().Underlying().(type) {
			case *types.Struct:
				kind = "Struct"
			case *types.Interface:
				kind = "Interface"
			}
		case *types.Var:
			kind = "Variable"
			if o.IsField() {
				kind = "Field"
			}
		case *types.Const:
			kind = "Constant"
		case *types.PkgName:
			kind = "Module"
		}
		d := defs[o]
		v := Obj{"token": tokens[o], "key": k, "name": o.Name(), "qualified_name": qualified(o), "kind": kind, "signature": types.ObjectString(o, func(p *types.Package) string { return p.Path() }), "external": d == nil, "exported": o.Exported(), "trait_member": trait, "local": kind == "Variable" || kind == "Parameter"}
		typ := o.Type()
		if sig, ok := typ.(*types.Signature); ok {
			if sig.Results().Len() == 1 {
				typ = sig.Results().At(0).Type()
			} else {
				typ = nil
			}
		}
		if pointer, ok := typ.(*types.Pointer); ok {
			typ = pointer.Elem()
		}
		if named, ok := typ.(*types.Named); ok {
			if target := defs[named.Obj()]; target != nil {
				v["type_definitions"] = []Obj{{"source": target["definition"], "body": target["body"]}}
			}
		}

		for key, val := range d {
			v[key] = val
		}
		symbols = append(symbols, v)
	}
	return Obj{"protocol": "codequest-hir-export", "schema_version": 1, "provider": "go/types", "version": runtime.Version(), "symbols": symbols, "references": references, "parents": parents, "relations": relations, "bodies": bodies, "calls": calls, "files": rawFiles, "doc_links": []Obj{}, "comments": comments, "parameters": parameters, "diagnostics": diagnostics}
}

// Same signature/name/output checks as CKG's go/packages producer, evaluated
// against the actual compiler-selected syntax and go/types declarations.
func isTestFunction(file string, syntax *ast.File, declaration *ast.FuncDecl, info *types.Info) bool {
	if !strings.HasSuffix(file, "_test.go") {
		return false
	}
	object, ok := info.Defs[declaration.Name].(*types.Func)
	if !ok {
		return false
	}
	signature, ok := object.Type().(*types.Signature)
	if !ok || signature.Recv() != nil || signature.Results().Len() != 0 || signature.TypeParams() != nil && signature.TypeParams().Len() != 0 {
		return false
	}
	name := declaration.Name.Name
	if validTestName(name, "Example") {
		if signature.Params().Len() != 0 {
			return false
		}
		for _, example := range doc.Examples(syntax) {
			if example.Name == strings.TrimPrefix(name, "Example") && (example.Output != "" || example.EmptyOutput) {
				return true
			}
		}
		return false
	}
	expected := ""
	switch {
	case name == "TestMain":
		expected = "M"
	case validTestName(name, "Test"):
		expected = "T"
	case validTestName(name, "Benchmark"):
		expected = "B"
	case validTestName(name, "Fuzz"):
		expected = "F"
	default:
		return false
	}
	if signature.Params().Len() != 1 {
		return false
	}
	typ := types.Unalias(signature.Params().At(0).Type())
	if ptr, ok := typ.(*types.Pointer); ok {
		typ = ptr.Elem()
	} else {
		return false
	}
	named, ok := types.Unalias(typ).(*types.Named)
	return ok && named.Obj().Pkg() != nil && named.Obj().Pkg().Path() == "testing" && named.Obj().Name() == expected
}
func validTestName(name, prefix string) bool {
	if !strings.HasPrefix(name, prefix) {
		return false
	}
	if len(name) == len(prefix) {
		return true
	}
	first, _ := utf8.DecodeRuneInString(name[len(prefix):])
	return !unicode.IsLower(first)
}
