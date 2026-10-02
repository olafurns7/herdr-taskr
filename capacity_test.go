package main

import (
	"bufio"
	"bytes"
	"database/sql"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"
)

const nativeCapacity = "■ Selected model is at capacity. Please try a different model."
const capacityFooter = "\n\n› Summarize recent commits\n\n  ? for shortcuts                                      98% context left\n"

// These are presentation fixtures, not provider failures or live runtime proof.
func TestCapacityTail(t *testing.T) {
	for _, suggestion := range []string{"Summarize recent commits", "Write tests for @filename", "Explain this codebase", "Implement {feature}", "Find and fix a bug in @filename", "Improve documentation in @filename"} {
		text := nativeCapacity + "\n\n› " + suggestion + "\n\n  ? for shortcuts\n"
		if capacityOf(text).signal != capacityWarning {
			t.Fatalf("known footer %q rejected", suggestion)
		}
	}
	for _, tc := range []struct {
		name, text string
		want       capacitySignal
	}{
		{"current", nativeCapacity + capacityFooter, capacityWarning},
		{"bare", nativeCapacity, capacityWarning},
		{"empty composer", nativeCapacity + "\n\n›\n", capacityWarning},
		{"footer only", nativeCapacity + "\n\n  ? for shortcuts    98% context left", capacityWarning},
		{"working chrome", nativeCapacity + "\n\n• Working (2s • esc to interrupt)" + capacityFooter, capacityWarning},
		{"ansi", "\x1b[31m" + nativeCapacity + "\x1b[0m\x1b]8;;https://example.test\x1b\\\x1b]8;;\x07" + capacityFooter, capacityWarning},
		{"wrapped", "■ Selected model is at capacity. Please try a\n  different model." + capacityFooter, capacityWarning},
		{"split cells", "■ Selected model is at capacity.\n• Please try a different model.", capacityUnknown},
		{"wrong punctuation", strings.TrimSuffix(nativeCapacity, "."), capacityUnknown},
		{"wrong case", strings.Replace(nativeCapacity, "Selected", "selected", 1), capacityUnknown},
		{"inline", "• The error was " + nativeCapacity, capacityUnknown},
		{"user quote", "› " + nativeCapacity, capacityUnknown},
		{"tool quote", "  " + nativeCapacity, capacityUnknown},
		{"blockquote", "> " + nativeCapacity, capacityUnknown},
		{"fence", "• Example:\n```text\n" + nativeCapacity + "\n```", capacityUnknown},
		{"tilde fence", "~~~\n" + nativeCapacity + "\n~~~", capacityUnknown},
		{"unknown suffix", nativeCapacity + "\n\nUnexpected UI message", capacityUnknown},
		{"submitted user", nativeCapacity + "\n\n› continue", capacityActivity},
		{"submitted suggestion", nativeCapacity + "\n\n› Summarize recent commits\n\n• New output" + capacityFooter, capacityActivity},
		{"tool activity", nativeCapacity + "\n\n• Ran a command\n  output" + capacityFooter, capacityActivity},
		{"social-login recovered", nativeCapacity + "\n\n› continue\n\n• New tool output\n\n• Working (3s • esc to interrupt)" + capacityFooter, capacityActivity},
		{"old warning", nativeCapacity + "\n\n› continue\n\n• Finished." + capacityFooter, capacityActivity},
		{"absent", "• Output without a visible warning" + capacityFooter, capacityUnknown},
		{"clipped warning", "  different model." + capacityFooter, capacityUnknown},
		{"draft ambiguity", nativeCapacity + "\n\n› arbitrary unsent draft\n\n  ? for shortcuts    98% context left", capacityUnknown},
		{"later current warning", nativeCapacity + "\n\n› continue\n\n" + nativeCapacity + capacityFooter, capacityWarning},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := capacityOf(tc.text).signal; got != tc.want {
				t.Fatalf("signal = %v, want %v", got, tc.want)
			}
		})
	}
}

