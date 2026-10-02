package main

import (
	"strings"
	"testing"
)

// A root orchestrator names itself with --as on note and ask; nothing else may.
func TestNoteAndAskAsRoot(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("camp", "orchestrator", 0)
	note := h.ok(nil, "note", "--as", id(root), "phase 1: briefing")
	if num(note, "task_id") != root || note["recipient_task_id"] != nil {
		t.Fatalf("note --as = %v", note)
	}
	ask := h.ok(nil, "ask", "Which base branch?", "--owner", "--as", id(root))
	if num(ask, "task_id") != root || ask["recipient_task_id"] != nil || num(ask, "ask_id") == 0 {
		t.Fatalf("ask --owner --as root = %v (a root's owner ask has no recipient)", ask)
	}
	// TASKR_TASK naming the same task is fine.
	h.ok(map[string]string{"TASKR_TASK": id(root)}, "note", "--as", id(root), "phase 2")
	// The key still makes a retry idempotent.
	first := h.ok(nil, "note", "--as", id(root), "retried", "--key", "n1")
	again := h.ok(nil, "note", "--as", id(root), "retried", "--key", "n1")
	if again["duplicate"] != true || num(again, "event_id") != num(first, "event_id") {
		t.Fatalf("keyed note --as retry = %v, first %v", again, first)
	}
	var notes int
	if err := h.openDB().QueryRow(`select count(*) from events where task_id = ? and kind = 'note'`, root).Scan(&notes); err != nil {
		t.Fatal(err)
	}
	if notes != 3 {
		t.Fatalf("notes on the root = %d, want 3", notes)
	}
}

func TestAsRejections(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("camp", "orchestrator", 0)
	other := h.newTask("camp-2", "orchestrator", 0)
	child := h.newTask("impl-a", "implementer", root)
	launched := h.newTask("impl-b", "implementer", root)
	l := h.launch(launched)
	rootWithLaunch := h.newTask("odd-root", "orchestrator", 0)
	// Older ledgers can contain a root launch written before cmdLaunch rejected it (the live hub has one).
	db := h.openDB()
	res, err := db.Exec(`insert into launches (task_id, provider, model, effort, recorded_at) values (?, 'claude', 'm', 'high', ?)`, rootWithLaunch, now())
	if err != nil {
		t.Fatal(err)
	}
	legacyLaunch, err := res.LastInsertId()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(`update tasks set current_launch_id = ? where id = ?`, legacyLaunch, rootWithLaunch); err != nil {
		t.Fatal(err)
	}
	closed := h.newTask("done-root", "orchestrator", 0)
	h.ok(nil, "close", id(closed))

	cases := []struct {
		name string
		env  map[string]string
		args []string
		want string
	}{
		{"child without launch", nil, []string{"note", "--as", id(child), "x"}, "has parent"},
		{"launched worker naming itself", as(launched, l), []string{"note", "--as", id(launched), "x"}, "TASKR_LAUNCH is set"},
		{"launched worker, no env", nil, []string{"ask", "--as", id(launched), "x", "--owner"}, "has parent"},
		{"root with a launch", nil, []string{"note", "--as", id(rootWithLaunch), "x"}, "has launch"},
		{"TASKR_TASK mismatch", map[string]string{"TASKR_TASK": id(other)}, []string{"note", "--as", id(root), "x"}, "different task"},
		{"TASKR_TASK mismatch on ask", map[string]string{"TASKR_TASK": id(child)}, []string{"ask", "--as", id(root), "x"}, "different task"},
		{"closed root", nil, []string{"note", "--as", id(closed), "x"}, "is closed"},
		{"missing task", nil, []string{"note", "--as", "999", "x"}, "does not exist"},
	}
	for _, c := range cases {
		out := h.one(exitReject, c.env, c.args...)
		if !strings.Contains(out["error"].(string), c.want) {
			t.Errorf("%s: error %q, want it to mention %q", c.name, out["error"], c.want)
		}
	}
	if out := h.one(exitUsage, nil, "note", "--as", "-3", "x"); out["kind"] != "usage" {
		t.Fatalf("negative --as = %v", out)
	}
	// Without --as, a worker still writes only as its own TASKR_TASK/TASKR_LAUNCH.
	h.ok(as(launched, l), "note", "still works")
	var n int
	if err := h.openDB().QueryRow(`select count(*) from events where kind in ('note', 'ask')`).Scan(&n); err != nil {
		t.Fatal(err)
	}
	if n != 1 {
		t.Fatalf("events written by rejected --as calls: %d, want only the worker's own note", n-1)
	}
}
