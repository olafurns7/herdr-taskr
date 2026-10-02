package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"
)

func TestPromptOutcomes(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p4")
	l := h.launch(w)
	h.ok(as(w, l), "done", "round 1")
	brief := filepath.Join(h.dir, "brief.md")
	os.WriteFile(brief, []byte("Round 2."), 0o644)

	h.write("prompt.stdout", `{"result":{"agent":{"agent_status":"working"}}}`, 0o644)
	out := h.ok(nil, "prompt", id(w), "--file", brief)
	if out["outcome"] != "activity_observed" || out["agent_status"] != "working" {
		t.Fatalf("prompt = %v", out)
	}
	_, st := h.run(nil, "status", "--tree", id(w))
	if st[0]["status"] != "open" {
		t.Fatalf("prompt must reopen the task: %v", st[0])
	}

	for code, want := range map[string]string{"agent_blocked": "rejected", "agent_prompt_stalled": "delivery_unknown"} {
		h.write("prompt.stderr", `{"error":{"code":"`+code+`"}}`, 0o644)
		h.write("prompt.exit", "1", 0o644)
		if out := h.one(exitHerdr, nil, "prompt", id(w), "--file", brief); out["outcome"] != want {
			t.Fatalf("%s: outcome %v, want %s", code, out["outcome"], want)
		}
	}

	// answer --prompt reports delivered only on observed activity.
	ask := num(h.ok(as(w, l), "ask", "Which copy?"), "ask_id")
	h.write("prompt.exit", "0", 0o644)
	os.Remove(filepath.Join(h.bin, "prompt.stderr"))
	ans := h.ok(nil, "answer", id(ask), "The approved one.", "--prompt")
	if ans["delivered"] != true || num(ans, "answer_id") == 0 {
		t.Fatalf("answer --prompt = %v", ans)
	}
	h.one(exitUsage, nil, "prompt", id(top), "--file", brief) // root has no pane
	h.ok(nil, "close", id(w))
	h.one(exitReject, nil, "prompt", id(w), "--file", brief)
	if n := len(h.calls("agent|prompt|")); n != 4 {
		t.Fatalf("herdr prompt calls = %d, want 4", n)
	}
}

func TestWaitLivenessEmitsAndThrottles(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	h.launch(w)
	h.ok(nil, "prompt", id(w), "--text", "go", "--receipt-timeout", "0") // an owed result, no receipt deadline: the idle hint emits
	h.setAgents("w9:p1/idle/3")

	got := h.ok(nil, "wait", "--as", id(top), "--timeout", "2000")
	ev := eventOf(got)
	if ev["kind"] != "herdr" || ev["data"].(map[string]any)["agent_status"] != "idle" {
		t.Fatalf("wait = %v, want herdr idle event", got)
	}
	h.ok(nil, "ack", id(num(ev, "id")), "--as", id(top))

	h.setAgents("w9:p1/done/4")
	h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "300")
	if n := len(h.calls("agent|list|")); n != 1 {
		t.Fatalf("agent list calls = %d, want 1 within the 15 s interval", n)
	}

	livenessInterval = 0
	defer func() { livenessInterval = 15 * time.Second }()
	got = h.ok(nil, "wait", "--as", id(top), "--timeout", "2000")
	if d := eventOf(got)["data"].(map[string]any); d["agent_status"] != "done" || d["state_change_seq"] != float64(4) {
		t.Fatalf("wait = %v, want herdr done seq 4", got)
	}
}

func TestExpiredWaitDoesNotRunHerdr(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	h.launch(w)
	h.setAgents("w9:p1/idle/3")
	// The zero-call assertion is primary; the elapsed bound is a backstop that
	// stays well under the fake list's 10 s sleep.
	h.write("list.sleep", "10", 0o644)
	began := time.Now()
	if out := h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "0"); out["timeout"] != true {
		t.Fatalf("wait --timeout 0 = %v", out)
	}
	if calls := h.calls("agent|list|"); len(calls) != 0 || time.Since(began) > 5*time.Second {
		t.Fatalf("expired wait ran herdr %d times in %v", len(calls), time.Since(began))
	}
}

func TestLivenessFailureLeavesObservations(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	h.launch(w)
	h.ok(nil, "prompt", id(w), "--text", "go", "--receipt-timeout", "0") // an owed result, no receipt deadline: the idle hint emits
	h.setAgents("w9:p1/idle/3")
	got := h.ok(nil, "wait", "--as", id(top), "--timeout", "2000")
	h.ok(nil, "ack", id(num(eventOf(got), "id")), "--as", id(top))
	for _, bad := range []struct{ list, exit string }{{"not json", "0"}, {`{"result":{}}`, "0"}, {"{}", "1"}} {
		h.write("list.json", bad.list, 0o644)
		h.write("list.exit", bad.exit, 0o644)
		h.openDB().Exec(`update tasks set last_poll_at = null`)
		if out := h.one(exitHerdr, nil, "wait", "--as", id(top), "--timeout", "500"); out["kind"] != "herdr" {
			t.Fatalf("wait with %q = %v", bad.list, out)
		}
	}
	var version, seq int
	var status string
	var present bool
	h.openDB().QueryRow(`select observed_version, observed_status, observed_seq, present from launches`).Scan(&version, &status, &seq, &present)
	if version != 1 || status != "idle" || seq != 3 || !present || h.countHerdr(top) != 1 {
		t.Fatalf("failed listings changed observations: version=%d status=%s seq=%d present=%v events=%d",
			version, status, seq, present, h.countHerdr(top))
	}
}