// Reuse the existing fake Unix socket and ledger harness. Each request is a
// direct socket message; the fake CLI is only used by existing prompt transport.
func capacitySocket(t *testing.T, reply func(map[string]any) []byte) *fakeSocket {
	t.Helper()
	s := newFakeSocket(t)
	done := make(chan struct{})
	t.Cleanup(func() { close(done) })
	go func() {
		for {
			select {
			case c := <-s.conns:
				go func(c net.Conn) {
					defer c.Close()
					c.SetDeadline(time.Now().Add(2 * time.Second))
					line, err := bufio.NewReader(c).ReadBytes('\n')
					if err != nil {
						return
					} // existing serverUp probes
					var req map[string]any
					if json.Unmarshal(line, &req) != nil {
						return
					}
					if b := reply(req); b != nil {
						c.Write(b)
					}
				}(c)
			case <-done:
				return
			}
		}
	}()
	return s
}

type capacityScreen struct {
	mu                                                                            sync.Mutex
	text, session, sessionKind, sessionProvider, name, provider, terminal, status string
	revision                                                                      int64
	reads, gets                                                                   int
	truncated                                                                     bool
	failRead                                                                      bool
	afterRead                                                                     func()
}

func newCapacityScreen() *capacityScreen {
	return &capacityScreen{text: nativeCapacity + capacityFooter, session: "thread-one", sessionKind: "id", name: "impl-capacity",
		provider: "codex", terminal: "terminal-one", status: "working", revision: 10}
}

func (p *capacityScreen) reply(req map[string]any) []byte {
	p.mu.Lock()
	defer p.mu.Unlock()
	params, _ := req["params"].(map[string]any)
	var result any
	switch req["method"] {
	case "agent.get":
		p.gets++
		result = map[string]any{"agent": map[string]any{"agent": p.provider, "name": p.name, "pane_id": params["target"],
			"terminal_id": p.terminal, "agent_status": p.status, "revision": p.revision,
			"agent_session": map[string]any{"agent": firstNonEmpty(p.sessionProvider, p.provider), "kind": p.sessionKind, "value": p.session, "source": "reporter"}}}
	case "agent.read":
		p.reads++
		if p.failRead {
			return nil
		}
		if params["source"] != "detection" || params["format"] != "text" || params["strip_ansi"] != true {
			return nil
		}
		result = map[string]any{"read": map[string]any{"pane_id": params["target"], "source": "detection", "format": "text",
			"revision": p.revision, "text": p.text, "truncated": p.truncated}}
		if p.afterRead != nil {
			p.afterRead()
		}
	default:
		return nil
	}
	b, _ := json.Marshal(map[string]any{"id": req["id"], "result": result})
	return append(b, '\n')
}

func capacityFixture(t *testing.T) (*harness, *sql.DB, int64, int64, int64, *capacityScreen) {
	t.Helper()
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-capacity", "implementer", top, "--pane", "w9:p1")
	l := num(h.ok(nil, "launch", id(w), "--provider", "codex", "--model", "gpt-6.1-sol", "--effort", "high"), "launch_id")
	db := h.openDB()
	if _, err := db.Exec(`update launches set session_kind = 'id', session_ref = 'thread-one' where id = ?`, l); err != nil {
		t.Fatal(err)
	}
	p := newCapacityScreen()
	h.herdrSock = capacitySocket(t, p.reply).path
	return h, db, top, w, l, p
}

func scanCapacityTest(t *testing.T, h *harness, db *sql.DB, top int64) {
	t.Helper()
	if err := scanChildrenCapacity(db, h.herdrSock, top, time.Now().Add(2*time.Second)); err != nil {
		t.Fatal(err)
	}
}

func TestCapacityCurrentWarningStates(t *testing.T) {
	for _, status := range []string{"idle", "unknown", "working"} {
		for variant, text := range map[string]string{"plain": nativeCapacity + capacityFooter,
			"wrapped": "■ Selected model is at capacity. Please try a\n  different model." + capacityFooter,
			"ansi":    "\x1b[31m" + nativeCapacity + "\x1b[0m" + capacityFooter} {
			t.Run(status+"/"+variant, func(t *testing.T) {
				h, db, top, _, _, p := capacityFixture(t)
				p.status, p.text = status, text
				scanCapacityTest(t, h, db, top)
				p.mu.Lock()
				reads, gets := p.reads, p.gets
				p.mu.Unlock()
				if h.countHerdr(top) != 1 || reads != 1 || gets != 2 {
					t.Fatalf("positive scan: events=%d reads=%d gets=%d", h.countHerdr(top), reads, gets)
				}
			})
		}
	}
}

