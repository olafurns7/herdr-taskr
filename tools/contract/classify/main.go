// Classify the compiled Go tests by resolved call reachability; regenerate after test edits.
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"go/ast"
	"go/importer"
	"go/parser"
	"go/token"
	"go/types"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

type packageInfo struct {
	ImportPath, Export, Dir string
	GoFiles, TestGoFiles    []string
}
type entry struct{ Name, Family, Ready, Group, Reason string }

func main() {
	packages := flag.String("packages", ".scratch/go-packages.json", "go list -deps -test -export -json output")
	output := flag.String("out", "testdata/contract/tests.tsv", "classification output")
	flag.Parse()
	f, err := os.Open(*packages)
	must(err)
	defer f.Close()
	exports := map[string]string{}
	var root packageInfo
	dec := json.NewDecoder(f)
	for {
		var p packageInfo
		err := dec.Decode(&p)
		if err == io.EOF {
			break
		}
		must(err)
		if p.Export != "" {
			exports[p.ImportPath] = p.Export
		}
		if p.ImportPath == "github.com/olafurns7/herdr-taskr" {
			root = p
		}
	}
	fset := token.NewFileSet()
	var files []*ast.File
	for _, name := range append(root.GoFiles, root.TestGoFiles...) {
		f, err := parser.ParseFile(fset, filepath.Join(root.Dir, name), nil, 0)
		must(err)
		files = append(files, f)
	}
	info := &types.Info{Defs: map[*ast.Ident]types.Object{}, Uses: map[*ast.Ident]types.Object{}}
	conf := types.Config{Importer: importer.ForCompiler(fset, "gc", func(path string) (io.ReadCloser, error) { return os.Open(exports[path]) })}
	pkg, err := conf.Check(root.ImportPath, fset, files, info)
	must(err)
	funcs := map[*types.Func]*ast.FuncDecl{}
	for _, f := range files {
		for _, d := range f.Decls {
			if fn, ok := d.(*ast.FuncDecl); ok {
				if obj, ok := info.Defs[fn.Name].(*types.Func); ok {
					funcs[obj] = fn
				}
			}
		}
	}
	position := func(o types.Object) string { return fset.Position(o.Pos()).Filename }
	isProduct := func(o types.Object) bool {
		return o != nil && o.Pkg() == pkg && !strings.HasSuffix(position(o), "_test.go")
	}
	seam := func(o *types.Func) bool {
		return o.Name() == "run" && o.Type().(*types.Signature).Recv() == nil || o.Name() == "cliMain" || o.Name() == "contractCLIMain" || o.Name() == "contractInvoke" || o.Name() == "contractCommand" || o.Name() == "buildTaskr"
	}
	var rows []entry
	for obj, fn := range funcs {
		if !strings.HasPrefix(obj.Name(), "Test") || obj.Name() == "TestMain" || fn.Recv != nil {
			continue
		}
		seen := map[*types.Func]bool{}
		reached := false
		fakes := map[string]bool{}
		blockers := map[string]bool{}
		var visit func(*types.Func)
		visit = func(o *types.Func) {
			if seen[o] {
				return
			}
			seen[o] = true
			if o.Name() == "startDaemon" {
				fakes["async/process lifecycle: replace with target-process fixture (cancellation and NI must return to test goroutine)"] = true
				return
			}
			if seam(o) {
				reached = true
				return
			}
			// SQL fixture setup is reusable across implementations; it never runs target commands.
			if o.Name() == "openDB" && strings.HasSuffix(position(o), "helpers_test.go") {
				return
			}
			if fixtureFunction(o) {
				return
			}
			if o.Name() == "contractGuard" || o.Name() == "contractOutcome" {
				return
			}
			if isProduct(o) && (o.Name() == "newDashboard" || o.Name() == "openDaemonLog" || strings.Contains(o.FullName(), ".dashboard)")) {
				fakes["Go HTTP/dashboard fixture: replace with standalone fixture server"] = true
				return
			}
			if isProduct(o) {
				blockers[o.FullName()] = true
				return
			}
			if body := funcs[o]; body != nil {
				ast.Inspect(body.Body, func(n ast.Node) bool {
					if call, ok := n.(*ast.CallExpr); ok {
						if selector, ok := call.Fun.(*ast.SelectorExpr); ok {
							if called, ok := info.Uses[selector.Sel].(*types.Func); ok && called.Pkg() != nil && called.Pkg().Path() == "os" && called.Name() == "Getpid" {
								fakes["self PID/signal probe: use selected target process PID"] = true
							}
							if called, ok := info.Uses[selector.Sel].(*types.Func); ok && called.Pkg() != nil && called.Pkg().Path() == "os/exec" && (called.Name() == "Command" || called.Name() == "CommandContext") {
								arg := 0
								if called.Name() == "CommandContext" {
									arg = 1
								}
								selected := false
								if len(call.Args) > arg {
									switch bin := call.Args[arg].(type) {
									case *ast.CallExpr:
										if id, ok := bin.Fun.(*ast.Ident); ok && id.Name == "buildTaskr" {
											selected = true
										}
									case *ast.SelectorExpr:
										selected = bin.Sel.Name == "bin"
									}
								}
								if !selected && o.Name() != "goRun" {
									fakes["subprocess fixture bypasses TASKR_BIN: needs target-process protocol"] = true
								}
							}
						}
					}
					if id, ok := n.(*ast.Ident); ok {
						used := info.Uses[id]
						switch used := used.(type) {
						case *types.Func:
							if used.Pkg() == pkg {
								visit(used)
							}
						case *types.Var:
							if isProduct(used) && used.Parent() == pkg.Scope() && used.Name() != "version" {
								hint := "move injected configuration to fixture server/target process"
								if used.Name() == "handoverNow" || used.Name() == "spoolNow" {
									hint = "replace clock hook with TASKR_FROZEN_NOW"
								}
								fakes["in-process global "+used.Name()+": "+hint] = true
							}
						case *types.TypeName:
							if isProduct(used) && !fixtureType(used.Name()) {
								blockers["type "+used.Name()] = true
							}
						}
					}
					return true
				})
			}
		}
		visit(obj)
		row := entry{Name: obj.Name(), Family: family(obj.Name(), filepath.Base(position(obj))), Ready: "no"}
		row.Group = "c"
		if len(fakes) > 0 {
			row.Group = "b"
			for reason := range fakes {
				blockers[reason] = true
			}
		}
		if !reached {
			blockers["no executable CLI seam"] = true
		}
		if len(blockers) > 0 {
			var reasons []string
			for b := range blockers {
				reasons = append(reasons, b)
			}
			sort.Strings(reasons)
			row.Reason = strings.Join(reasons, "; ")
		} else {
			row.Ready = "yes"
			row.Group = "a"
			row.Reason = "CLI through executable seam; Go SQL seed/read helpers and data types only"
		}
		// Generator tests must remain Go-only; Rust reads their checked-in artifacts.
		if strings.HasPrefix(row.Name, "TestContractGolden") || row.Name == "TestContractClassification" {
			row.Ready = "no"
			row.Group = "c"
			row.Family = "core"
			row.Reason = "Go artifact generator/source-of-truth assertions"
		}
		rows = append(rows, row)
	}
	sort.Slice(rows, func(i, j int) bool { return rows[i].Name < rows[j].Name })
	out, err := os.Create(*output)
	must(err)
	defer out.Close()
	fmt.Fprintln(out, "test\tfamily\tadapter-ready\tgroup\treason")
	counts := map[string]int{}
	for _, row := range rows {
		fmt.Fprintf(out, "%s\t%s\t%s\t%s\t%s\n", row.Name, row.Family, row.Ready, row.Group, row.Reason)
		counts[row.Ready]++
		counts[row.Group]++
	}
	fmt.Printf("classified %d tests: ready=%d internals=%d a=%d b=%d c=%d\n", len(rows), counts["yes"], counts["no"], counts["a"], counts["b"], counts["c"])
}

