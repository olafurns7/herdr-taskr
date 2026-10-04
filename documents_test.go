package main

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"
)

func docFile(t *testing.T, dir, name, body string) string {
	t.Helper()
	path := filepath.Join(dir, name)
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(body), 0600); err != nil {
		t.Fatal(err)
	}
	return path
}
func docExec(t *testing.T, db *sql.DB, query string, args ...any) {
	t.Helper()
	if _, err := db.Exec(query, args...); err != nil {
		t.Fatal(err)
	}
}
func docCount(t *testing.T, db *sql.DB, query string, args ...any) int {
	t.Helper()
	var n int
	if err := db.QueryRow(query, args...).Scan(&n); err != nil {
		t.Fatal(err)
	}
	return n
}
func docLatest(t *testing.T, db *sql.DB, taskID int64, kind, name string) document {
	t.Helper()
	d, err := latestDocument(db, taskID, kind, name)
	if err != nil {
		t.Fatal(err)
	}
	return d
}
func docLane(t *testing.T, h *harness) (int64, int64, int64) {
	t.Helper()
	root := h.newTask("root", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", root, "--pane", "w1:p1")
	launch := h.launch(lane)
	h.write("prompt.stdout", `{"result":{"agent":{"agent_status":"working"}}}`, 0644)
	return root, lane, launch
}

// 1. Schema creation is additive, including a pre-document ledger.
func TestDocumentsSchema(t *testing.T) {
	h := newHarness(t)
	db := h.openDB()
	if n := docCount(t, db, `select count(*) from sqlite_master where type = 'table' and name in ('documents', 'doc_blobs')`); n != 2 {
		t.Fatal(n)
	}
	root := h.newTask("root", "orchestrator", 0)
	docExec(t, db, `drop table documents`)
	docExec(t, db, `drop table doc_blobs`)
	reopened := h.openDB()
	if n := docCount(t, reopened, `select count(*) from sqlite_master where type = 'table' and name in ('documents', 'doc_blobs')`); n != 2 {
		t.Fatal(n)
	}
	if _, err := loadTask(reopened, root); err != nil {
		t.Fatal(err)
	}
}

// 2. File prompt captures the already-read bytes and versions only changes.
func TestDocumentsPromptFile(t *testing.T) {
	h := newHarness(t)
	_, lane, _ := docLane(t, h)
	db := h.openDB()
	path := docFile(t, h.dir, "brief.MARKDOWN", "brief one\n")
	docExec(t, db, `update tasks set brief_path = ? where id = ?`, path, lane)
	first := h.ok(nil, "prompt", id(lane), "--file", path, "--receipt-timeout", "0")
	d := docLatest(t, db, lane, "brief", "")
	if !d.Captured || d.Version != 1 || d.Format.String != "md" || d.EventID.Int64 != num(first, "attempt_id") {
		t.Fatalf("%+v", d)
	}
	h.ok(nil, "prompt", id(lane), "--file", path, "--receipt-timeout", "0")
	if n := docCount(t, db, `select count(*) from documents`); n != 1 {
		t.Fatal(n)
	}
	docFile(t, h.dir, "brief.MARKDOWN", "brief two\n")
	next := h.ok(nil, "prompt", id(lane), "--file", path, "--receipt-timeout", "0")
	if d := docLatest(t, db, lane, "brief", ""); d.Version != 2 || d.EventID.Int64 != num(next, "attempt_id") {
		t.Fatalf("%+v", d)
	}
	other := docFile(t, h.dir, "other.txt", "other")
	h.ok(nil, "prompt", id(lane), "--file", other, "--receipt-timeout", "0")
	if d := docLatest(t, db, lane, "prompt", "other.txt"); !d.Captured || d.Format.String != "text" {
		t.Fatalf("%+v", d)
	}
}

// 3. Literal prompts are text and have an empty name.
func TestDocumentsPromptText(t *testing.T) {
	h := newHarness(t)
	_, lane, _ := docLane(t, h)
	body := "literal\ntext λ"
	h.ok(nil, "prompt", id(lane), "--text", body, "--receipt-timeout", "0")
	d := docLatest(t, h.openDB(), lane, "prompt", "")
	if !d.Captured || d.Format.String != "text" {
		t.Fatalf("%+v", d)
	}
	code, out, _ := h.runText(nil, "doc", "get", id(d.ID))
	if code != 0 || out != body {
		t.Fatalf("%d %q", code, out)
	}
}

// 4. Every terminal capture re-reads the latest ready path.
func TestDocumentsReports(t *testing.T) {
	for _, terminal := range []string{"done", "fail", "close"} {
		t.Run(terminal, func(t *testing.T) {
			h := newHarness(t)
			_, lane, launch := docLane(t, h)
			db := h.openDB()
			fallback := docFile(t, h.dir, "fallback.md", "fallback")
			explicit := docFile(t, h.dir, "report.md", "report one")
			docExec(t, db, `update tasks set report_path = ? where id = ?`, fallback, lane)
			h.ok(as(lane, launch), "ready", "fallback")
			if d := docLatest(t, db, lane, "report", ""); d.Path.String != fallback {
				t.Fatalf("%+v", d)
			}
			ready := h.ok(as(lane, launch), "ready", "report", "--report", explicit)
			if d := docLatest(t, db, lane, "report", ""); d.Version != 2 || d.EventID.Int64 != num(ready, "event_id") {
				t.Fatalf("%+v", d)
			}
			call := func() map[string]any {
				if terminal == "close" {
					return h.ok(nil, "close", id(lane))
				}
				return h.ok(as(lane, launch), terminal, "finished")
			}
			call()
			if d := docLatest(t, db, lane, "report", ""); d.Version != 2 {
				t.Fatalf("%+v", d)
			}
			// Reopen only in the scratch DB to exercise the same close twice.
			if terminal == "close" {
				docExec(t, db, `update tasks set status = 'open' where id = ?`, lane)
			}
			docFile(t, h.dir, "report.md", "report two")
			ev := call()
			if d := docLatest(t, db, lane, "report", ""); d.Version != 3 || d.EventID.Int64 != num(ev, "event_id") || d.Path.String != explicit {
				t.Fatalf("%+v", d)
			}
		})
	}
}

// 5. Misses retain metadata, never truncate, and deduplicate.
func TestDocumentsMisses(t *testing.T) {
	for _, tc := range []struct{ name, body, reason string }{
		{"large", strings.Repeat("a", documentCap+1), "too_large"}, {"nul", "a\x00b", "binary"}, {"utf8", "\xff", "binary"}, {"missing", "x", "missing"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			h := newHarness(t)
			_, lane, launch := docLane(t, h)
			db := h.openDB()
			path := docFile(t, h.dir, "report.md", tc.body)
			docExec(t, db, `update tasks set report_path = ? where id = ?`, path, lane)
			if tc.reason == "missing" {
				if err := os.Remove(path); err != nil {
					t.Fatal(err)
				}
			}
			h.ok(as(lane, launch), "done", "finished")
			h.ok(as(lane, launch), "done", "finished again")
			d := docLatest(t, db, lane, "report", "")
			if d.Captured || d.Reason.String != tc.reason || d.Format.Valid || d.Version != 1 {
				t.Fatalf("%+v", d)
			}
			if tc.reason == "too_large" && (!d.Bytes.Valid || d.Bytes.Int64 != documentCap+1) {
				t.Fatalf("%+v", d)
			}
			if n := docCount(t, db, `select count(*) from doc_blobs`); n != 0 {
				t.Fatal(n)
			}
		})
	}
	// A file gone at done with no earlier capture is still a missing row.
	h := newHarness(t)
	_, lane, launch := docLane(t, h)
	path := docFile(t, h.dir, "gone.md", "ready")
	docExec(t, h.openDB(), `update tasks set report_path = ? where id = ?`, path, lane)
	os.Remove(path)
	h.ok(as(lane, launch), "done", "d")
	if d := docLatest(t, h.openDB(), lane, "report", ""); d.Version != 1 || d.Reason.String != "missing" {
		t.Fatalf("%+v", d)
	}
}

