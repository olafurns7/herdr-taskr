package main

import (
	"bytes"
	"database/sql"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// stateFiles lists every file under home's taskr state directory.
func stateFiles(t *testing.T, home string) []string {
	t.Helper()
	var out []string
	filepath.WalkDir(home, func(p string, d fs.DirEntry, err error) error {
		if err == nil && !d.IsDir() {
			rel, _ := filepath.Rel(home, p)
			out = append(out, rel)
		}
		return nil
	})
	return out
}

func TestRPCModes(t *testing.T) {
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 120 * time.Millisecond })
	fakeTailnetHooks(t) // [::1] counts as a tailnet address
	h := newHarness(t)
	dead := "http://[::1]:1"
	// TASKR_DB set: local, whatever server.url says.
	home := t.TempDir()
	os.MkdirAll(filepath.Join(home, ".local", "state", "taskr"), 0o755)
	os.WriteFile(filepath.Join(home, ".local", "state", "taskr", serverURLFile), []byte(dead+"\n"), 0o644)
	var out, errb bytes.Buffer
	env := func(k string) string {
		switch k {
		case "TASKR_DB":
			return h.db
		case "HOME":
			return home
		}
		return ""
	}
	if code := cliMain([]string{"--json", "new", "local", "--role", "gate"}, env, &out, &errb); code != 0 {
		t.Fatalf("TASKR_DB with server.url = %d %s", code, out.String())
	}

	// server.url without TASKR_DB: help, every --help and version run offline;
	// everything else goes to the server, here unreachable. No ledger file
	// is ever created or opened.
	for _, args := range [][]string{{"help"}, {"help", "wait"}, {"wait", "--help"}, {"new", "-h"}, {"version"}, {"status"},
		{"note", "x", "--as", "1"}, {"bogus"}, {}} {
		out.Reset()
		errb.Reset()
		code := cliMain(args, clientEnv(home, nil), &out, &errb)
		switch {
		case len(args) == 0 || args[0] == "bogus":
			if code != exitUsage {
				t.Errorf("%v = %d", args, code)
			}
		case args[0] == "note":
			if code != 0 || !strings.HasPrefix(strings.TrimSpace(out.String()), "qd1 ") || !strings.Contains(errb.String(), "server unreachable; queued (1 waiting)") {
				t.Errorf("%v = %d %q %q", args, code, out.String(), errb.String())
			}
		case args[0] == "status":
			if code != exitHerdr || !strings.Contains(errb.String(), "server unreachable") {
				t.Errorf("%v = %d %q", args, code, errb.String())
			}
		case code != 0:
			t.Errorf("%v offline = %d %s %s", args, code, out.String(), errb.String())
		case args[0] == "version" && !strings.Contains(out.String(), `"server":"`+dead+`"`):
			t.Errorf("version = %s", out.String())
		}
	}
	if files := stateFiles(t, home); len(files) != 3 || files[0] != filepath.Join(".local", "state", "taskr", serverURLFile) || !strings.Contains(filepath.ToSlash(files[1]), "/spool/lock") || !strings.Contains(filepath.ToSlash(files[2]), "/spool/queue/") {
		t.Fatalf("client mode left unexpected files: %v", files)
	}
	if _, err := os.Stat(filepath.Join(home, ".local", "state", "taskr", "taskr.db")); !os.IsNotExist(err) {
		t.Fatalf("client mode created local ledger: %v", err)
	}
	// A server.url that is not a tailnet address is refused, never local.
	bad := t.TempDir()
	os.MkdirAll(filepath.Join(bad, ".local", "state", "taskr"), 0o755)
	os.WriteFile(filepath.Join(bad, ".local", "state", "taskr", serverURLFile), []byte("http://192.168.1.5:7788\n"), 0o644)
	out.Reset()
	if code := cliMain([]string{"status"}, clientEnv(bad, nil), &out, &errb); code != exitUsage || !strings.Contains(out.String(), "server.url refused") {
		t.Fatalf("non-tailnet server.url = %d %s", code, out.String())
	}

	// hub.url alone is local mode.
	hubOnly := t.TempDir()
	os.MkdirAll(filepath.Join(hubOnly, ".local", "state", "taskr"), 0o755)
	os.WriteFile(filepath.Join(hubOnly, ".local", "state", "taskr", hubURLFile), []byte(dead+"\n"), 0o644)
	out.Reset()
	if code := cliMain([]string{"--json", "new", "local", "--role", "gate"}, clientEnv(hubOnly, nil), &out, &errb); code != 0 {
		t.Fatalf("hub.url only = %d %s", code, out.String())
	}
	if _, err := os.Stat(filepath.Join(hubOnly, ".local", "state", "taskr", "taskr.db")); err != nil {
		t.Fatalf("hub.url only did not use the local ledger: %v", err)
	}
	// --request-key belongs to client mode.
	h.one(exitUsage, nil, "--request-key", "local-key-1", "status")
}