func TestCapacityRecoveredHistory(t *testing.T) {
	h, db, top, _, _, p := capacityFixture(t)
	p.text = nativeCapacity + "\n\n› continue\n\n• New tool output\n\n• Working (3s • esc to interrupt)" + capacityFooter
	scanCapacityTest(t, h, db, top)
	p.mu.Lock()
	reads := p.reads
	p.mu.Unlock()
	if h.countHerdr(top) != 0 || reads != 1 {
		t.Fatalf("recovered warning alerted: events=%d reads=%d", h.countHerdr(top), reads)
	}
}

func TestCapacityEpisodesAndPrivacy(t *testing.T) {
	h, db, top, w, l, p := capacityFixture(t)
	const secret = "SECRET-CAPACITY-PANE-61f8"
	p.text = "› First taskr got 42. " + secret + "\n\n" + nativeCapacity + capacityFooter
	var first sync.WaitGroup
	for range 2 {
		first.Add(1)
		go func() {
			defer first.Done()
			if err := scanChildrenCapacity(db, h.herdrSock, top, time.Now().Add(2*time.Second)); err != nil {
				t.Error(err)
			}
		}()
	}
	first.Wait()
	if n := h.countHerdr(top); n != 1 {
		t.Fatalf("concurrent first warning: %d events", n)
	}
	for _, status := range []string{"idle", "unknown", "working"} {
		p.mu.Lock()
		p.status = status
		p.revision++
		p.mu.Unlock()
		scanCapacityTest(t, h, db, top)
	}
	// Concurrent readers and a fresh DB connection share the same durable latch.
	var wg sync.WaitGroup
	for range 2 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if err := scanChildrenCapacity(db, h.herdrSock, top, time.Now().Add(2*time.Second)); err != nil {
				t.Error(err)
			}
		}()
	}
	wg.Wait()
	scanCapacityTest(t, h, h.openDB(), top)
	if n := h.countHerdr(top); n != 1 {
		t.Fatalf("dedupe: %d events", n)
	}
	p.mu.Lock()
	p.text = ""
	p.revision++
	p.mu.Unlock()
	scanCapacityTest(t, h, db, top) // clipped/disappeared is not recovery
	p.mu.Lock()
	p.failRead = true
	p.revision++
	p.mu.Unlock()
	scanCapacityTest(t, h, db, top) // unreadable is not recovery either
	p.mu.Lock()
	p.failRead = false
	p.mu.Unlock()
	p.mu.Lock()
	p.text = nativeCapacity + capacityFooter
	p.revision++
	p.mu.Unlock()
	scanCapacityTest(t, h, db, top)
	if n := h.countHerdr(top); n != 1 {
		t.Fatalf("blank rearmed: %d", n)
	}
	p.mu.Lock()
	p.text = nativeCapacity + "\n\n› continue\n\n• New tool output\n\n• Working (3s • esc to interrupt)" + capacityFooter
	p.revision++
	p.mu.Unlock()
	scanCapacityTest(t, h, db, top)
	p.mu.Lock()
	p.text = nativeCapacity + capacityFooter
	p.revision++
	p.mu.Unlock()
	scanCapacityTest(t, h, db, top)
	if n := h.countHerdr(top); n != 2 {
		t.Fatalf("recurrence: %d", n)
	}
	// Older recovered snapshots cannot clear the newer warning.
	p.mu.Lock()
	p.text = nativeCapacity + "\n\n› continue"
	p.revision = 1
	p.mu.Unlock()
	scanCapacityTest(t, h, db, top)
	p.mu.Lock()
	p.text = nativeCapacity + capacityFooter
	p.revision = 100
	p.mu.Unlock()
	scanCapacityTest(t, h, db, top)
	if n := h.countHerdr(top); n != 2 {
		t.Fatalf("older snapshot rearmed: %d", n)
	}
	for episode := int64(1); episode <= 2; episode++ {
		got := h.ok(nil, "wait", "--as", id(top), "--timeout", "0")
		ev := eventOf(got)
		want := map[string]any{"reason": "model_capacity", "provider": "codex", "model": "gpt-6.1-sol", "model_source": "launch",
			"pane_id": "w9:p1", "agent_name": "impl-capacity", "episode": float64(episode), "source": "detection", "action": "inspect_before_retry"}
		if ev["event_key"] != fmt.Sprintf("capacity:%d:%d", l, episode) || num(ev, "task_id") != w || num(ev, "launch_id") != l || !reflect.DeepEqual(ev["data"], want) {
			t.Fatalf("event = %v", ev)
		}
		h.ok(nil, "ack", id(num(ev, "id")), "--as", id(top))
	}
	var status string
	db.QueryRow(`select status from tasks where id = ?`, w).Scan(&status)
	if status != "open" {
		t.Fatalf("capacity changed status to %s", status)
	}
	files, _ := filepath.Glob(filepath.Join(filepath.Dir(h.db), "*"))
	for _, f := range files {
		if b, _ := os.ReadFile(f); bytes.Contains(b, []byte(secret)) {
			t.Fatalf("pane text stored in %s", f)
		}
	}
	if h.lastStderr() != "" || len(h.calls("")) > 1 {
		t.Fatal("capacity scan invoked CLI or leaked diagnostics")
	}
}

