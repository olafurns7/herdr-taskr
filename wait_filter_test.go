package main

import (
	"database/sql"
	"fmt"
	"os"
	"strings"
	"syscall"
	"testing"
	"time"
)

func TestWaitCapacityFilterReplay(t *testing.T) {
	for _, tc := range []struct {
		name string
		data map[string]any
	}{
		{"capacity", map[string]any{"reason": "model_capacity"}},
		{"quota", map[string]any{"quota": "limit"}},
		{"stall", map[string]any{"reason": "stall", "last": "blocked"}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			h := newHarness(t)
			top := h.newTask("top", "orchestrator", 0)
			a := h.newTask("alpha", "implementer", top)
			b := h.newTask("beta", "implementer", top)
			la, lb := h.launch(a), h.launch(b)
			protected := insertCoalesceEvent(t, h.openDB(), event{TaskID: b, RecipientTaskID: ptr(top), LaunchID: ptr(lb), Kind: "herdr", Data: tc.data})
			ready := num(h.ok(as(a, la), "ready", "alpha"), "event_id")
			args := []string{"wait", "--as", id(top), "--for", "ready", "--from", id(a), "--timeout", "0"}
			got := h.ok(nil, args...)
			if num(eventOf(got), "id") != protected || got["replay"] != false {
				t.Fatalf("first bypass = %v", got)
			}
			got = h.ok(nil, args...)
			if num(eventOf(got), "id") != protected || got["replay"] != true {
				t.Fatalf("bypass replay = %v", got)
			}
			args = append([]string{"wait", "--as", id(top), "--ack", id(protected)}, args[3:]...)
			got = h.ok(nil, args...)
			if num(eventOf(got), "id") != ready || got["replay"] != false {
				t.Fatalf("ack disposition = %v", got)
			}
		})
	}
}

func TestWaitBypassStaysNarrow(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	a := h.newTask("alpha", "implementer", top)
	b := h.newTask("beta", "implementer", top)
	la, lb := h.launch(a), h.launch(b)
	db := h.openDB()
	insertCoalesceEvent(t, db, event{TaskID: b, RecipientTaskID: ptr(top), LaunchID: ptr(lb), Kind: "herdr", Data: map[string]any{"reason": "other"}})
	insertCoalesceEvent(t, db, event{TaskID: b, RecipientTaskID: ptr(top), LaunchID: ptr(lb), Kind: "herdr", Data: map[string]any{"agent_status": "idle"}})
	ready := num(h.ok(as(a, la), "ready", "alpha"), "event_id")
	code, raw, diag := h.compact(nil, "wait", "--as", id(top), "--for", "ready", "--from", id(a), "--timeout", "0")
	if code != exitOK || diag != "" || !strings.HasPrefix(raw, fmt.Sprintf("sk1 2\ne1\t%d\t", ready)) {
		t.Fatalf("narrow bypass = %d %q %q, want ready %d after two skips", code, raw, diag, ready)
	}
}

func TestWaitWorkerIdentity(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("worker", "implementer", top)
	l := h.launch(w)
	ask := num(h.ok(as(w, l), "ask", "which pin?"), "ask_id")
	answer := num(h.ok(nil, "answer", id(ask), "main"), "answer_id")
	got := h.ok(as(w, l), "wait", "--for", "answer", "--timeout", "0")
	if num(eventOf(got), "id") != answer || eventOf(got)["kind"] != "answer" {
		t.Fatalf("worker's implicit inbox = %v", got)
	}
	h.one(exitReject, as(w, l), "wait", "--as", id(top), "--timeout", "0")
	h.one(exitUsage, nil, "wait", "--timeout", "0")
	stale := l
	h.launch(w)
	h.one(exitReject, as(w, stale), "wait", "--timeout", "0")
	h.one(exitReject, as(w, stale), "wait", "--as", id(w), "--timeout", "0")
}

func TestWaitWorkerIdentityClientMode(t *testing.T) {
	r := newTwoHost(t)
	dir := t.TempDir()
	top := num(r.want(0, "host-a", nil, "new", "top", "--role", "orchestrator", "--cwd", dir), "task_id")
	w := num(r.want(0, "host-a", nil, "new", "worker", "--role", "implementer", "--parent", id(top), "--cwd", dir), "task_id")
	l := num(r.want(0, "host-a", nil, "launch", id(w), "--provider", "claude", "--model", "m", "--effort", "high"), "launch_id")
	ask := num(r.want(0, "host-a", as(w, l), "ask", "which pin?"), "ask_id")
	answer := num(r.want(0, "host-a", nil, "answer", id(ask), "main", "--as", id(top)), "answer_id")
	got := r.want(0, "host-a", as(w, l), "wait", "--for", "answer", "--timeout", "0")
	if num(eventOf(got), "id") != answer || eventOf(got)["kind"] != "answer" {
		t.Fatalf("client worker's implicit inbox = %v", got)
	}
}

