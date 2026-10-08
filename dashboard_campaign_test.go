package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"strings"
	"testing"
)

func campaignDoc(t *testing.T, db *sql.DB, task int64, kind, name, body string, event *int64) document {
	t.Helper()
	var d document
	err := withTx(db, func(tx *sql.Tx) error {
		var err error
		in := bodyDocument([]byte(body), "fixture.md")
		if body == "client" {
			in = documentInput{Path: "fixture.md", Host: "client-host", Reason: "client"}
		}
		d, _, err = storeDocument(tx, task, kind, name, in, event, 0)
		return err
	})
	if err != nil {
		t.Fatal(err)
	}
	return d
}

func campaignLegacyMiss(t *testing.T, db *sql.DB, task int64, kind, name, path, host string) document {
	t.Helper()
	var d document
	if err := withTx(db, func(tx *sql.Tx) error {
		root, err := rootOf(tx, task)
		if err != nil {
			return err
		}
		latest, err := latestDocument(tx, task, kind, name)
		if err != nil {
			return err
		}
		res, err := tx.Exec(`insert into documents (root_id, task_id, kind, name, version, sha256, bytes, format,
			captured, reason, source_path, source_host, event_id, backfill, created_at)
			values (?, ?, ?, ?, ?, NULL, NULL, NULL, 0, 'client', ?, ?, NULL, 0, ?)`,
			root, task, kind, name, latest.Version+1, path, host, now())
		if err != nil {
			return err
		}
		id, err := res.LastInsertId()
		if err != nil {
			return err
		}
		d, err = scanDocument(tx.QueryRow(`select `+documentCols+` from documents where id = ?`, id))
		return err
	}); err != nil {
		t.Fatal(err)
	}
	return d
}

// campaignGET is one campaign as `taskr campaign` and the TUI read it, through
// JSON like their output.
func campaignGET(t *testing.T, d *dashboard, root int64, page int) map[string]any {
	t.Helper()
	return readJSON(t, d, func(tx *sql.Tx) (map[string]any, error) { return readCampaignDetail(tx, root, page, false, true) })
}

func docGET(t *testing.T, d *dashboard, doc int64) map[string]any {
	t.Helper()
	return readJSON(t, d, func(tx *sql.Tx) (map[string]any, error) { return readDashboardDocument(tx, doc) })
}

func readJSON(t *testing.T, d *dashboard, read func(*sql.Tx) (map[string]any, error)) map[string]any {
	t.Helper()
	tx, err := d.db.BeginTx(context.Background(), &sql.TxOptions{ReadOnly: true})
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	v, err := read(tx)
	if err != nil {
		t.Fatal(err)
	}
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	var m map[string]any
	if err := json.Unmarshal(b, &m); err != nil {
		t.Fatal(err)
	}
	return m
}