func TestCapacityCandidateRaces(t *testing.T) {
	for _, scenario := range []string{"native session", "terminal", "name", "provider", "current launch", "parent", "pane", "waiting", "closed", "prompt attempt", "binding deleted"} {
		t.Run(scenario, func(t *testing.T) {
			h, db, top, w, l, p := capacityFixture(t)
			bindCapacityBeforePrompt(db, h.herdrSock, w, l)
			p.afterRead = func() {
				switch scenario {
				case "native session":
					p.session = "replacement"
				case "terminal":
					p.terminal = "replacement"
				case "name":
					p.name = "replacement"
				case "provider":
					p.provider = "claude"
				case "current launch":
					db.Exec(`update tasks set current_launch_id = null where id = ?`, w)
				case "parent":
					db.Exec(`update tasks set parent_id = null where id = ?`, w)
				case "pane":
					db.Exec(`update tasks set pane_id = 'w9:p8' where id = ?`, w)
				case "waiting":
					db.Exec(`update tasks set waiting_until = ? where id = ?`, stamp(time.Now().Add(time.Minute)), w)
				case "closed":
					db.Exec(`update tasks set status = 'closed' where id = ?`, w)
				case "prompt attempt":
					withTx(db, func(tx *sql.Tx) error {
						_, err := insertEvent(tx, event{TaskID: w, LaunchID: &l, Kind: "prompt"})
						return err
					})
				case "binding deleted":
					db.Exec(`delete from meta where key = ?`, fmt.Sprintf("capacity:%d", l))
				}
			}
			scanCapacityTest(t, h, db, top)
			if n := h.countHerdr(top); n != 0 {
				t.Fatalf("raced candidate emitted %d", n)
			}
		})
	}
}

