package main

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

// goRun runs taskr in a goroutine; it never calls t.Fatal off the test goroutine.
type goResult struct {
	code int
	out  map[string]any
}

func (h *harness) goRun(env map[string]string, args ...string) <-chan goResult {
	ch := make(chan goResult, 1)
	args = append([]string{"--json"}, args...)
	var out, errb bytes.Buffer
	if os.Getenv("TASKR_BIN") != "" {
		// Start in the test goroutine: NI is delivered as a result, never t.Skip off-thread.
		cmd := contractCommand(args, h.getenv(env), &out, &errb)
		if err := cmd.Start(); err != nil {
			ch <- goResult{code: 127}
			return ch
		}
		h.t.Cleanup(func() { _ = cmd.Process.Kill() })
		root := strings.SplitN(h.t.Name(), "/", 2)[0]
		go func() {
			code := 0
			if err := cmd.Wait(); err != nil {
				if exit, ok := err.(*exec.ExitError); ok {
					code = exit.ExitCode()
				} else {
					code = 127
				}
			}
			if code == contractNotImplemented {
				contractMissing.Store(root, true)
			}
			ch <- goResult{code, lastJSON(out.String())}
		}()
	} else {
		go func() {
			code := run(args, h.getenv(env), &out, &errb)
			ch <- goResult{code, lastJSON(out.String())}
		}()
	}
	return ch
}

func (h *harness) waitFor(what string, cond func(db *sql.DB) bool) {
	h.t.Helper()
	db := h.openDB()
	for end := time.Now().Add(20 * time.Second); time.Now().Before(end); time.Sleep(10 * time.Millisecond) {
		if _, missing := contractMissing.Load(strings.SplitN(h.t.Name(), "/", 2)[0]); missing {
			h.t.Skip("adapter: asynchronous command not implemented")
		}
		if cond(db) {
			return
		}
	}
	h.t.Fatalf("timed out waiting for %s", what)
}

func (h *harness) waitingUntil(task int64) sql.NullString {
	h.t.Helper()
	var v sql.NullString
	if err := h.openDB().QueryRow(`select waiting_until from tasks where id = ?`, task).Scan(&v); err != nil {
		h.t.Fatal(err)
	}
	return v
}

// eventByID loads one event with JSON-decoded numbers, as the CLI prints it.
func eventByID(h *harness, eid int64) map[string]any {
	h.t.Helper()
	ev, err := loadEvent(h.openDB(), eid)
	if err != nil {
		h.t.Fatal(err)
	}
	b, _ := json.Marshal(ev)
	var m map[string]any
	json.Unmarshal(b, &m)
	return m
}

func statusOf(h *harness, task int64) map[string]any {
	h.t.Helper()
	_, st := h.run(nil, "status", "--tree", id(task))
	return st[0]
}

func TestReceiptIdempotentAndWrongLaunchRejected(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p4")
	other := h.newTask("impl-b", "implementer", top, "--pane", "w9:p5")
	l1 := h.launch(w)
	h.launch(other)
	a1 := num(h.ok(nil, "prompt", id(w), "--text", "Go."), "attempt_id")

	// got without a prior start records the identity the way start does.
	env := as(w, l1)
	env["CLAUDE_CONFIG_DIR"] = "/Users/user/.local/share/agent/claude/account-b/native"
	first := h.ok(env, "got", id(a1))
	if first["ok"] != true || num(first, "attempt_id") != a1 || num(first, "round") != 1 || first["duplicate"] != nil {
		t.Fatalf("got = %v", first)
	}
	again := h.ok(env, "got", id(a1))
	if num(again, "event_id") != num(first, "event_id") || again["duplicate"] != true || num(again, "round") != 1 {
		t.Fatalf("repeated got = %v, first %v", again, first)
	}
	var home, acct string
	h.openDB().QueryRow(`select native_home, account from launches where id = ?`, l1).Scan(&home, &acct)
	if home != env["CLAUDE_CONFIG_DIR"] || acct != "account-b" {
		t.Fatalf("got did not merge identity: home %q account %q", home, acct)
	}
	ev := eventByID(h, num(first, "event_id"))
	if ev["kind"] != "got" || num(ev, "recipient_task_id") != top || num(ev, "related_event_id") != a1 ||
		ev["event_key"] != "got:"+id(a1) || ev["data"].(map[string]any)["identity"] == nil {
		t.Fatalf("got event = %v", ev)
	}
	var n int
	h.openDB().QueryRow(`select count(*) from events where kind = 'got'`).Scan(&n)
	if n != 1 {
		t.Fatalf("got events = %d, want 1", n)
	}

	// Callers cannot claim the receipt key namespace.
	a1b := num(h.ok(nil, "prompt", id(w), "--text", "Again."), "attempt_id")
	h.one(exitUsage, env, "note", "ordinary note", "--key", "got:"+id(a1b))
	h.one(exitUsage, env, "ask", "q", "--key", "got:x")
	// A row an older binary wrote under a receipt key is a collision, not a duplicate.
	if _, err := h.openDB().Exec(`insert into events (task_id, launch_id, kind, summary, event_key, created_at)
		values (?, ?, 'note', 'legacy note', ?, 'x')`, w, l1, "got:"+id(a1b)); err != nil {
		t.Fatal(err)
	}
	if col := h.one(exitReject, env, "got", id(a1b)); !strings.Contains(col["error"].(string), "key collision") {
		t.Fatalf("colliding got = %v", col)
	}
	h.openDB().QueryRow(`select count(*) from events where kind = 'got'`).Scan(&n)
	if n != 1 {
		t.Fatalf("got events after collision = %d, want 1", n)
	}

	// Not a prompt, another task's prompt, a replaced launch: all exit 6.
	h.one(exitReject, env, "got", id(num(first, "event_id")))
	h.one(exitReject, env, "got", "9999")
	ob := num(h.ok(nil, "prompt", id(other), "--text", "Go."), "attempt_id")
	h.one(exitReject, env, "got", id(ob))
	l2 := h.launch(w)
	env2 := as(w, l2)
	env2["CLAUDE_CONFIG_DIR"] = env["CLAUDE_CONFIG_DIR"]
	h.ok(env2, "start")
	a2 := num(h.ok(nil, "prompt", id(w), "--text", "Round 2."), "attempt_id")
	h.one(exitReject, as(w, l2), "got", id(a1)) // attempt of the old launch
	h.one(exitReject, env, "got", id(a2))       // stale launch
	h.ok(as(w, l2), "got", id(a2))
	a3 := num(h.ok(nil, "prompt", id(w), "--text", "Round 3."), "attempt_id")
	g3 := h.ok(as(w, l2), "got", id(a3))
	if num(g3, "round") != 2 {
		t.Fatalf("second prompt on launch 2: round %v, want 2", g3["round"])
	}
	ev = eventByID(h, num(g3, "event_id"))
	if ev["data"].(map[string]any)["identity"] != nil {
		t.Fatalf("got after start must not merge identity again: %v", ev)
	}
	st := statusOf(h, w)
	if num(st, "round") != 2 || num(st, "last_receipt") != a3 || st["waiting"] != false {
		t.Fatalf("status = %v, want round 2, last_receipt %d", st, a3)
	}
}