// 6. SQL capture failure leaves every command's output and event untouched.
func TestDocumentsCaptureIsolation(t *testing.T) {
	setVar(t, &handoverNow, func() time.Time { return time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC) })
	for _, cmd := range []string{"new", "prompt", "ready", "done", "fail", "close", "handover"} {
		t.Run(cmd, func(t *testing.T) {
			var baseline string
			var baselineCode int
			for _, broken := range []bool{false, true} {
				h := newHarness(t)
				root, lane, launch := docLane(t, h)
				db := h.openDB()
				path := docFile(t, h.dir, "report.md", "report")
				docExec(t, db, `update tasks set report_path = ? where id = ?`, path, lane)
				if broken {
					docExec(t, db, `create trigger capture_failure before insert on documents begin select raise(abort, 'capture test'); end`)
				}
				var args []string
				var env map[string]string
				kind := cmd
				switch cmd {
				case "new":
					args = []string{"new", "another", "--role", "orchestrator", "--cwd", h.dir, "--brief", path}
					kind = ""
				case "prompt":
					args = []string{"prompt", id(lane), "--file", path, "--receipt-timeout", "0"}
				case "close":
					args = []string{"close", id(lane)}
					kind = "closed"
				case "handover":
					args = []string{"handover", "--as", id(root)}
				default:
					args = []string{cmd, "finished"}
					env = as(lane, launch)
				}
				code, out, stderr := h.compact(env, args...)
				combined := strings.ReplaceAll(out+"\nSTDERR\n"+stderr, md(h.dir), "SCRATCH")
				combined = strings.ReplaceAll(combined, h.dir, "SCRATCH")
				if !broken {
					baseline, baselineCode = combined, code
				} else if code != baselineCode || combined != baseline {
					t.Fatalf("capture changed %s: %d %q versus %d %q", cmd, code, combined, baselineCode, baseline)
				}
				if code != 0 {
					t.Fatal(code)
				}
				if kind != "" && docCount(t, db, `select count(*) from events where kind = ?`, kind) != 1 {
					t.Fatal("event lost")
				}
				if broken && docCount(t, db, `select count(*) from doc_blobs`) != 0 {
					t.Fatal("orphan blob")
				}
			}
		})
	}
}

// 7. RPC host ownership governs reads; capable clients upload captured files.
func TestDocumentsRPCCapture(t *testing.T) {
	r := newTwoHost(t)
	root := r.newTask("root", "orchestrator", 0)
	brief := docFile(t, r.dir, "brief.md", "server brief must not be read")
	lane := num(r.want(0, "host-b", nil, "new", "remote", "--role", "implementer", "--parent", id(root), "--cwd", r.dir, "--pane", "w1:p1", "--brief", brief), "task_id")
	if d := docLatest(t, r.openDB(), lane, "brief", ""); !d.Captured || d.Reason.Valid || d.Path.String != brief || d.Host.String != "host-b" || !d.Hash.Valid {
		t.Fatalf("%+v", d)
	}
	launch := num(r.want(0, "host-b", nil, "launch", id(lane), "--provider", "codex", "--model", "gpt-6-luna", "--effort", "max"), "launch_id")
	path := docFile(t, r.dir, "remote.md", "server bytes must not be read")
	r.want(0, "host-b", as(lane, launch), "ready", "r", "--report", path)
	d := docLatest(t, r.openDB(), lane, "report", "")
	if !d.Captured || d.Reason.Valid || d.Path.String != path || d.Host.String != "host-b" {
		t.Fatalf("%+v", d)
	}
	_, rep, _ := r.post("host-b", rpcBody(r.dir, nil, "doc-prompt-file", "_prompt", "begin", id(lane), "--file", path, "--sha256", "abc", "--bytes", "42", "--local-herdr", "--receipt-timeout", "0"))
	if rep.Exit != 0 {
		t.Fatalf("%+v", rep)
	}
	d = docLatest(t, r.openDB(), lane, "prompt", filepath.Base(path))
	if d.Reason.String != "client" || d.Hash.String != "abc" || d.Bytes.Int64 != 42 || d.Host.String != "host-b" {
		t.Fatalf("%+v", d)
	}
	_, rep, _ = r.post("host-b", rpcBody(r.dir, nil, "doc-prompt-text", "_prompt", "begin", id(lane), "--text", "client literal", "--local-herdr", "--receipt-timeout", "0"))
	if rep.Exit != 0 {
		t.Fatalf("%+v", rep)
	}
	if d := docLatest(t, r.openDB(), lane, "prompt", ""); !d.Captured || d.Format.String != "text" {
		t.Fatalf("%+v", d)
	}
	serverLane := r.newTask("local", "implementer", root, "--pane", "w1:p2")
	r.write("prompt.stdout", `{"result":{"agent":{"agent_status":"working"}}}`, 0644)
	r.want(0, "host-b", nil, "prompt", id(serverLane), "--file", path, "--receipt-timeout", "0")
	if d := docLatest(t, r.openDB(), serverLane, "prompt", filepath.Base(path)); !d.Captured || d.Host.Valid {
		t.Fatalf("%+v", d)
	}
}