func TestCapacityIdentityAndLaunchFences(t *testing.T) {
	for _, scenario := range []string{"session", "terminal", "provider", "name", "read race", "relaunch", "parent", "pane", "waiting", "status", "session record"} {
		t.Run(scenario, func(t *testing.T) {
			h, db, top, w, l, p := capacityFixture(t)
			scanCapacityTest(t, h, db, top) // bind and latch old occupant
			// Clear only the event for this test; retain its binding.
			db.Exec(`delete from events where kind = 'herdr'`)
			p.revision++
			switch scenario {
			case "session":
				p.session = "replacement"
			case "terminal":
				p.terminal = "replacement"
			case "provider":
				p.provider = "claude"
			case "name":
				p.name = "someone-else"
			case "read race":
				p.afterRead = func() { p.session = "replacement" }
			case "relaunch":
				p.afterRead = func() { db.Exec(`update tasks set current_launch_id = null where id = ?`, w) }
			case "parent":
				p.afterRead = func() { db.Exec(`update tasks set parent_id = null where id = ?`, w) }
			case "pane":
				p.afterRead = func() { db.Exec(`update tasks set pane_id = 'w9:p8' where id = ?`, w) }
			case "waiting":
				p.afterRead = func() {
					db.Exec(`update tasks set waiting_until = ? where id = ?`, stamp(time.Now().Add(time.Minute)), w)
				}
			case "status":
				p.afterRead = func() { db.Exec(`update tasks set status = 'ready' where id = ?`, w) }
			case "session record":
				p.afterRead = func() { db.Exec(`update launches set session_ref = 'replacement' where id = ?`, l) }
			}
			// If the fence failed, this positive activity would rearm the latch.
			p.text = nativeCapacity + "\n\n› continue"
			scanCapacityTest(t, h, db, top)
			p.mu.Lock()
			p.afterRead = nil
			p.session = "thread-one"
			p.terminal = "terminal-one"
			p.provider = "codex"
			p.name = "impl-capacity"
			p.text = nativeCapacity + capacityFooter
			p.revision++
			p.mu.Unlock()
			scanCapacityTest(t, h, db, top)
			if n := h.countHerdr(top); n != 0 {
				t.Fatalf("identity change rearmed/emitted %d", n)
			}
		})
	}
	// A genuinely registered new launch has its own namespace.
	h, db, top, w, _, _ := capacityFixture(t)
	scanCapacityTest(t, h, db, top)
	l2 := num(h.ok(nil, "launch", id(w), "--provider", "codex", "--model", "gpt-6-luna", "--effort", "max"), "launch_id")
	db.Exec(`update launches set session_kind = 'id', session_ref = 'thread-one' where id = ?`, l2)
	scanCapacityTest(t, h, db, top)
	if n := h.countHerdr(top); n != 2 {
		t.Fatalf("new launch events = %d", n)
	}
}

func TestCapacityBootstrapAndPromptBinding(t *testing.T) {
	for _, scenario := range []string{"recorded", "alias", "parent-bound", "missing", "latest marker", "older marker", "old launch marker", "quoted marker", "wrong session", "alias changed value", "unknown kind", "live unknown kind", "session provider", "empty value"} {
		t.Run(scenario, func(t *testing.T) {
			h, db, top, w, l, p := capacityFixture(t)
			want := 1
			if scenario != "recorded" && scenario != "alias" && scenario != "alias changed value" && scenario != "unknown kind" && scenario != "live unknown kind" && scenario != "empty value" {
				db.Exec(`update launches set session_ref = null, session_kind = null where id = ?`, l)
			}
			var attempt int64
			if scenario == "parent-bound" {
				h.ok(nil, "prompt", id(w), "--text", "Do the slice.") // no worker got/start
				p.mu.Lock()
				p.revision++
				p.mu.Unlock()
			} else {
				if err := withTx(db, func(tx *sql.Tx) error {
					var err error
					attempt, err = insertEvent(tx, event{TaskID: w, LaunchID: &l, Kind: "prompt"})
					return err
				}); err != nil {
					t.Fatal(err)
				}
			}
			switch scenario {
			case "alias":
				db.Exec(`update launches set session_kind = 'thread_id' where id = ?`, l)
			case "missing":
				want = 0
			case "latest marker":
				p.text = fmt.Sprintf("› First taskr got %d; read /brief; execute exactly.\n\n%s%s", attempt, nativeCapacity, capacityFooter)
			case "older marker":
				p.text = fmt.Sprintf("› First taskr got %d. Old prompt.\n\n%s%s", attempt-1, nativeCapacity, capacityFooter)
				want = 0
			case "quoted marker":
				p.text = fmt.Sprintf("• Quoted: First taskr got %d.\n\n%s%s", attempt, nativeCapacity, capacityFooter)
				want = 0
			case "wrong session":
				db.Exec(`update launches set session_ref = 'other', session_kind = 'id' where id = ?`, l)
				want = 0
			case "alias changed value":
				db.Exec(`update launches set session_ref = 'other', session_kind = 'thread_id' where id = ?`, l)
				want = 0
			case "unknown kind":
				db.Exec(`update launches set session_kind = 'session_id' where id = ?`, l)
				want = 0
			case "live unknown kind":
				p.sessionKind = "session_id"
				want = 0
			case "session provider":
				p.sessionProvider = "claude"
				want = 0
			case "empty value":
				p.session = ""
				want = 0
			case "old launch marker":
				oldLaunch := l
				l = num(h.ok(nil, "launch", id(w), "--provider", "codex", "--model", "gpt-6-luna", "--effort", "max"), "launch_id")
				p.text = fmt.Sprintf("› First taskr got %d. Old launch %d.\n\n%s%s", attempt, oldLaunch, nativeCapacity, capacityFooter)
				want = 0
			}
			scanCapacityTest(t, h, db, top)
			if n := h.countHerdr(top); n != want {
				t.Fatalf("events = %d, want %d", n, want)
			}
		})
	}
}