func TestPromptConfirmReceipt(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p4")
	l := h.launch(w)
	// The fake worker acknowledges through the CLI only after the confirmation
	// loop has run one empty receipt query: a single immediate check would fail.
	polled := make(chan int64, 1) // buffered: a late hook never blocks after the watchdog fired
	var once sync.Once
	receiptPolled = func(attempt int64) { once.Do(func() { polled <- attempt }) }
	defer func() { receiptPolled = func(int64) {} }()
	worker := make(chan goResult, 1)
	go func() {
		select {
		case a := <-polled:
			worker <- <-h.goRun(as(w, l), "got", id(a))
		case <-time.After(20 * time.Second):
			worker <- goResult{code: -1}
		}
	}()
	out := h.ok(nil, "prompt", id(w), "--text", "Go.", "--confirm", "--confirm-timeout", "20000")
	g := <-worker
	if g.code != exitOK {
		t.Fatalf("worker got: exit %d %v", g.code, g.out)
	}
	if out["receipt"] != true || num(out, "receipt_event_id") != num(g.out, "event_id") || num(out, "round") != 1 ||
		out["outcome"] != "activity_observed" {
		t.Fatalf("prompt --confirm = %v, worker got %v", out, g.out)
	}
	if n := len(h.calls("agent|prompt|")); n != 1 {
		t.Fatalf("herdr prompt calls = %d, want 1", n)
	}
}

func TestPromptConfirmNoReceipt(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p4")
	l := h.launch(w)
	began := time.Now()
	out := h.one(exitHerdr, nil, "prompt", id(w), "--text", "Go.", "--confirm", "--confirm-timeout", "300")
	if el := time.Since(began); el < 300*time.Millisecond {
		t.Fatalf("no_receipt after %v, want at least the 300 ms confirm timeout", el)
	}
	attempt := num(out, "attempt_id")
	if out["receipt"] != false || out["outcome"] != "no_receipt" || out["kind"] != "no_receipt" || out["ok"] != false {
		t.Fatalf("prompt --confirm timeout = %v", out)
	}
	time.Sleep(300 * time.Millisecond)
	if n := len(h.calls("agent|prompt|")); n != 1 {
		t.Fatalf("no_receipt resent: herdr prompt calls = %d, want 1", n)
	}
	_, log := h.run(nil, "log", id(w))
	var outcomes []string
	for _, m := range log {
		if m["kind"] == "prompt_outcome" {
			if num(m, "related_event_id") != attempt {
				t.Fatalf("outcome not linked to attempt %d: %v", attempt, m)
			}
			outcomes = append(outcomes, m["summary"].(string))
		}
	}
	if strings.Join(outcomes, ",") != "activity_observed,no_receipt" ||
		num(out, "outcome_event_id") == num(out, "delivery_outcome_event_id") {
		t.Fatalf("outcomes = %v, output %v", outcomes, out)
	}
	h.ok(as(w, l), "got", id(attempt)) // a late receipt is still recorded

	// answer --prompt --confirm: the answer is committed; the receipt times out.
	ask := num(h.ok(as(w, l), "ask", "Which base?", "--blocking"), "ask_id")
	began = time.Now()
	ans := h.one(exitHerdr, nil, "answer", id(ask), "Main.", "--prompt", "--confirm", "--confirm-timeout", "200")
	if el := time.Since(began); el < 200*time.Millisecond {
		t.Fatalf("answer no_receipt after %v, want at least 200 ms", el)
	}
	if ans["outcome"] != "no_receipt" || ans["delivered"] != true || ans["asker_waiting"] != false || num(ans, "answer_id") == 0 {
		t.Fatalf("answer --prompt --confirm = %v", ans)
	}
	calls := h.calls("agent|prompt|")
	a := id(num(ans, "attempt_id"))
	if len(calls) != 2 || !strings.HasPrefix(calls[1],
		"agent|prompt|w9:p4|First taskr got "+a+". ask "+id(ask)+": Main.|--wait|") {
		t.Fatalf("answer prompt calls = %q", calls)
	}
	h.one(exitUsage, nil, "answer", id(ask), "x", "--confirm")

	// rejected skips the receipt wait.
	h.write("prompt.stderr", `{"error":{"code":"agent_blocked"}}`, 0o644)
	h.write("prompt.exit", "1", 0o644)
	began = time.Now()
	rej := h.one(exitHerdr, nil, "prompt", id(w), "--text", "Go.", "--confirm", "--confirm-timeout", "60000")
	if rej["outcome"] != "rejected" || rej["receipt"] != nil || time.Since(began) > 20*time.Second {
		t.Fatalf("rejected with --confirm = %v after %v", rej, time.Since(began))
	}
}