func TestWaitFilterDeadlinePreservesBacklog(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top)
	db := h.openDB()
	const backlog = 2000
	var match int64
	if err := withTx(db, func(tx *sql.Tx) error {
		for range backlog {
			if _, err := insertEvent(tx, event{TaskID: w, RecipientTaskID: &top, Kind: "got"}); err != nil {
				return err
			}
		}
		var err error
		match, err = insertEvent(tx, event{TaskID: w, RecipientTaskID: &top, Kind: "ready"})
		return err
	}); err != nil {
		t.Fatal(err)
	}
	code, raw, diag := h.compact(nil, "wait", "--as", id(top), "--for", "ready", "--timeout", "1")
	var skipped int
	if _, err := fmt.Sscanf(raw, "sk1 %d\n", &skipped); err != nil || code != exitOK || diag != "" ||
		skipped <= 0 || skipped >= backlog || raw != fmt.Sprintf("sk1 %d\nw1 {\"owed\":0,\"due\":0}\nx1 3 timeout\n", skipped) {
		t.Fatalf("bounded wait = %d %q %q", code, raw, diag)
	}
	task, err := loadTask(db, top)
	if err != nil {
		t.Fatal(err)
	}
	var retained, remaining int
	if err := db.QueryRow(`select count(*), count(case when id > ? then 1 end) from events where recipient_task_id = ?`, task.AckedEventID, top).
		Scan(&retained, &remaining); err != nil {
		t.Fatal(err)
	}
	if retained != backlog+1 || remaining != backlog+1-skipped || task.PendingEventID.Valid || task.WaitingUntil.Valid {
		t.Fatalf("retained=%d remaining=%d skipped=%d task=%+v", retained, remaining, skipped, task)
	}
	// The next unfiltered poll sees the remaining oldest event; timeout0 may
	// still drain the finite nonmatching backlog to reach the ready event.
	got := h.ok(nil, "wait", "--as", id(top), "--timeout", "0")
	if eventOf(got)["kind"] != "got" || got["replay"] != false {
		t.Fatalf("remaining event = %v", got)
	}
	code, raw, _ = h.compact(nil, "wait", "--as", id(top), "--for", "ready", "--timeout", "0")
	if code != exitOK || !strings.HasPrefix(raw, fmt.Sprintf("sk1 %d\ne1\t%d\t", backlog-skipped, match)) {
		t.Fatalf("immediate filtered poll = %d %q", code, raw)
	}
}

func TestWaitFilterSkipAckReplay(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	a := h.newTask("alpha", "implementer", top)
	b := h.newTask("beta", "implementer", top)
	la, lb := h.launch(a), h.launch(b)
	skipKind := num(h.ok(as(a, la), "ask", "Which pin?"), "event_id")
	skipSource := num(h.ok(as(b, lb), "ready", "beta"), "event_id")
	match := num(h.ok(as(a, la), "ready", "alpha"), "event_id")
	// A pending non-match from an earlier wait is skipped and stays in the log.
	h.ok(nil, "wait", "--as", id(top), "--timeout", "0")
	code, raw, _ := h.compact(nil, "wait", "--as", id(top), "--for", "ready,fail", "--from", "alpha", "--timeout", "0")
	if code != exitOK || !strings.HasPrefix(raw, "sk1 2\ne1\t"+id(match)+"\t") || strings.Split(strings.TrimSpace(raw), "\t")[5] != "0" {
		t.Fatalf("filtered wait = %d %q", code, raw)
	}
	got := h.ok(nil, "wait", "--as", id(top), "--for", "ready", "--from", id(a), "--from", "beta", "--timeout", "0")
	if num(eventOf(got), "id") != match || got["replay"] != true {
		t.Fatalf("matching event not pending: %v", got)
	}
	var pending sql.NullInt64
	var acked, retained int64
	db := h.openDB()
	db.QueryRow(`select pending_event_id, acked_event_id from tasks where id = ?`, top).Scan(&pending, &acked)
	db.QueryRow(`select count(*) from events where id in (?, ?)`, skipKind, skipSource).Scan(&retained)
	if pending.Int64 != match || acked != skipSource || retained != 2 {
		t.Fatalf("pending=%v ack=%d retained=%d", pending, acked, retained)
	}
	// Ack only the pending handled event; any filter error must precede the ack.
	h.one(exitReject, nil, "wait", "--as", id(top), "--ack", id(skipSource), "--timeout", "0")
	h.one(exitUsage, nil, "wait", "--as", id(top), "--ack", id(match), "--for", "typo", "--timeout", "0")
	got = h.ok(nil, "wait", "--as", id(top), "--timeout", "0")
	if num(eventOf(got), "id") != match {
		t.Fatalf("invalid filter acknowledged match: %v", got)
	}
	next := num(h.ok(as(a, la), "fail", "blocked"), "event_id")
	got = h.ok(nil, "wait", "--as", id(top), "--ack", id(match), "--for", "fail", "--timeout", "0")
	if num(eventOf(got), "id") != next || got["replay"] != false {
		t.Fatalf("ack then wait = %v", got)
	}
	h.one(exitTimeout, nil, "wait", "--as", id(top), "--ack", id(next), "--for", "ready", "--timeout", "0")
	h.one(exitReject, nil, "wait", "--as", id(top), "--ack", id(next), "--timeout", "0")
	h.ok(nil, "ack", id(next), "--as", id(top)) // standalone ack still idempotent
}