func TestOwnerAsksRouteToRoot(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	sub := h.newTask("orch-sub", "sub-orchestrator", top)
	ls := h.launch(sub)
	w := h.newTask("impl-a", "implementer", sub)
	lw := h.launch(w)

	ask := h.ok(as(w, lw), "ask", "Ship the banner copy?", "--owner", "--blocking")
	if num(ask, "recipient_task_id") != top {
		t.Fatalf("owner ask = %v, want recipient %d", ask, top)
	}
	h.ok(as(sub, ls), "ask", "Normal question")
	rootAsk := h.ok(as(top, 0), "ask", "Owner: approve release?", "--owner")
	if _, ok := rootAsk["recipient_task_id"]; ok {
		t.Fatalf("root's own owner ask must have no recipient: %v", rootAsk)
	}
	_, owner := h.run(nil, "asks", "--owner", "--open")
	if len(owner) != 2 {
		t.Fatalf("owner asks = %v", owner)
	}
	ans := h.ok(nil, "answer", id(num(ask, "ask_id")), "Yes.")
	got := h.ok(nil, "wait", "--as", id(w), "--timeout", "0")
	if num(eventOf(got), "id") != num(ans, "answer_id") {
		t.Fatalf("worker inbox = %v, want the answer", got)
	}
	_, owner = h.run(nil, "asks", "--owner", "--open")
	if len(owner) != 1 {
		t.Fatalf("open owner asks after answer = %v", owner)
	}
}

func TestExitCodesAndIdempotentKeys(t *testing.T) {
	h := newHarness(t)
	h.one(exitUsage, nil)
	h.one(exitUsage, nil, "bogus")
	h.one(exitUsage, nil, "new", "x")
	h.one(exitUsage, nil, "note", "no task env")
	h.one(exitUsage, nil, "wait")
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top)
	l := h.launch(w)
	h.one(exitReject, as(999, 0), "note", "no such task")
	h.one(exitReject, nil, "new", "y", "--role", "implementer", "--parent", "999")

	first := h.ok(as(w, l), "ready", "slice 1", "--key", "ready-s1")
	again := h.ok(as(w, l), "ready", "slice 1", "--key", "ready-s1")
	if num(again, "event_id") != num(first, "event_id") || again["duplicate"] != true {
		t.Fatalf("retried write = %v, first = %v", again, first)
	}
	h.one(exitReject, as(w, l), "note", "same key, other kind", "--key", "ready-s1")
	a1 := h.ok(as(w, l), "ask", "which base?", "--key", "ask-base")
	a2 := h.ok(as(w, l), "ask", "which base?", "--key", "ask-base")
	if num(a2, "ask_id") != num(a1, "ask_id") || a2["duplicate"] != true {
		t.Fatalf("retried ask = %v, first = %v", a2, a1)
	}

	h.db = h.dir // a directory: SQLite cannot open it
	code, out := h.run(nil, "status")
	if code != exitDB || len(out) != 1 || out[0]["kind"] != "database" {
		t.Fatalf("database error: exit %d %v", code, out)
	}
}

func TestPragmasOnEveryConnection(t *testing.T) {
	h := newHarness(t)
	db := h.openDB()
	cx := context.Background()
	for i := 0; i < 3; i++ {
		conn, err := db.Conn(cx)
		if err != nil {
			t.Fatal(err)
		}
		defer conn.Close()
		var fk, busy int
		var mode string
		conn.QueryRowContext(cx, `pragma foreign_keys`).Scan(&fk)
		conn.QueryRowContext(cx, `pragma busy_timeout`).Scan(&busy)
		conn.QueryRowContext(cx, `pragma journal_mode`).Scan(&mode)
		if fk != 1 || busy != 5000 || mode != "wal" {
			t.Fatalf("connection %d: foreign_keys=%d busy_timeout=%d journal_mode=%s", i, fk, busy, mode)
		}
	}
	if _, err := db.Exec(`insert into events (task_id, kind, created_at) values (999, 'note', 'x')`); err == nil {
		t.Fatal("foreign key not enforced")
	}
}

