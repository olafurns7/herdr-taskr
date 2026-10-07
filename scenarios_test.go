package main

import (
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

// Section 7 scenario 1: a wait process that dies after the offer (no ack)
// leaves the event pending; the next wait offers the same event again.
func TestReplayAfterWaitProcessDies(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top)
	l := h.launch(w)
	ready := num(h.ok(as(w, l), "ready", "slice 1", "--report", "r.md", "--kv", "gate=PASS"), "event_id")
	h.ok(as(w, l), "note", "not addressed to anyone")
	done := num(h.ok(as(w, l), "done", "all slices"), "event_id")

	first := h.ok(nil, "wait", "--as", id(top), "--timeout", "0")
	if num(eventOf(first), "id") != ready || first["replay"] != false {
		t.Fatalf("first offer = %v, want event %d", first, ready)
	}
	// The consumer dies here. A new wait replays the same event.
	second := h.ok(nil, "wait", "--as", id(top), "--timeout", "0")
	if num(eventOf(second), "id") != ready || second["replay"] != true {
		t.Fatalf("replay = %v, want event %d with replay:true", second, ready)
	}
	h.one(exitReject, nil, "ack", id(done), "--as", id(top)) // not the pending event
	h.ok(nil, "ack", id(ready), "--as", id(top))
	if again := h.ok(nil, "ack", id(ready), "--as", id(top)); again["already"] != true {
		t.Fatalf("acked retry = %v, want already:true", again)
	}
	next := h.ok(nil, "wait", "--as", id(top), "--timeout", "0")
	if num(eventOf(next), "id") != done {
		t.Fatalf("next offer = %v, want event %d", next, done)
	}
	h.ok(nil, "ack", id(done), "--as", id(top))
	if to := h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "100"); to["timeout"] != true {
		t.Fatalf("timeout output = %v", to)
	}
}

// Scenario 2: two orchestrators on one host have separate inboxes and cursors.
func TestTwoIndependentParentInboxes(t *testing.T) {
	h := newHarness(t)
	r1 := h.newTask("orch-one", "orchestrator", 0)
	r2 := h.newTask("orch-two", "orchestrator", 0)
	c1 := h.newTask("impl-one", "implementer", r1)
	c2 := h.newTask("impl-two", "implementer", r2)
	l1, l2 := h.launch(c1), h.launch(c2)
	e1 := num(h.ok(as(c1, l1), "ready", "one"), "event_id")
	e2 := num(h.ok(as(c2, l2), "ready", "two"), "event_id")

	got1 := h.ok(nil, "wait", "--as", id(r1), "--timeout", "0")
	if num(eventOf(got1), "id") != e1 {
		t.Fatalf("r1 got %v, want %d", got1, e1)
	}
	h.ok(nil, "ack", id(e1), "--as", id(r1))
	h.one(exitTimeout, nil, "wait", "--as", id(r1), "--timeout", "0")

	got2 := h.ok(nil, "wait", "--as", id(r2), "--timeout", "0")
	if num(eventOf(got2), "id") != e2 {
		t.Fatalf("r2 got %v, want %d", got2, e2)
	}
	h.one(exitReject, nil, "ack", id(e2), "--as", id(r1)) // another inbox's event
	h.ok(nil, "ack", id(e2), "--as", id(r2))
}