// 8. Explicit sets validate before any ledger write and keep stable versions.
func TestDocumentsSet(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", root)
	path := docFile(t, h.dir, "goal.md", "goal one")
	first := h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	same := h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	if same["same"] != true || num(same, "doc_id") != num(first, "doc_id") || num(same, "version") != 1 {
		t.Fatal(same)
	}
	docFile(t, h.dir, "goal.md", "goal two")
	second := h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	if num(second, "version") != 2 {
		t.Fatal(second)
	}
	db := h.openDB()
	d := docLatest(t, db, root, "goal", "")
	ev, err := loadEvent(db, d.EventID.Int64)
	if err != nil || ev["kind"] != "doc" || ev["summary"] != "goal v2" || ev["recipient_task_id"] != nil {
		t.Fatalf("%v %v", ev, err)
	}
	h.one(exitReject, nil, "doc", "set", id(lane), "goal", "--file", path)
	h.ok(nil, "doc", "set", id(lane), "plan", "--name", "spec-capture", "--file", path)
	if d := docLatest(t, db, lane, "plan", "spec-capture"); d.Version != 1 {
		t.Fatal(d)
	}
	for _, args := range [][]string{
		{"goal", "--name", "x", "--file", path}, {"plan", "--name", "INVALID", "--file", path}, {"plan", "--name", "", "--file", path},
		{"plan", "--file", filepath.Join(h.dir, "missing")}, {"plan", "--file", t.TempDir()}, {"plan", "--file", docFile(t, h.dir, "binary", "\x00")}, {"plan", "--file", docFile(t, h.dir, "utf8", "\xff")}, {"plan", "--file", docFile(t, h.dir, "large", strings.Repeat("a", documentCap+1))},
	} {
		before := docCount(t, db, `select count(*) from documents`)
		events := docCount(t, db, `select count(*) from events`)
		h.one(exitUsage, nil, append([]string{"doc", "set", id(root)}, args...)...)
		if before != docCount(t, db, `select count(*) from documents`) || events != docCount(t, db, `select count(*) from events`) {
			t.Fatal("invalid set wrote")
		}
	}
	h.ok(nil, "close", id(lane))
	h.one(exitReject, nil, "doc", "set", id(lane), "plan", "--file", path)
}

// 9. Get is byte-exact and ls combines filters, history and continuation.
func TestDocumentsGetAndLs(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", root)
	body := "α\n\nlast line without newline"
	path := docFile(t, h.dir, "goal.md", body)
	first := h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	code, out, _ := h.runText(nil, "doc", "get", id(num(first, "doc_id")))
	if code != 0 || out != body {
		t.Fatalf("%d %q", code, out)
	}
	docFile(t, h.dir, "goal.md", body+" changed")
	h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	h.ok(nil, "doc", "set", id(lane), "plan", "--name", "spec", "--file", path)
	_, lines := h.run(nil, "doc", "ls", id(root))
	if len(lines) != 1 || num(lines[0], "version") != 2 {
		t.Fatal(lines)
	}
	_, lines = h.run(nil, "doc", "ls", id(root), "--tree")
	if len(lines) != 2 {
		t.Fatal(lines)
	}
	_, lines = h.run(nil, "doc", "ls", id(root), "--tree", "--kind", "plan")
	if len(lines) != 1 || lines[0]["name"] != "spec" {
		t.Fatal(lines)
	}
	_, lines = h.run(nil, "doc", "ls", id(root), "--versions")
	if len(lines) != 2 {
		t.Fatal(lines)
	}
	code, out, _ = h.compact(nil, "doc", "ls", id(root), "--tree", "--versions", "--limit", "1")
	if code != 0 || strings.Count(out, "j1 ") != 1 || !strings.Contains(out, `m1 {"all":"--limit 0","older":2}`) {
		t.Fatalf("%d %q", code, out)
	}
	missing := filepath.Join(h.dir, "gone")
	docExec(t, h.openDB(), `update tasks set report_path = ? where id = ?`, missing, root)
	h.ok(as(root, 0), "done", "d")
	d := docLatest(t, h.openDB(), root, "report", "")
	bad := h.one(exitReject, nil, "doc", "get", id(d.ID))
	if !strings.Contains(errOf(bad), "missing") || !strings.Contains(errOf(bad), missing) {
		t.Fatal(bad)
	}
	h.one(exitUsage, nil, "doc", "ls", id(root), "--kind", "unknown")
	h.one(exitUsage, nil, "doc", "ls", id(root), "--limit", "-1")
}

// 10. Read RPCs never enter the request ledger; writes do.
func TestDocumentsRPCRequests(t *testing.T) {
	r := newTwoHost(t)
	root := r.newTask("root", "orchestrator", 0)
	dir := t.TempDir()
	docFile(t, dir, "goal.md", "goal")
	_, rep, _ := r.post("host-b", rpcBody(dir, nil, "doc-set-key-01", "doc", "set", id(root), "goal", "--file", "goal.md"))
	if rep.Exit != 0 {
		t.Fatalf("%+v", rep)
	}
	docID := num(lastJSON(rep.Stdout), "doc_id")
	if r.count(`select count(*) from requests where key = 'doc-set-key-01'`) != 1 {
		t.Fatal("set not stored")
	}
	for i, args := range [][]string{{"doc", "get", id(docID)}, {"doc", "ls", id(root)}} {
		key := fmt.Sprintf("doc-read-key-%d", i)
		_, rep, _ := r.post("host-b", rpcBody(dir, nil, key, args...))
		if rep.Exit != 0 || r.count(`select count(*) from requests where key = ?`, key) != 0 {
			t.Fatalf("%+v", rep)
		}
	}
	_, rep, _ = r.post("host-b", rpcBody(dir, nil, "doc-missing-01", "doc", "set", id(root), "plan", "--file", "absent.md"))
	if rep.Exit != exitUsage || !strings.Contains(rep.Stdout, "file not found on the server host") {
		t.Fatalf("%+v", rep)
	}
}

// 11. Both handover and adopt show escaped document summaries and counts.
func TestDocumentsHandover(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	for _, args := range [][]string{{"handover", "--as", id(root)}, {"adopt", id(root), "--pane", "w2:p1"}} {
		code, out, _ := h.runText(nil, args...)
		if code != 0 || !strings.Contains(out, "Goal: none recorded; run `taskr doc set "+id(root)+" goal --file PATH`") {
			t.Fatalf("%d %q", code, out)
		}
	}
	long := strings.Repeat("λ", 161)
	path := docFile(t, h.dir, "goal.md", "\n# goal <tag>\n\n"+long+"\nthird\nfourth\n")
	goal := h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	plan := h.ok(nil, "doc", "set", id(root), "plan", "--file", path)
	h.ok(nil, "doc", "set", id(root), "plan", "--name", "spec-capture", "--file", path)
	h.ok(nil, "decide", "--as", id(root), "decision")
	lane := h.newTask("lane", "implementer", root)
	h.ok(nil, "close", id(lane))
	for _, args := range [][]string{{"handover", "--as", id(root)}, {"adopt", id(root), "--pane", "w2:p1"}} {
		code, out, _ := h.runText(nil, args...)
		for _, want := range []string{fmt.Sprintf("Goal (doc %d, v1):", num(goal, "doc_id")), `\# goal &lt;tag&gt;`, strings.Repeat("λ", 160), "Full text: `taskr doc get ", fmt.Sprintf("Plan (doc %d, v1, event ", num(plan, "doc_id")), "since then 1 decisions, 1 lanes closed.", "Documents: spec\\-capture (doc "} {
			if code != 0 || !strings.Contains(out, want) {
				t.Fatalf("missing %q: %d %q", want, code, out)
			}
		}
		if strings.Contains(out, strings.Repeat("λ", 161)) || strings.Contains(out, "fourth") {
			t.Fatal("goal excerpt not bounded")
		}
		if strings.Index(out, "Goal (") > strings.Index(out, "## Identity") {
			t.Fatal("goal misplaced")
		}
	}
	d := docLatest(t, db, root, "handover", "")
	if !d.Captured || d.Format.String != "md" || !d.EventID.Valid {
		t.Fatalf("%+v", d)
	}
	missingRoot := h.newTask("missingroot", "orchestrator", 0)
	miss := filepath.Join(h.dir, "missing")
	if err := withTx(db, func(tx *sql.Tx) error {
		_, _, err := storeDocument(tx, missingRoot, "goal", "", fileDocument(miss, "hostb"), nil, 0)
		return err
	}); err != nil {
		t.Fatal(err)
	}
	for _, args := range [][]string{{"handover", "--as", id(missingRoot)}, {"adopt", id(missingRoot), "--pane", "w2:p2"}} {
		code, out, _ := h.runText(nil, args...)
		if code != 0 || !strings.Contains(out, "on hostb, not captured (client)") {
			t.Fatalf("%d %q", code, out)
		}
	}
}