func TestBlockingAskRoundTrip(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p4")
	l := h.launch(w)
	h.setAgents("w9:p4/working/5")
	ask := num(h.ok(as(w, l), "ask", "Which base branch?", "--blocking"), "ask_id")

	waited := h.goRun(as(w, l), "wait", "--as", id(w), "--timeout", "60000")
	h.waitFor("worker wait to block", func(db *sql.DB) bool {
		var v sql.NullString
		db.QueryRow(`select waiting_until from tasks where id = ?`, w).Scan(&v)
		return v.Valid
	})
	if st := statusOf(h, w); st["waiting"] != true || st["waiting_until"] == nil {
		t.Fatalf("status of a waiting worker = %v", st)
	}
	ans := h.ok(nil, "answer", id(ask), "From main.")
	if ans["asker_waiting"] != true || ans["delivered"] != false {
		t.Fatalf("answer = %v, want asker_waiting true", ans)
	}
	var got goResult
	select {
	case got = <-waited:
	case <-time.After(30 * time.Second):
		t.Fatal("worker wait did not wake on the answer")
	}
	ev := eventOf(got.out)
	if got.code != exitOK || ev["kind"] != "answer" || num(ev, "related_event_id") != ask || num(ev, "id") != num(ans, "answer_id") {
		t.Fatalf("worker wait = %d %v", got.code, got.out)
	}
	h.ok(as(w, l), "ack", id(num(ev, "id")), "--as", id(w))
	if v := h.waitingUntil(w); v.Valid {
		t.Fatalf("waiting_until not cleared: %v", v.String)
	}
	if st := statusOf(h, w); st["waiting"] != false || num(st, "acked_event_id") != num(ev, "id") {
		t.Fatalf("status after ack = %v", st)
	}
	ask2 := num(h.ok(as(w, l), "ask", "And the tag?", "--blocking"), "ask_id")
	if a := h.ok(nil, "answer", id(ask2), "v1."); a["asker_waiting"] != false {
		t.Fatalf("answer to a worker not waiting = %v", a)
	}
	if calls := h.calls("agent|"); len(calls) != 0 {
		t.Fatalf("round trip ran herdr: %q", calls)
	}
}

func TestWorkerWaitMakesNoHerdrCalls(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p4")
	l := h.launch(w)
	h.setAgents("w9:p4/idle/1")
	h.write("read.w9:p4", "You've hit your weekly limit.", 0o644)
	livenessInterval = 0
	defer func() { livenessInterval = 15 * time.Second }()
	if out := h.one(exitTimeout, as(w, l), "wait", "--as", id(w), "--timeout", "300", "--scan-quota"); out["timeout"] != true {
		t.Fatalf("worker wait = %v", out)
	}
	h.one(exitReject, as(w, l), "ack", "1", "--as", id(w))
	if calls := h.calls("agent|"); len(calls) != 0 {
		t.Fatalf("worker wait ran herdr: %q", calls)
	}
	if v := h.waitingUntil(w); v.Valid {
		t.Fatalf("waiting_until not cleared after timeout: %v", v.String)
	}
}