func TestCapacityWaitDefaultAndHeartbeat(t *testing.T) {
	h, db, top, w, l, p := capacityFixture(t)
	setMeta(db, heartbeatKey, now()) // old daemon knows nothing about capacity
	// A pending ordinary event must not starve the default scan.
	var ready int64
	if err := withTx(db, func(tx *sql.Tx) error {
		var err error
		ready, err = insertEvent(tx, event{TaskID: w, RecipientTaskID: &top, LaunchID: &l, Kind: "ready"})
		return err
	}); err != nil {
		t.Fatal(err)
	}
	got := h.ok(nil, "wait", "--as", id(top), "--for", "ready", "--timeout", "1000")
	if num(eventOf(got), "id") != ready || h.countHerdr(top) != 1 {
		t.Fatalf("backlog scan: %v", got)
	}
	p.mu.Lock()
	reads := p.reads
	p.mu.Unlock()
	h.ok(nil, "wait", "--as", id(top), "--for", "ready", "--timeout", "1000")
	p.mu.Lock()
	after := p.reads
	p.mu.Unlock()
	if reads != 1 || after != reads {
		t.Fatalf("throttle reads %d -> %d", reads, after)
	}
	h.ok(nil, "ack", id(ready), "--as", id(top))
	got = h.ok(nil, "wait", "--as", id(top), "--for", "ready,ask,answer,done,fail", "--from", id(top), "--timeout", "1000")
	if eventOf(got)["data"].(map[string]any)["reason"] != "model_capacity" {
		t.Fatalf("default observation = %v", got)
	}
	// Stale and absent heartbeats still use the independent scan claim.
	for _, heartbeat := range []string{stamp(time.Now().Add(-time.Minute)), ""} {
		setMeta(db, heartbeatKey, heartbeat)
		db.Exec(`delete from meta where key = ?`, fmt.Sprintf("capacity_poll:%d", top))
		h.ok(nil, "wait", "--as", id(top), "--for", "ready", "--timeout", "1000")
	}
	p.mu.Lock()
	after = p.reads
	p.mu.Unlock()
	if after != reads+2 || len(h.calls("agent|")) != 0 {
		t.Fatalf("heartbeat reads = %d; CLI = %v", after, h.calls("agent|"))
	}
}