func TestRPCBudget(t *testing.T) {
	for _, c := range []struct {
		argv []string
		want time.Duration
	}{
		{[]string{"wait", "--as", "1"}, 540 * time.Second},
		{[]string{"--json", "wait", "--as", "1", "--timeout", "20000"}, 20 * time.Second},
		{[]string{"wait", "--timeout=5000", "--as", "1"}, 5 * time.Second},
		{[]string{"prompt", "3", "--text", "x"}, 30 * time.Second},
		{[]string{"prompt", "3", "--text", "x", "--confirm"}, 90 * time.Second},
		{[]string{"prompt", "3", "--text", "x", "--confirm", "--confirm-timeout", "1000"}, 31 * time.Second},
		{[]string{"answer", "4", "y", "--prompt", "--confirm", "--confirm-timeout=2000"}, 32 * time.Second},
		{[]string{"answer", "4", "y"}, 30 * time.Second},
		{[]string{"note", "--timeout"}, 30 * time.Second},
		{[]string{"wait", "--help", "--timeout", "900000"}, 30 * time.Second},
	} {
		if got := rpcBudget(c.argv); got != c.want {
			t.Errorf("rpcBudget(%v) = %v, want %v", c.argv, got, c.want)
		}
	}
}

func TestRPCRepeatedFlagTokens(t *testing.T) {
	for name := range rpcScalarFlags {
		args := []string{"--" + name}
		if !rpcBoolFlags[name] {
			args = append(args, "first")
		}
		args = append(args, "--"+name)
		if got := repeatedRPCFlag(args); got != name {
			t.Errorf("repeatedRPCFlag(%v) = %q, want %q", args, got, name)
		}
	}
	for _, args := range [][]string{
		{"--cwd", "--machine", "/path"}, // --machine is cwd's consumed value
		{"--cwd", "/one", "--", "--cwd", "/two"},
	} {
		if got := repeatedRPCFlag(args); got != "" {
			t.Errorf("repeatedRPCFlag(%v) = %q, want no repeated flag", args, got)
		}
	}
	if got := repeatedRPCFlag([]string{"--cwd=/one", "-cwd=/two"}); got != "cwd" {
		t.Errorf("inline repeated cwd = %q, want cwd", got)
	}
}

func dumpTable(t *testing.T, db *sql.DB, table string, cols string) []string {
	t.Helper()
	rows, err := db.Query(`select ` + cols + ` from ` + table + ` order by 1`)
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()
	n := len(strings.Split(cols, ","))
	var out []string
	for rows.Next() {
		v := make([]any, n)
		p := make([]any, n)
		for i := range v {
			p[i] = &v[i]
		}
		rows.Scan(p...)
		out = append(out, fmt.Sprint(v...))
	}
	return out
}

