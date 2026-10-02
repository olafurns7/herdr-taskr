package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

var updateGolden = flag.Bool("update", false, "rewrite testdata golden files")

func errOf(m map[string]any) string { s, _ := m["error"].(string); return s }

// runText runs a command whose stdout is text (handover, adopt).
func (h *harness) runText(env map[string]string, args ...string) (int, string, string) {
	h.t.Helper()
	var out, errb bytes.Buffer
	code := run(append([]string{"--json"}, args...), h.getenv(env), &out, &errb)
	return code, out.String(), errb.String()
}

func TestPlannedLifecycle(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	if out := h.one(exitUsage, nil, "new", "p", "--role", "implementer", "--planned"); !strings.Contains(errOf(out), "--planned needs --parent") {
		t.Fatalf("planned root = %v", out)
	}
	plan := num(h.ok(nil, "new", "later", "--role", "reviewer", "--parent", id(top), "--planned", "--pane", "w9:p7"), "task_id")
	live := h.newTask("now", "implementer", top, "--pane", "w9:p1")
	liveL := h.launch(live)
	if st := h.ok(nil, "new", "x", "--role", "gate", "--parent", id(top), "--planned"); st["status"] != "planned" {
		t.Fatalf("new --planned = %v", st)
	}

	// Every worker command is refused with a clear message.
	for _, args := range [][]string{{"got", "1"}, {"start"}, {"note", "n"}, {"ready", "r"}, {"ask", "q"}, {"done"}, {"fail", "f"}} {
		out := h.one(exitReject, as(plan, 0), args...)
		if !strings.Contains(errOf(out), "planned, not launched") {
			t.Fatalf("%v on a planned task = %v", args, out)
		}
	}
	if out := h.one(exitReject, nil, "prompt", id(plan), "--text", "go"); !strings.Contains(errOf(out), "planned") {
		t.Fatalf("prompt to a planned task = %v", out)
	}
	if calls := h.calls("agent|prompt"); len(calls) != 0 {
		t.Fatalf("a planned task was prompted: %v", calls)
	}

	// Not observed: no liveness entry, no event, no pane token, no inbox traffic.
	db := h.openDB()
	ws, err := watchedTasks(db, nil)
	if err != nil || len(ws) != 1 || ws[0].TaskID != live {
		t.Fatalf("watched = %+v %v", ws, err)
	}
	h.setAgents("w9:p1/idle/2", "w9:p7/idle/2")
	h.ok(nil, "daemon", "--once")
	var n int
	db.QueryRow(`select count(*) from events where task_id = ? or recipient_task_id = ?`, plan, plan).Scan(&n)
	if n != 0 {
		t.Fatalf("planned task has %d events", n)
	}
	d := &daemon{db: db, log: &daemonLog{}, sock: h.herdrSock}
	d.writeTokens()
	if _, ok := d.tokens[plan]; ok {
		t.Fatalf("planned task has a pane token: %+v", d.tokens)
	}
	if _, ok := d.tokens[live]; !ok {
		t.Fatalf("live task lacks a pane token: %+v", d.tokens)
	}

	// status --tree shows it.
	_, lines := h.run(nil, "status", "--tree", id(top))
	found := false
	for _, l := range lines {
		found = found || (num(l, "id") == plan && l["status"] == "planned")
	}
	if !found {
		t.Fatalf("status --tree = %v", lines)
	}

	// launch opens it; the worker can write from then on.
	out := h.ok(nil, "launch", id(plan), "--provider", "codex", "--model", "gpt-6-luna", "--effort", "max")
	if out["status"] != "open" || out["was_planned"] != true {
		t.Fatalf("launch of a planned task = %v", out)
	}
	h.ok(as(plan, num(out, "launch_id")), "note", "started")
	if again := h.ok(nil, "launch", id(live), "--provider", "claude", "--model", "m", "--effort", "e"); again["was_planned"] != nil {
		t.Fatalf("relaunch of an open task = %v", again)
	}
	_ = liveL

	// close drops a plan.
	dropped := num(h.ok(nil, "new", "dropped", "--role", "reviewer", "--parent", id(top), "--planned"), "task_id")
	if out := h.ok(nil, "close", id(dropped)); out["status"] != "closed" {
		t.Fatalf("close planned = %v", out)
	}
	h.one(exitReject, nil, "launch", id(dropped), "--provider", "p", "--model", "m", "--effort", "e")
}

