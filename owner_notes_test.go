package main

import (
	"fmt"
	"reflect"
	"strings"
	"testing"
	"time"
)

func TestOwnerNoteStoredAndWhole(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("campaign", "orchestrator", 0)
	text := strings.Repeat("界", 1650)
	event := num(h.ok(nil, "note", text, "--owner", "--as", id(root)), "event_id")
	code, notes := h.run(nil, "notes", "--owner", "--root", id(root))
	if code != exitOK || len(notes) != 1 || num(notes[0], "id") != event || notes[0]["summary"] != text || notes[0]["data"].(map[string]any)["owner"] != true {
		t.Fatalf("owner notes = %d %v", code, notes)
	}
	m := campaignGET(t, h.dash(), root, 1)
	rows := m["notes"].([]any)
	if len(rows) != 1 || rows[0].(map[string]any)["text"] != text || rows[0].(map[string]any)["owner"] != true {
		t.Fatalf("campaign notes = %v", rows)
	}
}

func TestOwnerNoteRejectsWorkersWithoutWriting(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", root)
	launch := h.launch(lane)
	sub := h.newTask("sub", "sub-orchestrator", root)
	db := h.openDB()
	var before, after int
	if err := db.QueryRow(`select count(*) from events`).Scan(&before); err != nil {
		t.Fatal(err)
	}
	for _, tc := range []struct {
		env map[string]string
		as  int64
	}{
		{as(lane, launch), 0}, {as(sub, 0), 0}, {nil, sub},
		{as(root, 0), root}, {as(root, 0), 0}, {as(lane, launch), root}, {nil, 0},
	} {
		args := []string{"note", "for owner", "--owner"}
		if tc.as != 0 {
			args = append(args, "--as", id(tc.as))
		}
		h.one(exitReject, tc.env, args...)
	}
	if err := db.QueryRow(`select count(*) from events`).Scan(&after); err != nil || before != after {
		t.Fatalf("rejected writes changed events: %d -> %d, %v", before, after, err)
	}
	// Ordinary worker notes are still accepted.
	h.ok(as(lane, launch), "note", "lane progress")
}

func TestOwnerNoteKeyMatchesOwnerFlag(t *testing.T) {
	contractGuard(t)
	for _, storedOwner := range []bool{false, true} {
		t.Run(fmt.Sprint(storedOwner), func(t *testing.T) {
			h := newHarness(t)
			root := h.newTask("root", "orchestrator", 0)
			args := []string{"note", "original", "--key", "note-key", "--as", id(root)}
			if storedOwner {
				args = append(args, "--owner")
			}
			first := num(h.ok(nil, args...), "event_id")
			code, raw, _ := h.compact(nil, args...)
			if code != exitOK || !strings.Contains(raw, "dup") {
				t.Fatalf("same flag duplicate = %d %q", code, raw)
			}
			mismatch := []string{"note", "replacement", "--key", "note-key", "--as", id(root)}
			flag := "without"
			if storedOwner {
				flag = "with"
			} else {
				mismatch = append(mismatch, "--owner")
			}
			got := h.one(exitReject, nil, mismatch...)
			if got["error"] != fmt.Sprintf("event key %q is a note %s --owner", "note-key", flag) {
				t.Fatalf("mismatch error = %v", got)
			}
			db := h.openDB()
			var count int
			var summary string
			if err := db.QueryRow(`select count(*) from events where kind = 'note'`).Scan(&count); err != nil || count != 1 {
				t.Fatalf("note count = %d, %v", count, err)
			}
			if err := db.QueryRow(`select summary from events where id = ?`, first).Scan(&summary); err != nil || summary != "original" {
				t.Fatalf("stored note = %q, %v", summary, err)
			}
		})
	}
	// Ask key semantics remain unchanged when its owner flag changes.
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	args := []string{"ask", "question", "--key", "ask-key", "--as", id(root)}
	first := num(h.ok(nil, args...), "event_id")
	got := h.ok(nil, append(args, "--owner")...)
	if got["duplicate"] != true || num(got, "event_id") != first {
		t.Fatalf("ask duplicate = %v", got)
	}
}