func TestAccountLabel(t *testing.T) {
	for in, want := range map[string]string{
		"/Users/user/.local/share/agent/claude/account-a/native":       "account-a",
		"/Users/user/.local/share/agent/codex/account-e/native/.codex": "account-e",
		"/Users/user/.claude": "",
		"":                    "",
		"/native":             "",
	} {
		if got := accountLabel(in); got != want {
			t.Errorf("accountLabel(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestPromptFileSendsPathAndTextSendsLiteral(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p4")
	h.launch(w)
	t.Chdir(h.dir)
	os.WriteFile(filepath.Join(h.dir, "brief.md"), []byte("A long brief body."), 0o644)
	abs, _ := filepath.Abs("brief.md")

	file := h.ok(nil, "prompt", id(w), "--file", "brief.md")
	text := h.ok(nil, "prompt", id(w), "--text", "Continue with slice 4.")
	calls := h.calls("agent|prompt|")
	fa, ta := id(num(file, "attempt_id")), id(num(text, "attempt_id"))
	if len(calls) != 2 ||
		!strings.HasPrefix(calls[0], "agent|prompt|w9:p4|First taskr got "+fa+"; read "+abs+"; execute exactly.|--wait|") ||
		!strings.HasPrefix(calls[1], "agent|prompt|w9:p4|First taskr got "+ta+". Continue with slice 4.|--wait|") {
		t.Fatalf("herdr prompt calls = %q", calls)
	}
	fileSum := sha256.Sum256([]byte("A long brief body."))
	textSum := sha256.Sum256([]byte("Continue with slice 4."))
	for attempt, want := range map[int64][2]any{
		num(file, "attempt_id"): {hex.EncodeToString(fileSum[:]), abs},
		num(text, "attempt_id"): {hex.EncodeToString(textSum[:]), nil},
	} {
		var data string
		h.openDB().QueryRow(`select data from events where id = ?`, attempt).Scan(&data)
		var d map[string]any
		json.Unmarshal([]byte(data), &d)
		if d["sha256"] != want[0] || d["file"] != want[1] {
			t.Fatalf("attempt %d data = %v, want sha256 %v file %v", attempt, d, want[0], want[1])
		}
	}
	h.one(exitUsage, nil, "prompt", id(w))
	h.one(exitUsage, nil, "prompt", id(w), "--file", "brief.md", "--text", "both")
	h.one(exitUsage, nil, "prompt", id(w), "--file", "missing.md")
	if n := len(h.calls("agent|prompt|")); n != 2 {
		t.Fatalf("usage errors ran herdr: %d prompt calls", n)
	}
}

func TestGateTaskHasNoLiveness(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	g := h.newTask("gate-sa1", "gate", top, "--pane", "w9:p1")
	l := num(h.ok(nil, "launch", id(g), "--provider", "shell", "--model", "none", "--effort", "none"), "launch_id")
	h.setAgents("w9:p1/idle/3")
	h.write("read.w9:p1", "You've hit your weekly limit.", 0o644)
	h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "300", "--scan-quota")
	if n := h.countHerdr(top); n != 0 || len(h.calls("agent|")) != 0 {
		t.Fatalf("gate task: herdr events = %d, herdr calls = %q", n, h.calls("agent|"))
	}
	ready := num(h.ok(as(g, l), "ready", "gate sa1", "--kv", "exit=0", "--kv", "static_fail=0"), "event_id")
	got := h.ok(nil, "wait", "--as", id(top), "--timeout", "0")
	if num(eventOf(got), "id") != ready {
		t.Fatalf("wait = %v, want gate ready %d", got, ready)
	}
}

func TestQuotaScanEmitsOncePerDistinctKey(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	a := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	b := h.newTask("impl-b", "implementer", top, "--pane", "w9:p2")
	c := h.newTask("impl-c", "implementer", top, "--pane", "w9:p3")
	la, lb := h.launch(a), h.launch(b)
	h.launch(c) // w9:p3 has no read file: agent read fails, silently
	h.setAgents("w9:p1/working/1", "w9:p2/working/1", "w9:p3/working/1")
	const marker = "SECRET-PANE-MARKER-7f3a"
	h.write("read.w9:p1", `{"result":{"text":"`+marker+` Weekly limit: 50% left"}}`, 0o644)
	h.write("read.w9:p2", marker+" all quiet", 0o644)
	db := h.openDB()
	due := func() { db.Exec(`update tasks set last_poll_at = null where id = ?`, top) }

	// The CLI flag: one throttled pass, quota lines absent, nothing emitted.
	// w9:p3's read fails with an error on its stderr; taskr stays silent.
	due()
	h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "1500", "--scan-quota")
	if n := h.countHerdr(top); n != 0 {
		t.Fatalf("no quota lines: herdr events = %d", n)
	}
	if calls := h.calls("agent|read|"); len(calls) != 3 || calls[0] != "agent|read|w9:p1|--source|visible|" {
		t.Fatalf("agent read calls = %q, want one per child", calls)
	}
	if errs := h.calls("stderr|"); len(errs) != 1 || errs[0] != `stderr|w9:p3|{"error":{"code":"agent_not_idle"}}` {
		t.Fatalf("failed reads = %q, want w9:p3 once", errs)
	}
	if e := h.lastStderr(); e != "" {
		t.Fatalf("failed read surfaced on taskr stderr: %q", e)
	}
	due()
	h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "1500")
	if n := len(h.calls("agent|read|")); n != 3 {
		t.Fatalf("wait without --scan-quota read panes: %d calls", n)
	}

	scan := func() {
		t.Helper()
		due()
		if err := maybeObserve(db, h.herdrSock, top, time.Now().Add(5*time.Second), true); err != nil {
			t.Fatal(err)
		}
	}
	// A, A: one event per launch and key.
	h.write("read.w9:p1", marker+"\nweekly limit: 8% left\n", 0o644)
	h.write("read.w9:p2", marker+"\nYou've HIT YOUR USAGE LIMIT. Resets at 5pm.\n", 0o644)
	scan()
	scan()
	if n := h.countHerdr(top); n != 2 {
		t.Fatalf("after two scans with the same keys: herdr events = %d, want 2", n)
	}
	// A -> B -> A on one launch: two events, not three.
	h.write("read.w9:p1", marker+"\nweekly limit: 5% left\n", 0o644)
	scan()
	if n := h.countHerdr(top); n != 3 {
		t.Fatalf("distinct key: herdr events = %d, want 3", n)
	}
	h.write("read.w9:p1", marker+"\nweekly limit: 8% left\n", 0o644)
	scan()
	if n := h.countHerdr(top); n != 3 {
		t.Fatalf("A->B->A on one launch: herdr events = %d, want 3", n)
	}
	// A new launch on the same task emits A again.
	la2 := h.launch(a)
	scan()
	scan()
	if n := h.countHerdr(top); n != 4 {
		t.Fatalf("new launch: herdr events = %d, want 4", n)
	}

	want := []struct {
		task    int64
		key     string
		summary string
		data    map[string]any
	}{
		{a, fmt.Sprintf("quota:%d:low:8", la), "quota 8% left", map[string]any{"quota": "low", "percent": float64(8), "pane_id": "w9:p1"}},
		{b, fmt.Sprintf("quota:%d:limit", lb), "quota limit hit", map[string]any{"quota": "limit", "percent": float64(0), "pane_id": "w9:p2"}},
		{a, fmt.Sprintf("quota:%d:low:5", la), "quota 5% left", map[string]any{"quota": "low", "percent": float64(5), "pane_id": "w9:p1"}},
		{a, fmt.Sprintf("quota:%d:low:8", la2), "quota 8% left", map[string]any{"quota": "low", "percent": float64(8), "pane_id": "w9:p1"}},
	}
	for i, w := range want {
		got := h.ok(nil, "wait", "--as", id(top), "--timeout", "0")
		ev := eventOf(got)
		if num(ev, "task_id") != w.task || ev["kind"] != "herdr" || ev["event_key"] != w.key || ev["summary"] != w.summary ||
			!reflect.DeepEqual(ev["data"], w.data) {
			t.Fatalf("quota event %d = %v, want task %d %s %q %v", i, ev, w.task, w.key, w.summary, w.data)
		}
		h.ok(nil, "ack", id(num(ev, "id")), "--as", id(top))
	}

	// Every w9:p3 read failed on stderr and produced nothing.
	if reads, errs := h.calls("agent|read|w9:p3|"), h.calls("stderr|w9:p3|"); len(reads) < 7 || len(errs) != len(reads) {
		t.Fatalf("w9:p3 reads = %d, failed reads logged = %d", len(reads), len(errs))
	}
	var nc int
	db.QueryRow(`select count(*) from events where task_id = ? and kind = 'herdr'`, c).Scan(&nc)
	if nc != 0 {
		t.Fatalf("failed reads emitted %d events", nc)
	}
	files, _ := filepath.Glob(filepath.Join(filepath.Dir(h.db), "*"))
	for _, f := range files {
		if b, _ := os.ReadFile(f); bytes.Contains(b, []byte(marker)) {
			t.Fatalf("pane text stored in %s", f)
		}
	}
}

