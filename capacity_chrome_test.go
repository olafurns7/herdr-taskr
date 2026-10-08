package main

import (
	"database/sql"
	"encoding/json"
	"testing"
)

const (
	incPrev    = "• Failed (exit 1) command\n  └ command output\n    + output hidden (ctrl+t to expand)\n\n"
	incWrapped = "■ Selected model is at capacity. Please try a\ndifferent model."
	incChrome  = "\n\n\n› Ask Codex to do anything\n\n  feature/example-issue · /workspace/example…\n  ? for shortcuts                       ⚠ 3 · f2\n"
)

func TestCapacityIncidentChrome(t *testing.T) {
	contractGuard(t)
	for _, tc := range []struct{ name, text string }{
		{"F1 real screen", incPrev + incWrapped + incChrome},
		{"F2 real chrome, indented wrap", incPrev + "■ Selected model is at capacity. Please try a\n  different model." + incChrome},
		{"F3 real chrome, one-line sentence", incPrev + nativeCapacity + incChrome},
		{"F4 unindented wrap, fixture chrome", incPrev + incWrapped + capacityFooter},
		{"F5 placeholder, no status row", incPrev + nativeCapacity + "\n\n› Ask Codex to do anything\n\n  ? for shortcuts\n"},
		{"F6 fixture composer, real footer", incPrev + nativeCapacity + "\n\n› Summarize recent commits\n\n  ? for shortcuts                       ⚠ 3 · f2\n"},
		{"F0 existing fixture", incPrev + nativeCapacity + capacityFooter},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := capacityOf(tc.text).signal; got != capacityWarning {
				t.Fatalf("signal = %v, want warning", got)
			}
		})
	}
}

func TestCapacityIncidentChromeRejectsAmbiguity(t *testing.T) {
	contractGuard(t)
	for _, tc := range []struct{ name, text string }{
		{"draft composer", incPrev + nativeCapacity + "\n\n› change the plan\n\n  feature/example-issue · /workspace/example…\n  ? for shortcuts                       ⚠ 3 · f2\n"},
		{"two status rows", incPrev + nativeCapacity + "\n\n› Ask Codex to do anything\n\n  feature/example-issue · /workspace/example…\n  another-branch · /another/path\n  ? for shortcuts                       ⚠ 3 · f2\n"},
		{"extra text in same cell", "■ Selected model is at capacity. Please try a\ndifferent model.\nextra" + incChrome},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := capacityOf(tc.text).signal; got != capacityUnknown {
				t.Fatalf("signal = %v, want unknown", got)
			}
		})
	}
}

func zeroReadRevision(p *capacityScreen) func(map[string]any) []byte {
	return func(req map[string]any) []byte {
		b := p.reply(req)
		if req["method"] != "agent.read" || b == nil {
			return b
		}
		var m map[string]any
		if json.Unmarshal(b, &m) != nil {
			return nil
		}
		m["result"].(map[string]any)["read"].(map[string]any)["revision"] = 0
		out, _ := json.Marshal(m)
		return append(out, '\n')
	}
}

func TestCapacityReadRevisionZero(t *testing.T) {
	contractGuard(t)
	for _, promptBound := range []bool{false, true} {
		name := "recorded session"
		if promptBound {
			name = "prompt-bound"
		}
		t.Run(name, func(t *testing.T) {
			h, db, top, w, l, p := capacityFixture(t)
			h.herdrSock = capacitySocket(t, zeroReadRevision(p)).path
			if promptBound {
				if _, err := db.Exec(`update launches set session_ref = null, session_kind = null where id = ?`, l); err != nil {
					t.Fatal(err)
				}
				h.ok(nil, "prompt", id(w), "--text", "Do the slice.")
				p.mu.Lock()
				p.revision++
				p.mu.Unlock()
			}
			scanCapacityTest(t, h, db, top)
			first := h.countHerdr(top)
			scanCapacityTest(t, h, db, top)
			if first != 1 || h.countHerdr(top) != 1 {
				t.Fatalf("first episode events = %d, then %d; want one", first, h.countHerdr(top))
			}
			p.mu.Lock()
			p.text = nativeCapacity + "\n\n› retry\n\n• New output\n\n• Working (3s • esc to interrupt)" + capacityFooter
			p.revision++
			p.mu.Unlock()
			scanCapacityTest(t, h, db, top)
			p.mu.Lock()
			p.text = nativeCapacity + capacityFooter
			p.revision++
			p.mu.Unlock()
			scanCapacityTest(t, h, db, top)
			scanCapacityTest(t, h, db, top)
			if got := h.countHerdr(top); got != 2 {
				t.Fatalf("two episodes emitted %d events, want 2", got)
			}
		})
	}
}

func TestCapacityIncidentWaitFilter(t *testing.T) {
	contractGuard(t)
	h, db, top, w, l, p := capacityFixture(t)
	p.text = incPrev + incWrapped + incChrome
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
		t.Fatalf("incident scan = %v; capacity events = %d", got, h.countHerdr(top))
	}
	h.ok(nil, "ack", id(ready), "--as", id(top))
	p.mu.Lock()
	reads := p.reads
	p.mu.Unlock()
	got = h.ok(nil, "wait", "--as", id(top), "--for", "ready,ask,answer,done,fail", "--from", id(top), "--timeout", "1000")
	ev := eventOf(got)
	if num(ev, "task_id") != w || ev["data"].(map[string]any)["reason"] != "model_capacity" || h.countHerdr(top) != 1 || reads != 1 {
		t.Fatalf("filtered capacity observation = %v", got)
	}
}
