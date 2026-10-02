package main

import (
	"database/sql"
	"fmt"
	"testing"
)

// hintFixture is one orchestrator with one launched implementer in w9:p1.
// It returns the harness, the orchestrator, the worker and the launch.
func hintFixture(t *testing.T) (*harness, int64, int64, int64) {
	t.Helper()
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	l := h.launch(w)
	return h, top, w, l
}

// hintCounterKey is today's hint_suppressed counter key for status/phase.
func hintCounterKey(status, phase string) string {
	return fmt.Sprintf("hint_suppressed:%s:%s:%s", now()[:10], status, phase)
}

// hintCounter reads one hint_suppressed counter; 0 when absent.
func hintCounter(t *testing.T, db *sql.DB, status, phase string) int {
	t.Helper()
	var v sql.NullString
	if err := db.QueryRow(`select value from meta where key = ?`,
		hintCounterKey(status, phase)).Scan(&v); err != nil && err != sql.ErrNoRows {
		t.Fatal(err)
	}
	var n int
	fmt.Sscan(v.String, &n)
	return n
}

// hintCounters counts every hint_suppressed key in meta.
func hintCounters(t *testing.T, db *sql.DB) int {
	t.Helper()
	var n int
	if err := db.QueryRow(`select count(*) from meta where key like 'hint_suppressed:%'`).Scan(&n); err != nil {
		t.Fatal(err)
	}
	return n
}

// promptOwed records a prompt attempt on the task's current launch, so the
// lane owes its parent a result. A prompt arms its receipt_due row by
// default; extra args adjust that.
func promptOwed(t *testing.T, h *harness, task int64, extra ...string) int64 {
	t.Helper()
	return num(h.ok(nil, append([]string{"prompt", id(task), "--text", "Go."}, extra...)...), "attempt_id")
}

func TestHintStartupUnknownSilent(t *testing.T) {
	h, top, _, l := hintFixture(t)
	h.setAgents("w9:p1/unknown/1")
	if out := h.ok(nil, "daemon", "--once"); out["once"] != true {
		t.Fatalf("daemon --once = %v", out)
	}
	if n := h.countHerdr(top); n != 0 {
		t.Fatalf("startup unknown emitted: herdr events = %d", n)
	}
	if n := hintCounter(t, h.openDB(), "unknown", "startup"); n != 1 {
		t.Fatalf("startup counter = %d, want 1", n)
	}
	var version int
	var status string
	var present bool
	if err := h.openDB().QueryRow(`select observed_version, observed_status, present from launches where id = ?`, l).
		Scan(&version, &status, &present); err != nil || version != 1 || status != "unknown" || !present {
		t.Fatalf("startup observation not written: version=%d status=%s present=%v err=%v", version, status, present, err)
	}
}

func TestHintMidTurnIdleWithGotEmits(t *testing.T) {
	h, top, w, l := hintFixture(t)
	a := promptOwed(t, h, w)
	h.ok(as(w, l), "got", id(a))
	h.setAgents("w9:p1/idle/2")
	h.ok(nil, "daemon", "--once")
	if n := h.countHerdr(top); n != 1 {
		t.Fatalf("mid-turn idle with a got: herdr events = %d, want 1", n)
	}
	if n := hintCounters(t, h.openDB()); n != 0 {
		t.Fatalf("emitted hint was counted suppressed: %d", n)
	}
	var status string
	h.openDB().QueryRow(`select observed_status from launches where id = ?`, l).Scan(&status)
	if status != "idle" {
		t.Fatalf("observation = %s, want idle", status)
	}
}

func TestHintArmedRowSilent(t *testing.T) {
	h, top, w, l := hintFixture(t)
	promptOwed(t, h, w) // the prompt arms receipt_due:<attempt> itself
	h.setAgents("w9:p1/idle/2")
	h.ok(nil, "daemon", "--once")
	if n := h.countHerdr(top); n != 0 {
		t.Fatalf("armed idle emitted: herdr events = %d", n)
	}
	if n := hintCounter(t, h.openDB(), "idle", "armed"); n != 1 {
		t.Fatalf("armed counter = %d, want 1", n)
	}
	var status string
	var present bool
	h.openDB().QueryRow(`select observed_status, present from launches where id = ?`, l).Scan(&status, &present)
	if status != "idle" || !present {
		t.Fatalf("armed idle observation not written: %s %v", status, present)
	}
}

func TestHintUnarmedEmits(t *testing.T) {
	h, top, w, l := hintFixture(t)
	promptOwed(t, h, w, "--receipt-timeout", "0") // --receipt-timeout 0 disarms
	h.setAgents("w9:p1/idle/2")
	h.ok(nil, "daemon", "--once")
	if n := h.countHerdr(top); n != 1 {
		t.Fatalf("unarmed idle: herdr events = %d, want 1", n)
	}
	if n := hintCounters(t, h.openDB()); n != 0 {
		t.Fatalf("suppressed counters = %d, want 0", n)
	}
	var status string
	var present bool
	h.openDB().QueryRow(`select observed_status, present from launches where id = ?`, l).Scan(&status, &present)
	if status != "idle" || !present {
		t.Fatalf("observation = %s present %v", status, present)
	}
}