// A descendant of herdr that keeps stdout open cannot hold wait past its
// deadline by more than herdrWaitDelay.
func TestHerdrSubprocessPipeIsBounded(t *testing.T) {
	// 2 s leaves the fake room to start (and the list to finish) before the
	// deadline even under load, so the held call really runs; wait still ends
	// within timeout + herdrWaitDelay + 1 s = 5 s, well short of the 10 s descendant.
	for _, tc := range []struct {
		hold, calls string
		exit        int
	}{
		{"read.hold", "agent|read|w9:p1|", exitTimeout},
		{"list.hold", "agent|list|", exitHerdr},
	} {
		t.Run(tc.hold, func(t *testing.T) {
			h := newHarness(t)
			top := h.newTask("top", "orchestrator", 0)
			w := h.newTask("impl", "implementer", top, "--pane", "w9:p1")
			h.launch(w)
			h.setAgents("w9:p1/working/1")
			h.write(tc.hold, "", 0o644)
			start := time.Now()
			h.one(tc.exit, nil, "wait", "--as", id(top), "--timeout", "2000", "--scan-quota")
			if d := time.Since(start); d > 2*time.Second+herdrWaitDelay+time.Second {
				t.Fatalf("wait took %v with a descendant holding stdout", d)
			}
			if n := len(h.calls(tc.calls)); n != 1 {
				t.Fatalf("%s calls = %d, want 1", tc.calls, n)
			}
		})
	}
}

