package main

import (
	"encoding/json"
	"reflect"
	"testing"
	"time"
)

// ownerAskPanes writes the fake pane list: each pane's taskr_owner_ask, or ""
// for a pane without it.
func ownerAskPanes(h *harness, tokens map[string]string) {
	h.t.Helper()
	panes := []map[string]any{}
	for pane, v := range tokens {
		tok := map[string]string{"taskr_state": "open"}
		if v != "" {
			tok["taskr_owner_ask"] = v
		}
		panes = append(panes, map[string]any{"pane_id": pane, "tokens": tok})
	}
	b, _ := json.Marshal(map[string]any{"result": map[string]any{"panes": panes}})
	h.write("panes.json", string(b), 0o644)
}

func ownerAskHarness(t *testing.T) (*harness, *daemon, func() []string) {
	h := newHarness(t)
	d := &daemon{db: h.openDB(), log: &daemonLog{}, sock: h.herdrSock}
	d.connected.Store(true)
	return h, d, func() []string { return h.calls("pane|report-metadata|") }
}

const (
	setA   = "pane|report-metadata|w9:p1|--source|taskr|--token|taskr_owner_ask=1|"
	clearA = "pane|report-metadata|w9:p1|--source|taskr|--clear-token|taskr_owner_ask|"
	setB   = "pane|report-metadata|w9:p2|--source|taskr|--token|taskr_owner_ask=1|"
	clearB = "pane|report-metadata|w9:p2|--source|taskr|--clear-token|taskr_owner_ask|"
	setR   = "pane|report-metadata|w1:p1|--source|taskr|--token|taskr_owner_ask=1|"
	stateA = "pane|report-metadata|w9:p1|--source|taskr|--token|taskr_state="
)

func wantCalls(t *testing.T, got []string, want ...string) {
	t.Helper()
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("token writes = %q, want %q", got, want)
	}
}

func TestDaemonOwnerAskToken(t *testing.T) {
	h, d, writes := ownerAskHarness(t)
	top := h.newTask("top", "orchestrator", 0, "--pane", "w1:p1")
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	l := h.launch(w)
	h.setAgents("w9:p1/working/1")
	ownerAskPanes(h, map[string]string{"w1:p1": "", "w9:p1": ""})

	// An owner ask open before the first pass is set once, even while the
	// lane waits; state/round keep their baseline.
	owner := num(h.ok(as(w, l), "ask", "ship it?", "--owner", "--blocking"), "ask_id")
	h.openDB().Exec(`update tasks set waiting_until = ? where id = ?`, stamp(time.Now().Add(time.Hour)), w)
	d.pass()
	ownerAskPanes(h, map[string]string{"w1:p1": "", "w9:p1": "1"})
	d.pass()
	d.pass()
	wantCalls(t, writes(), setA)

	// A non-owner ask changes only taskr_state, in the unchanged call.
	h.openDB().Exec(`update tasks set waiting_until = null where id = ?`, w)
	h.ok(as(w, l), "ask", "which base?")
	d.pass()
	wantCalls(t, writes(), setA, stateA+"ask|--token|taskr_round=0|")

	// Answering clears it.
	h.ok(nil, "answer", id(owner), "yes", "--as", id(top))
	d.pass()
	ownerAskPanes(h, map[string]string{"w1:p1": "", "w9:p1": ""})
	d.pass()
	wantCalls(t, writes(), setA, stateA+"ask|--token|taskr_round=0|", clearA)

	// A root's own owner ask marks the root's pane.
	h.ok(nil, "ask", "budget?", "--owner", "--as", id(top))
	d.pass()
	d.pass()
	wantCalls(t, writes(), setA, stateA+"ask|--token|taskr_round=0|", clearA,
		"pane|report-metadata|w1:p1|--source|taskr|--token|taskr_state=ask|--token|taskr_round=0|", setR)
}