func TestWaitFilterObservedEventAndTimeout(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top, "--pane", "w9:p1")
	l := h.launch(w)
	h.ok(nil, "prompt", id(w), "--text", "go", "--receipt-timeout", "0") // an owed result, no receipt deadline: the idle hint emits
	h.setAgents("w9:p1/idle/3")
	// A newly observed matching event is a first delivery, not a replay.
	got := h.ok(nil, "wait", "--as", id(top), "--for", "herdr", "--timeout", "1000")
	if got["replay"] != false || eventOf(got)["kind"] != "herdr" {
		t.Fatalf("observed match = %v", got)
	}
	eid := num(eventOf(got), "id")
	h.ok(as(w, l), "ask", "skip this")
	code, raw, _ := h.compact(nil, "wait", "--as", id(top), "--ack", id(eid), "--for", "ready", "--timeout", "50")
	if code != exitOK || raw != "sk1 1\nw1 {\"owed\":1,\"due\":0}\nx1 3 timeout\n" || h.waitingUntil(top).Valid {
		t.Fatalf("skip then timeout = %d %q", code, raw)
	}
	for _, args := range [][]string{{"--for", ""}, {"--for", "ready,"}, {"--ack", "-1"}, {"--ack", "0"}, {"--from", "0"}} {
		h.one(exitUsage, nil, append([]string{"wait", "--as", id(top), "--timeout", "0"}, args...)...)
	}
	h.one(exitReject, nil, "wait", "--as", id(top), "--from", "missing", "--timeout", "0")
	h.newTask("lane", "implementer", top)
	h.one(exitReject, nil, "wait", "--as", id(top), "--from", "lane", "--timeout", "0")
}

func TestWaitCountsIgnoreClosedChild(t *testing.T) {
	h, top, w, _ := hintFixture(t)
	h.ok(nil, "prompt", id(w), "--text", "Go.", "--receipt-timeout", "0")
	h.ok(nil, "close", id(w))
	for i := 0; ; i++ {
		_, lines := h.run(nil, "wait", "--as", id(top), "--timeout", "0")
		var ev map[string]any
		for _, m := range lines {
			if event := eventOf(m); event != nil {
				ev = event
			}
		}
		if ev == nil {
			break
		}
		h.ok(nil, "ack", id(num(ev, "id")), "--as", id(top))
		if i > 20 {
			t.Fatal("inbox did not drain")
		}
	}
	code, raw, diag := h.compact(nil, "wait", "--as", id(top), "--timeout", "0")
	want := "w1 {\"owed\":0,\"due\":0}\nx1 3 timeout\n"
	if code != exitOK || raw != want || diag != "" {
		t.Fatalf("closed child: %d %q %q, want %q", code, raw, diag, want)
	}
}