func TestLivenessSkipsWaitingTask(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	a := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	b := h.newTask("impl-b", "implementer", top, "--pane", "w9:p2")
	la := h.launch(a)
	h.launch(b)
	h.ok(nil, "prompt", id(a), "--text", "go", "--receipt-timeout", "0")
	h.ok(nil, "prompt", id(b), "--text", "go", "--receipt-timeout", "0")
	h.setAgents("w9:p1/idle/3", "w9:p2/idle/4")
	// a's ask gives the test a way to end a's wait explicitly later.
	ask := num(h.ok(as(a, la), "ask", "Which base?", "--blocking"), "ask_id")
	h.ok(nil, "ack", id(num(eventOf(h.ok(nil, "wait", "--as", id(top), "--timeout", "0")), "id")), "--as", id(top))

	// a's wait outlives every parent snapshot below; it ends only on the answer.
	waited := h.goRun(as(a, la), "wait", "--as", id(a), "--timeout", "60000")
	h.waitFor("a's wait to block", func(db *sql.DB) bool {
		var v sql.NullString
		db.QueryRow(`select waiting_until from tasks where id = ?`, a).Scan(&v)
		return v.Valid
	})
	got := h.ok(nil, "wait", "--as", id(top), "--timeout", "10000")
	if ev := eventOf(got); num(ev, "task_id") != b || ev["kind"] != "herdr" {
		t.Fatalf("wait = %v, want b's herdr event only", got)
	}
	h.ok(nil, "ack", id(num(eventOf(got), "id")), "--as", id(top))
	var observed sql.NullString
	h.openDB().QueryRow(`select observed_at from launches where id = ?`, la).Scan(&observed)
	if observed.Valid || h.countHerdr(top) != 1 {
		t.Fatalf("waiting task was observed: observed_at %v, herdr events %d", observed, h.countHerdr(top))
	}

	// End a's wait explicitly; its marker is cleared and the next pass sees a.
	ans := num(h.ok(nil, "answer", id(ask), "Main."), "answer_id")
	var r goResult
	select {
	case r = <-waited:
	case <-time.After(30 * time.Second):
		t.Fatal("a's wait did not wake on the answer")
	}
	if r.code != exitOK || num(eventOf(r.out), "id") != ans {
		t.Fatalf("a's wait = %d %v", r.code, r.out)
	}
	if v := h.waitingUntil(a); v.Valid {
		t.Fatalf("a's waiting_until not cleared: %v", v.String)
	}
	// Each parent wait below gets exactly one liveness pass, at its start and
	// with its full budget: the throttle is reset rather than disabled.
	db := h.openDB()
	due := func() { db.Exec(`update tasks set last_poll_at = null where id = ?`, top) }
	due()
	got = h.ok(nil, "wait", "--as", id(top), "--timeout", "10000")
	if ev := eventOf(got); num(ev, "task_id") != a || ev["kind"] != "herdr" {
		t.Fatalf("after a's wait ended: %v, want a's herdr event", got)
	}
	h.ok(nil, "ack", id(num(eventOf(got), "id")), "--as", id(top))

	// Expiry, separately: a marker left by a killed wait is honoured until its
	// deadline and ignored after it.
	h.setAgents("w9:p1/idle/5", "w9:p2/idle/4")
	db.Exec(`update tasks set waiting_until = ? where id = ?`, stamp(time.Now().Add(time.Hour)), a)
	due()
	if out := h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "3000"); out["timeout"] != true {
		t.Fatalf("future marker: wait = %v, want a skipped", out)
	}
	if n := len(h.calls("agent|list|")); n != 3 {
		t.Fatalf("agent list calls = %d, want 3 (one per parent pass)", n)
	}
	db.Exec(`update tasks set waiting_until = ? where id = ?`, stamp(time.Now().Add(-time.Second)), a)
	due()
	got = h.ok(nil, "wait", "--as", id(top), "--timeout", "10000")
	if ev := eventOf(got); num(ev, "task_id") != a || ev["data"].(map[string]any)["state_change_seq"] != float64(5) {
		t.Fatalf("expired marker: wait = %v, want a's seq 5 event", got)
	}
}

func TestMigrationAddsWaitingUntil(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	os.MkdirAll(filepath.Dir(h.db), 0o755)
	oldSchema := strings.Replace(schemaSQL, "  waiting_until text,\n", "", 1)
	if oldSchema == schemaSQL {
		t.Fatal("schema.sql has no waiting_until line to strip")
	}
	old, err := sql.Open("sqlite", h.db)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := old.Exec(oldSchema); err != nil {
		t.Fatal(err)
	}
	if _, err := old.Exec(`insert into tasks (name, role, created_at, updated_at) values ('legacy', 'orchestrator', 'x', 'x')`); err != nil {
		t.Fatal(err)
	}
	old.Close()
	for i := 0; i < 2; i++ { // the second open finds the column and leaves it
		var n int
		h.openDB().QueryRow(`select count(*) from pragma_table_info('tasks') where name = 'waiting_until'`).Scan(&n)
		if n != 1 {
			t.Fatalf("open %d: waiting_until columns = %d, want 1", i, n)
		}
	}
	if st := statusOf(h, 1); st["name"] != "legacy" || st["waiting"] != false {
		t.Fatalf("legacy status = %v", st)
	}
}