func TestDaemonOwnerAskTokenMove(t *testing.T) {
	h, d, writes := ownerAskHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	l := h.launch(w)
	h.setAgents("w9:p1/working/1", "w9:p2/working/1")
	ask := num(h.ok(as(w, l), "ask", "ship it?", "--owner"), "ask_id")
	ownerAskPanes(h, map[string]string{"w9:p1": "", "w9:p2": ""})
	d.pass()
	wantCalls(t, writes(), setA)

	// Moving the launch to pane B clears A and sets B; state/round follow the pane as before.
	h.openDB().Exec(`update launches set pane_id = 'w9:p2' where id = ?`, l)
	ownerAskPanes(h, map[string]string{"w9:p1": "1", "w9:p2": ""})
	d.pass()
	moved := "pane|report-metadata|w9:p2|--source|taskr|--token|taskr_state=ask|--token|taskr_round=0|"
	wantCalls(t, writes(), setA, moved, clearA, setB)

	// Answering then clears B only.
	h.ok(nil, "answer", id(ask), "yes", "--as", id(top))
	ownerAskPanes(h, map[string]string{"w9:p1": "", "w9:p2": "1"})
	d.pass()
	wantCalls(t, writes(), setA, moved, clearA, setB,
		"pane|report-metadata|w9:p2|--source|taskr|--token|taskr_state=open|--token|taskr_round=0|", clearB)
}

func TestDaemonOwnerAskTokenRestart(t *testing.T) {
	h, d, writes := ownerAskHarness(t)
	top := h.newTask("top", "orchestrator", 0, "--pane", "w1:p1")
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	h.launch(w)
	h.setAgents("w9:p1/working/1")

	// Herdr still shows tokens a previous daemon wrote: the lane's ask was
	// answered while down, and a closed task's pane kept its token.
	closed := h.newTask("impl-b", "implementer", top, "--pane", "w9:p2")
	h.ok(nil, "close", id(closed))
	ownerAskPanes(h, map[string]string{"w1:p1": "", "w9:p1": "1", "w9:p2": "2"})
	d.pass()
	wantCalls(t, writes(), clearA, clearB)

	// The listed state now matches; no list or write until the want changes.
	lists := len(h.calls("pane|list|"))
	d.pass()
	if n := len(h.calls("pane|list|")); n != lists || len(writes()) != 2 {
		t.Fatalf("unchanged pass listed %d times, writes %q", n-lists, writes())
	}
}

func TestDaemonOwnerAskTokenRetry(t *testing.T) {
	h, d, writes := ownerAskHarness(t)
	top := h.newTask("top", "orchestrator", 0, "--pane", "w1:p1")
	ownerAskPanes(h, map[string]string{"w1:p1": ""})
	h.ok(nil, "ask", "budget?", "--owner", "--as", id(top))

	// A failed list waits for a changed want or the fallback tick.
	h.write("panes.exit", "1", 0o644)
	d.pass()
	d.pass()
	if n := len(h.calls("pane|list|")); n != 1 {
		t.Fatalf("pane lists after a failed list = %d, want 1", n)
	}
	h.write("panes.exit", "0", 0o644)
	d.fallbackTick()

	// A failed write is retried on the next pass.
	h.write("meta.exit", "1", 0o644)
	d.pass()
	h.write("meta.exit", "0", 0o644)
	d.pass()
	d.pass()
	wantCalls(t, writes(), setR, setR)
}

func TestDaemonOwnerAskTokenUnlistedPane(t *testing.T) {
	h, d, writes := ownerAskHarness(t)

	// A launchless root registered before its pane exists: the first list is empty.
	top := h.newTask("top", "orchestrator", 0, "--pane", "w1:p1")
	h.ok(nil, "ask", "budget?", "--owner", "--as", id(top))
	ownerAskPanes(h, map[string]string{})
	d.pass()
	wantCalls(t, writes())

	// The pane appears without the token; same ask count, then the fallback tick.
	ownerAskPanes(h, map[string]string{"w1:p1": ""})
	d.fallbackTick()
	d.pass()
	ownerAskPanes(h, map[string]string{"w1:p1": "1"})
	d.pass()
	d.fallbackTick()
	d.pass()
	wantCalls(t, writes(), setR)
}

func TestDaemonOwnerAskTokenStaleUnlistedPane(t *testing.T) {
	h, d, writes := ownerAskHarness(t)
	h.newTask("top", "orchestrator", 0, "--pane", "w1:p1")

	// A stale-token pane is absent at startup, then listed: the fallback tick clears it.
	ownerAskPanes(h, map[string]string{})
	d.pass()
	ownerAskPanes(h, map[string]string{"w9:p1": "1"})
	d.pass()
	wantCalls(t, writes())
	d.fallbackTick()
	d.pass()
	ownerAskPanes(h, map[string]string{"w9:p1": ""})
	d.pass()
	wantCalls(t, writes(), clearA)
}