func TestOwnerNoteClientRejectsBeforeSpoolOrRPC(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := "host-a"
	root, lane, launch := spoolMakeWorker(t, r, host, 1)
	home := r.clientHome(spoolDeadURL(t, r))
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	for _, tc := range []struct {
		env map[string]string
		as  int64
	}{
		{as(lane, launch), 0}, {as(lane, launch), root}, {nil, 0},
	} {
		args := []string{"note", "owner text", "--owner"}
		if tc.as != 0 {
			args = append(args, "--as", id(tc.as))
		}
		code, raw, stderr := spoolRunCLI(r, host, home, tc.env, args...)
		if code != exitReject || !strings.Contains(raw, "--owner: only a root orchestrator's own note") {
			t.Fatalf("offline rejection = %d %q %q", code, raw, stderr)
		}
		listing, err := readSpoolListing(spoolStateDir(home))
		if err != nil || len(listing["queued"].([]spoolListItem)) != 0 || len(listing["refused"].([]spoolListItem)) != 0 || len(listing["bad"].([]spoolListItem)) != 0 {
			t.Fatalf("rejected note spool = %v, %v", listing, err)
		}
	}
	var count int
	if err := r.openDB().QueryRow(`select count(*) from events where kind = 'note'`).Scan(&count); err != nil || count != 0 {
		t.Fatalf("rejected note count = %d, %v", count, err)
	}
}

func TestNotesStrictInput(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	var before, after int
	db.QueryRow(`select count(*) from events`).Scan(&before)
	for _, args := range [][]string{
		{"--unknown"}, {"positional"}, {"--owner=invalid"}, {"--root", "0"},
		{"--root", "-1"}, {"--root", "bad"}, {"--root"}, {"--limit", "-1"},
		{"--limit", "bad"}, {"--limit"}, {"--since", ""}, {"--since", "bad"},
		{"--since", "-1"}, {"--since", "0h"}, {"--since", "-1h"}, {"--since"},
		{"--since", "9223372036854775808"},
	} {
		h.one(exitUsage, nil, append([]string{"notes"}, args...)...)
	}
	h.one(exitUsage, nil, "note", "x", "--as", id(root), "--owner=invalid")
	if err := db.QueryRow(`select count(*) from events`).Scan(&after); err != nil || before != after {
		t.Fatalf("invalid reads changed events: %d -> %d, %v", before, after, err)
	}
	for _, args := range [][]string{{"help", "notes"}, {"notes", "--help"}, {"note", "--help"}} {
		code, raw, _ := h.compact(nil, args...)
		if code != exitOK || !strings.Contains(raw, "--owner") {
			t.Fatalf("help %v = %d %q", args, code, raw)
		}
	}
}