// A failed clear of waiting_until is a database error, not a delivered event:
// the offered event stays pending and the next wait replays it.
func TestWaitCleanupFailureKeepsEventPending(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p4")
	l := h.launch(w)
	ask := num(h.ok(as(w, l), "ask", "Which base?", "--blocking"), "ask_id")
	db := h.openDB()
	if _, err := db.Exec(`create trigger reject_clear before update of waiting_until on tasks
		when new.waiting_until is null and old.waiting_until is not null
		begin select raise(abort, 'injected clear failure'); end`); err != nil {
		t.Fatal(err)
	}
	waited := h.goRun(as(w, l), "wait", "--as", id(w), "--timeout", "60000")
	h.waitFor("worker wait to block", func(db *sql.DB) bool {
		var v sql.NullString
		db.QueryRow(`select waiting_until from tasks where id = ?`, w).Scan(&v)
		return v.Valid
	})
	answer := num(h.ok(nil, "answer", id(ask), "Main."), "answer_id")
	var r goResult
	select {
	case r = <-waited:
	case <-time.After(30 * time.Second):
		t.Fatal("wait did not return")
	}
	if r.code != exitDB || r.out["kind"] != "database" || r.out["event"] != nil ||
		num(r.out, "pending_event_id") != answer || r.out["stale_waiting_until"] == nil ||
		!strings.Contains(r.out["error"].(string), "injected clear failure") {
		t.Fatalf("wait with failed cleanup = %d %v", r.code, r.out)
	}
	var pending sql.NullInt64
	var acked int64
	db.QueryRow(`select pending_event_id, acked_event_id from tasks where id = ?`, w).Scan(&pending, &acked)
	if pending.Int64 != answer || acked != 0 {
		t.Fatalf("pending %v acked %d, want pending %d and nothing acked", pending, acked, answer)
	}
	if _, err := db.Exec(`drop trigger reject_clear`); err != nil {
		t.Fatal(err)
	}
	got := h.ok(as(w, l), "wait", "--as", id(w), "--timeout", "0")
	if num(eventOf(got), "id") != answer || got["replay"] != true {
		t.Fatalf("replay wait = %v, want answer %d with replay", got, answer)
	}
}

// Asynchronous receipts (I1).

func countRows(h *harness, q string, args ...any) int {
	h.t.Helper()
	var n int
	if err := h.openDB().QueryRow(q, args...).Scan(&n); err != nil {
		h.t.Fatal(err)
	}
	return n
}

// receiptRow reads attempt's pending deadline.
func receiptRow(h *harness, attempt int64) (receiptDue, bool) {
	h.t.Helper()
	v, ok, err := getMeta(h.openDB(), receiptDueKey(attempt))
	if err != nil {
		h.t.Fatal(err)
	}
	var d receiptDue
	if ok {
		if err := json.Unmarshal([]byte(v), &d); err != nil {
			h.t.Fatal(err)
		}
	}
	return d, ok
}

// overdue lets attempt's whole window pass: the attempt and its deadline
// move back together, so the deadline is a second old.
func overdue(h *harness, attempt int64) {
	h.t.Helper()
	d, ok := receiptRow(h, attempt)
	if !ok {
		h.t.Fatalf("attempt %d has no receipt deadline", attempt)
	}
	shift := time.Until(parseTime(d.DueAt)) + time.Second
	db := h.openDB()
	created := parseTime(eventByID(h, attempt)["created_at"].(string))
	if _, err := db.Exec(`update events set created_at = ? where id = ?`, stamp(created.Add(-shift)), attempt); err != nil {
		h.t.Fatal(err)
	}
	if _, err := db.Exec(`update meta set value = json_set(value, '$.due_at', ?) where key = ?`,
		stamp(parseTime(d.DueAt).Add(-shift)), receiptDueKey(attempt)); err != nil {
		h.t.Fatal(err)
	}
}

// pollAlarm runs `wait --timeout 0 --for prompt_outcome` and returns the
// matched event, or nil on a timeout.
func pollAlarm(h *harness, as int64) map[string]any {
	h.t.Helper()
	_, lines := h.run(nil, "wait", "--as", id(as), "--timeout", "0", "--for", "prompt_outcome")
	for _, m := range lines {
		if ev := eventOf(m); ev != nil {
			return ev
		}
	}
	return nil
}

// deadDaemonPass runs one daemon pass with no Herdr server.
func deadDaemonPass(h *harness) {
	h.t.Helper()
	d := &daemon{db: h.openDB(), log: &daemonLog{}, sock: filepath.Join(h.t.TempDir(), "none.sock")}
	if r := d.pass(); r.err == nil {
		h.t.Fatal("a pass without a Herdr server reported no error")
	}
}

func alarms(h *harness, summary string, attempt int64) int {
	h.t.Helper()
	return countRows(h, `select count(*) from events where kind = 'prompt_outcome' and summary = ? and related_event_id = ?`, summary, attempt)
}

func receiptLane(h *harness) (top, w, l int64) {
	h.t.Helper()
	top = h.newTask("top", "orchestrator", 0)
	w = h.newTask("impl-a", "implementer", top, "--pane", "w9:p4")
	return top, w, h.launch(w)
}