func TestCapacityWaitSkipsIneligible(t *testing.T) {
	for _, scenario := range []string{"timeout0", "gate", "ready", "done", "failed", "planned", "closed", "waiting", "no pane", "no launch", "other provider", "worker inbox", "no children"} {
		t.Run(scenario, func(t *testing.T) {
			h, db, top, w, _, p := capacityFixture(t)
			setMeta(db, heartbeatKey, now())
			timeout := "50"
			switch scenario {
			case "timeout0":
				timeout = "0"
			case "gate":
				db.Exec(`update tasks set role = 'gate' where id = ?`, w)
			case "ready", "done", "failed", "planned", "closed":
				db.Exec(`update tasks set status = ? where id = ?`, scenario, w)
			case "waiting":
				db.Exec(`update tasks set waiting_until = ? where id = ?`, stamp(time.Now().Add(time.Minute)), w)
			case "no pane":
				db.Exec(`update tasks set pane_id = null where id = ?`, w)
				db.Exec(`update launches set pane_id = null`)
			case "no launch":
				db.Exec(`update tasks set current_launch_id = null where id = ?`, w)
			case "other provider":
				db.Exec(`update launches set provider = 'claude'`)
			case "worker inbox":
				top = w
			case "no children":
				db.Exec(`update tasks set parent_id = null where id = ?`, w)
			}
			h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", timeout)
			p.mu.Lock()
			reads, gets := p.reads, p.gets
			p.mu.Unlock()
			if reads != 0 || gets != 0 || h.countHerdr(top) != 0 {
				t.Fatalf("ineligible scan: reads=%d gets=%d", reads, gets)
			}
		})
	}
}

func TestCapacitySocketFailureBounds(t *testing.T) {
	h := newHarness(t)
	for _, executable := range []string{"herdr", "ssh", "sh"} {
		h.write(executable, "#!/bin/sh\necho called >> \""+filepath.Join(h.bin, "forbidden")+"\"\n", 0o755)
	}
	for _, scenario := range []string{"missing", "stale", "disconnect", "malformed", "oversize", "wrong id", "error", "null", "held"} {
		t.Run(scenario, func(t *testing.T) {
			sock := filepath.Join(h.dir, "missing.sock")
			if scenario == "stale" {
				s := newFakeSocket(t)
				s.ln.SetUnlinkOnClose(false)
				s.ln.Close()
				sock = s.path
			} else if scenario != "missing" {
				s := capacitySocket(t, func(req map[string]any) []byte {
					switch scenario {
					case "disconnect":
						return nil
					case "malformed":
						return []byte("SECRET malformed\n")
					case "oversize":
						return []byte(strings.Repeat("x", 256*1024+1) + "\n")
					case "wrong id":
						return []byte(`{"id":"wrong","result":{}}` + "\n")
					case "error":
						return []byte(`{"id":"taskr-capacity","error":{"message":"SECRET"}}` + "\n")
					case "null":
						return []byte(`{"id":"taskr-capacity","result":null}` + "\n")
					case "held":
						time.Sleep(250 * time.Millisecond)
						return nil
					}
					return nil
				})
				sock = s.path
			}
			start := time.Now()
			var result map[string]any
			err := herdrCapacityRequest(sock, "agent.get", map[string]any{"target": "w9:p1"}, start.Add(80*time.Millisecond), &result)
			if err == nil || time.Since(start) > 500*time.Millisecond || strings.Contains(err.Error(), "SECRET") {
				t.Fatalf("unbounded/leaked reply: %v after %v", err, time.Since(start))
			}
		})
	}
	if _, err := os.Stat(filepath.Join(h.bin, "forbidden")); !os.IsNotExist(err) {
		t.Fatal("socket reader executed a process")
	}
}

func TestCapacityUnavailableServer(t *testing.T) {
	for _, scenario := range []string{"missing", "stale", "disconnect"} {
		t.Run(scenario, func(t *testing.T) {
			h, db, top, _, _, _ := capacityFixture(t)
			sock := filepath.Join(h.dir, "missing.sock")
			if scenario == "stale" {
				s := newFakeSocket(t)
				s.ln.SetUnlinkOnClose(false)
				s.ln.Close()
				sock = s.path
			} else if scenario == "disconnect" {
				sock = capacitySocket(t, func(map[string]any) []byte { return nil }).path
			}
			start := time.Now()
			if err := scanChildrenCapacity(db, sock, top, start.Add(80*time.Millisecond)); err != nil {
				t.Fatal(err)
			}
			if time.Since(start) > 500*time.Millisecond || h.countHerdr(top) != 0 || len(h.calls("")) > 1 {
				t.Fatal("unavailable server executed/emitted/overran")
			}
		})
	}
}