func TestNotesWindowsOrderAndBounds(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	other := h.newTask("other", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", root)
	db := h.openDB()
	old := num(h.ok(nil, "note", "old", "--owner", "--as", id(root)), "event_id")
	backdate(t, db, old, 49*time.Hour)
	first := num(h.ok(nil, "note", "first", "--owner", "--as", id(root)), "event_id")
	backdate(t, db, first, 2*time.Hour)
	plain := num(h.ok(nil, "note", "plain", "--as", id(root)), "event_id")
	last := num(h.ok(nil, "note", "latest", "--owner", "--as", id(other)), "event_id")
	h.ok(as(lane, h.launch(lane)), "note", "lane")
	for _, tc := range []struct {
		args []string
		want []int64
	}{
		{nil, []int64{last, plain, first}},
		{[]string{"--owner"}, []int64{last, first}},
		{[]string{"--root", id(root)}, []int64{plain, first}},
		{[]string{"--since", id(first)}, []int64{last, plain}},
		{[]string{"--since", "90m"}, []int64{last, plain}},
		{[]string{"--since", "72h", "--owner"}, []int64{last, first, old}},
		{[]string{"--since", "0", "--root", id(root), "--limit", "0"}, []int64{plain, first, old}},
	} {
		code, rows := h.run(nil, append([]string{"notes"}, tc.args...)...)
		var got []int64
		for _, row := range rows {
			got = append(got, num(row, "id"))
		}
		if code != exitOK || !reflect.DeepEqual(got, tc.want) {
			t.Fatalf("notes %v = %d %v, want %v", tc.args, code, got, tc.want)
		}
	}
	for i := 0; i < 55; i++ {
		h.ok(nil, "note", strings.Repeat("界", 1650), "--as", id(root))
	}
	code, rows := h.run(nil, "notes", "--root", id(root))
	if code != exitOK || len(rows) != 50 {
		t.Fatalf("JSON default limit = %d %d", code, len(rows))
	}
	code, raw, _ := h.compact(nil, "notes", "--root", id(root))
	frames, trailer := readFrames(t, raw)
	if code != exitOK || len(frames) >= 50 || len(frames) == 0 || trailer["older"] != float64(57-len(frames)) || len(raw) > readCapBytes {
		t.Fatalf("compact cap = %d %d %v (%d bytes)", code, len(frames), trailer, len(raw))
	}
	for i, frame := range frames {
		if frame["s"] != strings.Repeat("界", 1650) || frame["rec"] != "event" || frame["k"] != "note" || i > 0 && num(frame, "i") >= num(frames[i-1], "i") {
			t.Fatal("compact record was clipped or not in descending log shape")
		}
	}
	code, raw, _ = h.compact(nil, "notes", "--root", id(root), "--limit", "1")
	frames, trailer = readFrames(t, raw)
	if code != exitOK || len(frames) != 1 || trailer["older"] != float64(56) {
		t.Fatalf("explicit limit = %d %d %v", code, len(frames), trailer)
	}
	code, raw, _ = h.compact(nil, "notes", "--root", id(root), "--limit", "0")
	frames, trailer = readFrames(t, raw)
	if code != exitOK || len(frames) != 57 || trailer != nil || len(raw) < readCapBytes {
		t.Fatalf("unlimited = %d %d %v (%d bytes)", code, len(frames), trailer, len(raw))
	}
}

func TestCampaignNotesOrderCapAndOwner(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", root)
	var ids []int64
	for i := 0; i < 53; i++ {
		args := []string{"note", "plain", "--as", id(root)}
		if i%2 == 0 {
			args = append(args, "--owner")
		}
		ids = append(ids, num(h.ok(nil, args...), "event_id"))
	}
	long := strings.Repeat("界", 4100)
	idLong := num(h.ok(nil, "note", long, "--owner", "--as", id(root)), "event_id")
	h.ok(as(lane, h.launch(lane)), "note", "lane")
	m := campaignGET(t, h.dash(), root, 1)
	notes := m["notes"].([]any)
	if len(notes) != 50 || num(notes[0].(map[string]any), "id") != idLong || notes[0].(map[string]any)["text"] != clip(long, 4000) {
		t.Fatalf("campaign cap or clip = %v", notes)
	}
	for i, row := range notes[1:] {
		n := row.(map[string]any)
		index := 52 - i
		if num(n, "id") != ids[index] || n["owner"] != (index%2 == 0) || n["at"] == "" || num(n, "age_ms") < 0 || len(n) != 5 {
			t.Fatalf("campaign note %d = %v", i, n)
		}
	}
}

func TestSpoolOwnerNoteAndFreshRPCNotes(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	root, _, _ := spoolMakeWorker(t, r, host, 1)
	dead := spoolDeadURL(t, r)
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	home := r.clientHome(dead)
	text := strings.Repeat("界", 1650)
	args := []string{"note", text, "--owner", "--as", id(root)}
	code, raw, stderr := spoolRunCLI(r, host, home, nil, args...)
	if code != exitOK || !strings.HasPrefix(raw, "qd1 ") {
		t.Fatalf("queue = %d %q %q", code, raw, stderr)
	}
	files, err := readSpoolFiles(spoolQueuePath(home))
	if err != nil || len(files) != 1 || !reflect.DeepEqual(files[0].record.Request.Argv, args) || spoolTaskID(files[0].record.Request) != root {
		t.Fatalf("queued owner flag/task lost: %v", err)
	}
	r.caller.Store(host)
	if sent, err := contractNetSendSpool(t, spoolStateDir(home), r.url, nil); err != nil || sent != 1 {
		t.Fatalf("delivery = %d %v", sent, err)
	}
	if files, err := readSpoolFiles(spoolQueuePath(home)); err != nil || len(files) != 0 {
		t.Fatalf("queue after delivery = %d %v", len(files), err)
	}
	// Repeating a read under the same request key must see a later note.
	read := []string{"--request-key", "fresh-owner-notes-read", "notes", "--owner", "--root", id(root)}
	code, raw, _ = spoolRunCLI(r, host, r.homes[host], nil, read...)
	frames, _ := readFrames(t, raw)
	if code != exitOK || len(frames) != 1 || frames[0]["s"] != text || frames[0]["d"].(map[string]any)["owner"] != true {
		t.Fatal("spooled owner note not returned whole over RPC")
	}
	r.want(exitOK, host, nil, "note", "newer", "--owner", "--as", id(root))
	code, raw, _ = spoolRunCLI(r, host, r.homes[host], nil, read...)
	frames, _ = readFrames(t, raw)
	if code != exitOK || len(frames) != 2 || frames[0]["s"] != "newer" || storedCommand([]string{"notes"}) {
		t.Fatal("RPC notes read was cached")
	}
}