// Acceptance: an async send followed by a got before the deadline produces no inbox event.
func TestAsyncReceiptOnTimeIsSilent(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top, w, l := receiptLane(h)
	out := h.ok(nil, "prompt", id(w), "--text", "Go.")
	a := num(out, "attempt_id")
	if out["outcome"] != "activity_observed" || num(out, "receipt_due_ms") != 300000 {
		t.Fatalf("async prompt = %v", out)
	}
	d, ok := receiptRow(h, a)
	created := parseTime(eventByID(h, a)["created_at"].(string))
	if !ok || d.Recipient != top || d.Task != w || d.Launch == nil || *d.Launch != l ||
		parseTime(d.DueAt).Sub(created) != 300*time.Second {
		t.Fatalf("receipt_due:%d = %+v (armed %t), attempt at %v", a, d, ok, created)
	}
	code, raw, _ := h.compact(nil, "prompt", id(w), "--text", "Again.")
	m := decodeFrame(t, raw)
	a2 := num(m, "p")
	if code != exitOK || !strings.HasPrefix(raw, "p1 ") || m["o"] != "activity_observed" || num(m, "due") != 300000 || num(m, "oe") == 0 {
		t.Fatalf("compact async prompt = %d %q", code, raw)
	}
	for _, at := range []int64{a, a2} {
		h.ok(as(w, l), "got", id(at))
		if _, ok := receiptRow(h, at); ok {
			t.Fatalf("got %d left its receipt deadline", at)
		}
	}
	deadDaemonPass(h)
	if ev := pollAlarm(h, top); ev != nil {
		t.Fatalf("on-time receipts raised %v", ev)
	}
	if n := countRows(h, `select count(*) from events where kind = 'prompt_outcome' and recipient_task_id is not null`); n != 0 {
		t.Fatalf("on-time receipts wrote %d inbox outcomes", n)
	}
}

// Acceptance: without a got, exactly one no_receipt appears across two waits and a daemon pass.
func TestAsyncNoReceiptExactlyOnce(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top, w, l := receiptLane(h)
	a := num(h.ok(nil, "prompt", id(w), "--text", "Go."), "attempt_id")
	overdue(h, a)
	deadDaemonPass(h) // before pass returns early for the missing Herdr server
	if _, ok := receiptRow(h, a); ok {
		t.Fatal("the daemon pass left an overdue deadline")
	}
	first := pollAlarm(h, top)
	second := pollAlarm(h, top)
	deadDaemonPass(h)
	if first == nil || second == nil || num(first, "id") != num(second, "id") || alarms(h, "no_receipt", a) != 1 {
		t.Fatalf("alarms: first %v second %v, %d rows", first, second, alarms(h, "no_receipt", a))
	}
	want := map[string]any{"outcome": "no_receipt", "window_ms": float64(300000), "async": true}
	if first["summary"] != "no_receipt" || first["event_key"] != "no_receipt:"+id(a) || num(first, "related_event_id") != a ||
		num(first, "recipient_task_id") != top || num(first, "task_id") != w || num(first, "launch_id") != l ||
		fmt.Sprint(first["data"]) != fmt.Sprint(want) {
		t.Fatalf("no_receipt = %v", first)
	}
	// A deadline row that reappears (a rollback to v0.9.1 and back) expires silently.
	h.openDB().Exec(`insert into meta (key, value) values (?, ?)`, receiptDueKey(a),
		jsonText(receiptDue{DueAt: stamp(time.Now().Add(-time.Second)), Recipient: top, Task: w, Launch: &l}))
	deadDaemonPass(h)
	if _, ok := receiptRow(h, a); ok || alarms(h, "no_receipt", a) != 1 {
		t.Fatalf("a repeated deadline: %d no_receipt rows", alarms(h, "no_receipt", a))
	}
	// The wait line is a po frame with d.outcome.
	_, raw, _ := h.compact(nil, "wait", "--as", id(top), "--timeout", "0", "--for", "prompt_outcome")
	if f := strings.Split(strings.TrimSpace(raw), "\t"); len(f) != 9 || f[4] != "po" || f[6] != id(a) || !strings.Contains(f[8], `"outcome":"no_receipt"`) {
		t.Fatalf("compact alarm = %q", raw)
	}
}

// Acceptance: a got after an async or confirm no_receipt produces one late_receipt.
func TestLateReceiptAfterNoReceipt(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top, w, l := receiptLane(h)
	a := num(h.ok(nil, "prompt", id(w), "--text", "Go."), "attempt_id")
	overdue(h, a)
	nr := pollAlarm(h, top)
	g := h.ok(as(w, l), "got", id(a))
	h.ok(as(w, l), "got", id(a)) // a repeat writes nothing
	check := func(attempt, noReceipt, gotID int64) {
		t.Helper()
		var eid int64
		h.openDB().QueryRow(`select id from events where event_key = ?`, "late_receipt:"+id(attempt)).Scan(&eid)
		ev := eventByID(h, eid)
		d, _ := ev["data"].(map[string]any)
		if alarms(h, "late_receipt", attempt) != 1 || ev["kind"] != "prompt_outcome" || ev["summary"] != "late_receipt" ||
			num(ev, "recipient_task_id") != top || num(ev, "related_event_id") != attempt || num(ev, "task_id") != w ||
			d["outcome"] != "late_receipt" || num(d, "got_event_id") != gotID || num(d, "no_receipt_event_id") != noReceipt ||
			d["delay_ms"] == nil || num(d, "delay_ms") != parseTime(eventByID(h, gotID)["created_at"].(string)).
			Sub(parseTime(eventByID(h, noReceipt)["created_at"].(string))).Milliseconds() {
			t.Fatalf("late_receipt for %d = %v (%d rows)", attempt, ev, alarms(h, "late_receipt", attempt))
		}
	}
	check(a, num(nr, "id"), num(g, "event_id"))

	// --confirm arms nothing; its synchronous no_receipt gets the same late_receipt.
	c := h.one(exitHerdr, nil, "prompt", id(w), "--text", "Go.", "--confirm", "--confirm-timeout", "300")
	ca := num(c, "attempt_id")
	if _, ok := receiptRow(h, ca); ok || c["receipt_due_ms"] != nil {
		t.Fatalf("--confirm armed a deadline: %v", c)
	}
	cg := h.ok(as(w, l), "got", id(ca))
	check(ca, num(c, "outcome_event_id"), num(cg, "event_id"))
}

