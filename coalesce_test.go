package main

import (
	"database/sql"
	"fmt"
	"strings"
	"testing"
)

func insertCoalesceEvent(t *testing.T, db *sql.DB, e event) int64 {
	t.Helper()
	var id int64
	if err := withTx(db, func(tx *sql.Tx) error {
		var err error
		id, err = insertEvent(tx, e)
		return err
	}); err != nil {
		t.Fatal(err)
	}
	return id
}

func TestCoalescePlainHintRules(t *testing.T) {
	tests := []struct {
		name      string
		coalesced bool
		setup     func(*testing.T, *harness, *sql.DB, int64, int64, int64) (want int64)
	}{
		{name: "newer hint on same launch", coalesced: true, setup: func(t *testing.T, h *harness, db *sql.DB, top, w, l int64) int64 {
			insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
			return insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
		}},
		{name: "newer ready", coalesced: true, setup: func(t *testing.T, h *harness, db *sql.DB, top, w, l int64) int64 {
			insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
			return num(h.ok(as(w, l), "ready", "ready"), "event_id")
		}},
		{name: "newer done", coalesced: true, setup: func(t *testing.T, h *harness, db *sql.DB, top, w, l int64) int64 {
			insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
			return num(h.ok(as(w, l), "done", "done"), "event_id")
		}},
		{name: "newer fail", coalesced: true, setup: func(t *testing.T, h *harness, db *sql.DB, top, w, l int64) int64 {
			insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
			return num(h.ok(as(w, l), "fail", "fail"), "event_id")
		}},
		{name: "newer ask", coalesced: true, setup: func(t *testing.T, h *harness, db *sql.DB, top, w, l int64) int64 {
			insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
			return num(h.ok(as(w, l), "ask", "ask"), "event_id")
		}},
		{name: "launch replaced", coalesced: true, setup: func(t *testing.T, h *harness, db *sql.DB, top, w, l int64) int64 {
			insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
			h.launch(w)
			return 0
		}},
		{name: "task closed", coalesced: true, setup: func(t *testing.T, h *harness, db *sql.DB, top, w, l int64) int64 {
			insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
			h.ok(nil, "close", id(w))
			return 0
		}},
		{name: "no newer event", setup: func(t *testing.T, h *harness, db *sql.DB, top, w, l int64) int64 {
			return insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
		}},
		{name: "newer report from another launch", setup: func(t *testing.T, h *harness, db *sql.DB, top, w, l int64) int64 {
			hint := insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
			other := h.newTask("other", "implementer", top)
			otherLaunch := h.launch(other)
			h.ok(as(other, otherLaunch), "ready", "other")
			return hint
		}},
		{name: "newer hint from another launch", setup: func(t *testing.T, h *harness, db *sql.DB, top, w, l int64) int64 {
			hint := insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
			other := h.newTask("other", "implementer", top)
			otherLaunch := h.launch(other)
			insertCoalesceEvent(t, db, event{TaskID: other, RecipientTaskID: ptr(top), LaunchID: ptr(otherLaunch), Kind: "herdr"})
			return hint
		}},
		{name: "quota is not a plain hint", setup: func(t *testing.T, h *harness, db *sql.DB, top, w, l int64) int64 {
			hint := insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
			insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr", Data: map[string]any{"quota": "limit"}})
			return hint
		}},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			h, top, w, l := hintFixture(t)
			want := tt.setup(t, h, h.openDB(), top, w, l)
			code, raw, diag := h.compact(nil, "wait", "--as", id(top), "--for", "herdr,ready,done,fail,ask", "--timeout", "0")
			if diag != "" {
				t.Fatalf("wait diagnostic = %q", diag)
			}
			if want == 0 {
				if code != exitOK || !strings.Contains(raw, "sk1 ") || !strings.HasSuffix(raw, "x1 3 timeout\n") {
					t.Fatalf("stale hint = %d %q", code, raw)
				}
				return
			}
			got := strings.Split(strings.TrimSpace(raw), "\t")
			prefix := fmt.Sprintf("e1\t%d\t", want)
			if tt.coalesced {
				prefix = "sk1 1\n" + prefix
			}
			if code != exitOK || !strings.HasPrefix(raw, prefix) || len(got) != 9 || got[1] != id(want) || got[2] != id(w) {
				t.Fatalf("wait = %d %q, want event %d after one skip", code, raw, want)
			}
		})
	}
}

