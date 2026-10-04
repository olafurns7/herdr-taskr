package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"net/http"
	"reflect"
	"strings"
	"testing"
	"time"
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

func campaignGET(t *testing.T, d *dashboard, path string) map[string]any {
	t.Helper()
	w, m := serve(d, req("GET", path, ""))
	if w.Code != 200 {
		t.Fatalf("%s: %d %s", path, w.Code, w.Body)
	}
	if w.Header().Get("Content-Type") != "application/json; charset=utf-8" || w.Header().Get("Cache-Control") != "no-store" {
		t.Fatal(w.Header())
	}
	return m
}
func TestCampaignRoutes(t *testing.T) {
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
	m := campaignGET(t, d, "/api/campaign/"+id(root))
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
	doc := campaignGET(t, d, "/api/doc/"+id(plan.ID))
	if doc["body"] != "# Current" || doc["lane"] != "root" || doc["task_id"] != float64(root) || doc["source_path"] != "fixture.md" || doc["source_host"] != nil || doc["format"] != "md" || doc["bytes"] != float64(9) || doc["captured_at"] == nil || doc["sha256"] == nil || doc["backfill"] != float64(0) {
		t.Fatal(doc)
	}
	vs := doc["versions"].([]any)
	if len(vs) != 2 || vs[0].(map[string]any)["id"] != float64(first.ID) || vs[1].(map[string]any)["event_id"] != float64(planEvent) {
		t.Fatal(vs)
	}
	missing := campaignGET(t, d, "/api/doc/"+id(client.ID))
	if missing["captured"] != false || missing["reason"] != "client" || missing["source_host"] != "client-host" {
		t.Fatal(missing)
	}
	if _, ok := missing["body"]; ok {
		t.Fatal("miss has body")
	}
	archive := campaignGET(t, d, "/api/campaigns")
	a := archive["campaigns"].([]any)[0].(map[string]any)
	if a["goal"] != "# Objective" || a["lane_counts"].(map[string]any)["closed"] != float64(1) {
		t.Fatal(a)
	}
	longGoal := "目" + strings.Repeat("界", 180)
	campaignDoc(t, db, root, "goal", "", longGoal, nil)
	preview := campaignGET(t, d, "/api/campaigns")["campaigns"].([]any)[0].(map[string]any)["goal"]
	if preview != clip(longGoal, 160) {
		t.Fatal(preview)
	}

}

