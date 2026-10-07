package main

import (
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// leadOf is the root's reported lead status.
func leadOf(t *testing.T, db *sql.DB, root int64) string {
	t.Helper()
	obs, err := leadObservations(db)
	if err != nil {
		t.Fatal(err)
	}
	o, ok := obs[root]
	if !ok {
		t.Fatalf("root %d has no lead observation: %v", root, obs)
	}
	return o.Status
}

// leadRow is the stored observation, with NULLs as "-".
func leadRow(t *testing.T, db *sql.DB, root int64) string {
	t.Helper()
	var s string
	if err := db.QueryRow(`select coalesce(lead_status, '-') || ' ' || coalesce(lead_present, '-') || ' ' ||
		coalesce(lead_observed_at, '-') from tasks where id = ?`, root).Scan(&s); err != nil {
		t.Fatal(err)
	}
	return s
}

func countEvents(t *testing.T, db *sql.DB) int {
	t.Helper()
	var n int
	if err := db.QueryRow(`select count(*) from events`).Scan(&n); err != nil {
		t.Fatal(err)
	}
	return n
}

// A root with no child launches is observed by the hub pass alone, from the
// one listing that pass fetches, and writes no event.
func TestLeadObservedByHubPass(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("top", "orchestrator", 0, "--pane", "w1:p1")
	bare := h.newTask("bare", "orchestrator", 0)
	db := h.openDB()
	events := countEvents(t, db)
	pass := func() error {
		_, err := observe(db, h.herdrSock, nil, time.Now().Add(5*time.Second))
		return err
	}
	beat := func(age time.Duration) {
		if err := setMeta(db, heartbeatKey, stamp(time.Now().Add(-age))); err != nil {
			t.Fatal(err)
		}
	}
	beat(0)
	if got := leadOf(t, db, root); got != "unknown" {
		t.Fatalf("a never observed root = %s, want unknown", got)
	}
	for _, step := range []struct {
		agents []string
		want   string
	}{
		{[]string{"w1:p1/working/1"}, "working"},
		{[]string{"w1:p1/idle/2"}, "idle"},
		{nil, "gone"},
		{[]string{"w1:p1/blocked/3"}, "blocked"},
	} {
		h.setAgents(step.agents...)
		if err := pass(); err != nil {
			t.Fatal(err)
		}
		if got := leadOf(t, db, root); got != step.want {
			t.Fatalf("listing %v: lead = %s, want %s (%s)", step.agents, got, step.want, leadRow(t, db, root))
		}
	}
	if n := len(h.calls("agent|list|")); n != 4 {
		t.Fatalf("4 passes ran herdr agent list %d times", n)
	}

	// Only a change is written: lead_observed_at is the time of the last one.
	if _, err := db.Exec(`update tasks set lead_observed_at = 'mark' where id = ?`, root); err != nil {
		t.Fatal(err)
	}
	if err := pass(); err != nil {
		t.Fatal(err)
	}
	if got := leadRow(t, db, root); got != "blocked 1 mark" {
		t.Fatalf("an unchanged listing rewrote the observation: %s", got)
	}

	// A failed listing writes nothing.
	for _, bad := range []struct{ list, exit string }{{"not json", "0"}, {`{"result":{}}`, "0"}, {"{}", "1"}} {
		h.write("list.json", bad.list, 0o644)
		h.write("list.exit", bad.exit, 0o644)
		if err := pass(); err == nil {
			t.Fatalf("listing %q: the pass reported no error", bad.list)
		}
		if got := leadRow(t, db, root); got != "blocked 1 mark" {
			t.Fatalf("listing %q changed the observation: %s", bad.list, got)
		}
	}
	h.write("list.exit", "0", 0o644)

	// A root without a pane is never observed.
	if got := leadRow(t, db, bare) + "/" + leadOf(t, db, bare); got != "- - -/unknown" {
		t.Fatalf("a root without a pane = %s", got)
	}

	// The stored status is a claim only while the hub daemon is fresh.
	beat(heartbeatFresh + time.Second)
	if got := leadOf(t, db, root); got != "unknown" {
		t.Fatalf("stale hub daemon: lead = %s, want unknown", got)
	}
	if _, err := db.Exec(`delete from meta where key = ?`, heartbeatKey); err != nil {
		t.Fatal(err)
	}
	if got := leadOf(t, db, root); got != "unknown" {
		t.Fatalf("no hub daemon: lead = %s, want unknown", got)
	}
	beat(0)
	if got := leadOf(t, db, root); got != "blocked" {
		t.Fatalf("fresh hub daemon again: lead = %s, want blocked", got)
	}

	// A closed root is not a lead.
	h.ok(nil, "close", id(bare))
	if obs, err := leadObservations(db); err != nil || len(obs) != 1 {
		t.Fatalf("open roots = %v, %v; want only %d", obs, err, root)
	}
	if n := countEvents(t, db) - events; n != 1 { // the close
		t.Fatalf("root observations wrote events: %d new, want 1", n)
	}
}

// Pane ids repeat across hosts: each host's listing reaches only its own
// roots, a stale client heartbeat hides the stored status, and adopt forgets
// the old pane's observation.
func TestLeadHostsDoNotCross(t *testing.T) {
	r := newTwoHost(t)
	db := r.openDB()
	const pane = "w1:p1"
	srv := r.newTask("srv-top", "orchestrator", 0, "--pane", pane)
	mac := num(r.want(0, "host-a", nil, "new", "mac-top", "--role", "orchestrator", "--cwd", r.dir, "--pane", pane), "task_id")
	if r.machineOf("tasks", srv) != "NULL" || r.machineOf("tasks", mac) != "host-a" {
		t.Fatalf("root hosts = %s, %s", r.machineOf("tasks", srv), r.machineOf("tasks", mac))
	}
	events := countEvents(t, db)
	calls := 0
	hostObserve := func(host, agents string) {
		t.Helper()
		calls++
		_, rep, raw := r.post(host, rpcBody(r.dir, nil, fmt.Sprintf("lead-observe-%02d", calls), "_host", "observe", "--agents", agents))
		if rep.Exit != exitOK {
			t.Fatalf("%s _host observe = %+v %s", host, rep, raw)
		}
	}
	if err := setMeta(db, heartbeatKey, now()); err != nil {
		t.Fatal(err)
	}

	// host-a's listing: its root only.
	hostObserve("host-a", `[{"pane_id":"w1:p1","agent_status":"working","state_change_seq":1}]`)
	if a, s := leadOf(t, db, mac), leadRow(t, db, srv); a != "working" || s != "- - -" {
		t.Fatalf("after host-a's listing: mac = %s, server root row = %s", a, s)
	}
	// The hub's listing: the server host's root only.
	r.setAgents(pane + "/blocked/1")
	if _, err := observe(db, r.herdrSock, nil, time.Now().Add(5*time.Second)); err != nil {
		t.Fatal(err)
	}
	if a, s := leadOf(t, db, mac), leadOf(t, db, srv); a != "working" || s != "blocked" {
		t.Fatalf("after the hub's listing: mac = %s, srv = %s", a, s)
	}
	// host-b has neither root: its empty listing marks nothing gone.
	hostObserve("host-b", `[]`)
	if a, s := leadOf(t, db, mac), leadOf(t, db, srv); a != "working" || s != "blocked" {
		t.Fatalf("after host-b's listing: mac = %s, srv = %s", a, s)
	}
	// host-a's empty listing: its root is gone, the server's is not.
	hostObserve("host-a", `[]`)
	if a, s := leadOf(t, db, mac), leadOf(t, db, srv); a != "gone" || s != "blocked" {
		t.Fatalf("after host-a's empty listing: mac = %s, srv = %s", a, s)
	}
	hostObserve("host-a", `[{"pane_id":"w1:p1","agent_status":"idle","state_change_seq":2}]`)

	// A stale client heartbeat: unknown, whatever is stored.
	r.beat("host-a", heartbeatFresh+time.Second)
	if a, s := leadOf(t, db, mac), leadOf(t, db, srv); a != "unknown" || s != "blocked" {
		t.Fatalf("stale host-a: mac = %s, srv = %s", a, s)
	}
	r.beat("host-a", 0)
	if a := leadOf(t, db, mac); a != "idle" {
		t.Fatalf("fresh host-a: mac = %s, want idle", a)
	}

	// Adopt on host-b at another pane: the observation of the old pane is gone.
	r.want(0, "host-b", map[string]string{"HERDR_PANE_ID": "w2:p9"}, "adopt", id(mac))
	if row := leadRow(t, db, mac); row != "- - -" || leadOf(t, db, mac) != "unknown" {
		t.Fatalf("after adopt: row = %s, lead = %s", row, leadOf(t, db, mac))
	}
	// The old host no longer observes it; the new one does.
	hostObserve("host-a", `[{"pane_id":"w1:p1","agent_status":"working","state_change_seq":3}]`)
	if row := leadRow(t, db, mac); row != "- - -" {
		t.Fatalf("host-a observed a root that moved to host-b: %s", row)
	}
	hostObserve("host-b", `[{"pane_id":"w2:p9","agent_status":"done","state_change_seq":1}]`)
	if a := leadOf(t, db, mac); a != "done" {
		t.Fatalf("after host-b's listing: mac = %s, want done", a)
	}
	if n := countEvents(t, db) - events; n != 1 { // the adopt
		t.Fatalf("root observations wrote events: %d new, want 1", n)
	}
}

func TestMigrationAddsLeadColumns(t *testing.T) {
	h := newHarness(t)
	os.MkdirAll(filepath.Dir(h.db), 0o755)
	var kept []string
	for _, l := range strings.Split(schemaSQL, "\n") {
		if !strings.Contains(l, "lead_") {
			kept = append(kept, l)
		}
	}
	oldSchema := strings.Join(kept, "\n")
	if oldSchema == schemaSQL {
		t.Fatal("schema.sql has no lead_ lines to strip")
	}
	old, err := sql.Open("sqlite", h.db)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := old.Exec(oldSchema); err != nil {
		t.Fatal(err)
	}
	if _, err := old.Exec(`insert into tasks (name, role, pane_id, created_at, updated_at) values ('legacy', 'orchestrator', 'w1:p1', 'x', 'x')`); err != nil {
		t.Fatal(err)
	}
	old.Close()
	for i := 0; i < 2; i++ { // the second open finds the columns and leaves them
		var cols string
		h.openDB().QueryRow(`select group_concat(name || ' ' || lower(type), ', ') from (select name, type from pragma_table_info('tasks')
			where name like 'lead_%' order by name)`).Scan(&cols)
		if cols != "lead_observed_at text, lead_present integer, lead_status text" {
			t.Fatalf("open %d: lead columns = %q", i, cols)
		}
	}
	db := h.openDB()
	if got := leadOf(t, db, 1); got != "unknown" {
		t.Fatalf("legacy root = %s, want unknown", got)
	}
	setMeta(db, heartbeatKey, now())
	h.setAgents("w1:p1/working/1")
	if _, err := observe(db, h.herdrSock, nil, time.Now().Add(5*time.Second)); err != nil {
		t.Fatal(err)
	}
	if got := leadOf(t, db, 1); got != "working" {
		t.Fatalf("legacy root after a pass = %s, want working", got)
	}
}