func TestCampaignRead(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	open := h.newTask("open", "implementer", root)
	launch := h.launch(open)
	sub := h.newTask("sub", "sub-orchestrator", root)
	nested := h.newTask("nested", "reviewer", sub)
	closed := h.newTask("closed", "implementer", root)
	cl := h.launch(closed)
	miss := h.newTask("miss", "implementer", root)
	h.ok(nil, "next", id(root), "next step")
	db := h.openDB()
	if _, err := db.Exec(`update tasks set machine = 'old-host' where id = ?`, open); err != nil {
		t.Fatal(err)
	}
	goal := campaignDoc(t, db, root, "goal", "", "\n# Objective\nMore", nil)
	first := campaignDoc(t, db, root, "plan", "", "# First", nil)
	planEvent := num(h.ok(nil, "note", "--as", id(root), "plan checkpoint"), "event_id")
	plan := campaignDoc(t, db, root, "plan", "", "# Current", ptr(planEvent))
	named := campaignDoc(t, db, root, "plan", "reference", "# Named", nil)
	d1 := num(h.ok(nil, "decide", "--as", id(root), "retired"), "event_id")
	d2 := num(h.ok(nil, "decide", "--as", id(root), "in force"), "event_id")
	h.ok(nil, "decide", "--as", id(root), "--revoke", id(d1))
	h.ok(as(closed, cl), "ready", strings.Repeat("界", 350))
	final := "finished " + strings.Repeat("界", 350)
	h.ok(as(closed, cl), "done", final)
	h.ok(nil, "close", id(closed))
	brief := campaignDoc(t, db, closed, "brief", "", "# Brief", nil)
	report := campaignDoc(t, db, closed, "report", "", "# Report", nil)
	client := campaignDoc(t, db, miss, "brief", "", "client", nil)
	// An event with a captured handover document, and one legacy handover without one.
	var handover int64
	if err := withTx(db, func(tx *sql.Tx) error {
		var err error
		handover, err = insertEvent(tx, event{TaskID: root, Kind: "handover", Summary: "handover note"})
		return err
	}); err != nil {
		t.Fatal(err)
	}
	hd := campaignDoc(t, db, root, "handover", "", "# Handover", ptr(handover))
	if err := withTx(db, func(tx *sql.Tx) error {
		_, err := insertEvent(tx, event{TaskID: root, Kind: "handover", Summary: "legacy note"})
		return err
	}); err != nil {
		t.Fatal(err)
	}
	d := h.dash()
	m := campaignGET(t, d, root, 1)
	r := m["root"].(map[string]any)
	for k, want := range map[string]any{"id": float64(root), "name": "root", "role": "orchestrator", "status": "open", "host": "", "next": "next step"} {
		if r[k] != want {
			t.Errorf("root %s: %v", k, r[k])
		}
	}
	if r["created_at"] == "" || r["closed_at"] != "" {
		t.Fatal(r)
	}
	if m["goal"].(map[string]any)["doc_id"] != float64(goal.ID) {
		t.Fatal(m["goal"])
	}
	p := m["plan"].(map[string]any)
	if p["doc_id"] != float64(plan.ID) || p["version"] != float64(2) || p["event_id"] != float64(planEvent) || p["decisions_since"] != float64(2) || p["closed_since"] != float64(1) {
		t.Fatal(p)
	}
	handoverDocs := loadHandoverDocuments(db, root)
	if p["decisions_since"] != float64(handoverDocs.Decisions) || p["closed_since"] != float64(handoverDocs.Closed) {
		t.Fatal("handover counters differ")
	}
	docs := m["documents"].([]any)
	if len(docs) != 1 || docs[0].(map[string]any)["doc_id"] != float64(named.ID) {
		t.Fatal(docs)
	}
	decisions := m["decisions"].([]any)
	if len(decisions) != 1 || decisions[0].(map[string]any)["id"] != float64(d2) || decisions[0].(map[string]any)["text"] != "in force" || decisions[0].(map[string]any)["time"] == "" {
		t.Fatal(decisions)
	}
	hs := m["handovers"].([]any)
	if len(hs) != 2 || hs[0].(map[string]any)["doc_id"] != float64(hd.ID) || hs[0].(map[string]any)["note"] != "handover note" || hs[1].(map[string]any)["doc_id"] != nil {
		t.Fatal(hs)
	}
	lanes := m["lanes"].([]any)
	if len(lanes) != 5 || m["total"] != float64(5) || m["pages"] != float64(1) {
		t.Fatal(m)
	}
	for i, x := range lanes {
		l := x.(map[string]any)
		if i > 0 && l["id"].(float64) <= lanes[i-1].(map[string]any)["id"].(float64) {
			t.Fatal("id order")
		}
		for _, key := range []string{"parent_id", "role", "status", "host", "created_at", "closed_at", "provider", "model", "effort", "summary", "brief", "report"} {
			if _, ok := l[key]; !ok {
				t.Errorf("missing lane %s", key)
			}
		}
		switch int64(l["id"].(float64)) {
		case open:
			if l["provider"] != "claude" || l["model"] != "claude-opus-5-5" || l["effort"] != "high" || l["host"] != "" || launch == 0 {
				t.Fatal(l)
			}
		case nested:
			if l["parent_id"] != float64(sub) || l["depth"] != float64(2) {
				t.Fatal(l)
			}
		case closed:
			if l["status"] != "closed" || l["summary"] != clip(final, 300) || l["closed_at"] == "" || l["brief"].(map[string]any)["doc_id"] != float64(brief.ID) || l["report"].(map[string]any)["doc_id"] != float64(report.ID) {
				t.Fatal(l)
			}
		case miss:
			if l["brief"].(map[string]any)["reason"] != "client" {
				t.Fatal(l)
			}
		}
	}
	if _, ok := m["events"]; ok {
		t.Fatal("event stream leaked")
	}
	doc := docGET(t, d, plan.ID)
	if doc["body"] != "# Current" || doc["lane"] != "root" || doc["task_id"] != float64(root) || doc["source_path"] != "fixture.md" || doc["source_host"] != nil || doc["format"] != "md" || doc["bytes"] != float64(9) || doc["captured_at"] == nil || doc["sha256"] == nil || doc["backfill"] != float64(0) {
		t.Fatal(doc)
	}
	vs := doc["versions"].([]any)
	if len(vs) != 2 || vs[0].(map[string]any)["id"] != float64(first.ID) || vs[1].(map[string]any)["event_id"] != float64(planEvent) {
		t.Fatal(vs)
	}
	missing := docGET(t, d, client.ID)
	if missing["captured"] != false || missing["reason"] != "client" || missing["source_host"] != "client-host" {
		t.Fatal(missing)
	}
	if _, ok := missing["body"]; ok {
		t.Fatal("miss has body")
	}
}