// Acceptance: --timeout 0 materializes an overdue alarm.
func TestWaitTimeoutZeroMaterializesAlarm(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top, w, _ := receiptLane(h)
	a := num(h.ok(nil, "prompt", id(w), "--text", "Go."), "attempt_id")
	if ev := pollAlarm(h, top); ev != nil || alarms(h, "no_receipt", a) != 0 {
		t.Fatalf("an armed deadline alarmed early: %v", ev)
	}
	overdue(h, a)
	if ev := pollAlarm(h, top); ev == nil || ev["event_key"] != "no_receipt:"+id(a) {
		t.Fatalf("--timeout 0 = %v, want the no_receipt of %d", ev, a)
	}
}

// Acceptance: a relaunch or close inside the window gives no alarm.
func TestRelaunchOrCloseDropsReceipt(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top, w, l := receiptLane(h)
	a := num(h.ok(nil, "prompt", id(w), "--text", "Go."), "attempt_id")
	row, _ := receiptRow(h, a)
	h.launch(w)
	if _, ok := receiptRow(h, a); ok {
		t.Fatal("a replacement launch left the old launch's deadline")
	}
	// A row the launch transaction never saw (rollback) is dropped at expiry.
	row.DueAt = stamp(time.Now().Add(-time.Second))
	h.openDB().Exec(`insert into meta (key, value) values (?, ?)`, receiptDueKey(a), jsonText(row))
	deadDaemonPass(h)
	if _, ok := receiptRow(h, a); ok || pollAlarm(h, top) != nil || alarms(h, "no_receipt", a) != 0 {
		t.Fatalf("a deadline of replaced launch %d alarmed", l)
	}

	b := num(h.ok(nil, "prompt", id(w), "--text", "Go."), "attempt_id")
	row, _ = receiptRow(h, b)
	h.ok(nil, "close", id(w))
	if _, ok := receiptRow(h, b); ok {
		t.Fatal("close left the task's deadline")
	}
	row.DueAt = stamp(time.Now().Add(-time.Second))
	h.openDB().Exec(`insert into meta (key, value) values (?, ?)`, receiptDueKey(b), jsonText(row))
	if pollAlarm(h, top) != nil || alarms(h, "no_receipt", b) != 0 {
		t.Fatal("a closed task's deadline alarmed")
	}
	if _, ok := receiptRow(h, b); ok {
		t.Fatal("expiry kept a closed task's deadline")
	}
}

// Acceptance: a root target arms no deadline.
func TestRootTargetArmsNoReceipt(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("top", "orchestrator", 0, "--pane", "w9:p1")
	out := h.ok(nil, "prompt", id(root), "--text", "Go.")
	if out["outcome"] != "activity_observed" || out["receipt_due_ms"] != nil ||
		countRows(h, `select count(*) from meta where key like 'receipt_due:%'`) != 0 {
		t.Fatalf("root prompt = %v", out)
	}
}

// Acceptance: note --key X:1 exits 2 for each reserved prefix.
func TestReservedEventKeyPrefixes(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top, w, l := receiptLane(h)
	for _, p := range []string{"got:", "no_receipt:", "late_receipt:", "quota:", "capacity:"} {
		h.one(exitUsage, as(w, l), "note", "x", "--key", p+"1")
		h.one(exitUsage, nil, "note", "x", "--key", p+"1", "--as", id(top))
		h.one(exitUsage, as(w, l), "ready", "x", "--key", p+"1")
	}
	if n := countRows(h, `select count(*) from events where kind in ('note', 'ready')`); n != 0 {
		t.Fatalf("reserved keys wrote %d events", n)
	}
}