func TestQuotaOf(t *testing.T) {
	for in, want := range map[string][3]any{
		"You've hit your weekly limit":                   {"limit", 0, true},
		"hit your session limit · weekly limit: 3% left": {"limit", 0, true},
		"Weekly limit: 10% left":                         {"low", 10, true},
		"weekly limit: 11% left":                         {"", 0, false},
		"weekly limit: 40% left\nweekly limit: 9% left":  {"low", 9, true},
		"hit your daily limit":                           {"", 0, false},
		"":                                               {"", 0, false},
	} {
		k, p, ok := quotaOf([]byte(in))
		if k != want[0] || p != want[1] || ok != want[2] {
			t.Errorf("quotaOf(%q) = %q %d %v, want %v", in, k, p, ok, want)
		}
	}
}

// A ledger from the last-key design keeps its launches.quota_key column;
// opening and scanning it still works.
func TestLegacyQuotaKeyColumnIsHarmless(t *testing.T) {
	h := newHarness(t)
	os.MkdirAll(filepath.Dir(h.db), 0o755)
	old, err := sql.Open("sqlite", h.db)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := old.Exec(schemaSQL + `; alter table launches add column quota_key text`); err != nil {
		t.Fatal(err)
	}
	old.Close()
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl", "implementer", top, "--pane", "w9:p1")
	h.launch(w)
	h.setAgents("w9:p1/working/1")
	h.write("read.w9:p1", "hit your weekly limit", 0o644)
	if err := maybeObserve(h.openDB(), h.herdrSock, top, time.Now().Add(5*time.Second), true); err != nil {
		t.Fatal(err)
	}
	if n := h.countHerdr(top); n != 1 {
		t.Fatalf("legacy ledger: herdr events = %d, want 1", n)
	}
}

// launch --workspace/--tab/--pane moves a lane: the task and the new launch
// record the new location, a launch event records old and new, prompt
// targets the new pane, and omitted flags keep the old location.
func TestLaunchRelocates(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl", "implementer", top, "--workspace", "w1", "--tab", "w1:t1", "--pane", "w1:p1")
	h.launch(w)
	db := h.openDB()
	where := func(q string, args ...any) (ws, tab, pane string) {
		t.Helper()
		var a, b, c sql.NullString
		if err := db.QueryRow(q, args...).Scan(&a, &b, &c); err != nil {
			t.Fatal(err)
		}
		return a.String, b.String, c.String
	}
	var before int
	db.QueryRow(`select count(*) from events where kind = 'launch'`).Scan(&before)
	if before != 0 {
		t.Fatalf("a launch without location flags wrote %d launch events", before)
	}

	out := h.ok(nil, "launch", id(w), "--provider", "claude", "--model", "m", "--effort", "e",
		"--workspace", "w2", "--tab", "w2:t4", "--pane", "w2:p7")
	lid := num(out, "launch_id")
	if out["pane_id"] != "w2:p7" || out["tab_id"] != "w2:t4" || out["workspace_id"] != "w2" {
		t.Fatalf("launch output = %v", out)
	}
	if ws, tab, pane := where(`select workspace_id, tab_id, pane_id from tasks where id = ?`, w); ws != "w2" || tab != "w2:t4" || pane != "w2:p7" {
		t.Fatalf("task location = %s %s %s", ws, tab, pane)
	}
	if ws, tab, pane := where(`select workspace_id, tab_id, pane_id from launches where id = ?`, lid); ws != "w2" || tab != "w2:t4" || pane != "w2:p7" {
		t.Fatalf("launch location = %s %s %s", ws, tab, pane)
	}
	var data string
	var recip sql.NullInt64
	db.QueryRow(`select data, recipient_task_id from events where kind = 'launch' and task_id = ?`, w).Scan(&data, &recip)
	if want := fmt.Sprintf(`{"launch_id":%d,"new":{"pane_id":"w2:p7","tab_id":"w2:t4","workspace_id":"w2"},"old":{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1"}}`, lid); data != want || recip.Valid {
		t.Fatalf("launch event = %s (recipient %v), want %s", data, recip, want)
	}

	// The prompt path follows the move.
	h.ok(nil, "prompt", id(w), "--text", "resume")
	if calls := h.calls("agent|prompt|"); len(calls) != 1 || !strings.HasPrefix(calls[0], "agent|prompt|w2:p7|") {
		t.Fatalf("prompt calls = %q", calls)
	}

	// Only the pane moves: workspace and tab stay.
	lid2 := num(h.ok(nil, "launch", id(w), "--provider", "claude", "--model", "m", "--effort", "e", "--pane", "w2:p8"), "launch_id")
	if ws, tab, pane := where(`select workspace_id, tab_id, pane_id from launches where id = ?`, lid2); ws != "w2" || tab != "w2:t4" || pane != "w2:p8" {
		t.Fatalf("pane-only move = %s %s %s", ws, tab, pane)
	}
	// No flags: everything stays, and no launch event is written.
	lid3 := num(h.ok(nil, "launch", id(w), "--provider", "claude", "--model", "m", "--effort", "e"), "launch_id")
	if ws, tab, pane := where(`select workspace_id, tab_id, pane_id from tasks where id = ?`, w); ws != "w2" || tab != "w2:t4" || pane != "w2:p8" {
		t.Fatalf("task after a plain relaunch = %s %s %s", ws, tab, pane)
	}
	if ws, tab, pane := where(`select workspace_id, tab_id, pane_id from launches where id = ?`, lid3); ws != "w2" || tab != "w2:t4" || pane != "w2:p8" {
		t.Fatalf("plain relaunch location = %s %s %s", ws, tab, pane)
	}
	var n int
	db.QueryRow(`select count(*) from events where kind = 'launch'`).Scan(&n)
	if n != 2 {
		t.Fatalf("launch events = %d, want 2", n)
	}
}