// 12. New root hints go only to stderr; --brief records an eventless goal.
func TestDocumentsNew(t *testing.T) {
	h := newHarness(t)
	code, out, stderr := h.compact(nil, "new", "root", "--role", "orchestrator", "--cwd", h.dir)
	if code != 0 || out != "n1 1\n" || stderr != "taskr: no goal recorded for root 1; run `taskr doc set 1 goal --file PATH`\n" {
		t.Fatalf("%d %q %q", code, out, stderr)
	}
	path := docFile(t, h.dir, "goal.md", "goal")
	root := h.newTask("withgoal", "orchestrator", 0, "--brief", path)
	d := docLatest(t, h.openDB(), root, "goal", "")
	if !d.Captured || d.EventID.Valid || h.lastStderr() != "" {
		t.Fatalf("%+v", d)
	}
	if n := docCount(t, h.openDB(), `select count(*) from events where task_id = ?`, root); n != 0 {
		t.Fatal("new wrote an event")
	}
	child := h.newTask("child", "implementer", root, "--brief", path)
	if d := docLatest(t, h.openDB(), child, "brief", ""); !d.Captured || d.EventID.Valid {
		t.Fatalf("%+v", d)
	}
}

// 13. Backfill verifies historical hashes, labels changes and is idempotent.
func TestDocumentsBackfill(t *testing.T) {
	h := newHarness(t)
	root, lane, _ := docLane(t, h)
	db := h.openDB()
	brief := docFile(t, h.dir, "brief.md", "historical")
	report := docFile(t, h.dir, "report.md", "report")
	docExec(t, db, `update tasks set brief_path = ?, report_path = ? where id = ?`, brief, report, lane)
	sum := sha256.Sum256([]byte("historical"))
	var eid int64
	if err := withTx(db, func(tx *sql.Tx) error {
		var err error
		eid, err = insertEvent(tx, event{TaskID: lane, Kind: "prompt", Data: map[string]any{"file": brief, "sha256": hex.EncodeToString(sum[:])}})
		return err
	}); err != nil {
		t.Fatal(err)
	}
	dry := h.ok(nil, "doc", "backfill", "--tree", id(root), "--dry-run")
	if num(dry, "captured") != 2 || docCount(t, db, `select count(*) from documents`) != 0 || docCount(t, db, `select count(*) from doc_blobs`) != 0 {
		t.Fatal(dry)
	}
	h.ok(nil, "doc", "backfill", "--tree", id(root))
	d := docLatest(t, db, lane, "brief", "")
	if d.Backfill != 1 || d.EventID.Int64 != eid {
		t.Fatalf("%+v", d)
	}
	if d := docLatest(t, db, lane, "report", ""); d.Backfill != 2 || d.EventID.Valid {
		t.Fatalf("%+v", d)
	}
	again := h.ok(nil, "doc", "backfill", "--tree", id(root))
	if num(again, "unchanged") != 2 || docCount(t, db, `select count(*) from documents`) != 2 {
		t.Fatal(again)
	}
	docFile(t, h.dir, "brief.md", "changed")
	h.ok(nil, "doc", "backfill", "--tree", id(root))
	if d := docLatest(t, db, lane, "brief", ""); d.Backfill != 2 || d.Version != 2 || d.EventID.Valid {
		t.Fatalf("%+v", d)
	}
	// Backfill misses must not hide previously captured history.
	docExec(t, db, `update tasks set machine = 'hostb', current_launch_id = null where id = ?`, lane)
	h.ok(nil, "doc", "backfill", "--tree", id(root))
	if d := docLatest(t, db, lane, "brief", ""); !d.Captured || d.Version != 2 {
		t.Fatalf("%+v", d)
	}
	remote := h.newTask("remote", "implementer", root, "--report", report)
	docExec(t, db, `update tasks set machine = 'hostb' where id = ?`, remote)
	h.ok(nil, "doc", "backfill", "--tree", id(remote))
	if d := docLatest(t, db, remote, "report", ""); d.Reason.String != "client" || d.Host.String != "hostb" {
		t.Fatalf("%+v", d)
	}
	r := newTwoHost(t)
	_, rep, _ := r.post("host-b", rpcBody(r.dir, nil, "backfill-key-01", "doc", "backfill"))
	if rep.Exit != exitReject || !strings.Contains(rep.Stdout, "only available locally") || r.count(`select count(*) from requests where key = 'backfill-key-01'`) != 0 {
		t.Fatalf("%+v", rep)
	}
}

// 14. Purge deletes all versions, retaining shared blobs and writing an event.
func TestDocumentsPurge(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	path := docFile(t, h.dir, "goal.md", "shared")
	first := h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	h.ok(nil, "doc", "set", id(root), "plan", "--file", path)
	docFile(t, h.dir, "goal.md", "unshared")
	h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	h.one(exitUsage, nil, "doc", "rm", id(num(first, "doc_id")))
	out := h.ok(nil, "doc", "rm", id(num(first, "doc_id")), "--purge")
	if num(out, "removed") != 2 || docCount(t, db, `select count(*) from documents where kind = 'goal'`) != 0 || docCount(t, db, `select count(*) from doc_blobs`) != 1 {
		t.Fatal(out)
	}
	var body, summary string
	if err := db.QueryRow(`select body from doc_blobs`).Scan(&body); err != nil || body != "shared" {
		t.Fatalf("%q %v", body, err)
	}
	if err := db.QueryRow(`select summary from events where kind = 'doc' order by id desc limit 1`).Scan(&summary); err != nil || summary != "purged goal (2 versions)" {
		t.Fatalf("%q %v", summary, err)
	}
	plan := docLatest(t, db, root, "plan", "")
	h.ok(nil, "doc", "rm", id(plan.ID), "--purge")
	if docCount(t, db, `select count(*) from doc_blobs`) != 0 {
		t.Fatal("last blob remains")
	}
}