func TestCapacityMalformedReadAndWriteFailure(t *testing.T) {
	for _, scenario := range []string{"pane", "source", "format", "revision", "truncated cell", "truncated complete", "read error", "write failure"} {
		t.Run(scenario, func(t *testing.T) {
			h, db, top, _, _, p := capacityFixture(t)
			h.herdrSock = capacitySocket(t, func(req map[string]any) []byte {
				b := p.reply(req)
				if req["method"] != "agent.read" {
					return b
				}
				if scenario == "read error" {
					return nil
				}
				var m map[string]any
				json.Unmarshal(b, &m)
				r := m["result"].(map[string]any)["read"].(map[string]any)
				switch scenario {
				case "pane":
					r["pane_id"] = "w9:p99"
				case "source":
					r["source"] = "recent"
				case "format":
					r["format"] = "ansi"
				case "revision":
					delete(r, "revision")
				case "truncated cell":
					r["truncated"] = true
					r["text"] = "  different model." + capacityFooter
				case "truncated complete":
					r["truncated"] = true
				}
				b, _ = json.Marshal(m)
				return append(b, '\n')
			}).path
			if scenario == "write failure" {
				db.Exec(`create trigger reject_capacity before insert on events when new.kind = 'herdr' begin select raise(abort, 'capacity write rejected'); end`)
			}
			err := scanChildrenCapacity(db, h.herdrSock, top, time.Now().Add(time.Second))
			want := 0
			if scenario == "truncated complete" {
				want = 1
			}
			if (err != nil) != (scenario == "write failure") || h.countHerdr(top) != want {
				t.Fatalf("scan = %v, events = %d", err, h.countHerdr(top))
			}
		})
	}
}

func TestCapacityBudgetCursor(t *testing.T) {
	h, db, top, _, _, p := capacityFixture(t)
	w2 := h.newTask("second", "implementer", top, "--pane", "w9:p2")
	l2 := num(h.ok(nil, "launch", id(w2), "--provider", "codex", "--model", "gpt-6-luna", "--effort", "max"), "launch_id")
	db.Exec(`update launches set session_kind = 'id', session_ref = 'thread-one' where id = ?`, l2)
	h.herdrSock = capacitySocket(t, func(req map[string]any) []byte {
		params := req["params"].(map[string]any)
		if params["target"] == "w9:p1" {
			time.Sleep(200 * time.Millisecond)
			return nil
		}
		p.mu.Lock()
		p.name = "second"
		p.mu.Unlock()
		return p.reply(req)
	}).path
	if err := scanChildrenCapacity(db, h.herdrSock, top, time.Now().Add(40*time.Millisecond)); err != nil {
		t.Fatal(err)
	}
	scanCapacityTest(t, h, db, top)
	if h.countHerdr(top) != 1 {
		t.Fatal("slow first child starved second")
	}
}

func TestCapacityNewCLIProcessDedupe(t *testing.T) {
	h, db, top, _, _, p := capacityFixture(t)
	setMeta(db, heartbeatKey, now())
	scanCapacityTest(t, h, db, top)
	// Existing subprocess harness: fresh address space, same scratch ledger/socket.
	cmd := exec.Command(buildTaskr(t), "--json", "wait", "--as", id(top), "--for", "ready", "--timeout", "1000")
	cmd.Env = []string{"HOME=" + h.dir, "TASKR_DB=" + h.db, "HERDR_SOCKET_PATH=" + h.herdrSock, "PATH=" + os.Getenv("PATH")}
	if out, err := cmd.Output(); err != nil || !bytes.Contains(out, []byte(`"model_capacity"`)) {
		t.Fatalf("fresh process = %s %v", out, err)
	}
	p.mu.Lock()
	p.revision++
	p.mu.Unlock()
	scanCapacityTest(t, h, db, top)
	if h.countHerdr(top) != 1 {
		t.Fatal("fresh process reset latch")
	}
}