// Scenario 3: a sub-orchestrator's ask goes to its parent; the answer wakes
// the sub-orchestrator's own blocking wait.
func TestAnswerWakesSubOrchestratorWait(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	sub := h.newTask("orch-sub", "sub-orchestrator", top)
	ls := h.launch(sub)
	w := h.newTask("impl-a", "implementer", sub)
	lw := h.launch(w)

	wr := num(h.ok(as(w, lw), "ready", "slice 1"), "event_id")
	got := h.ok(nil, "wait", "--as", id(sub), "--timeout", "0")
	if num(eventOf(got), "id") != wr {
		t.Fatalf("sub got %v, want worker ready %d", got, wr)
	}
	h.ok(nil, "ack", id(wr), "--as", id(sub))

	ask := num(h.ok(as(sub, ls), "ask", "which base?", "--blocking"), "ask_id")

	woke := make(chan map[string]any, 1)
	go func() {
		var out []map[string]any
		code, out := h.run(nil, "wait", "--as", id(sub), "--timeout", "30000")
		if code != exitOK || len(out) != 1 {
			woke <- nil
			return
		}
		woke <- out[0]
	}()

	gotAsk := h.ok(nil, "wait", "--as", id(top), "--timeout", "0")
	if num(eventOf(gotAsk), "id") != ask {
		t.Fatalf("top got %v, want ask %d", gotAsk, ask)
	}
	h.waitFor("the sub's wait to block", func(db *sql.DB) bool {
		var v sql.NullString
		db.QueryRow(`select waiting_until from tasks where id = ?`, sub).Scan(&v)
		return v.Valid
	})
	ans := num(h.ok(nil, "answer", id(ask), "From the tip after slice 3."), "answer_id")
	h.ok(nil, "ack", id(ask), "--as", id(top))

	select {
	case m := <-woke:
		ev := eventOf(m)
		if m == nil || num(ev, "id") != ans || ev["kind"] != "answer" || num(ev, "related_event_id") != ask {
			t.Fatalf("sub wait woke with %v, want answer %d to ask %d", m, ans, ask)
		}
	case <-time.After(20 * time.Second):
		t.Fatal("sub-orchestrator wait did not wake on the answer")
	}
	h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "0") // the answer is not in top's inbox
}

// Scenario 4: two open asks, one answered: the other stays open, the work
// status is unchanged, and a second answer to the same ask fails.
func TestTwoOpenAsksOneAnswered(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top)
	l := h.launch(w)
	a1 := num(h.ok(as(w, l), "ask", "Slice 4 base?"), "ask_id")
	a2 := num(h.ok(as(w, l), "ask", "Owner copy for the banner?", "--blocking"), "ask_id")

	status := func() map[string]any {
		_, lines := h.run(nil, "status", "--tree", id(w))
		return lines[0]
	}
	if s := status(); s["status"] != "open" || num(s, "open_asks") != 2 || num(s, "blocking_asks") != 1 {
		t.Fatalf("before answer: %v", s)
	}
	out := h.ok(nil, "answer", id(a1), "From the tip.")
	if out["delivered"] != false {
		t.Fatalf("answer without --prompt must report delivered:false, got %v", out)
	}
	h.one(exitReject, nil, "answer", id(a1), "A conflicting second answer.")

	_, open := h.run(nil, "asks", "--open", "--tree", id(top))
	if len(open) != 1 || num(open[0], "id") != a2 {
		t.Fatalf("open asks = %v, want only %d", open, a2)
	}
	if s := status(); s["status"] != "open" || num(s, "open_asks") != 1 || num(s, "blocking_asks") != 1 {
		t.Fatalf("after one answer: %v", s)
	}

	// An answer never reopens a done task.
	h.ok(as(w, l), "done", "finished")
	h.ok(nil, "answer", id(a2), "Use the approved copy.")
	if s := status(); s["status"] != "done" || num(s, "open_asks") != 0 {
		t.Fatalf("after done and answer: %v", s)
	}
	_, all := h.run(nil, "asks", "--tree", id(top))
	if len(all) != 2 || all[0]["answer"] != "From the tip." {
		t.Fatalf("all asks = %v", all)
	}
}