// 15. Frames, documented code and inbox behavior are independent contracts.
func TestDocumentsCompactContract(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	path := docFile(t, h.dir, "goal.md", "goal")
	code, out, _ := h.compact(nil, "doc", "set", id(root), "goal", "--file", path)
	if code != 0 || out != "ds1 1 1\n" {
		t.Fatalf("%d %q", code, out)
	}
	code, out, _ = h.compact(nil, "doc", "set", id(root), "goal", "--file", path)
	if code != 0 || out != "ds1 1 1 same\n" {
		t.Fatalf("%d %q", code, out)
	}
	if kindCodes["doc"] != "do" {
		t.Fatal(kindCodes["doc"])
	}
	c := &ctx{getenv: h.getenv(nil), out: new(strings.Builder), errw: new(strings.Builder), cmd: "wait"}
	c.emit(map[string]any{"event": map[string]any{"id": int64(1), "task_id": root, "kind": "doc", "summary": "goal v1"}})
	if !strings.Contains(c.out.(*strings.Builder).String(), "\tdo\t") {
		t.Fatal(c.out)
	}
	for _, filter := range [][]string{nil, {"--for", "doc"}} {
		args := append([]string{"wait", "--as", id(root), "--timeout", "0"}, filter...)
		code, out, _ := h.compact(nil, args...)
		if code != 0 || !strings.Contains(out, "x1 3 timeout") {
			t.Fatalf("doc woke wait: %d %q", code, out)
		}
	}
	code, out, _ = h.compact(nil, "doc", "rm", "1", "--purge")
	if code != 0 || out != "dr1 1 1\n" {
		t.Fatalf("%d %q", code, out)
	}
	contract, err := os.ReadFile("references/format.md")
	if err != nil {
		t.Fatal(err)
	}
	for _, s := range []string{"ds1 DOC VERSION [same]", "dr1 DOC VERSIONS_REMOVED", "do=doc"} {
		if !strings.Contains(string(contract), s) {
			t.Fatal(s)
		}
	}
	for _, args := range [][]string{{"doc", "--help"}, {"doc", "rm", "--help"}} {
		code, out, _ := h.compact(nil, args...)
		if code != 0 || !strings.Contains(out, "doc") {
			t.Fatalf("%d %q", code, out)
		}
	}
	for _, sub := range []string{"get", "ls", "backfill"} {
		if storedCommand([]string{"doc", sub}) {
			t.Fatal(sub)
		}
	}
	for _, sub := range []string{"set", "rm"} {
		if !storedCommand([]string{"doc", sub}) {
			t.Fatal(sub)
		}
	}
}

func TestDocumentsBackfillSameBasename(t *testing.T) {
	h := newHarness(t)
	_, lane, _ := docLane(t, h)
	db := h.openDB()
	for _, name := range []string{"a/prompt.md", "b/prompt.md"} {
		path := docFile(t, h.dir, name, name)
		sum := sha256.Sum256([]byte(name))
		if err := withTx(db, func(tx *sql.Tx) error {
			_, err := insertEvent(tx, event{TaskID: lane, Kind: "prompt", Data: map[string]any{"file": path, "sha256": hex.EncodeToString(sum[:])}})
			return err
		}); err != nil {
			t.Fatal(err)
		}
	}
	h.ok(nil, "doc", "backfill", "--tree", id(lane))
	before := docCount(t, db, `select count(*) from documents`)
	again := h.ok(nil, "doc", "backfill", "--tree", id(lane))
	if after := docCount(t, db, `select count(*) from documents`); after != before || num(again, "unchanged") != 2 {
		t.Fatalf("second run added %d versions; counts %v", after-before, again)
	}
}

func TestDocumentsFixBackfillHistory(t *testing.T) {
	for _, scenario := range []string{"backfill_over_set", "backfill_miss", "backfill_report"} {
		t.Run(scenario, func(t *testing.T) {
			h := newHarness(t)
			brief := docFile(t, h.dir, "brief.md", "brief")
			root := h.newTask("root", "orchestrator", 0, "--brief", brief)
			taskID, kind := root, "goal"
			switch scenario {
			case "backfill_over_set":
				docExec(t, h.openDB(), `delete from documents`)
				docExec(t, h.openDB(), `delete from doc_blobs`)
				goal := docFile(t, h.dir, "goal.md", "owner goal")
				h.ok(nil, "doc", "set", id(root), "goal", "--file", goal)
			case "backfill_miss":
				os.Remove(brief)
			case "backfill_report":
				taskID = h.newTask("lane", "implementer", root, "--report", "stale.md")
				report := docFile(t, h.dir, "ready.md", "current report")
				h.ok(as(taskID, 0), "ready", "r", "--report", report)
				kind = "report"
			}
			db := h.openDB()
			before := docLatest(t, db, taskID, kind, "")
			h.ok(nil, "doc", "backfill", "--tree", id(root))
			after := docLatest(t, db, taskID, kind, "")
			if after.ID != before.ID || !after.Captured {
				t.Fatalf("backfill replaced %+v with %+v", before, after)
			}
		})
	}
	// A history-only report lookup uses ready's override, even without a captured row.
	h := newHarness(t)
	root, lane, launch := docLane(t, h)
	db := h.openDB()
	report := docFile(t, h.dir, "ready.md", "report")
	docExec(t, db, `update tasks set report_path = ? where id = ?`, filepath.Join(h.dir, "stale.md"), lane)
	h.ok(as(lane, launch), "ready", "r", "--report", report)
	docExec(t, db, `delete from documents`)
	docExec(t, db, `delete from doc_blobs`)
	h.ok(nil, "doc", "backfill", "--tree", id(root))
	if d := docLatest(t, db, lane, "report", ""); d.Path.String != report || !d.Captured {
		t.Fatalf("%+v", d)
	}
}

func TestDocumentsFixRemovedReportAndGoal(t *testing.T) {
	for _, terminal := range []string{"done", "close"} {
		t.Run(terminal, func(t *testing.T) {
			h := newHarness(t)
			_, lane, launch := docLane(t, h)
			db := h.openDB()
			path := docFile(t, h.dir, "report.md", "saved report")
			h.ok(as(lane, launch), "ready", "r", "--report", path)
			before := docLatest(t, db, lane, "report", "")
			os.Remove(path)
			if terminal == "close" {
				h.ok(nil, "close", id(lane))
			} else {
				h.ok(as(lane, launch), "done", "d")
			}
			if after := docLatest(t, db, lane, "report", ""); after.ID != before.ID {
				t.Fatalf("%+v", after)
			}
			_, rows := h.run(nil, "doc", "ls", id(lane), "--kind", "report")
			if len(rows) != 1 || rows[0]["captured"] != true {
				t.Fatal(rows)
			}
		})
	}
	h := newHarness(t)
	path := docFile(t, h.dir, "goal.md", "saved goal")
	root := h.newTask("root", "orchestrator", 0, "--brief", path)
	db := h.openDB()
	captured := docLatest(t, db, root, "goal", "")
	if err := withTx(db, func(tx *sql.Tx) error {
		_, _, err := storeDocument(tx, root, "goal", "", bodyDocument([]byte{0}, path), nil, 0)
		return err
	}); err != nil {
		t.Fatal(err)
	}
	if miss := docLatest(t, db, root, "goal", ""); miss.Version != 2 || miss.Reason.String != "binary" || miss.Captured {
		t.Fatalf("%+v", miss)
	}
	for _, args := range [][]string{{"handover", "--as", id(root)}, {"adopt", id(root), "--pane", "w2:p1"}} {
		code, out, _ := h.runText(nil, args...)
		if code != 0 || !strings.Contains(out, fmt.Sprintf("Goal (doc %d, v1):", captured.ID)) || !strings.Contains(out, "saved goal") {
			t.Fatalf("%d %q", code, out)
		}
	}
}