func TestNextLatestWinsAndClear(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", top)
	plan := num(h.ok(nil, "new", "p", "--role", "reviewer", "--parent", id(top), "--planned"), "task_id")
	h.one(exitUsage, nil, "next", id(lane))
	h.one(exitUsage, nil, "next", id(lane), "x", "--clear")
	h.one(exitUsage, nil, "next", id(lane), "   ")
	h.one(exitReject, nil, "next", "999", "x")

	for _, tid := range []int64{top, lane, plan} {
		h.ok(nil, "next", id(tid), "first")
		out := h.ok(nil, "next", id(tid), "second for "+id(tid))
		if out["next"] != "second for "+id(tid) || out["recipient_task_id"] != nil {
			t.Fatalf("next = %v", out)
		}
		n, err := taskNext(h.openDB(), tid)
		if err != nil || n == nil || n.Text != "second for "+id(tid) {
			t.Fatalf("latest next of %d = %+v %v", tid, n, err)
		}
	}
	// Nobody is woken.
	var inbox int
	h.openDB().QueryRow(`select count(*) from events where kind = 'next' and recipient_task_id is not null`).Scan(&inbox)
	if inbox != 0 {
		t.Fatalf("next events with a recipient: %d", inbox)
	}
	if _, st := h.run(nil, "status", "--tree", id(lane)); st[0]["next"] != "second for "+id(lane) {
		t.Fatalf("status next = %v", st)
	}
	if out := h.ok(nil, "next", id(lane), "--clear"); out["next"] != nil {
		t.Fatalf("clear = %v", out)
	}
	if n, _ := taskNext(h.openDB(), lane); n != nil {
		t.Fatalf("cleared next = %+v", n)
	}
	if _, st := h.run(nil, "status", "--tree", id(lane)); st[0]["next"] != nil {
		t.Fatalf("status after clear = %v", st)
	}
	// History stays in the log.
	_, lines := h.run(nil, "log", id(lane))
	var kinds []string
	for _, l := range lines {
		if l["kind"] == "next" {
			kinds = append(kinds, fmt.Sprint(l["summary"]))
		}
	}
	if strings.Join(kinds, ",") != "first,second for "+id(lane)+",<nil>" {
		t.Fatalf("next history = %v", kinds)
	}
	h.ok(nil, "close", id(lane))
	h.one(exitReject, nil, "next", id(lane), "late")
}

func TestDecideRevokeAndOwnerAsks(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("w", "implementer", top)
	wl := h.launch(w)
	other := h.newTask("other", "orchestrator", 0)

	h.one(exitUsage, nil, "decide", "rule")
	h.one(exitUsage, nil, "decide", "--as", id(top))
	h.one(exitUsage, nil, "decide", "--as", id(top), "rule", "--revoke", "3")
	if out := h.one(exitReject, nil, "decide", "--as", id(w), "rule"); !strings.Contains(errOf(out), "root orchestrator") {
		t.Fatalf("decide --as a child = %v", out)
	}
	h.one(exitReject, map[string]string{"TASKR_LAUNCH": "1"}, "decide", "--as", id(top), "rule")
	h.one(exitReject, map[string]string{"TASKR_TASK": id(other)}, "decide", "--as", id(top), "rule")

	d1 := num(h.ok(nil, "decide", "--as", id(top), "Prioritize user-facing failures."), "decision_id")
	d2 := num(h.ok(nil, "decide", "--as", id(top), "Review the model choice."), "decision_id")
	ask := num(h.ok(as(w, wl), "ask", "Merge without asking?", "--owner"), "ask_id")
	plainAsk := num(h.ok(as(w, wl), "ask", "Which file?"), "ask_id")
	openOwner := num(h.ok(as(w, wl), "ask", "Still open?", "--owner"), "ask_id")
	h.ok(nil, "answer", id(ask), "Yes when CI is green.")
	h.ok(nil, "answer", id(plainAsk), "a.go")
	h.ok(nil, "decide", "--as", id(other), "Separate task rule.")

	ds, err := decisionsInForce(h.openDB(), top)
	if err != nil {
		t.Fatal(err)
	}
	var got []string
	for _, d := range ds {
		got = append(got, fmt.Sprintf("%d:%s:%s:%s", d.ID, d.Kind, d.Text, d.Answer))
	}
	want := []string{fmt.Sprintf("%d:decision:Prioritize user-facing failures.:", d1), fmt.Sprintf("%d:decision:Review the model choice.:", d2),
		fmt.Sprintf("%d:owner_ask:Merge without asking?:Yes when CI is green.", ask)}
	if strings.Join(got, "|") != strings.Join(want, "|") {
		t.Fatalf("decisions = %v, want %v (open owner ask %d and plain ask excluded)", got, want, openOwner)
	}

	// Revoke a decision and the owner ask; each once.
	if out := h.ok(nil, "decide", "--as", id(top), "--revoke", id(d1)); num(out, "revoked") != d1 {
		t.Fatalf("revoke = %v", out)
	}
	if out := h.one(exitReject, nil, "decide", "--as", id(top), "--revoke", id(d1)); !strings.Contains(errOf(out), "already revoked") {
		t.Fatalf("second revoke = %v", out)
	}
	h.one(exitReject, nil, "decide", "--as", id(top), "--revoke", id(plainAsk))
	h.one(exitReject, nil, "decide", "--as", id(top), "--revoke", id(openOwner))
	h.one(exitReject, nil, "decide", "--as", id(other), "--revoke", id(d2)) // another root's decision
	h.ok(nil, "decide", "--as", id(top), "--revoke", id(ask))
	ds, _ = decisionsInForce(h.openDB(), top)
	if len(ds) != 1 || ds[0].ID != d2 {
		t.Fatalf("after revokes = %+v", ds)
	}
}

