//go:build !taskr_contract

package main

import (
	"go/ast"
	"go/build"
	"go/parser"
	"go/token"
	"os"
	"strings"
	"testing"
	"time"
)

func TestContractClassification(t *testing.T) {
	contractGuard(t)
	data, err := os.ReadFile("testdata/contract/tests.tsv")
	if err != nil {
		t.Fatal(err)
	}
	classes := map[string]bool{}
	for _, line := range strings.Split(strings.TrimSpace(string(data)), "\n")[1:] {
		fields := strings.SplitN(line, "\t", 5)
		if len(fields) != 5 || fields[1] == "" || fields[4] == "" || (fields[3] != "a" && fields[3] != "b" && fields[3] != "c") || (fields[2] != "yes" && fields[2] != "no") {
			t.Fatal("invalid classification", line)
		}
		if (fields[2] == "yes") != (fields[3] == "a") {
			t.Fatal("adapter readiness disagrees with group", line)
		}
		if classes[fields[0]] {
			t.Fatal("duplicate classification", fields[0])
		}
		classes[fields[0]] = true
	}
	pkg, err := build.Default.ImportDir(".", 0)
	if err != nil {
		t.Fatal(err)
	}
	fset := token.NewFileSet()
	for _, name := range pkg.TestGoFiles {
		file, err := parser.ParseFile(fset, name, nil, 0)
		if err != nil {
			t.Fatal(err)
		}
		for _, decl := range file.Decls {
			fn, ok := decl.(*ast.FuncDecl)
			if !ok || fn.Recv != nil || !strings.HasPrefix(fn.Name.Name, "Test") || fn.Name.Name == "TestMain" {
				continue
			}
			if !classes[fn.Name.Name] {
				t.Fatal("test missing from classification", fn.Name.Name)
			}
			delete(classes, fn.Name.Name)
			if len(fn.Body.List) == 0 {
				t.Fatal("missing guard", fn.Name.Name)
			}
			stmt, ok := fn.Body.List[0].(*ast.ExprStmt)
			if !ok {
				t.Fatal("first statement is not guard", fn.Name.Name)
			}
			call, ok := stmt.X.(*ast.CallExpr)
			if !ok {
				t.Fatal("first statement is not guard", fn.Name.Name)
			}
			ident, ok := call.Fun.(*ast.Ident)
			if !ok || ident.Name != "contractGuard" {
				t.Fatal("first call is not guard", fn.Name.Name)
			}
		}
	}
	if len(classes) != 0 {
		t.Fatal("stale classifications", classes)
	}
}

func TestContractGoldenProductionClock(t *testing.T) {
	contractGuard(t)
	t.Setenv("TASKR_FROZEN_NOW", "invalid timestamp that production must ignore")
	before := time.Now()
	observed := clockNow()
	after := time.Now()
	if observed.Before(before) || observed.After(after) {
		t.Fatal("production clock read contract environment", observed)
	}
}