// Acceptance: --receipt-timeout 1000 exits 2 (D15: --confirm-timeout keeps short values).
func TestReceiptTimeoutFlag(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	_, w, _ := receiptLane(h)
	for _, args := range [][]string{
		{"--receipt-timeout", "1000"}, {"--receipt-timeout", "59999"}, {"--receipt-timeout", "-1"},
		{"--receipt-timeout", "60000", "--confirm"}, {"--receipt-timeout", "0", "--confirm"},
	} {
		h.one(exitUsage, nil, append([]string{"prompt", id(w), "--text", "Go."}, args...)...)
	}
	if n := len(h.calls("agent|prompt|")); n != 0 || countRows(h, `select count(*) from events where kind = 'prompt'`) != 0 {
		t.Fatalf("a usage error sent %d prompts", n)
	}
	off := h.ok(nil, "prompt", id(w), "--text", "Go.", "--receipt-timeout", "0")
	if _, ok := receiptRow(h, num(off, "attempt_id")); ok || off["receipt_due_ms"] != nil {
		t.Fatalf("--receipt-timeout 0 armed: %v", off)
	}
	floor := h.ok(nil, "prompt", id(w), "--text", "Go.", "--receipt-timeout", "60000")
	if _, ok := receiptRow(h, num(floor, "attempt_id")); !ok || num(floor, "receipt_due_ms") != 60000 {
		t.Fatalf("--receipt-timeout 60000 = %v", floor)
	}
	h.one(exitHerdr, nil, "prompt", id(w), "--text", "Go.", "--confirm", "--confirm-timeout", "1")
	// rejected deletes the deadline; delivery_unknown keeps it.
	h.write("prompt.stderr", `{"error":{"code":"agent_blocked"}}`, 0o644)
	h.write("prompt.exit", "1", 0o644)
	rej := h.one(exitHerdr, nil, "prompt", id(w), "--text", "Go.")
	if _, ok := receiptRow(h, num(rej, "attempt_id")); ok || rej["outcome"] != "rejected" {
		t.Fatalf("rejected kept its deadline: %v", rej)
	}
	h.write("prompt.stderr", "Transport failed\n", 0o644)
	unk := h.one(exitHerdr, nil, "prompt", id(w), "--text", "Go.")
	if _, ok := receiptRow(h, num(unk, "attempt_id")); !ok || unk["outcome"] != "delivery_unknown" {
		t.Fatalf("delivery_unknown dropped its deadline: %v", unk)
	}
}

// answer --prompt arms the default window; an answer --as routes the alarm
// to the answering root.
func TestAnswerPromptReceiptRouting(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("top", "orchestrator", 0)
	sub := h.newTask("sub", "sub-orchestrator", root)
	h.launch(sub)
	w := h.newTask("impl-a", "implementer", sub, "--pane", "w9:p4")
	l := h.launch(w)
	plain := num(h.ok(as(w, l), "ask", "Which base?"), "ask_id")
	out := h.ok(nil, "answer", id(plain), "Main.", "--prompt")
	if d, ok := receiptRow(h, num(out, "attempt_id")); !ok || d.Recipient != sub || num(out, "receipt_due_ms") != 300000 {
		t.Fatalf("answer --prompt deadline = %+v (%t), out %v", d, ok, out)
	}
	owner := num(h.ok(as(w, l), "ask", "Ship it?", "--owner"), "ask_id")
	out = h.ok(nil, "answer", id(owner), "Yes.", "--prompt", "--as", id(root))
	if d, ok := receiptRow(h, num(out, "attempt_id")); !ok || d.Recipient != root {
		t.Fatalf("answer --as --prompt deadline = %+v (%t)", d, ok)
	}
	overdue(h, num(out, "attempt_id"))
	if ev := pollAlarm(h, root); ev == nil || num(ev, "related_event_id") != num(out, "attempt_id") {
		t.Fatalf("root alarm = %v", ev)
	}
}

// Client mode: `_prompt begin` arms the deadline on the server ledger.
func TestRelayAsyncReceipt(t *testing.T) {
	contractGuard(t)
	r := newRelay(t)
	_, _, hostATop, mw, ml := r.lanes()
	m := r.want(0, "host-a", r.hostA, "prompt", id(mw), "--text", "go")
	a := num(m, "attempt_id")
	d, ok := receiptRow(r.harness, a)
	if !ok || d.Recipient != hostATop || d.Task != mw || d.Launch == nil || *d.Launch != ml || num(m, "receipt_due_ms") != 300000 {
		t.Fatalf("host-a async prompt = %v, deadline %+v (%t)", m, d, ok)
	}
	r.want(0, "host-a", as(mw, ml), "got", id(a))
	if _, ok := receiptRow(r.harness, a); ok {
		t.Fatal("a host-a got left the deadline")
	}
	m = r.want(0, "host-a", r.hostA, "prompt", id(mw), "--text", "go", "--receipt-timeout", "60000")
	b := num(m, "attempt_id")
	if num(m, "receipt_due_ms") != 60000 {
		t.Fatalf("host-a --receipt-timeout 60000 = %v", m)
	}
	m = r.want(exitHerdr, "host-a", r.hostA, "prompt", id(mw), "--text", "go", "--confirm", "--confirm-timeout", "300")
	if _, ok := receiptRow(r.harness, num(m, "attempt_id")); ok || m["outcome"] != "no_receipt" {
		t.Fatalf("host-a --confirm armed a deadline: %v", m)
	}
	attempts := r.count(`select count(*) from events where kind = 'prompt'`)
	r.want(exitUsage, "host-a", r.hostA, "prompt", id(mw), "--text", "go", "--receipt-timeout", "1000")
	if r.count(`select count(*) from events where kind = 'prompt'`) != attempts {
		t.Fatal("a host-a usage error recorded an attempt")
	}
	overdue(r.harness, b)
	_, lines, _ := r.cli("host-a", nil, "wait", "--as", id(hostATop), "--timeout", "0", "--for", "prompt_outcome")
	if !strings.Contains(fmt.Sprint(lines), "no_receipt:"+id(b)) {
		t.Fatalf("host-a wait = %v", lines)
	}
}