func TestDocumentsFixFatalRollback(t *testing.T) {
	for _, command := range []string{"ready", "done", "close", "prompt", "handover"} {
		t.Run(command, func(t *testing.T) {
			h := newHarness(t)
			root, lane, launch := docLane(t, h)
			db := h.openDB()
			path := docFile(t, h.dir, "report.md", "report")
			docExec(t, db, `update tasks set report_path = ?, status = 'ready' where id = ?`, path, lane)
			docExec(t, db, `create trigger capture_rollback before insert on documents begin select raise(rollback, 'capture rollback'); end`)
			docExec(t, db, `update tasks set updated_at = '2026-01-01T00:00:00.000Z'`)
			taskRow := func(taskID int64) string {
				rows, err := db.Query(`select * from tasks where id = ?`, taskID)
				if err != nil {
					t.Fatal(err)
				}
				defer rows.Close()
				columns, err := rows.Columns()
				if err != nil {
					t.Fatal(err)
				}
				values, destinations := make([]any, len(columns)), make([]any, len(columns))
				for i := range values {
					destinations[i] = &values[i]
				}
				if !rows.Next() {
					t.Fatal("missing task row")
				}
				if err := rows.Scan(destinations...); err != nil {
					t.Fatal(err)
				}
				return jsonText(values)
			}
			rowBefore, rootBefore := taskRow(lane), taskRow(root)
			taskBefore, _ := loadTask(db, lane)
			before := docCount(t, db, `select count(*) from events`)
			var args []string
			var env map[string]string
			switch command {
			case "close":
				args = []string{"close", id(lane)}
			case "prompt":
				args = []string{"prompt", id(lane), "--text", "go"}
			case "handover":
				args = []string{"handover", "--as", id(root)}
			default:
				args = []string{command, "finished"}
				env = as(lane, launch)
			}
			code, _, _ := h.compact(env, args...)
			if code != exitDB {
				t.Fatal(code)
			}
			taskAfter, _ := loadTask(db, lane)
			if taskRow(lane) != rowBefore || taskRow(root) != rootBefore || *taskAfter != *taskBefore || docCount(t, db, `select count(*) from events`) != before || docCount(t, db, `select count(*) from doc_blobs`) != 0 {
				t.Fatalf("torn write: before %+v after %+v", taskBefore, taskAfter)
			}
			if command == "prompt" && len(h.calls("agent|prompt")) != 0 {
				t.Fatal("delivered a rolled-back prompt")
			}
		})
	}
}

func TestDocumentsFixPurgeMissReference(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	path := docFile(t, h.dir, "goal.md", "private marker")
	out := h.ok(nil, "doc", "set", id(root), "goal", "--file", path)
	d := docLatest(t, db, root, "goal", "")
	if err := withTx(db, func(tx *sql.Tx) error {
		_, _, err := storeDocument(tx, root, "prompt", "remote.md", documentInput{Path: path, Host: "hostb", Hash: d.Hash.String, Bytes: ptr(d.Bytes.Int64), Reason: "client"}, nil, 0)
		return err
	}); err != nil {
		t.Fatal(err)
	}
	h.ok(nil, "doc", "rm", id(num(out, "doc_id")), "--purge")
	if docCount(t, db, `select count(*) from doc_blobs`) != 0 || docCount(t, db, `select count(*) from documents where reason = 'client'`) != 1 {
		t.Fatal("miss retained body")
	}
}

func TestDocumentsFixPurgeClosedAndIDs(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", root)
	path := docFile(t, h.dir, "plan.md", "plan")
	first := h.ok(nil, "doc", "set", id(lane), "plan", "--file", path)
	h.ok(nil, "close", id(lane))
	h.ok(nil, "doc", "rm", id(num(first, "doc_id")), "--purge")
	next := h.ok(nil, "doc", "set", id(root), "plan", "--file", path)
	if num(next, "doc_id") <= num(first, "doc_id") {
		t.Fatalf("id reused: %v %v", first, next)
	}
}

func TestDocumentsFixDashboardSkip(t *testing.T) {
	h := newHarness(t)
	_, lane, launch := docLane(t, h)
	h.ok(as(lane, launch), "ready", "slice ready")
	d := h.dash()
	_, before := getState(t, d)
	laneState := func(state dashState) string {
		tasks := state.Orchestrators[0].Tasks
		for i := range tasks {
			if tasks[i].LastEvent != nil {
				tasks[i].LastEvent.AgeMS = 0
			}
		}
		return jsonText(tasks)
	}
	path := docFile(t, h.dir, "plan.md", "plan")
	doc := h.ok(nil, "doc", "set", id(lane), "plan", "--file", path)
	_, after := getState(t, d)
	if laneState(before) != laneState(after) {
		t.Fatal("doc set changed lane state")
	}
	h.ok(nil, "doc", "rm", id(num(doc, "doc_id")), "--purge")
	_, after = getState(t, d)
	if laneState(before) != laneState(after) {
		t.Fatal("purge changed lane state")
	}
}

func TestDocumentsFixReadsOutsideTransactions(t *testing.T) {
	for _, command := range []string{"new", "ready", "done", "fail", "close", "backfill"} {
		t.Run(command, func(t *testing.T) {
			h := newHarness(t)
			root, lane, launch := docLane(t, h)
			db := h.openDB()
			path := docFile(t, h.dir, "report.md", "report")
			docExec(t, db, `update tasks set report_path = ? where id = ?`, path, lane)
			conn, err := db.Conn(context.Background())
			if err != nil {
				t.Fatal(err)
			}
			defer conn.Close()
			if _, err := conn.ExecContext(context.Background(), `pragma busy_timeout = 0`); err != nil {
				t.Fatal(err)
			}
			reads := 0
			setVar(t, &readDocumentBody, func(reader io.Reader) ([]byte, error) {
				reads++
				// This connection's immediate writer must succeed while capture is reading.
				tx, err := conn.BeginTx(context.Background(), nil)
				if err != nil {
					t.Errorf("reader holds a write transaction: %v", err)
					return nil, err
				}
				defer tx.Rollback()
				if _, err := tx.Exec(`insert or replace into meta(key,value) values ('read-probe','ok')`); err != nil {
					t.Error(err)
					return nil, err
				}
				if err := tx.Commit(); err != nil {
					t.Error(err)
					return nil, err
				}
				return io.ReadAll(reader)
			})
			switch command {
			case "new":
				h.ok(nil, "new", "fresh", "--role", "orchestrator", "--cwd", h.dir, "--brief", path)
			case "close":
				h.ok(nil, "close", id(lane))
			case "backfill":
				h.ok(nil, "doc", "backfill", "--tree", id(root))
			default:
				h.ok(as(lane, launch), command, "finished")
			}
			if reads != 1 {
				t.Fatalf("%s read %d times, want 1", command, reads)
			}
		})
	}
}