// Scenario 5: two consumers observing the same agent list concurrently write
// one herdr event per change, never two.
func TestConcurrentLivenessNoDuplicateHerdrEvents(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	c1 := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	c2 := h.newTask("impl-b", "implementer", top, "--pane", "w9:p2")
	h.launch(c1)
	h.launch(c2)
	// Both lanes owe a result without a receipt deadline: the idle hint emits.
	h.ok(nil, "prompt", id(c1), "--text", "go", "--receipt-timeout", "0")
	h.ok(nil, "prompt", id(c2), "--text", "go", "--receipt-timeout", "0")
	h.setAgents("w9:p1/idle/5", "w9:p2/working/7")

	dbs := make([]*sql.DB, 2)
	for i := range dbs {
		dbs[i] = h.openDB()
	}
	agents, err := herdrAgentList(h.herdrSock, time.Now().Add(5*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	// Both consumers read the same stored observations before either writes.
	plans := make([][]change, 2)
	for i, d := range dbs {
		ws, err := watchedChildren(d, top)
		if err != nil {
			t.Fatal(err)
		}
		plans[i] = planChanges(ws, agents)
	}
	var wg sync.WaitGroup
	for i := range dbs {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			for _, ch := range plans[i] {
				if _, err := applyChange(dbs[i], ch); err != nil {
					t.Error(err)
				}
			}
		}(i)
	}
	wg.Wait()
	if n := h.countHerdr(top); n != 1 {
		t.Fatalf("herdr events after concurrent apply = %d, want 1 (idle only; working emits none)", n)
	}

	// Full passes in parallel: unchanged list adds nothing; a vanished pane adds one.
	parallelObserve := func() {
		var wg sync.WaitGroup
		for _, d := range dbs {
			wg.Add(1)
			go func(d *sql.DB) {
				defer wg.Done()
				if err := observeChildren(d, h.herdrSock, top, time.Now().Add(5*time.Second)); err != nil {
					t.Error(err)
				}
			}(d)
		}
		wg.Wait()
	}
	parallelObserve()
	if n := h.countHerdr(top); n != 1 {
		t.Fatalf("unchanged list added events: %d", n)
	}
	h.setAgents("w9:p2/working/7")
	parallelObserve()
	if n := h.countHerdr(top); n != 2 {
		t.Fatalf("missing pane: herdr events = %d, want 2", n)
	}
	parallelObserve()
	if n := h.countHerdr(top); n != 2 {
		t.Fatalf("unchanged absence added events: %d", n)
	}
	h.setAgents("w9:p2/done/8")
	parallelObserve()
	if n := h.countHerdr(top); n != 3 {
		t.Fatalf("working to done: herdr events = %d, want 3", n)
	}

	// A launch replaced, or a task closed, between snapshot and apply gets
	// no event and its old launch keeps its observation version.
	c3 := h.newTask("impl-c", "implementer", top, "--pane", "w9:p3")
	c4 := h.newTask("impl-d", "implementer", top, "--pane", "w9:p5")
	old3, old4 := h.launch(c3), h.launch(c4)
	h.setAgents("w9:p2/done/8", "w9:p3/idle/1", "w9:p5/idle/1")
	if agents, err = herdrAgentList(h.herdrSock, time.Now().Add(5*time.Second)); err != nil {
		t.Fatal(err)
	}
	for i, d := range dbs {
		ws, err := watchedChildren(d, top)
		if err != nil {
			t.Fatal(err)
		}
		plans[i] = planChanges(ws, agents)
		if len(plans[i]) != 2 {
			t.Fatalf("plan %d = %d changes, want 2", i, len(plans[i]))
		}
	}
	h.launch(c3, "--pane", "w9:p6")
	h.ok(nil, "close", id(c4))
	for i := range dbs {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			for _, ch := range plans[i] {
				if wrote, err := applyChange(dbs[i], ch); err != nil || wrote {
					t.Errorf("stale change for launch %d: wrote=%v err=%v", ch.w.LaunchID, wrote, err)
				}
			}
		}(i)
	}
	wg.Wait()
	if n := h.countHerdr(top); n != 3 {
		t.Fatalf("stale generation added events: %d, want 3", n)
	}
	for _, l := range []int64{old3, old4} {
		var version int
		dbs[0].QueryRow(`select observed_version from launches where id = ?`, l).Scan(&version)
		if version != 0 {
			t.Fatalf("launch %d observed_version = %d, want 0", l, version)
		}
	}
}

// Scenario 6: a Herdr name reused by a new task does not let the old
// worker's launch write to either task.
func TestReusedNameRejectsOldLaunch(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	a := h.newTask("impl-x", "implementer", top)
	la := h.launch(a)
	h.ok(as(a, la), "note", "working")
	h.ok(nil, "close", id(a))

	b := h.newTask("impl-x", "implementer", top)
	lb := h.launch(b)
	if out := h.one(exitReject, as(a, la), "ready", "late report"); out["kind"] != "rejected" {
		t.Fatalf("closed task write = %v", out)
	}
	h.one(exitReject, as(b, la), "ready", "old launch on the new task")
	h.one(exitReject, as(b, 0), "ready", "no launch id")
	fresh := num(h.ok(as(b, lb), "ready", "new worker"), "event_id")

	got := h.ok(nil, "wait", "--as", id(top), "--timeout", "0")
	if num(eventOf(got), "id") != fresh {
		t.Fatalf("top inbox = %v, want only the new launch's event %d", got, fresh)
	}
}