func TestSetRefs(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", top)
	out := h.ok(nil, "set", id(lane), "pr=123", "build.run=run-1", "commit=abc123")
	if len(out["event_ids"].([]any)) != 3 {
		t.Fatalf("set = %v", out)
	}
	out = h.ok(nil, "set", id(lane), "pr=3690", "commit=abc123", "build.run=")
	if ids := out["event_ids"].([]any); len(ids) != 2 {
		t.Fatalf("unchanged value should write no event: %v", out)
	}
	refs, err := taskRefs(h.openDB(), lane)
	if err != nil || fmt.Sprint(refs) != "[{commit abc123} {pr 3690}]" {
		t.Fatalf("refs = %v %v", refs, err)
	}
	h.ok(nil, "set", id(lane), "gone=") // deleting an absent key is a no-op

	for name, args := range map[string][]string{
		"upper":      {"A=1"},
		"digit":      {"1a=x"},
		"long key":   {strings.Repeat("k", 33) + "=x"},
		"space":      {"a b=x"},
		"no equals":  {"pr"},
		"long value": {"v=" + strings.Repeat("é", 201)},
		"newline":    {"v=a\nb"},
		"dup":        {"a=1", "a=2"},
	} {
		if out := h.one(exitUsage, nil, append([]string{"set", id(lane)}, args...)...); errOf(out) == "" {
			t.Errorf("%s: %v", name, out)
		}
	}
	h.ok(nil, "set", id(lane), "v="+strings.Repeat("é", 200), strings.Repeat("k", 32)+"=x", "a.b-c_d=1")

	// At most 20 keys per task: 5 now, 15 more fit, one more is refused.
	var many []string
	for i := 0; i < 15; i++ {
		many = append(many, fmt.Sprintf("k%d=%d", i, i))
	}
	h.ok(nil, append([]string{"set", id(lane)}, many...)...)
	if out := h.one(exitReject, nil, "set", id(lane), "one.more=x"); !strings.Contains(errOf(out), "at most 20") {
		t.Fatalf("21st key = %v", out)
	}
	h.ok(nil, "set", id(lane), "k0=", "one.more=x") // a delete makes room in the same call
	h.ok(nil, "close", id(lane))
	h.one(exitReject, nil, "set", id(lane), "pr=1")
}

// handoverFixture seeds a representative ledger: decisions (one revoked), an answered
// and an open owner ask, live lanes (nested, observed, gone), a planned lane,
// refs and next steps, and one closed lane. Timestamps are pinned so the
// rendering is golden.
type handoverFixture struct {
	top, sub, impl, rev, plan, closed int64
	implL                             int64
}

const (
	tSeed     = "2026-09-29T10:00:00.000Z"
	tClosed   = "2026-09-29T10:30:00.000Z"
	tObserved = "2026-09-29T11:50:00.000Z"
)

var tRender = time.Date(2026, 9, 29, 12, 0, 0, 0, time.UTC)

func seedHandover(h *harness) handoverFixture {
	h.t.Helper()
	var f handoverFixture
	f.top = h.newTask("orch-a", "orchestrator", 0, "--workspace", "w1", "--tab", "w1:t1", "--pane", "w1:p1")
	f.sub = h.newTask("orch-b", "sub-orchestrator", f.top, "--pane", "w2:p1", "--report", "reports/design.md")
	h.launch(f.sub, "--agent", "orch-b")
	f.impl = h.newTask("impl-a", "implementer", f.sub, "--pane", "w3:p1", "--report", "reports/worker-a.md")
	f.implL = h.launch(f.impl)
	f.rev = h.newTask("rev-a", "reviewer", f.top, "--pane", "w1:p2")
	h.launch(f.rev)
	f.plan = num(h.ok(nil, "new", "impl-b", "--role", "implementer", "--parent", id(f.top), "--planned"), "task_id")
	f.closed = h.newTask("impl-c", "implementer", f.top, "--pane", "w1:p3", "--report", "reports/worker-c.md")
	cl := h.launch(f.closed)
	h.ok(as(f.closed, cl), "done", "change #123 merged")
	h.ok(nil, "close", id(f.closed))

	h.ok(nil, "decide", "--as", id(f.top), "Resolve user-facing failures first.")
	old := num(h.ok(nil, "decide", "--as", id(f.top), "Previous priority."), "decision_id")
	h.ok(nil, "decide", "--as", id(f.top), "--revoke", id(old))
	h.ok(nil, "decide", "--as", id(f.top), "Merge when checks pass and there is no user impact.\nOtherwise ask.")
	ans := num(h.ok(as(f.impl, f.implL), "ask", "Ship the release tonight?", "--owner"), "ask_id")
	h.ok(nil, "answer", id(ans), "Yes, after the final test.")
	h.ok(as(f.impl, f.implL), "ask", "Which release channel?", "--blocking")
	h.ok(nil, "ask", "--as", id(f.top), "Approve the initial design?", "--owner")

	h.ok(nil, "next", id(f.top), "Check the current release, then prepare the next change.")
	h.ok(nil, "next", id(f.impl), "After ready, ask the owner to confirm the settings page.")
	h.ok(nil, "next", id(f.plan), "Start after the current task completes.")
	h.ok(nil, "next", id(f.rev), "stale")
	h.ok(nil, "next", id(f.rev), "--clear")
	h.ok(nil, "set", id(f.top), "pr=123", "branch=feature/dashboard")
	h.ok(nil, "set", id(f.impl), "build.run=run-1", "commit=abc123")
	h.ok(nil, "set", id(f.plan), "brief=plans/worker-b.md")

	db := h.openDB()
	for _, q := range []string{
		`update events set created_at = '` + tSeed + `'`,
		`update tasks set created_at = '` + tSeed + `', updated_at = '` + tSeed + `'`,
		`update tasks set closed_at = '` + tClosed + `' where closed_at is not null`,
		`update launches set observed_status = 'working', observed_at = '` + tObserved + `', present = 1 where task_id = ` + id(f.impl),
		`update launches set observed_at = '` + tObserved + `', present = 0 where task_id = ` + id(f.rev),
	} {
		if _, err := db.Exec(q); err != nil {
			h.t.Fatal(err)
		}
	}
	return f
}