func TestDocumentsFixReportABAAndAnswer(t *testing.T) {
	h := newHarness(t)
	root, lane, launch := docLane(t, h)
	db := h.openDB()
	for _, body := range []string{"A", "B", "A"} {
		path := docFile(t, h.dir, "report.md", body)
		h.ok(as(lane, launch), "ready", "r", "--report", path)
	}
	if d := docLatest(t, db, lane, "report", ""); d.Version != 3 {
		t.Fatalf("%+v", d)
	}
	if n := docCount(t, db, `select count(*) from doc_blobs`); n != 2 {
		t.Fatal(n)
	}
	ask := h.ok(as(lane, launch), "ask", "question")
	before := docCount(t, db, `select count(*) from documents`)
	h.ok(nil, "answer", id(num(ask, "ask_id")), "answer", "--as", id(root), "--prompt")
	if n := docCount(t, db, `select count(*) from documents`); n != before {
		t.Fatal("answer prompt captured")
	}
}

func TestDocumentsFixLaneDocNoRecipient(t *testing.T) {
	h := newHarness(t)
	root, lane, _ := docLane(t, h)
	db := h.openDB()
	path := docFile(t, h.dir, "plan.md", "plan")
	h.ok(nil, "doc", "set", id(lane), "plan", "--file", path)
	d := docLatest(t, db, lane, "plan", "")
	if n := docCount(t, db, `select count(*) from events where id = ? and recipient_task_id is not null`, d.EventID.Int64); n != 0 {
		t.Fatal("doc has recipient")
	}
	code, out, _ := h.compact(nil, "wait", "--as", id(root), "--timeout", "0")
	if code != 0 || !strings.Contains(out, "x1 3 timeout") {
		t.Fatalf("doc woke parent: %d %q", code, out)
	}
}

func TestDocumentsFixReadCapAndFileTypes(t *testing.T) {
	t.Run("cap", func(t *testing.T) {
		dir := t.TempDir()
		path := docFile(t, dir, "report.md", "small")
		read := 0
		setVar(t, &readDocumentBody, func(reader io.Reader) ([]byte, error) {
			// Grow the same inode after the initial stat, so the read limit is exercised.
			docFile(t, dir, "report.md", strings.Repeat("x", 2*documentCap))
			body, err := io.ReadAll(reader)
			read += len(body)
			return body, err
		})
		d := fileDocument(path, "")
		if read != documentCap+1 || d.Reason != "too_large" || d.Hash != "" || *d.Bytes != 2*documentCap {
			t.Fatalf("read=%d input=%+v", read, d)
		}
	})
	for _, kind := range []string{"fifo", "directory"} {
		t.Run(kind, func(t *testing.T) {
			h := newHarness(t)
			_, lane, launch := docLane(t, h)
			path := filepath.Join(h.dir, "report")
			if kind == "fifo" {
				if err := syscall.Mkfifo(path, 0600); err != nil {
					t.Fatal(err)
				}
				// Pair the reader even in a guard mutant; the read hook fails before blocking.
				fd, err := syscall.Open(path, syscall.O_RDWR|syscall.O_NONBLOCK, 0)
				if err != nil {
					t.Fatal(err)
				}
				defer syscall.Close(fd)
			} else {
				if err := os.Mkdir(path, 0700); err != nil {
					t.Fatal(err)
				}
			}
			reads := 0
			setVar(t, &readDocumentBody, func(io.Reader) ([]byte, error) {
				reads++
				return nil, fmt.Errorf("unexpected nonregular read")
			})
			docExec(t, h.openDB(), `update tasks set report_path = ? where id = ?`, path, lane)
			code, out, _ := h.compact(as(lane, launch), "ready", "r")
			if code != 0 || reads != 0 {
				t.Fatalf("code=%d reads=%d output=%q", code, reads, out)
			}
			if d := docLatest(t, h.openDB(), lane, "report", ""); d.Reason.String != "missing" {
				t.Fatalf("%+v", d)
			}
		})
	}
}

func TestDocumentsFixPurgeSecureAndCheckpoint(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	marker := strings.Repeat("purge-marker-", 100)
	path := docFile(t, h.dir, "plan.md", marker)
	out := h.ok(nil, "doc", "set", id(root), "plan", "--file", path)
	db.SetMaxOpenConns(1)
	docExec(t, db, `pragma secure_delete = off`)
	docExec(t, db, `create trigger assert_secure_delete before delete on documents begin
  select case when (select secure_delete from pragma_secure_delete) != 1 then raise(abort,'secure delete disabled') end; end`)
	var stdout, stderr strings.Builder
	c := &ctx{getenv: h.getenv(nil), db: db, out: &stdout, errw: &stderr}
	if code := runCtx(c, []string{"doc", "rm", id(num(out, "doc_id")), "--purge"}); code != 0 {
		t.Fatalf("%d %s %s", code, stdout.String(), stderr.String())
	}
	main, err := os.ReadFile(h.db)
	if err != nil {
		t.Fatal(err)
	}
	wal, err := os.ReadFile(h.db + "-wal")
	if err != nil && !os.IsNotExist(err) {
		t.Fatal(err)
	}
	if strings.Contains(string(main), marker) || strings.Contains(string(wal), marker) || len(wal) != 0 {
		t.Fatal("purge retained marker or failed to truncate WAL")
	}
	code, help, _ := h.compact(nil, "doc", "rm", "--help")
	if code != 0 || !strings.Contains(help, "write-ahead log") || !strings.Contains(help, "earlier backups") {
		t.Fatalf("%d %q", code, help)
	}
}

func assertReportRetained(t *testing.T, h *harness, db *sql.DB, lane int64, expected document, body string) {
	t.Helper()
	if d := docLatest(t, db, lane, "report", ""); d.ID != expected.ID {
		t.Fatalf("stale capture replaced report: %+v", d)
	}
	if n := docCount(t, db, `select count(*) from documents where task_id = ? and kind = 'report'`, lane); n != 1 {
		t.Fatalf("stored %d report rows", n)
	}
	code, rows := h.run(nil, "doc", "ls", id(lane), "--kind", "report")
	if code != 0 || len(rows) != 1 || num(rows[0], "doc_id") != expected.ID || rows[0]["captured"] != true {
		t.Fatalf("doc ls: %d %v", code, rows)
	}
	code, out, errout := h.runText(nil, "doc", "get", id(expected.ID))
	if code != 0 || out != body {
		t.Fatalf("doc get: %d %q %q", code, out, errout)
	}
}