func TestCampaignPagingAndEmpty(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	if err := withTx(db, func(tx *sql.Tx) error {
		for i := 0; i < 101; i++ {
			if _, err := tx.Exec(`insert into tasks(parent_id,name,role,status,created_at,updated_at) values(?,?,'implementer','open',?,?)`, root, fmt.Sprint("lane-", i), now(), now()); err != nil {
				return err
			}
		}
		for i := 0; i < 50; i++ {
			if _, err := tx.Exec(`insert into tasks(name,role,status,created_at,updated_at,closed_at) values(?,'orchestrator','closed',?,?,?)`, fmt.Sprint("root-", i), now(), now(), "2000-01-01T00:00:00Z"); err != nil {
				return err
			}
		}
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	d := h.dash()
	for page, want := range map[int]int{1: 100, 2: 1} {
		m := campaignGET(t, d, fmt.Sprintf("/api/campaign/%d?page=%d", root, page))
		if len(m["lanes"].([]any)) != want || m["pages"] != float64(2) || m["total"] != float64(101) || m["page"] != float64(page) {
			t.Fatal(m)
		}
	}
	for page, want := range map[int]int{1: 50, 2: 1} {
		m := campaignGET(t, d, fmt.Sprintf("/api/campaigns?page=%d", page))
		if len(m["campaigns"].([]any)) != want || m["pages"] != float64(2) || m["total"] != float64(51) {
			t.Fatal(m)
		}
	}
	m := campaignGET(t, d, "/api/campaign/"+id(root)+"?page=bogus")
	if m["page"] != float64(1) || m["goal"] != nil || m["plan"] != nil || len(m["documents"].([]any)) != 0 {
		t.Fatal(m)
	}
	empty := campaignGET(t, newHarness(t).dash(), "/api/campaigns")
	if empty["total"] != float64(0) || len(empty["campaigns"].([]any)) != 0 {
		t.Fatal(empty)
	}
}
func TestCampaignNotFoundAndAdmission(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", root)
	doc := campaignDoc(t, h.openDB(), root, "goal", "", "text", nil)
	d := h.dash()
	for _, path := range []string{"/api/campaign/99999", "/api/campaign/" + id(lane), "/api/doc/99999", "/api/campaign/x", "/api/doc/x", "/api/doc/0", "/api/campaign/-1", "/api/doc/9223372036854775808", "/api/campaign/0" + id(root), "/api/doc/00" + id(doc.ID), "/api/campaign/%30" + id(root), "/api/doc/%30" + id(doc.ID)} {
		w, _ := serve(d, req("GET", path, ""))
		if w.Code != 404 {
			t.Errorf("%s: %d", path, w.Code)
		}
	}
	for _, path := range []string{"/api/campaigns", "/api/campaign/" + id(root), "/api/doc/" + id(doc.ID)} {
		for _, method := range []string{"POST", "PUT", "DELETE"} {
			w, _ := serve(d, req(method, path, ""))
			if w.Code != 405 {
				t.Errorf("%s %s: %d", method, path, w.Code)
			}
		}
		for _, mod := range []func(*http.Request){func(r *http.Request) { r.Host = "evil.example:7788" }, func(r *http.Request) { r.Host = "127.0.0.1:9" }, func(r *http.Request) { r.RemoteAddr = "192.0.2.1:50000" }, func(r *http.Request) { r.RemoteAddr = "invalid" }} {
			state, _ := serve(d, req("GET", "/api/state", "", mod))
			route, _ := serve(d, req("GET", path, "", mod))
			if route.Code != state.Code || route.Code == 200 || route.Header().Get("Access-Control-Allow-Origin") != "" {
				t.Fatalf("gate %s: %d vs %d", path, route.Code, state.Code)
			}
		}
		w, _ := serve(d, req("GET", path, "", func(r *http.Request) { r.Host = "localhost:7788" }))
		if w.Code != 200 {
			t.Fatal(w.Code)
		}
	}
	hub := h.hubDash()
	for _, path := range []string{"/api/campaigns", "/api/campaign/" + id(root), "/api/doc/" + id(doc.ID)} {
		for _, ip := range []string{hostAIP, otherIP, taggedIP, taggedOwnerIP, errIP, junkIP} {
			mod := from(ip, hubIP+":7788")
			state, _ := serve(hub, req("GET", "/api/state", "", mod))
			route, _ := serve(hub, req("GET", path, "", mod))
			if route.Code != state.Code {
				t.Fatalf("hub gate %s %s: %d vs %d", path, ip, route.Code, state.Code)
			}
		}
	}
}
func TestCampaignDocumentsDoNotChangeSnapshots(t *testing.T) {
	h := newHarness(t)
	f := seedDashboard(h)
	d := h.dash()
	at := time.Now()
	snapshot := func() ([]byte, []byte) {
		s, err := readState(context.Background(), d.db, at)
		if err != nil {
			t.Fatal(err)
		}
		v, err := d.view(context.Background(), s, at)
		if err != nil {
			t.Fatal(err)
		}
		state, err := json.Marshal(v)
		if err != nil {
			t.Fatal(err)
		}
		raw, err := json.Marshal(s)
		if err != nil {
			t.Fatal(err)
		}
		peer, err := json.Marshal(pushBody{State: raw, Now: s.Now})
		if err != nil {
			t.Fatal(err)
		}
		return state, peer
	}
	beforeState, beforePeer := snapshot()
	campaignDoc(t, d.db, f.a, "goal", "", "# Goal", nil)
	campaignDoc(t, d.db, f.a, "plan", "", "# Plan", nil)
	campaignDoc(t, d.db, f.impl, "report", "", "# Report", nil)
	afterState, afterPeer := snapshot()
	if !reflect.DeepEqual(beforeState, afterState) || !reflect.DeepEqual(beforePeer, afterPeer) {
		t.Fatal("documents changed state or peer wire bytes")
	}
}

func TestCampaignGoalMatchesHandover(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	db := h.openDB()
	first := campaignDoc(t, db, root, "goal", "", "# Captured first", nil)
	latest := campaignDoc(t, db, root, "goal", "", "# Captured latest", nil)
	miss := campaignLegacyMiss(t, db, root, "goal", "", "fixture.md", "client-host")
	d := h.dash()
	m := campaignGET(t, d, "/api/campaign/"+id(root))
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
	archive := campaignGET(t, d, "/api/campaigns")
	if archive["campaigns"].([]any)[0].(map[string]any)["goal"] != "# Captured latest" {
		t.Fatal(archive)
	}
	// A new capture supersedes the miss and needs no warning.
	campaignDoc(t, db, root, "goal", "", "# Recovered", nil)
	if _, ok := campaignGET(t, d, "/api/campaign/"+id(root))["goal_miss"]; ok {
		t.Fatal("stale miss warning")
	}
	onlyMiss := h.newTask("miss-only", "orchestrator", 0)
	missing := campaignDoc(t, db, onlyMiss, "goal", "", "client", nil)
	m = campaignGET(t, d, "/api/campaign/"+id(onlyMiss))
	if m["goal"].(map[string]any)["doc_id"] != float64(missing.ID) || m["goal"].(map[string]any)["captured"] != false {
		t.Fatal(m)
	}
	if _, ok := m["goal_miss"]; ok {
		t.Fatal("miss-only has later-version warning")
	}
	if hd := loadHandoverDocuments(db, onlyMiss); hd.Goal == nil || hd.Goal.ID != missing.ID {
		t.Fatal("miss differs from handover")
	}
	archive = campaignGET(t, d, "/api/campaigns")
	if archive["campaigns"].([]any)[0].(map[string]any)["goal"] != "" {
		t.Fatal(archive)
	}
}