func TestCoalesceUnfilteredWaitKeepsOldestEnvelope(t *testing.T) {
	h, top, w, l := hintFixture(t)
	db := h.openDB()
	first := insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
	insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l), Kind: "herdr"})
	code, raw, diag := h.compact(nil, "wait", "--as", id(top), "--timeout", "0")
	want := fmt.Sprintf("e1\t%d\t%d\t%d\th\t0\t-\t\"\"\t{}\n", first, w, l)
	if code != exitOK || raw != want || diag != "" {
		t.Fatalf("unfiltered wait = %d %q, want %q", code, raw, want)
	}
}

func TestCoalesceResolvedReceiptPair(t *testing.T) {
	h, top, w, l := hintFixture(t)
	db := h.openDB()
	attempt := insertCoalesceEvent(t, db, event{TaskID: w, LaunchID: ptr(l), Kind: "prompt"})
	noReceipt := insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l),
		Kind: "prompt_outcome", Summary: "no_receipt", RelatedEventID: ptr(attempt), Data: map[string]any{"outcome": "no_receipt"}})
	insertCoalesceEvent(t, db, event{TaskID: w, LaunchID: ptr(l), Kind: "got", RelatedEventID: ptr(attempt)})
	lateReceipt := insertCoalesceEvent(t, db, event{TaskID: w, RecipientTaskID: ptr(top), LaunchID: ptr(l),
		Kind: "prompt_outcome", Summary: "late_receipt", RelatedEventID: ptr(attempt), Data: map[string]any{"outcome": "late_receipt", "no_receipt_event_id": noReceipt}})
	code, raw, diag := h.compact(nil, "wait", "--as", id(top), "--for", "prompt_outcome", "--timeout", "0")
	var acked int64
	if err := db.QueryRow(`select acked_event_id from tasks where id = ?`, top).Scan(&acked); err != nil {
		t.Fatal(err)
	}
	var pairRows int
	if err := db.QueryRow(`select count(*) from meta where key = ?`, noReceiptCoalescedKey(noReceipt)).Scan(&pairRows); err != nil {
		t.Fatal(err)
	}
	if code != exitOK || diag != "" || !strings.HasPrefix(raw, "sk1 2\nw1 ") || !strings.HasSuffix(raw, "x1 3 timeout\n") ||
		acked != lateReceipt || acked == noReceipt || pairRows != 0 {
		t.Fatalf("resolved pair wait = %d %q acked=%d pair rows=%d", code, raw, acked, pairRows)
	}
}

func TestCoalesceLateReceiptAfterDeliveredNoReceipt(t *testing.T) {
	h, top, w, l := hintFixture(t)
	attempt := num(h.ok(nil, "prompt", id(w), "--text", "Go."), "attempt_id")
	overdue(h, attempt)
	if err := expireReceiptsNow(h.openDB()); err != nil {
		t.Fatal(err)
	}
	var noReceipt int64
	if err := h.openDB().QueryRow(`select id from events where event_key = ?`, noReceiptKeyPrefix+id(attempt)).Scan(&noReceipt); err != nil {
		t.Fatal(err)
	}
	got := h.ok(nil, "wait", "--as", id(top), "--for", "prompt_outcome", "--timeout", "0")
	if num(eventOf(got), "id") != noReceipt {
		t.Fatalf("no_receipt offer = %v, want %d", got, noReceipt)
	}
	h.ok(nil, "ack", id(noReceipt), "--as", id(top))
	h.ok(as(w, l), "got", id(attempt))
	_, lines := h.run(nil, "wait", "--as", id(top), "--for", "prompt_outcome", "--timeout", "0")
	var ev map[string]any
	for _, line := range lines {
		if event := eventOf(line); event != nil {
			ev = event
		}
	}
	if ev["summary"] != "late_receipt" || num(ev, "related_event_id") != attempt {
		t.Fatalf("late_receipt after delivered no_receipt = %v", lines)
	}
}

func TestCoalesceLateReceiptAfterConfirmNoReceipt(t *testing.T) {
	h, top, w, l := hintFixture(t)
	c := h.one(exitHerdr, nil, "prompt", id(w), "--text", "Go.", "--confirm", "--confirm-timeout", "300")
	attempt := num(c, "attempt_id")
	h.ok(as(w, l), "got", id(attempt))
	_, lines := h.run(nil, "wait", "--as", id(top), "--for", "prompt_outcome", "--timeout", "0")
	var ev map[string]any
	for _, line := range lines {
		if event := eventOf(line); event != nil {
			ev = event
		}
	}
	if ev["summary"] != "late_receipt" || num(ev, "related_event_id") != attempt {
		t.Fatalf("late_receipt after --confirm no_receipt = %v", lines)
	}
}