func TestDocumentsReportStaleRead(t *testing.T) {
	h := newHarness(t)
	_, lane, launch := docLane(t, h)
	db := h.openDB()
	path := docFile(t, h.dir, "report.md", "older text")
	docExec(t, db, `update tasks set report_path = ? where id = ?`, path, lane)
	var committed document
	n := 0
	setVar(t, &readDocumentBody, func(r io.Reader) ([]byte, error) {
		n++
		body, err := io.ReadAll(r)
		if n == 1 {
			docFile(t, h.dir, "report.md", "newer text")
			h.ok(as(lane, launch), "ready", "r")
			committed = docLatest(t, db, lane, "report", "")
		}
		return body, err
	})
	h.ok(nil, "close", id(lane))
	assertReportRetained(t, h, db, lane, committed, "newer text")
}

func TestDocumentsReportPathRace(t *testing.T) {
	for _, command := range []string{"done", "close"} {
		t.Run(command, func(t *testing.T) {
			h := newHarness(t)
			_, lane, launch := docLane(t, h)
			db := h.openDB()
			planned := docFile(t, h.dir, "planned.md", "wrong report")
			actual := docFile(t, h.dir, "actual.md", "actual report")
			docExec(t, db, `update tasks set report_path = ? where id = ?`, planned, lane)
			var committed document
			n := 0
			setVar(t, &readDocumentBody, func(r io.Reader) ([]byte, error) {
				n++
				if n == 1 {
					h.ok(as(lane, launch), "ready", "r", "--report", actual)
					committed = docLatest(t, db, lane, "report", "")
				}
				return io.ReadAll(r)
			})
			if command == "close" {
				h.ok(nil, "close", id(lane))
			} else {
				h.ok(as(lane, launch), "done", "fin")
			}
			assertReportRetained(t, h, db, lane, committed, "actual report")
		})
	}
}

func TestDocumentsReportPathOnlyRace(t *testing.T) {
	for _, command := range []string{"done", "close"} {
		t.Run(command, func(t *testing.T) {
			h := newHarness(t)
			_, lane, launch := docLane(t, h)
			db := h.openDB()
			planned := docFile(t, h.dir, "planned.md", "saved report")
			actual := docFile(t, h.dir, "actual.md", "saved report")
			h.ok(as(lane, launch), "ready", "r", "--report", planned)
			committed := docLatest(t, db, lane, "report", "")
			docFile(t, h.dir, "planned.md", "wrong report")
			n := 0
			setVar(t, &readDocumentBody, func(r io.Reader) ([]byte, error) {
				n++
				if n == 1 {
					h.ok(as(lane, launch), "ready", "r", "--report", actual)
					assertReportRetained(t, h, db, lane, committed, "saved report")
					path, err := reportDocumentPath(db, lane)
					if err != nil || path != actual {
						t.Fatalf("racing ready path: %q %v", path, err)
					}
				}
				return io.ReadAll(r)
			})
			if command == "close" {
				h.ok(nil, "close", id(lane))
			} else {
				h.ok(as(lane, launch), "done", "fin")
			}
			assertReportRetained(t, h, db, lane, committed, "saved report")
		})
	}
}

func TestDocumentsReportHostChangeBetween(t *testing.T) {
	h := newHarness(t)
	_, lane, launch := docLane(t, h)
	db := h.openDB()
	path := docFile(t, h.dir, "report.md", "saved report")
	h.ok(as(lane, launch), "ready", "r", "--report", path)
	committed := docLatest(t, db, lane, "report", "")
	docFile(t, h.dir, "report.md", "wrong server text")
	setVar(t, &readDocumentBody, func(r io.Reader) ([]byte, error) {
		docExec(t, db, `update launches set machine = 'host-b' where id = ?`, launch)
		return io.ReadAll(r)
	})
	h.ok(nil, "close", id(lane))
	assertReportRetained(t, h, db, lane, committed, "saved report")
}

func TestDocumentsPurgeBusyReader(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	path := docFile(t, h.dir, "plan.md", "purge text")
	out := h.ok(nil, "doc", "set", id(root), "plan", "--file", path)
	db := h.openDB()
	db.SetMaxOpenConns(1)
	docExec(t, db, `pragma busy_timeout = 5000`)
	readerDB, err := sql.Open("sqlite", "file:"+h.db)
	if err != nil {
		t.Fatal(err)
	}
	defer readerDB.Close()
	reader, err := readerDB.Conn(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	defer reader.Close()
	if _, err := reader.ExecContext(context.Background(), `begin`); err != nil {
		t.Fatal(err)
	}
	defer reader.ExecContext(context.Background(), `rollback`)
	var count int
	if err := reader.QueryRowContext(context.Background(), `select count(*) from doc_blobs`).Scan(&count); err != nil || count != 1 {
		t.Fatalf("reader snapshot: %d %v", count, err)
	}
	var stdout, stderr strings.Builder
	c := &ctx{getenv: h.getenv(nil), db: db, out: &stdout, errw: &stderr}
	start := time.Now()
	if code := runCtx(c, []string{"doc", "rm", id(num(out, "doc_id")), "--purge"}); code != 0 {
		t.Fatalf("purge: %d %s %s", code, stdout.String(), stderr.String())
	}
	if elapsed := time.Since(start); elapsed >= 750*time.Millisecond {
		t.Errorf("purge waited for reader: %s", elapsed)
	} else {
		t.Logf("purge with active reader: %s", elapsed)
	}
	start = time.Now()
	if code := runCtx(c, []string{"note", "after purge", "--as", id(root)}); code != 0 {
		t.Fatalf("next write: %d %s %s", code, stdout.String(), stderr.String())
	}
	if elapsed := time.Since(start); elapsed >= 750*time.Millisecond {
		t.Errorf("next write delayed: %s", elapsed)
	} else {
		t.Logf("next write with active reader: %s", elapsed)
	}
	if timeout := docCount(t, db, `pragma busy_timeout`); timeout != 5000 {
		t.Fatalf("busy timeout not restored: %d", timeout)
	}
	if n := docCount(t, db, `select count(*) from documents`); n != 0 {
		t.Fatalf("purge retained %d rows", n)
	}
	if n := docCount(t, db, `select count(*) from events where kind = 'note' and summary = 'after purge'`); n != 1 {
		t.Fatalf("next write not committed: %d", n)
	}
}

func TestDocumentsFixCapturePanic(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	if err := withTx(db, func(tx *sql.Tx) error {
		if _, err := insertEvent(tx, event{TaskID: root, Kind: "note", Summary: "event survives"}); err != nil {
			return err
		}
		return captureDocument(tx, func() error {
			if _, err := tx.Exec(`insert into doc_blobs(sha256,bytes,body) values ('panic-probe',1,'x')`); err != nil {
				return err
			}
			panic("capture panic")
		})
	}); err != nil {
		t.Fatal(err)
	}
	if docCount(t, db, `select count(*) from doc_blobs`) != 0 || docCount(t, db, `select count(*) from events where summary = 'event survives'`) != 1 {
		t.Fatal("panic leaked capture or lost event")
	}
}