func TestLaunchRejectsRootTask(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0, "--workspace", "w1", "--tab", "w1:t1", "--pane", "w1:p1")
	db := h.openDB()
	var launch, attempt int64
	if err := withTx(db, func(tx *sql.Tx) error {
		res, err := tx.Exec(`insert into launches (task_id, provider, model, effort, workspace_id, tab_id, pane_id, recorded_at)
			values (?, 'claude', 'old', 'high', 'w1', 'w1:t1', 'w1:p1', ?)`, root, now())
		if err != nil {
			return err
		}
		launch, err = res.LastInsertId()
		if err != nil {
			return err
		}
		if _, err = tx.Exec(`update tasks set current_launch_id = ?, agent_name = 'legacy', updated_at = ? where id = ?`, launch, now(), root); err != nil {
			return err
		}
		attempt, err = insertEvent(tx, event{TaskID: root, LaunchID: &launch, Kind: "prompt", Summary: "pending"})
		if err != nil {
			return err
		}
		value, err := json.Marshal(receiptDue{DueAt: stamp(time.Now().Add(time.Hour)), Recipient: root, Task: root, Launch: &launch})
		if err != nil {
			return err
		}
		_, err = tx.Exec(`insert into meta (key, value) values (?, ?)`, receiptDueKey(attempt), string(value))
		return err
	}); err != nil {
		t.Fatal(err)
	}
	taskRow := func() []any {
		rows, err := db.Query(`select * from tasks where id = ?`, root)
		if err != nil {
			t.Fatal(err)
		}
		defer rows.Close()
		columns, err := rows.Columns()
		if err != nil {
			t.Fatal(err)
		}
		values, scan := make([]any, len(columns)), make([]any, len(columns))
		for i := range values {
			scan[i] = &values[i]
		}
		if !rows.Next() {
			t.Fatalf("task %d is missing", root)
		}
		if err := rows.Scan(scan...); err != nil {
			t.Fatal(err)
		}
		return values
	}
	beforeTask := taskRow()
	beforeReceipt, found, err := getMeta(db, receiptDueKey(attempt))
	if err != nil || !found {
		t.Fatalf("receipt before launch = %q, %t, %v", beforeReceipt, found, err)
	}
	out := h.one(exitReject, nil, "launch", id(root), "--provider", "claude", "--model", "new", "--effort", "high",
		"--workspace", "w2", "--tab", "w2:t2", "--pane", "w2:p2")
	want := "task " + id(root) + " is a root task; launch records a lane, so give it a task created with --parent"
	if errOf(out) != want {
		t.Fatalf("launch root = %v, want error %q", out, want)
	}
	if afterTask := taskRow(); !reflect.DeepEqual(afterTask, beforeTask) {
		t.Fatalf("task row changed after rejected launch: before %v, after %v", beforeTask, afterTask)
	}
	if after, found, err := getMeta(db, receiptDueKey(attempt)); err != nil || !found || after != beforeReceipt {
		t.Fatalf("receipt changed after rejected launch: before %q, after %q, found %t, err %v", beforeReceipt, after, found, err)
	}

	closed := h.newTask("closed-root", "orchestrator", 0)
	h.ok(nil, "close", id(closed))
	if out := h.one(exitReject, nil, "launch", id(closed), "--provider", "claude", "--model", "m", "--effort", "e"); errOf(out) != "task "+id(closed)+" is closed" {
		t.Fatalf("launch closed root = %v", out)
	}
}