func TestWaitFilterInterruptClearsWaiting(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top)
	l := h.launch(w)
	h.ok(as(w, l), "ask", "skip")
	waited := h.goRun(nil, "wait", "--as", id(top), "--for", "ready", "--timeout", "60000")
	h.waitFor("filtered wait marker", func(db *sql.DB) bool {
		var v sql.NullString
		db.QueryRow(`select waiting_until from tasks where id = ?`, top).Scan(&v)
		return v.Valid
	})
	if err := syscall.Kill(os.Getpid(), syscall.SIGINT); err != nil {
		t.Fatal(err)
	}
	select {
	case r := <-waited:
		if r.code != exitTimeout || r.out["interrupted"] != true || h.waitingUntil(top).Valid {
			t.Fatalf("interrupt = %d %v", r.code, r.out)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("interrupt did not end wait")
	}
}

func TestCompactWaitInterruptClearsWaiting(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top)
	l := h.launch(w)
	h.ok(as(w, l), "ask", "skip")
	result := make(chan struct {
		code      int
		raw, diag string
	}, 1)
	go func() {
		code, raw, diag := h.compact(nil, "wait", "--as", id(top), "--for", "ready", "--timeout", "60000")
		result <- struct {
			code      int
			raw, diag string
		}{code, raw, diag}
	}()
	h.waitFor("compact wait marker", func(db *sql.DB) bool {
		var v sql.NullString
		db.QueryRow(`select waiting_until from tasks where id = ?`, top).Scan(&v)
		return v.Valid
	})
	if err := syscall.Kill(os.Getpid(), syscall.SIGINT); err != nil {
		t.Fatal(err)
	}
	select {
	case r := <-result:
		if r.code != exitTimeout || r.raw != "sk1 1\nx1 3 timeout interrupted\n" || r.diag != "" || h.waitingUntil(top).Valid {
			t.Fatalf("compact interrupt = %d %q %q", r.code, r.raw, r.diag)
		}
		t.Logf("exit=%d stdout=%q stderr=%q", r.code, r.raw, r.diag)
	case <-time.After(5 * time.Second):
		t.Fatal("compact interrupt did not end wait")
	}
}

func TestCompactWaitTimeoutCleanupFailure(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	if _, err := h.openDB().Exec(`create trigger reject_clear before update of waiting_until on tasks
 when new.waiting_until is null and old.waiting_until is not null
 begin select raise(abort, 'injected clear failure'); end`); err != nil {
		t.Fatal(err)
	}
	code, raw, diag := h.compact(nil, "wait", "--as", id(top), "--timeout", "20")
	if code != exitDB || !strings.HasPrefix(raw, "x1 4 ") || !strings.Contains(raw, "injected clear failure") ||
		strings.Contains(raw, "timeout") || diag != "" || !h.waitingUntil(top).Valid {
		t.Fatalf("compact timeout cleanup failure = %d %q %q", code, raw, diag)
	}
}

func TestWaitAckCleanupFailureRetainsBothIDs(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top)
	l := h.launch(w)
	firstAsk := num(h.ok(as(w, l), "ask", "first"), "ask_id")
	first := num(h.ok(nil, "answer", id(firstAsk), "one"), "answer_id")
	h.ok(nil, "wait", "--as", id(w), "--timeout", "0")
	secondAsk := num(h.ok(as(w, l), "ask", "second"), "ask_id")
	db := h.openDB()
	if _, err := db.Exec(`create trigger reject_clear before update of waiting_until on tasks
 when new.waiting_until is null and old.waiting_until is not null
 begin select raise(abort, 'injected clear failure'); end`); err != nil {
		t.Fatal(err)
	}
	waited := h.goRun(nil, "wait", "--as", id(w), "--ack", id(first), "--for", "answer", "--timeout", "60000")
	h.waitFor("combined wait marker", func(db *sql.DB) bool {
		var v sql.NullString
		db.QueryRow(`select waiting_until from tasks where id=?`, w).Scan(&v)
		return v.Valid
	})
	second := num(h.ok(nil, "answer", id(secondAsk), "two"), "answer_id")
	select {
	case r := <-waited:
		if r.code != exitDB || num(r.out, "acked_event_id") != first || num(r.out, "pending_event_id") != second {
			t.Fatalf("partial combined wait: %d %v", r.code, r.out)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("combined wait did not return")
	}
	db.Exec(`drop trigger reject_clear`)
	got := h.ok(nil, "wait", "--as", id(w), "--for", "answer", "--timeout", "0")
	if num(eventOf(got), "id") != second || got["replay"] != true {
		t.Fatalf("cleanup replay: %v", got)
	}
}