func TestHintPostReady(t *testing.T) {
	h, top, w, l := hintFixture(t)
	promptOwed(t, h, w)
	h.ok(as(w, l), "ready", "slice")
	h.setAgents("w9:p1/idle/2")
	h.ok(nil, "daemon", "--once")
	if n := h.countHerdr(top); n != 0 {
		t.Fatalf("post-ready idle emitted: herdr events = %d", n)
	}
	if n := hintCounter(t, h.openDB(), "idle", "post_ready"); n != 1 {
		t.Fatalf("post_ready counter = %d, want 1", n)
	}
	h.setAgents()
	h.ok(nil, "daemon", "--once")
	if n := h.countHerdr(top); n != 1 {
		t.Fatalf("post-ready missing: herdr events = %d, want 1", n)
	}
	var present bool
	h.openDB().QueryRow(`select present from launches where id = ?`, l).Scan(&present)
	if present {
		t.Fatal("post-ready missing did not write the observation")
	}
	if n := hintCounters(t, h.openDB()); n != 1 {
		t.Fatalf("post-ready missing added a counter: %d keys", n)
	}
}

func TestHintAfterDoneSilent(t *testing.T) {
	h, top, w, l := hintFixture(t)
	promptOwed(t, h, w)
	h.ok(as(w, l), "done", "did it")
	h.setAgents("w9:p1/idle/2")
	h.ok(nil, "daemon", "--once")
	h.setAgents()
	h.ok(nil, "daemon", "--once")
	if n := h.countHerdr(top); n != 0 {
		t.Fatalf("after done emitted: herdr events = %d", n)
	}
	if n := hintCounter(t, h.openDB(), "idle", "post_done"); n != 1 {
		t.Fatalf("idle counter = %d, want 1", n)
	}
	if n := hintCounter(t, h.openDB(), "missing", "post_done"); n != 1 {
		t.Fatalf("missing counter = %d, want 1", n)
	}
	var version int
	var present bool
	h.openDB().QueryRow(`select observed_version, present from launches where id = ?`, l).Scan(&version, &present)
	if version != 2 || present {
		t.Fatalf("observations not written: version %d present %v", version, present)
	}
	if n := hintCounters(t, h.openDB()); n != 2 {
		t.Fatalf("counter keys = %d, want 2", n)
	}
}

func TestHintBlockedAlwaysEmits(t *testing.T) {
	h, top, _, l := hintFixture(t)
	h.setAgents("w9:p1/blocked/1")
	h.ok(nil, "daemon", "--once")
	if n := h.countHerdr(top); n != 1 {
		t.Fatalf("blocked without any prompt: herdr events = %d, want 1", n)
	}
	if n := hintCounters(t, h.openDB()); n != 0 {
		t.Fatalf("blocked added a counter: %d keys", n)
	}
	var version int
	var status string
	h.openDB().QueryRow(`select observed_version, observed_status from launches where id = ?`, l).Scan(&version, &status)
	if version != 1 || status != "blocked" {
		t.Fatalf("blocked observation = version %d status %s", version, status)
	}
}

func TestHintCapacityEmitsOnOwnPath(t *testing.T) {
	h, db, top, _, _, p := capacityFixture(t)
	p.status, p.text = "idle", nativeCapacity+capacityFooter
	scanCapacityTest(t, h, db, top)
	if n := h.countHerdr(top); n != 1 {
		t.Fatalf("capacity events = %d, want 1", n)
	}
	if n := hintCounters(t, db); n != 0 {
		t.Fatalf("capacity path added a counter: %d keys", n)
	}
}

func TestOwes(t *testing.T) {
	h, _, w, _ := hintFixture(t)
	db := h.openDB()
	owed := func() bool {
		t.Helper()
		o, err := owes(db, h.launchID(t, w))
		if err != nil {
			t.Fatal(err)
		}
		return o
	}
	if owed() {
		t.Fatal("no prompt yet: owes = true")
	}
	promptOwed(t, h, w)
	if !owed() {
		t.Fatal("prompt newer than any done: owes = false")
	}
	h.ok(as(w, h.launchID(t, w)), "ready", "slice") // a mid-brief ready does not end the debt
	if !owed() {
		t.Fatal("ready does not end the debt: owes = false")
	}
	h.ok(as(w, h.launchID(t, w)), "done", "did it")
	if owed() {
		t.Fatal("done ends the debt: owes = true")
	}
	promptOwed(t, h, w)
	if !owed() {
		t.Fatal("a newer prompt re-arms the debt: owes = false")
	}
	h.ok(as(w, h.launchID(t, w)), "fail", "no")
	if owed() {
		t.Fatal("fail ends the debt: owes = true")
	}
}

func (h *harness) launchID(t *testing.T, task int64) int64 {
	t.Helper()
	var l int64
	h.openDB().QueryRow(`select current_launch_id from tasks where id = ?`, task).Scan(&l)
	return l
}