func family(name, file string) string {
	if file == "cmd_campaign_test.go" {
		return "read:campaign"
	}
	text := name
	if strings.Contains(name, "Documents") {
		switch {
		case strings.Contains(name, "Schema"):
			return "schema"
		case strings.Contains(name, "GetAndLs"):
			return "read:doc"
		case strings.Contains(name, "Handover"):
			return "read:handover"
		case strings.Contains(name, "Prompt"):
			return "write:prompt"
		case strings.Contains(name, "New"):
			return "write:new"
		case strings.Contains(name, "Reports"):
			return "write:ready"
		default:
			return "write:doc"
		}
	}
	for _, pair := range [][2]string{{"spool", "net:spool"}, {"discovery", "net:discovery"}, {"rpc", "net:rpc-client"}, {"daemon", "net:daemon"}, {"glance", "read:glance"}, {"search", "read:search"}, {"document", "read:doc"}, {"doc", "read:doc"}, {"version", "read:version"}, {"log", "read:log"}, {"status", "read:status"}, {"asks", "read:asks"}, {"answer", "write:answer"}, {"prompt", "write:prompt"}, {"ready", "write:ready"}, {"close", "write:close"}, {"done", "write:done"}, {"ack", "write:ack"}, {"wait", "write:wait"}, {"note", "write:note"}, {"ask", "write:ask"}, {"set", "write:set"}, {"new", "write:new"}, {"launch", "write:launch"}, {"got", "write:got"}, {"start", "write:start"}} {
		word := strings.ToUpper(pair[0][:1]) + pair[0][1:]
		if pair[0] == "rpc" {
			word = "RPC"
		}
		if strings.Contains(text, word) {
			return pair[1]
		}
	}
	for _, pair := range [][2]string{{"bounded_read", "read:log"}, {"glance", "read:glance"}, {"search", "read:search"}, {"documents", "read:doc"}, {"rpc", "net:rpc-client"}, {"spool", "net:spool"}, {"daemon", "net:daemon"}, {"glance", "read:glance"}, {"wait", "write:wait"}, {"notes", "write:note"}, {"asks", "read:asks"}} {
		if strings.Contains(file, pair[0]) {
			return pair[1]
		}
	}
	return "write:new"
}
func must(err error) {
	if err != nil {
		panic(err)
	}
}

// These helpers only seed/read the shared scratch SQL state; no CLI behavior is delegated to Go.
// ponytail: explicit reviewed allowlist, extend when a new SQL fixture helper is introduced.
func fixtureFunction(o *types.Func) bool {
	if o.Type().(*types.Signature).Recv() != nil {
		return false
	}
	switch o.Name() {
	case "stamp", "parseTime", "now", "withTx", "insertEvent", "loadEvent", "scanEvent", "loadTask", "latestDocument", "scanDocument", "storeDocument", "bodyDocument", "fileDocument", "setMeta", "getMeta", "ptr", "nullStr", "jsonText", "firstNonEmpty", "receiptDueKey", "hostHeartbeatKey", "exists":
		return true
	}
	return false
}
func fixtureType(name string) bool {
	switch name {
	case "event", "document", "documentInput", "task", "queryer", "receiptDue":
		return true
	}
	return false
}
