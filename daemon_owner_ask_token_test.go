package main

import (
	"reflect"
	"testing"
	"time"
)

func TestDaemonOwnerAskToken(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0, "--pane", "w1:p1")
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	l := h.launch(w)
	h.setAgents("w9:p1/working/1")
	d := &daemon{db: h.openDB(), log: &daemonLog{}, sock: h.herdrSock}
	writes := func() []string { return h.calls("pane|report-metadata|") }
	check := func(want ...string) {
		t.Helper()
		d.pass()
		d.pass()
		if got := writes(); !reflect.DeepEqual(got, want) {
			t.Fatalf("token writes = %q, want %q", got, want)
		}
	}
	const lane = "pane|report-metadata|w9:p1|--source|taskr|--token|taskr_state="
	const root = "pane|report-metadata|w1:p1|--source|taskr|--token|taskr_state="
	d.pass()

	// (b) A non-owner ask leaves taskr_owner_ask unset; (e) state and round as before.
	plain := num(h.ok(as(w, l), "ask", "which base?"), "ask_id")
	w1 := lane + "ask|--token|taskr_round=0|"
	check(w1)
	h.ok(nil, "answer", id(plain), "main", "--as", id(top))
	w2 := lane + "open|--token|taskr_round=0|"
	check(w1, w2)

	// (a) A lane's owner ask marks the lane's pane, even while it waits.
	owner := num(h.ok(as(w, l), "ask", "ship it?", "--owner", "--blocking"), "ask_id")
	h.openDB().Exec(`update tasks set waiting_until = ? where id = ?`, stamp(time.Now().Add(time.Hour)), w)
	w3 := lane + "waiting|--token|taskr_round=0|--token|taskr_owner_ask=1|"
	check(w1, w2, w3)

	// (c) Answering clears it.
	h.ok(nil, "answer", id(owner), "yes", "--as", id(top))
	w4 := lane + "waiting|--token|taskr_round=0|--clear-token|taskr_owner_ask|"
	check(w1, w2, w3, w4)

	// (d) A root's own owner ask marks the root's pane.
	rootAsk := num(h.ok(nil, "ask", "budget?", "--owner", "--as", id(top)), "ask_id")
	r1 := root + "ask|--token|taskr_round=0|--token|taskr_owner_ask=1|"
	check(w1, w2, w3, w4, r1)
	h.ok(nil, "answer", id(rootAsk), "ok", "--as", id(top))
	r2 := root + "open|--token|taskr_round=0|--clear-token|taskr_owner_ask|"
	check(w1, w2, w3, w4, r1, r2)

	// A closed task with an open owner ask gets the token cleared once.
	h.ok(as(w, l), "ask", "again?", "--owner")
	h.openDB().Exec(`update tasks set waiting_until = null where id = ?`, w)
	w5 := lane + "ask|--token|taskr_round=0|--token|taskr_owner_ask=1|"
	check(w1, w2, w3, w4, r1, r2, w5)
	h.ok(nil, "close", id(w))
	w6 := "pane|report-metadata|w9:p1|--source|taskr|--clear-token|taskr_owner_ask|"
	check(w1, w2, w3, w4, r1, r2, w5, w6)
}