func TestCampaignPaging(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	if err := withTx(db, func(tx *sql.Tx) error {
		for i := 0; i < 101; i++ {
			if _, err := tx.Exec(`insert into tasks(parent_id,name,role,status,created_at,updated_at) values(?,?,'implementer','open',?,?)`, root, fmt.Sprint("lane-", i), now(), now()); err != nil {
				return err
			}
		}
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	d := h.dash()
	for page, want := range map[int]int{1: 100, 2: 1} {
		m := campaignGET(t, d, root, page)
		if len(m["lanes"].([]any)) != want || m["pages"] != float64(2) || m["total"] != float64(101) || m["page"] != float64(page) {
			t.Fatal(m)
		}
	}
	m := campaignGET(t, d, root, 99)
	if m["page"] != float64(2) || m["goal"] != nil || m["plan"] != nil || len(m["documents"].([]any)) != 0 {
		t.Fatal(m)
	}
}

func TestCampaignGoalMatchesHandover(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	first := campaignDoc(t, db, root, "goal", "", "# Captured first", nil)
	latest := campaignDoc(t, db, root, "goal", "", "# Captured latest", nil)
	miss := campaignLegacyMiss(t, db, root, "goal", "", "fixture.md", "client-host")
	d := h.dash()
	m := campaignGET(t, d, root, 1)
	goal := m["goal"].(map[string]any)
	if goal["doc_id"] != float64(latest.ID) || goal["doc_id"] == float64(first.ID) || goal["captured"] != true {
		t.Fatal(goal)
	}
	later := m["goal_miss"].(map[string]any)
	if later["doc_id"] != float64(miss.ID) || later["reason"] != "client" || later["source_path"] != "fixture.md" || later["source_host"] != "client-host" {
		t.Fatal(later)
	}
	if hd := loadHandoverDocuments(db, root); hd.Goal == nil || hd.Goal.ID != latest.ID {
		t.Fatal("goal differs from handover")
	}
	// A new capture supersedes the miss and needs no warning.
	campaignDoc(t, db, root, "goal", "", "# Recovered", nil)
	if _, ok := campaignGET(t, d, root, 1)["goal_miss"]; ok {
		t.Fatal("stale miss warning")
	}
	onlyMiss := h.newTask("miss-only", "orchestrator", 0)
	missing := campaignDoc(t, db, onlyMiss, "goal", "", "client", nil)
	m = campaignGET(t, d, onlyMiss, 1)
	if m["goal"].(map[string]any)["doc_id"] != float64(missing.ID) || m["goal"].(map[string]any)["captured"] != false {
		t.Fatal(m)
	}
	if _, ok := m["goal_miss"]; ok {
		t.Fatal("miss-only has later-version warning")
	}
	if hd := loadHandoverDocuments(db, onlyMiss); hd.Goal == nil || hd.Goal.ID != missing.ID {
		t.Fatal("miss differs from handover")
	}
}