func TestRPCMigration(t *testing.T) {
	h := newHarness(t)
	// A v0.9.1 ledger with rows, written through the literal 8715071 schema.
	db, err := sql.Open("sqlite", "file:"+h.db+"?_pragma=foreign_keys(1)")
	if err != nil {
		t.Fatal(err)
	}
	os.MkdirAll(filepath.Dir(h.db), 0o755)
	schema, err := os.ReadFile(filepath.Join("testdata", "taskr-v0.9.1-schema.sql"))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(string(schema)); err != nil {
		t.Fatal(err)
	}
	ts := now()
	for _, q := range []string{
		`insert into tasks (id, name, role, status, cwd, acked_event_id, pending_event_id, created_at, updated_at) values (1, 'top', 'orchestrator', 'open', '/', 2, 3, '` + ts + `', '` + ts + `')`,
		`insert into tasks (id, parent_id, name, role, status, cwd, current_launch_id, created_at, updated_at) values (2, 1, 'impl', 'implementer', 'open', '/', 1, '` + ts + `', '` + ts + `')`,
		`insert into launches (id, task_id, provider, model, effort, pane_id, recorded_at) values (1, 2, 'claude', 'm', 'high', 'w1:p1', '` + ts + `')`,
		`insert into events (id, task_id, recipient_task_id, launch_id, kind, summary, created_at) values (2, 2, 1, 1, 'ready', 'r', '` + ts + `')`,
		`insert into events (id, task_id, recipient_task_id, launch_id, kind, summary, created_at) values (3, 2, 1, 1, 'ask', 'q', '` + ts + `')`,
		`insert into meta (key, value) values ('k', 'v')`,
	} {
		if _, err := db.Exec(q); err != nil {
			t.Fatal(err)
		}
	}
	oldCols := map[string]string{
		"tasks":    "id, parent_id, name, role, status, cwd, current_launch_id, acked_event_id, pending_event_id, created_at, updated_at",
		"launches": "id, task_id, provider, model, effort, pane_id, recorded_at",
		"events":   "id, task_id, recipient_task_id, launch_id, kind, summary, created_at",
		"meta":     "key, value",
	}
	before := map[string][]string{}
	for table, cols := range oldCols {
		before[table] = dumpTable(t, db, table, cols)
	}
	db.Close()

	// The v0.10 open migrates: new columns, the requests table, rows unchanged.
	ndb := h.openDB()
	for table, cols := range oldCols {
		if got := dumpTable(t, ndb, table, cols); fmt.Sprint(got) != fmt.Sprint(before[table]) {
			t.Fatalf("%s changed:\n%v\n%v", table, before[table], got)
		}
	}
	if n := len(dumpTable(t, ndb, "tasks", "id")); n != 2 || dumpTable(t, ndb, "tasks", "machine")[0] != "<nil>" ||
		dumpTable(t, ndb, "launches", "machine")[0] != "<nil>" || len(dumpTable(t, ndb, "requests", "key")) != 0 {
		t.Fatal("migrated columns or table missing")
	}
	var check string
	ndb.QueryRow(`pragma integrity_check`).Scan(&check)
	if check != "ok" {
		t.Fatalf("integrity_check = %s", check)
	}
	// Opening again is idempotent, and the local CLI works on it.
	h.ok(as(2, 1), "note", "after the migration")

	t.Run("v0.9.1 binary compatibility", func(t *testing.T) {
		if err := exec.Command("git", "cat-file", "-e", "8715071^{commit}").Run(); err != nil {
			t.Skip("commit 8715071 is unavailable in this clone")
		}
		old := buildV091(t)
		cmd := exec.Command(old, "--json", "note", "from v0.9.1")
		cmd.Env = []string{"TASKR_DB=" + h.db, "HOME=" + h.dir, "TASKR_TASK=2", "TASKR_LAUNCH=1", "PATH=" + os.Getenv("PATH")}
		if b, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("v0.9.1 note: %v %s", err, b)
		}
		cmd = exec.Command(old, "--json", "status")
		cmd.Env = []string{"TASKR_DB=" + h.db, "HOME=" + h.dir, "PATH=" + os.Getenv("PATH")}
		if b, err := cmd.CombinedOutput(); err != nil || !strings.Contains(string(b), `"impl"`) {
			t.Fatalf("v0.9.1 status: %v %s", err, b)
		}
	})
}

// buildV091 builds taskr at 8715071 (v0.9.1) from this repository's history.
func buildV091(t *testing.T) string {
	t.Helper()
	src := t.TempDir()
	archive := exec.Command("sh", "-c", `git archive 8715071 | tar -x -C "$1"`, "sh", src)
	if b, err := archive.CombinedOutput(); err != nil {
		t.Fatalf("git archive 8715071 (the v0.9.1 build needs this repository's history): %v %s", err, b)
	}
	bin := filepath.Join(t.TempDir(), "taskr-v0.9.1")
	build := exec.Command("go", "build", "-o", bin, ".")
	build.Dir = src
	if b, err := build.CombinedOutput(); err != nil {
		t.Fatalf("go build v0.9.1: %v %s", err, b)
	}
	return bin
}