// No Herdr server behind the socket (a stale file with its listener gone, or
// no file): no taskr command runs the herdr CLI, which could start a server
// from the agent's shell. wait skips its poll quietly; prompt and
// answer --prompt fail and record no attempt.
func TestNoHerdrCallWithoutServer(t *testing.T) {
	dir, err := os.MkdirTemp("", "hs")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	stale := filepath.Join(dir, "stale.sock")
	ln, err := net.ListenUnix("unix", &net.UnixAddr{Name: stale, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	ln.SetUnlinkOnClose(false)
	ln.Close()
	if !exists(stale) {
		t.Fatal("the stale socket file is gone")
	}
	for name, sock := range map[string]string{"stale": stale, "absent": filepath.Join(dir, "absent.sock")} {
		t.Run(name, func(t *testing.T) {
			h := newHarness(t)
			env := map[string]string{"HERDR_SOCKET_PATH": sock}
			top := h.newTask("top", "orchestrator", 0)
			w := h.newTask("impl", "implementer", top, "--pane", "w9:p1")
			l := h.launch(w)
			h.setAgents("w9:p1/idle/3")
			h.write("read.w9:p1", "You've hit your limit", 0o644)

			h.one(exitTimeout, env, "wait", "--as", id(top), "--timeout", "300", "--scan-quota")
			ev := h.ok(as(w, l), "ready", "slice 1")
			if got := h.ok(env, "wait", "--as", id(top), "--timeout", "300"); num(eventOf(got), "id") != num(ev, "event_id") {
				t.Fatalf("the inbox without a server = %v", got)
			}
			out := h.one(exitHerdr, env, "prompt", id(w), "--text", "go")
			if !strings.Contains(errOf(out), "Herdr server not reachable") || !strings.Contains(errOf(out), "not delivering") {
				t.Fatalf("prompt without a server = %v", out)
			}
			ask := num(h.ok(as(w, l), "ask", "which base?"), "ask_id")
			if out := h.one(exitHerdr, env, "answer", id(ask), "main", "--prompt"); !strings.Contains(errOf(out), "not delivering") {
				t.Fatalf("answer --prompt without a server = %v", out)
			}
			if out := h.one(exitHerdr, env, "daemon", "--once"); out["once"] != true {
				t.Fatalf("daemon --once without a server = %v", out)
			}
			var n int
			h.openDB().QueryRow(`select count(*) from events where kind in ('prompt', 'prompt_outcome', 'herdr')`).Scan(&n)
			if calls := h.calls(""); len(calls) > 1 || n != 0 {
				t.Fatalf("herdr calls %q, prompt/herdr events %d; want none", calls, n)
			}

			// The same commands with a server run herdr as before.
			h.ok(nil, "prompt", id(w), "--text", "go")
			if n := len(h.calls("agent|prompt|")); n != 1 {
				t.Fatalf("prompt with a server: %d herdr calls", n)
			}
		})
	}
}

func TestParseAfterDoubleDash(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("worker", "implementer", top)
	l := h.launch(w)
	h.ok(as(w, l), "note", "--", "-x")
	var got string
	if err := h.openDB().QueryRow(`select summary from events where task_id = ? and kind = 'note'`, w).Scan(&got); err != nil {
		t.Fatal(err)
	}
	if got != "-x" {
		t.Fatalf("note summary = %q, want -x", got)
	}
}

func TestCommandHelpAndUnknownSuggestions(t *testing.T) {
	h := newHarness(t)
	for _, args := range [][]string{{"help", "wait"}, {"wait", "--help"}, {"wait", "-h"}, {"--json", "wait", "--help"}, {"help", "version"}, {"version", "--help"}, {"version", "-h"}} {
		var stdout, stderr bytes.Buffer
		if code := run(args, h.getenv(nil), &stdout, &stderr); code != exitOK {
			t.Fatalf("taskr %v: exit %d, want 0; stdout=%q stderr=%q", args, code, stdout.String(), stderr.String())
		}
		want := "wait [--as ID]"
		if args[0] == "version" || (args[0] == "help" && args[1] == "version") {
			want = "info:         version | help [CMD]"
		}
		if !strings.Contains(stdout.String(), want) || (want == "wait [--as ID]" && !strings.Contains(stdout.String(), "--timeout")) || stderr.Len() != 0 {
			t.Fatalf("taskr %v: stdout=%q stderr=%q", args, stdout.String(), stderr.String())
		}
	}
	for name := range commands {
		var stdout, stderr bytes.Buffer
		if code := run([]string{name, "--help"}, h.getenv(nil), &stdout, &stderr); code != exitOK {
			t.Fatalf("taskr %s --help: exit %d, want 0; stdout=%q stderr=%q", name, code, stdout.String(), stderr.String())
		}
		line := strings.SplitN(stdout.String(), "\n", 2)[0]
		usage := line
		if _, rest, ok := strings.Cut(line, ":"); ok {
			usage = rest
		}
		found := false
		for _, segment := range strings.Split(usage, "|") {
			fields := strings.Fields(segment)
			if len(fields) > 0 && fields[0] == name {
				found = true
				break
			}
		}
		if !found {
			t.Fatalf("taskr %s --help: usage line does not start a segment with %q: %q", name, name, line)
		}
		if name == "launch" && !strings.Contains(line, "launch ID --provider") {
			t.Fatalf("taskr launch --help: usage line=%q", line)
		}
	}
	var stdout, stderr bytes.Buffer
	if code := run([]string{"help"}, h.getenv(nil), &stdout, &stderr); code != exitOK || !strings.Contains(stdout.String(), "help [CMD]") || stderr.Len() != 0 {
		t.Fatalf("taskr help: exit=%d stdout=%q stderr=%q", code, stdout.String(), stderr.String())
	}
	if got := h.one(exitUsage, nil, "stauts")["try"]; got != "status" {
		t.Fatalf("stauts suggestion = %v, want status", got)
	}
	if got := h.one(exitUsage, nil, "show", "1")["try"]; got != "status --tree ID / log ID" {
		t.Fatalf("show suggestion = %v", got)
	}
	if got := h.one(exitUsage, nil, "inbox")["try"]; got != "asks --open or wait --as ID --timeout 0 (consuming)" {
		t.Fatalf("inbox suggestion = %v", got)
	}
	if got := h.one(exitUsage, nil, "help", "unknwon")["try"]; got != "taskr help" {
		t.Fatalf("unknown help suggestion = %v", got)
	}
}

func TestStrictInputContracts(t *testing.T) {
	h := newHarness(t)
	if out := h.one(exitUsage, nil, "new", "a b", "--role", "implementer"); !strings.Contains(errOf(out), "[a-z][a-z0-9_-]{0,31}") {
		t.Fatalf("invalid name = %v", out)
	}
	if out := h.one(exitUsage, nil, "new", "x", "--tab", "--pane", "w1:p1"); errOf(out) != "--tab needs a value, got flag --pane" {
		t.Fatalf("missing flag value = %v", out)
	}
	if out := h.one(exitUsage, nil, "new", "x", "--parent", "--planned"); errOf(out) != "--parent needs a value, got flag --planned" {
		t.Fatalf("typed missing flag value = %v", out)
	}
	if out := h.one(exitUsage, nil, "new", "x", "--role", "implementer", "--tab", "null"); !strings.Contains(errOf(out), "[0-9A-Za-z:_-]+") {
		t.Fatalf("invalid tab = %v", out)
	}
	if out := h.one(exitUsage, nil, "new", "x", "--role", "implementer", "--cwd", "/nonexistent"); !strings.Contains(errOf(out), "existing directory") {
		t.Fatalf("invalid cwd = %v", out)
	}
	root := h.newTask("root", "orchestrator", 0)
	h.ok(nil, "new", "future", "--role", "implementer", "--parent", id(root), "--planned", "--cwd", filepath.Join(h.dir, "not-created"))
	h.ok(nil, "new", "gate", "--role", "gate", "--cwd", "/remote/path")
	if out := h.one(exitUsage, nil, "new", "x", "--role", "implementer", "--workspace", "w1", "--pane", "w2:p1"); !strings.Contains(errOf(out), "--workspace prefix") {
		t.Fatalf("mismatched workspace prefix = %v", out)
	}

	worker := h.newTask("worker", "implementer", root, "--workspace", "w1", "--pane", "w1:p1")
	h.launch(worker)
	h.launch(worker, "--workspace", "w2", "--pane", "w2:p1")
	if out := h.one(exitUsage, nil, "launch", id(worker), "--provider", "claude", "--model", "m", "--effort", "e", "--agent", "Bad"); !strings.Contains(errOf(out), "[a-z][a-z0-9_-]{0,31}") {
		t.Fatalf("invalid agent = %v", out)
	}

	brief := filepath.Join(h.dir, "brief.md")
	if err := os.WriteFile(brief, []byte("brief"), 0o644); err != nil {
		t.Fatal(err)
	}
	paths := h.ok(nil, "new", "paths", "--role", "implementer", "--cwd", h.dir, "--brief", "brief.md", "--report", "missing/report.md")
	var cwd, briefPath, reportPath string
	if err := h.openDB().QueryRow(`select cwd, brief_path, report_path from tasks where id = ?`, num(paths, "task_id")).Scan(&cwd, &briefPath, &reportPath); err != nil {
		t.Fatal(err)
	}
	if cwd != h.dir || briefPath != brief || reportPath != filepath.Join(h.dir, "missing/report.md") {
		t.Fatalf("stored paths = %q %q %q", cwd, briefPath, reportPath)
	}
}

func TestAnswerRecipientAndHandoverOutputContracts(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	worker := h.newTask("worker", "implementer", root)
	l := h.launch(worker)
	ask := num(h.ok(as(worker, l), "ask", "question"), "ask_id")
	out := h.one(exitReject, nil, "answer", id(ask), "answer", "--as", id(worker))
	if out["kind"] != "rejected" {
		t.Fatalf("wrong recipient answer = %v", out)
	}
	var answered sql.NullInt64
	if err := h.openDB().QueryRow(`select answered_by from events where id = ?`, ask).Scan(&answered); err != nil {
		t.Fatal(err)
	}
	if answered.Valid {
		t.Fatalf("mismatched answer wrote event %d", answered.Int64)
	}
	h.ok(nil, "answer", id(ask), "answer", "--as", id(root))

	parent := h.newTask("parent", "sub-orchestrator", root)
	grandchild := h.newTask("grandchild", "implementer", parent)
	grandchildLaunch := h.launch(grandchild)
	ownerAsk := h.ok(as(grandchild, grandchildLaunch), "ask", "owner question", "--owner")
	h.one(exitReject, nil, "answer", id(num(ownerAsk, "ask_id")), "answer", "--as", id(parent))
	h.ok(nil, "answer", id(num(ownerAsk, "ask_id")), "answer", "--as", id(root))

	rootAsk := h.ok(as(root, 0), "ask", "root owner question", "--owner")
	h.ok(nil, "answer", id(num(rootAsk, "ask_id")), "answer", "--as", id(root))
	h.one(exitUsage, nil, "handover", "--as", id(root), "--out", "--note")
	h.one(exitUsage, nil, "handover", "--as", id(root), "--out=-dash")
}