func checkGolden(t *testing.T, name, got string) {
	t.Helper()
	path := filepath.Join("testdata", name)
	if *updateGolden {
		os.MkdirAll("testdata", 0o755)
		if err := os.WriteFile(path, []byte(got), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	want, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("%v (run go test -run %s -update once)", err, t.Name())
	}
	if got != string(want) {
		t.Fatalf("%s differs from the golden file:\n--- got\n%s\n--- want\n%s", name, got, want)
	}
}

func pinHandoverClock(t *testing.T) {
	prev := handoverNow
	handoverNow = func() time.Time { return tRender }
	t.Cleanup(func() { handoverNow = prev })
}

func TestHandoverGolden(t *testing.T) {
	h := newHarness(t)
	pinHandoverClock(t)
	f := seedHandover(h)
	norm := func(s string) string {
		return strings.ReplaceAll(strings.ReplaceAll(s, md(h.dir), "$DIR"), h.dir, "$DIR")
	}

	h.one(exitUsage, nil, "handover")
	h.one(exitReject, nil, "handover", "--as", id(f.sub))
	h.one(exitReject, map[string]string{"TASKR_LAUNCH": "1"}, "handover", "--as", id(f.top))

	outFile := filepath.Join(h.dir, "handoff.md")
	code, text, stderr := h.runText(nil, "handover", "--as", id(f.top), "--out", outFile)
	if code != 0 {
		t.Fatalf("handover exit %d: %s", code, stderr)
	}
	checkGolden(t, "handover-1.md", norm(text))
	if b, _ := os.ReadFile(outFile); string(b) != text {
		t.Fatalf("--out file differs from stdout")
	}
	// Stable: the same ledger renders the same text.
	db := h.openDB()
	d1, err := loadHandover(db, f.top, nil)
	if err != nil {
		t.Fatal(err)
	}
	if again := renderHandover(d1, tRender); again != text {
		t.Fatalf("re-render differs:\n%s", again)
	}

	// The recorded event.
	var eid int64
	var data string
	db.QueryRow(`select id, data from events where kind = 'handover' and task_id = ?`, f.top).Scan(&eid, &data)
	if !strings.Contains(stderr, fmt.Sprintf("recorded event %d", eid)) {
		t.Fatalf("stderr = %q", stderr)
	}
	var rec struct {
		SHA256 string         `json:"sha256"`
		Out    string         `json:"out"`
		Counts map[string]int `json:"counts"`
	}
	json.Unmarshal([]byte(data), &rec)
	sum := sha256.Sum256([]byte(text))
	if rec.SHA256 != hex.EncodeToString(sum[:]) || rec.Out != outFile ||
		fmt.Sprint(rec.Counts) != "map[closed_since:1 decisions:3 live:3 open_asks:2 planned:1]" {
		t.Fatalf("handover event data = %s", data)
	}

	// Close a lane after the first handover: the second lists only that one.
	db.Exec(`update events set created_at = '2026-09-29T11:00:00.000Z' where id = ?`, eid)
	h.ok(nil, "close", id(f.rev))
	db.Exec(`update tasks set closed_at = '2026-09-29T11:30:00.000Z' where id = ?`, f.rev)
	code, text2, _ := h.runText(nil, "handover", "--as", id(f.top), "--note", "Wait for owner review before merging.")
	if code != 0 {
		t.Fatal("second handover failed")
	}
	checkGolden(t, "handover-2.md", norm(text2))
	if !strings.Contains(text2, fmt.Sprintf("Closed since the previous handover (event %d", eid)) ||
		strings.Contains(text2, md("impl-c")) || !strings.Contains(text2, md("rev-a")) {
		t.Fatalf("second handover closed section:\n%s", text2)
	}
}

func TestAdopt(t *testing.T) {
	h := newHarness(t)
	pinHandoverClock(t)
	f := seedHandover(h)
	db := h.openDB()

	h.one(exitUsage, nil, "adopt", id(f.top)) // no pane anywhere
	if out := h.one(exitReject, map[string]string{"HERDR_PANE_ID": "w9:p1"}, "adopt", id(f.impl)); !strings.Contains(errOf(out), "root orchestrator") {
		t.Fatalf("adopt a child = %v", out)
	}
	h.one(exitReject, map[string]string{"HERDR_PANE_ID": "w9:p1"}, "adopt", "999")
	gone := h.newTask("gone", "orchestrator", 0)
	h.ok(nil, "close", id(gone))
	h.one(exitReject, map[string]string{"HERDR_PANE_ID": "w9:p1"}, "adopt", id(gone))

	// An event offered to the predecessor and never acked.
	h.ok(as(f.impl, f.implL), "ready", "slice", "--report", "r.md") // to orch-b, not the root
	h.ok(nil, "note", "--as", id(f.top), "phase 3")
	w := h.ok(nil, "wait", "--as", id(f.top), "--timeout", "0")
	pending := num(eventOf(w), "id")
	var acked int64
	db.QueryRow(`select acked_event_id from tasks where id = ?`, f.top).Scan(&acked)
	db.Exec(`update tasks set waiting_until = '2099-01-01T00:00:00.000Z' where id = ?`, f.top)

	h.runText(nil, "handover", "--as", id(f.top), "--note", "first")
	db.Exec(`update events set created_at = '2026-09-29T11:00:00.000Z' where kind = 'handover'`)
	h.ok(nil, "close", id(f.rev))
	db.Exec(`update tasks set closed_at = '2026-09-29T11:30:00.000Z' where id = ?`, f.rev)
	h.runText(nil, "handover", "--as", id(f.top), "--note", "Resume the active work.")
	db.Exec(`update events set created_at = '2026-09-29T11:40:00.000Z' where kind = 'handover' and created_at != '2026-09-29T11:00:00.000Z'`)

	// Defaults from the environment.
	env := map[string]string{"HERDR_WORKSPACE_ID": "w1", "HERDR_TAB_ID": "w1:t1", "HERDR_PANE_ID": "w1:p1"}
	code, text, stderr := h.runText(env, "adopt", id(f.top))
	if code != 0 {
		t.Fatalf("adopt exit %d: %s", code, stderr)
	}
	checkGolden(t, "adopt.md", strings.ReplaceAll(strings.ReplaceAll(text, md(h.dir), "$DIR"), h.dir, "$DIR"))
	if !strings.Contains(text, "## Note\n\n"+md("Resume the active work.")) || !strings.Contains(text, "Closed since the previous handover") ||
		!strings.Contains(text, md("rev-a")) || !strings.Contains(text, "pane w1:p1") {
		t.Fatalf("adopt rendering:\n%s", text)
	}
	var ws, tab, pane string
	var waiting *string
	var acked2 int64
	db.QueryRow(`select workspace_id, tab_id, pane_id, waiting_until, acked_event_id from tasks where id = ?`, f.top).
		Scan(&ws, &tab, &pane, &waiting, &acked2)
	if ws != "w1" || tab != "w1:t1" || pane != "w1:p1" || waiting != nil || acked2 != acked {
		t.Fatalf("root after adopt = %s %s %s waiting %v acked %d (was %d)", ws, tab, pane, waiting, acked2, acked)
	}
	var adata string
	db.QueryRow(`select data from events where kind = 'adopt' order by id desc limit 1`).Scan(&adata)
	if adata != `{"new":{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1"},"old":{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1"}}` {
		t.Fatalf("adopt event data = %s", adata)
	}

	// Explicit flags win over the environment.
	code, _, _ = h.runText(env, "adopt", id(f.top), "--pane", "w5:p1", "--tab", "w5:t1", "--workspace", "w5")
	db.QueryRow(`select workspace_id, tab_id, pane_id from tasks where id = ?`, f.top).Scan(&ws, &tab, &pane)
	if code != 0 || ws != "w5" || tab != "w5:t1" || pane != "w5:p1" {
		t.Fatalf("explicit adopt = %d %s %s %s", code, ws, tab, pane)
	}

	// The successor's wait replays the predecessor's pending event, then moves on.
	w = h.ok(nil, "wait", "--as", id(f.top), "--timeout", "0")
	if num(eventOf(w), "id") != pending || w["replay"] != true {
		t.Fatalf("successor wait = %v, want replay of %d", w, pending)
	}
	h.ok(nil, "ack", id(pending), "--as", id(f.top))
}

func TestAdoptWithoutHandover(t *testing.T) {
	h := newHarness(t)
	pinHandoverClock(t)
	top := h.newTask("solo", "orchestrator", 0)
	code, text, _ := h.runText(map[string]string{"HERDR_PANE_ID": "w1:p1"}, "adopt", id(top))
	if code != 0 || !strings.Contains(text, "No handover was recorded") || !strings.Contains(text, "Closed lanes (no earlier handover)") {
		t.Fatalf("adopt without a handover = %d\n%s", code, text)
	}
}

func TestDashboardPlanFields(t *testing.T) {
	h := newHarness(t)
	f := seedHandover(h)
	h.runText(nil, "handover", "--as", id(f.top), "--note", "away")
	_, s := getState(t, h.dash())
	if len(s.Orchestrators) != 1 {
		t.Fatalf("orchestrators = %+v", s.Orchestrators)
	}
	o := s.Orchestrators[0]
	if o.Next == nil || o.Next.Text != "Check the current release, then prepare the next change." || fmt.Sprint(o.Refs) != "[{branch feature/dashboard} {pr 123}]" ||
		o.LastHandover == nil || o.LastHandover.Text != "away" {
		t.Fatalf("root plan = next %+v refs %v handover %+v", o.Next, o.Refs, o.LastHandover)
	}
	var kinds []string
	for _, d := range o.Decisions {
		kinds = append(kinds, d.Kind)
	}
	if strings.Join(kinds, ",") != "decision,decision,owner_ask" || o.Decisions[2].Answer != "Yes, after the final test." || o.Decisions[2].From != "impl-a" {
		t.Fatalf("decisions = %+v", o.Decisions)
	}
	byName := map[string]stateTask{}
	for _, tk := range o.Tasks {
		byName[tk.Name] = tk
	}
	impl, plan, rev := byName["impl-a"], byName["impl-b"], byName["rev-a"]
	if impl.Next == nil || impl.Next.Text != "After ready, ask the owner to confirm the settings page." || len(impl.Refs) != 2 {
		t.Fatalf("impl lane = %+v", impl)
	}
	if plan.Status != "planned" || plan.Next == nil || fmt.Sprint(plan.Refs) != "[{brief plans/worker-b.md}]" {
		t.Fatalf("planned lane = %+v", plan)
	}
	if rev.Next != nil || (rev.LastEvent != nil && (rev.LastEvent.Kind == "next" || rev.LastEvent.Kind == "ref")) {
		t.Fatalf("rev lane: a cleared next and bookkeeping must not show: %+v %+v", rev.Next, rev.LastEvent)
	}
	if impl.LastEvent == nil || impl.LastEvent.Kind != "ask" {
		t.Fatalf("impl last event = %+v (next/ref are not the lane's activity)", impl.LastEvent)
	}
	// The page (web/src, v0.7) shows the plan: next steps on each monitor and
	// lane, rules in force in the wire, handovers and pr/release refs as
	// milestones in the cue log.
	src := ""
	for _, f := range []string{"web/src/work.ts", "web/src/components/Dashboard.tsx", "web/src/model.ts"} {
		b, err := os.ReadFile(f)
		if err != nil {
			t.Fatal(err)
		}
		src += string(b)
	}
	for _, want := range []string{"o.next?.text", "t.next?.text", "o.decisions", `handover: { word: "handover"`, `ref: { word: "ref"`} {
		if !strings.Contains(src, want) {
			t.Fatalf("web/src lacks %s", want)
		}
	}
}

func TestHubPeerStateWithAndWithoutPlanFields(t *testing.T) {
	h := newHarness(t)
	d := h.hubDash()
	at := time.Now()
	nowS := `"now":"` + at.UTC().Format(time.RFC3339Nano) + `","applied":[]`
	ts := stamp(at.Add(-time.Minute))
	older := `{"state":{"now":"x","version":"v0.5.0","owner_asks":[],"orchestrators":[{"id":1,"name":"o","role":"orchestrator",` +
		`"status":"open","created_at":"` + ts + `","note":null,"tasks":[{"id":2,"name":"w","status":"open"}],"closed_tasks":0}],` +
		`"activity":[],"closed":[]},` + nowS + `}`
	if c, _, body := pushTo(t, d, hostAIP, older); c != 200 {
		t.Fatalf("v0.5 snapshot = %d %s", c, body)
	}
	v := hubView(t, d)
	o := v.Machines[1].State.Orchestrators[0]
	if o.Next != nil || o.Refs != nil || o.Decisions != nil || o.LastHandover != nil || o.Tasks[0].Next != nil {
		t.Fatalf("older peer decoded with plan fields: %+v", o)
	}

	var decisions []string
	for i := 0; i < stateDecisionMax+5; i++ {
		decisions = append(decisions, fmt.Sprintf(`{"id":%d,"kind":"decision","text":"r%d","at":"%s","age_ms":1}`, 100+i, i, ts))
	}
	newer := `{"state":{"now":"x","version":"v0.6.0","owner_asks":[],"orchestrators":[{"id":1,"name":"o","role":"orchestrator",` +
		`"status":"open","created_at":"` + ts + `","tasks":[{"id":2,"name":"w","status":"planned","next":{"text":"lane next","at":"` + ts +
		`","age_ms":5},"refs":[{"key":"pr","value":"1"}]}],"next":{"text":"root next","at":"` + ts + `","age_ms":5},` +
		`"refs":[{"key":"branch","value":"main"}],"decisions":[` + strings.Join(decisions, ",") + `],` +
		`"last_handover":{"text":"note","at":"` + ts + `","age_ms":5}}],"activity":[],"closed":[]},` + nowS + `}`
	if c, _, body := pushTo(t, d, hostAIP, newer); c != 200 {
		t.Fatalf("v0.6 snapshot = %d %s", c, body)
	}
	o = hubView(t, d).Machines[1].State.Orchestrators[0]
	if o.Next == nil || o.Next.Text != "root next" || o.Next.AgeMS < 50000 || fmt.Sprint(o.Refs) != "[{branch main}]" ||
		o.LastHandover == nil || o.Tasks[0].Next == nil || o.Tasks[0].Next.Text != "lane next" || len(o.Tasks[0].Refs) != 1 {
		t.Fatalf("v0.6 fields lost or not rebased: %+v", o)
	}
	if len(o.Decisions) != stateDecisionMax || o.Decisions[0].Text != "r5" || o.Decisions[0].AgeMS < 50000 {
		t.Fatalf("decisions capped to the latest %d and rebased: %d first %+v", stateDecisionMax, len(o.Decisions), o.Decisions[0])
	}
}

// Closed-since is decided by event ids: a lane closed in the same
// millisecond as the handover, or under a clock that went backwards, is still
// listed once, and consecutive handovers never repeat a lane.
func TestHandoverClosedSinceByEventID(t *testing.T) {
	for _, delta := range []time.Duration{0, -time.Second, -time.Hour} {
		t.Run(delta.String(), func(t *testing.T) {
			h := newHarness(t)
			root := h.newTask("top", "orchestrator", 0)
			a := h.newTask("alpha", "implementer", root)
			b := h.newTask("bravo", "implementer", root)
			db := h.openDB()
			handover := func() (string, handoverMark) {
				t.Helper()
				code, text, stderr := h.runText(nil, "handover", "--as", id(root))
				if code != 0 {
					t.Fatalf("handover exit %d: %s", code, stderr)
				}
				marks, err := handoverMarks(db, root, 1)
				if err != nil || len(marks) != 1 {
					t.Fatalf("marks = %v %v", marks, err)
				}
				return text, marks[0]
			}
			closeAt := func(task int64, m handoverMark) {
				t.Helper()
				h.ok(nil, "close", id(task))
				at := stamp(parseTime(m.At).Add(delta))
				if _, err := db.Exec(`update tasks set closed_at = ? where id = ?`, at, task); err != nil {
					t.Fatal(err)
				}
				db.Exec(`update events set created_at = ? where task_id = ? and kind = 'closed'`, at, task)
			}
			closed := func(m handoverMark) []int64 {
				t.Helper()
				d, err := loadHandover(db, root, &m)
				if err != nil {
					t.Fatal(err)
				}
				var ids []int64
				for _, l := range d.Closed {
					ids = append(ids, l.ID)
				}
				return ids
			}

			_, m1 := handover()
			closeAt(a, m1)
			if got := closed(m1); fmt.Sprint(got) != fmt.Sprint([]int64{a}) {
				t.Fatalf("closed after handover 1 = %v, want [%d]", got, a)
			}
			text2, m2 := handover()
			if !strings.Contains(text2, md("alpha")+" (task") || strings.Contains(text2, md("bravo")+" (task "+id(b)+"), implementer, closed") {
				t.Fatalf("handover 2:\n%s", text2)
			}
			closeAt(b, m2)
			text3, m3 := handover()
			if !strings.Contains(text3, md("bravo")+" (task "+id(b)+"), implementer, closed") || strings.Contains(text3, md("alpha")+" (task") {
				t.Fatalf("handover 3 repeats or misses a lane:\n%s", text3)
			}
			text4, _ := handover()
			if got := closed(m3); len(got) != 0 || !strings.Contains(text4, "(event "+id(m3.EventID)) || !strings.Contains(text4, "None.") {
				t.Fatalf("handover 4 closed = %v:\n%s", got, text4)
			}
		})
	}
}

// A task under a planned ancestor has no inbox path: it cannot be launched,
// cannot write, and the planned task's inbox is refused, until the ancestor
// is launched. The planned subtree itself may still be registered.
func TestPlannedAncestorBlocksSubtree(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("top", "orchestrator", 0)
	plan := h.newTask("future-sub", "sub-orchestrator", root, "--planned")
	child := h.newTask("child", "implementer", plan) // registration is allowed
	grand := num(h.ok(nil, "new", "grand", "--role", "implementer", "--parent", id(child), "--planned"), "task_id")

	for _, task := range []int64{child, grand} {
		if out := h.one(exitReject, nil, "launch", id(task), "--provider", "claude", "--model", "m", "--effort", "e"); !strings.Contains(errOf(out), "under planned task "+id(plan)) {
			t.Fatalf("launch %d under a planned task = %v", task, out)
		}
	}
	if out := h.one(exitReject, as(child, 0), "ready", "finished"); !strings.Contains(errOf(out), "under planned task") {
		t.Fatalf("launchless write under a planned task = %v", out)
	}
	if out := h.one(exitReject, nil, "prompt", id(child), "--text", "go"); !strings.Contains(errOf(out), "planned") {
		t.Fatalf("prompt under a planned task = %v", out)
	}
	if out := h.one(exitReject, nil, "wait", "--as", id(plan), "--timeout", "0"); !strings.Contains(errOf(out), "planned") {
		t.Fatalf("wait --as a planned task = %v", out)
	}
	if out := h.one(exitReject, nil, "ack", "1", "--as", id(plan)); !strings.Contains(errOf(out), "planned") {
		t.Fatalf("ack --as a planned task = %v", out)
	}
	var n int
	h.openDB().QueryRow(`select count(*) from events where recipient_task_id = ?`, plan).Scan(&n)
	if n != 0 {
		t.Fatalf("planned task received %d events", n)
	}

	// Launch the ancestor: the subtree works as usual.
	h.launch(plan)
	cl := h.launch(child)
	ev := h.ok(as(child, cl), "ready", "finished")
	got := h.ok(nil, "wait", "--as", id(plan), "--timeout", "0")
	if num(eventOf(got), "id") != num(ev, "event_id") {
		t.Fatalf("wait after launch = %v, want event %d", got, num(ev, "event_id"))
	}
	h.ok(nil, "ack", id(num(ev, "event_id")), "--as", id(plan))
}

// Worker-event routing refuses a planned recipient on its own, behind the
// launch and resolve guards: the insert paths call it inside their transaction.
func TestNotPlannedRecipientDefensive(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("top", "orchestrator", 0)
	plan := num(h.ok(nil, "new", "later", "--role", "sub-orchestrator", "--parent", id(root), "--planned"), "task_id")
	tx, err := h.openDB().Begin()
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	if err := notPlannedRecipient(tx, &plan); err == nil || !strings.Contains(err.Error(), "planned") {
		t.Fatalf("planned recipient = %v", err)
	}
	if err := notPlannedRecipient(tx, &root); err != nil {
		t.Fatalf("open recipient = %v", err)
	}
	if err := notPlannedRecipient(tx, nil); err != nil {
		t.Fatalf("no recipient = %v", err)
	}
}

// unescapedPipes counts the pipes a Markdown table would split on.
func unescapedPipes(s string) int {
	n := 0
	for i := 0; i < len(s); i++ {
		switch s[i] {
		case '\\':
			i++
		case '|':
			n++
		}
	}
	return n
}

// Every ledger string is escaped once: backticks, pipes, a backslash before a
// pipe, HTML and newlines cannot break the table or inject markup.
func TestHandoverEscapesLedgerText(t *testing.T) {
	h := newHarness(t)
	pinHandoverClock(t)
	root := h.newTask("top", "orchestrator", 0)
	child := h.newTask("worker", "implementer", root, "--pane", "w9:p1", "--report", "r|`x`.md")
	cl := h.launch(child, "--model", "m|`x`<b>", "--agent", "agent1")
	// Preserve escaping coverage for a legacy stored agent name that new input rejects.
	if _, err := h.openDB().Exec(`update tasks set agent_name = ? where id = ?`, "ag|ent", child); err != nil {
		t.Fatal(err)
	}
	h.ok(nil, "set", id(child), "tag=<details>hidden</details>", "literal=`code`", `slash=a\|b`, "amp=a&b")
	h.ok(nil, "next", id(child), "first\nsecond | <b>unsafe</b> `tick` \\| [x](y) # *z*")
	h.ok(nil, "decide", "--as", id(root), "Rule <script>x</script>\n| a | b |")
	h.ok(as(child, cl), "ask", "pick `a` | <i>b</i>?", "--blocking")
	db := h.openDB()
	db.Exec(`update events set created_at = '` + tSeed + `'`)
	db.Exec(`update tasks set created_at = '` + tSeed + `', updated_at = '` + tSeed + `'`)

	code, text, stderr := h.runText(nil, "handover", "--as", id(root), "--note", "line one\n<b>two</b> | `three`")
	if code != 0 {
		t.Fatalf("handover exit %d: %s", code, stderr)
	}
	for _, raw := range []string{"<b>", "<details>", "<script>", "<i>", "`code`", "`tick`", "a&b", "[x](y)"} {
		if strings.Contains(text, raw) {
			t.Fatalf("raw %q in the handover:\n%s", raw, text)
		}
	}
	var row string
	for _, l := range strings.Split(text, "\n") {
		if strings.HasPrefix(l, "| worker (task") {
			row = l
		}
	}
	if row == "" || unescapedPipes(row) != 11 {
		t.Fatalf("live-lane row has %d unescaped pipes, want 11: %q", unescapedPipes(row), row)
	}
	if !strings.Contains(row, `a\\\|b`) || !strings.Contains(row, "first second") {
		t.Fatalf("backslash-pipe or newline not escaped: %q", row)
	}
	checkGolden(t, "handover-escape.md", strings.ReplaceAll(strings.ReplaceAll(text, md(h.dir), "$DIR"), h.dir, "$DIR"))
}