// Scenario 7: a restored pane is re-registered with a new launch; the old
// launch's events are rejected and the new launch's are accepted.
func TestRestoredPaneReRegisteredByLaunch(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p4")
	l1 := num(h.ok(nil, "launch", id(w), "--provider", "codex", "--model", "gpt-6-luna", "--effort", "max"), "launch_id")
	start := h.ok(map[string]string{"TASKR_TASK": id(w), "TASKR_LAUNCH": id(l1),
		"CODEX_HOME": "/Users/user/.local/share/agent/codex/account-e/native", "CODEX_THREAD_ID": "thread-abc"}, "start")
	if start["account"] != "account-e" || start["session_ref"] != "thread-abc" {
		t.Fatalf("start = %v, want account account-e and session thread-abc", start)
	}
	h.ok(as(w, l1), "note", "before restore")

	relaunch := h.ok(nil, "launch", id(w), "--provider", "claude", "--model", "claude-opus-5-5", "--effort", "high", "--pane", "w9:p7")
	l2 := num(relaunch, "launch_id")
	if num(relaunch, "replaced_launch_id") != l1 || relaunch["pane_id"] != "w9:p7" {
		t.Fatalf("relaunch = %v", relaunch)
	}
	h.one(exitReject, as(w, l1), "ready", "from the replaced launch")
	h.ok(as(w, l2), "start")
	h.ok(as(w, l2), "ready", "from the new launch")

	_, log := h.run(nil, "log", id(w))
	var launches []map[string]any
	for _, m := range log {
		if m["record"] == "launch" {
			launches = append(launches, m)
		}
	}
	if len(launches) != 2 || launches[1]["pane_id"] != "w9:p7" || launches[1]["provider"] != "claude" {
		t.Fatalf("launch history = %v", launches)
	}
	for k, want := range map[string]string{"provider": "codex", "model": "gpt-6-luna", "effort": "max",
		"account": "account-e", "native_home": "/Users/user/.local/share/agent/codex/account-e/native",
		"session_ref": "thread-abc", "session_kind": "thread_id", "session_source": "env:CODEX_THREAD_ID", "pane_id": "w9:p4"} {
		if launches[0][k] != want {
			t.Fatalf("replaced launch %s = %v, want %q (row %v)", k, launches[0][k], want, launches[0])
		}
	}
	_, st := h.run(nil, "status", "--tree", id(w))
	if num(st[0], "current_launch_id") != l2 || st[0]["pane_id"] != "w9:p7" {
		t.Fatalf("status = %v", st[0])
	}
}

// Scenario 8: a prompt whose herdr call times out is recorded as
// delivery_unknown, and taskr does not send it again.
func TestAmbiguousPromptRecordedWithoutResend(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p4")
	h.launch(w)
	h.write("prompt.stderr", `{"error":{"code":"timeout","message":"timed out waiting for agent"}}`, 0o644)
	h.write("prompt.exit", "1", 0o644)
	brief := filepath.Join(h.dir, "round2.md")
	os.WriteFile(brief, []byte("Round 2: fix the price rounding."), 0o644)

	out := h.one(exitHerdr, nil, "prompt", id(w), "--file", brief)
	if out["outcome"] != "delivery_unknown" || out["herdr_error"] != "timeout" || num(out, "attempt_id") == 0 {
		t.Fatalf("prompt output = %v", out)
	}
	time.Sleep(300 * time.Millisecond)
	calls := h.calls("agent|prompt|")
	want := "agent|prompt|w9:p4|First taskr got " + id(num(out, "attempt_id")) + "; read " + brief + "; execute exactly." + wantImplementerContract + "|--wait|--until|working|--until|blocked|--timeout|20000|"
	if len(calls) != 1 || calls[0] != want {
		t.Fatalf("herdr prompt calls = %q, want exactly one %q", calls, want)
	}
	_, log := h.run(nil, "log", id(w))
	var attempts, outcomes int
	for _, m := range log {
		switch m["kind"] {
		case "prompt":
			attempts++
			sum := sha256.Sum256([]byte("Round 2: fix the price rounding."))
			if d, _ := m["data"].(map[string]any); d["sha256"] != hex.EncodeToString(sum[:]) || d["target"] != "w9:p4" || d["file"] != brief {
				t.Fatalf("attempt = %v", m)
			}
		case "prompt_outcome":
			outcomes++
			if m["summary"] != "delivery_unknown" || num(m, "related_event_id") != num(out, "attempt_id") {
				t.Fatalf("outcome = %v", m)
			}
		}
	}
	if attempts != 1 || outcomes != 1 {
		t.Fatalf("attempts=%d outcomes=%d, want 1 and 1", attempts, outcomes)
	}
}
