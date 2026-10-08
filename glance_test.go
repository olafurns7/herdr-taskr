package main

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"
)

type glanceFixture struct {
	t  *testing.T
	db *sql.DB
	at time.Time
}

func newGlanceFixture(t *testing.T) *glanceFixture {
	h := newHarness(t)
	f := &glanceFixture{t: t, db: h.openDB(), at: parseTime(now())}
	f.exec(`insert into meta(key, value) values (?, ?)`, heartbeatKey, stamp(f.at))
	f.exec(`insert into meta(key, value) values (?, ?)`, leadListedKey, stamp(f.at))
	return f
}

func (f *glanceFixture) exec(q string, args ...any) int64 {
	f.t.Helper()
	r, err := f.db.Exec(q, args...)
	if err != nil {
		f.t.Fatal(err)
	}
	id, err := r.LastInsertId()
	if err != nil {
		f.t.Fatal(err)
	}
	return id
}

func (f *glanceFixture) task(name string, parent int64, status string) int64 {
	var p any
	if parent != 0 {
		p = parent
	}
	task := f.exec(`insert into tasks(name, parent_id, role, status, pane_id, created_at, updated_at)
		values (?, ?, 'orchestrator', ?, ?, ?, ?)`, name, p, status, name+":p1", stamp(f.at), stamp(f.at))
	if parent == 0 {
		f.lead(task, "working")
	}
	return task
}

func (f *glanceFixture) lead(task int64, status string) {
	f.exec(`update tasks set lead_status = ?, lead_present = ?, lead_observed_at = ? where id = ?`,
		status, status != "gone", stamp(f.at.Add(-time.Minute)), task)
}

func (f *glanceFixture) launch(task int64, status string, present bool, host any) int64 {
	l := f.exec(`insert into launches(task_id, observed_status, present, machine, observed_at, recorded_at)
		values (?, ?, ?, ?, ?, ?)`, task, status, present, host, stamp(f.at.Add(-time.Minute)), stamp(f.at))
	f.exec(`update tasks set current_launch_id = ? where id = ?`, l, task)
	return l
}

func (f *glanceFixture) event(task, recipient, launch int64, kind, text, data string, age time.Duration) int64 {
	var r, l any
	if recipient != 0 {
		r = recipient
	}
	if launch != 0 {
		l = launch
	}
	return f.exec(`insert into events(task_id, recipient_task_id, launch_id, kind, summary, data, created_at)
		values (?, ?, ?, ?, ?, ?, ?)`, task, r, l, kind, text, data, stamp(f.at.Add(-age)))
}

func (f *glanceFixture) view() *glanceView {
	f.t.Helper()
	v, err := readGlance(f.db, f.at)
	if err != nil {
		f.t.Fatal(err)
	}
	return v
}

func glanceKinds(v *glanceView) []string {
	out := []string{}
	for _, a := range v.Attention {
		out = append(out, a.Kind)
	}
	return out
}

func TestGlanceNeedsYouOrder(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("root", 0, "open")
	f.event(r, 0, 0, "note", "OWNER: approve oldest todo", `{"owner":true}`, 48*time.Hour)
	ask := f.event(r, 0, 0, "ask", "older nonblocking", `{"owner":true}`, 24*time.Hour)
	newBlock := f.event(r, 0, 0, "ask", "new blocking", `{"owner":true,"blocking":true}`, time.Minute)
	oldBlock := f.event(r, 0, 0, "ask", "older blocking", `{"owner":true,"blocking":true}`, time.Hour)
	newAsk := f.event(r, 0, 0, "ask", "new nonblocking", `{"owner":true}`, 30*time.Second)
	v := f.view()
	ids := []int64{}
	for _, n := range v.NeedsYou {
		ids = append(ids, n.AskID)
	}
	if !reflect.DeepEqual(ids, []int64{oldBlock, newBlock, ask, newAsk}) {
		t.Fatalf("needs_you order = %v", ids)
	}
	rows := renderGlance(v, 80, 7, 0, "", false, watchTestNow)
	if !strings.Contains(strings.Join(rows, "\n"), "older blocking") {
		t.Fatalf("short frame dropped oldest blocking ask: %q", rows)
	}
}

func TestGlanceUnlaunchedGate(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("root", 0, "open")
	w := f.task("gate", r, "open")
	f.exec(`update tasks set role = 'gate' where id = ?`, w)
	f.event(w, 0, 0, "note", "planned gate", `{}`, 13*time.Hour)
	if v := f.view(); len(v.Campaigns) != 0 || v.Quiet.Count != 1 || v.Verdict != "rolling" {
		t.Fatalf("old unlaunched gate = %+v", v)
	}
	f.event(r, 0, 0, "note", "recent activity", `{}`, time.Minute)
	if v := f.view(); len(v.Campaigns) != 1 || v.Campaigns[0].Lanes != (glanceLanes{Open: 1}) {
		t.Fatalf("recent unlaunched gate = %+v", v)
	}
	f.launch(w, "working", true, nil)
	if v := f.view(); v.Campaigns[0].Lanes != (glanceLanes{Open: 1, Working: 1}) {
		t.Fatalf("launched gate = %+v", v)
	}
	for _, status := range []string{"failed", "ready"} {
		f := newGlanceFixture(t)
		r := f.task("root", 0, "open")
		g := f.task("gate", r, status)
		f.exec(`update tasks set role = 'gate' where id = ?`, g)
		f.event(g, 0, 0, "note", "gate ran unlaunched", `{}`, time.Minute)
		v := f.view()
		switch status {
		case "failed":
			if v.Verdict != "rolling" || len(v.Attention) != 0 {
				t.Fatalf("failed unlaunched gate = %+v", v)
			}
		case "ready":
			if len(v.Campaigns) != 1 || v.Campaigns[0].Lanes.Ready != 1 {
				t.Fatalf("ready unlaunched gate = %+v", v)
			}
		}
	}
}

func TestGlanceQuietBacklogVerdict(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("quiet", 0, "open")
	w := f.task("w", r, "done")
	f.event(w, r, 0, "fail", "unread failure", `{}`, 20*time.Hour)
	v := f.view()
	if v.Verdict != "rolling" || len(v.Attention) != 0 || v.Quiet.Count != 1 || len(v.Campaigns) != 0 {
		t.Fatalf("quiet result snapshot = %+v", v)
	}
	r2 := f.task("quiet2", 0, "open")
	w2 := f.task("w2", r2, "failed")
	f.event(w2, r2, 0, "fail", "failed lane", `{}`, 20*time.Hour)
	f.exec(`update tasks set acked_event_id = (select max(id) from events) where id = ?`, r2)
	v = f.view()
	if v.Verdict != "rolling" || len(v.NeedsYou) != 0 || len(v.Attention) != 0 || v.Quiet.Count != 2 || len(v.Campaigns) != 0 {
		t.Fatalf("quiet backlog snapshot = %+v", v)
	}
	f.exec(`delete from meta where key = ?`, heartbeatKey)
	if v := f.view(); v.Verdict != "unknown" || !reflect.DeepEqual(glanceKinds(v), []string{"daemon_unhealthy"}) {
		t.Fatalf("quiet backlog and unknown snapshot = %+v", v)
	}
}

func TestGlanceClosedIntermediate(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("root", 0, "open")
	mid := f.task("mid", r, "closed")
	w := f.task("deep", mid, "open")
	f.launch(w, "blocked", true, nil)
	v := f.view()
	if v.Verdict != "rolling" || len(v.Attention) != 0 || len(v.Campaigns) != 1 || v.Campaigns[0].Lanes.Open != 1 {
		t.Fatalf("snapshot = %+v", v)
	}
}

func TestGlanceOwnerAskStaleHost(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("root", 0, "open")
	w := f.task("worker", r, "open")
	l := f.launch(w, "blocked", true, "mac")
	f.exec(`insert into meta(key, value) values (?, ?)`, hostHeartbeatKey("mac"), stamp(f.at.Add(-time.Hour)))
	f.event(w, r, l, "ask", "approve X", `{"owner":true,"blocking":true}`, time.Minute)
	v := f.view()
	if v.Verdict != "needs_you" || !reflect.DeepEqual(glanceKinds(v), []string{"host_stale"}) || len(v.NeedsYou) != 1 || len(v.NeedsYou[0].Also) != 0 || v.NeedsYou[0].Asker != "worker" {
		t.Fatalf("snapshot = %+v", v)
	}
}

func TestGlanceSnapshot(t *testing.T) {
	for _, tc := range []struct {
		name string
		run  func(*testing.T, *glanceFixture)
	}{
		{"owner ask with folded lane marks and closed exclusions", func(t *testing.T, f *glanceFixture) {
			r := f.task("root", 0, "open")
			for i, mark := range []string{"failed", "blocked", "missing"} {
				status := "open"
				if mark == "failed" {
					status = "failed"
				}
				w := f.task(mark, r, status)
				l := f.launch(w, mark, mark != "missing", nil)
				f.exec(`update tasks set waiting_until = ? where id = ?`, stamp(f.at.Add(time.Minute)), w)
				text := "whole question\n" + strings.Repeat("q", 4100)
				f.event(w, r, l, "ask", text, `{"owner":true,"blocking":true}`, time.Duration(3-i)*time.Minute)
			}
			closed := f.task("closed", r, "closed")
			f.event(closed, r, 0, "ask", "closed task ask", `{"owner":true}`, time.Hour)
			cr := f.task("closed root", 0, "closed")
			orphan := f.task("open under closed", cr, "open")
			f.event(orphan, cr, 0, "ask", "closed tree ask", `{"owner":true}`, time.Hour)
			v := f.view()
			if len(v.NeedsYou) != 3 || len(v.Attention) != 0 || v.Verdict != "needs_you" {
				t.Fatalf("snapshot = %+v", v)
			}
			for i, n := range v.NeedsYou {
				mark := []string{"failed", "blocked", "missing"}[i]
				if !reflect.DeepEqual(n.Also, []string{"lane " + mark}) || n.Asker != mark || !*n.Blocking || !*n.AskerWaiting || n.PaneID != mark+":p1" || n.AskerTaskID == 0 || len(n.Text) < 4100 {
					t.Fatalf("ask = %+v", n)
				}
			}
		}},
		{"quiet failed lane and old owner ask", func(t *testing.T, f *glanceFixture) {
			r := f.task("quiet failed", 0, "open")
			w := f.task("failed", r, "failed")
			f.event(w, r, 0, "fail", "old failure", `{}`, 24*time.Hour)
			ar := f.task("old ask", 0, "open")
			f.event(ar, 0, 0, "ask", "still needs owner", `{"owner":true}`, 48*time.Hour)
			v := f.view()
			if v.Quiet.Count != 1 || !reflect.DeepEqual(v.Quiet.Names, []string{"quiet failed"}) || len(v.Attention) != 0 || len(v.NeedsYou) != 1 || len(v.Campaigns) != 1 || v.Campaigns[0].ID != ar {
				t.Fatalf("snapshot = %+v", v)
			}
		}},
		{"results at 29 and 31 minutes with waiting and sub recipient", func(t *testing.T, f *glanceFixture) {
			r := f.task("root", 0, "open")
			sub := f.task("sub", r, "open")
			f.exec(`update tasks set role = 'sub-orchestrator' where id = ?`, sub)
			w := f.task("worker", sub, "done")
			f.exec(`update tasks set waiting_until = ? where id in (?, ?)`, stamp(f.at.Add(time.Minute)), r, sub)
			f.event(w, r, 0, "ready", "31", `{}`, 31*time.Minute)
			f.event(w, r, 0, "done", "29", `{}`, 29*time.Minute)
			f.event(w, sub, 0, "fail", "sub result", `{}`, time.Hour)
			v := f.view()
			if len(v.Attention) != 0 || v.Verdict != "rolling" || v.Campaigns[0].Lead != "waiting" || !v.Campaigns[0].LeadWaiting {
				t.Fatalf("snapshot = %+v", v)
			}
		}},
		{"old launch stall excluded current included", func(t *testing.T, f *glanceFixture) {
			r := f.task("root", 0, "open")
			f.lead(r, "idle")
			w := f.task("worker", r, "open")
			old := f.launch(w, "working", true, nil)
			f.event(w, r, old, "herdr", "old stall", `{"reason":"stall"}`, time.Hour)
			current := f.launch(w, "working", true, nil)
			if len(f.view().Attention) != 0 {
				t.Fatal("old launch stall was included")
			}
			f.event(w, r, current, "herdr", "current stall", `{"reason":"stall"}`, 31*time.Minute)
			v := f.view()
			if len(v.Attention) != 1 || v.Attention[0].Count != 1 || v.Attention[0].Text != "worker herdr: current stall" {
				t.Fatalf("attention = %+v", v.Attention)
			}
		}},
		{"no receipt and quota limit included low excluded", func(t *testing.T, f *glanceFixture) {
			r := f.task("root", 0, "open")
			f.lead(r, "idle")
			w := f.task("worker", r, "open")
			old := f.launch(w, "working", true, nil)
			f.event(w, r, old, "prompt_outcome", "old receipt", `{"outcome":"no_receipt"}`, 2*time.Hour)
			l := f.launch(w, "working", true, nil)
			acked := f.event(w, r, l, "ready", "acked", `{}`, 2*time.Hour)
			f.exec(`update tasks set acked_event_id = ? where id = ?`, acked, r)
			f.event(w, r, l, "prompt_outcome", "no_receipt", `{"outcome":"no_receipt"}`, time.Hour)
			f.event(w, r, l, "herdr", "quota limit hit", `{"quota":"limit"}`, 40*time.Minute)
			f.event(w, r, l, "herdr", "quota low", `{"quota":"low"}`, 35*time.Minute)
			v := f.view()
			if len(v.Attention) != 1 || v.Attention[0].Count != 2 || v.Attention[0].Text != "worker herdr: quota limit hit" || v.Attention[0].AgeMS != time.Hour.Milliseconds() || *v.Attention[0].Waiting {
				t.Fatalf("attention = %+v", v.Attention)
			}
		}},
		{"60 open roots uncapped and per root milestone", func(t *testing.T, f *glanceFixture) {
			var oldest int64
			for i := 0; i < 60; i++ {
				r := f.task(fmt.Sprintf("root-%02d", i), 0, "open")
				if i == 0 {
					oldest = r
				}
				f.event(r, 0, 0, "decision", fmt.Sprintf("milestone-%d", i), `{}`, time.Duration(60-i)*time.Minute)
				f.event(r, 0, 0, "ask", fmt.Sprintf("ask-%d", i), `{"owner":true}`, time.Duration(60-i)*time.Minute)
			}
			v := f.view()
			if len(v.Campaigns) != 60 || len(v.NeedsYou) != 60 || v.Campaigns[59].ID != oldest || v.Campaigns[59].Last.Text != "milestone-0" || v.NeedsYou[0].RootID != oldest {
				t.Fatalf("snapshot = %+v", v)
			}
		}},
		{"stale client host is unknown", func(t *testing.T, f *glanceFixture) {
			r := f.task("root", 0, "open")
			f.exec(`update tasks set machine = 'mac' where id = ?`, r)
			f.lead(r, "working")
			for _, status := range []string{"open", "ready"} {
				w := f.task(status, r, status)
				f.launch(w, "blocked", false, "mac")
			}
			f.exec(`insert into meta(key, value) values (?, ?)`, hostHeartbeatKey("mac"), stamp(f.at.Add(-time.Hour)))
			v := f.view()
			if !reflect.DeepEqual(glanceKinds(v), []string{"host_stale", "lead_unknown"}) || v.Verdict != "unknown" || v.Campaigns[0].Lanes.Working != 0 || v.Campaigns[0].Lanes.Ready != 0 || v.Campaigns[0].Host != "mac" {
				t.Fatalf("snapshot = %+v", v)
			}
		}},
		{"failed lane plus stale host is attention", func(t *testing.T, f *glanceFixture) {
			r := f.task("root", 0, "open")
			w := f.task("failed", r, "failed")
			f.launch(w, "working", true, "mac")
			f.event(w, r, 0, "fail", "recent failure", `{}`, time.Minute)
			v := f.view()
			if !reflect.DeepEqual(glanceKinds(v), []string{"host_stale"}) || v.Verdict != "unknown" {
				t.Fatalf("snapshot = %+v", v)
			}
		}},
		{"healthy active campaign rolling and ref milestone", func(t *testing.T, f *glanceFixture) {
			r := f.task("root", 0, "open")
			w := f.task("working", r, "open")
			f.launch(w, "working", true, "mac")
			f.exec(`insert into meta(key, value) values (?, ?)`, hostHeartbeatKey("mac"), stamp(f.at))
			ready := f.task("ready", r, "ready")
			f.launch(ready, "working", true, nil)
			f.event(r, 0, 0, "note", "OWNER: nothing.", `{"owner":true}`, 2*time.Minute)
			f.event(w, 0, 0, "ref", "ignored summary", `{"key":"pr","value":"123"}`, time.Minute)
			f.event(w, 0, 0, "ref", "empty ref", `{"key":"pr","value":""}`, 0)
			v := f.view()
			if v.Verdict != "rolling" || len(v.Attention) != 0 || len(v.NeedsYou) != 0 || len(v.Campaigns) != 1 || v.Campaigns[0].Lanes != (glanceLanes{Working: 1, Ready: 1, Open: 2}) || v.Campaigns[0].Lead != "working" || v.Campaigns[0].Last.Text != "pr 123" {
				t.Fatalf("snapshot = %+v", v)
			}
		}},
		{"owner note milestone uses OWNER value", func(t *testing.T, f *glanceFixture) {
			r := f.task("root", 0, "open")
			f.event(r, 0, 0, "note", "OWNER: approve X\nDONE: checks", `{"owner":true}`, time.Minute)
			if got := f.view().Campaigns[0].Last.Text; got != "approve X" {
				t.Fatalf("last = %q", got)
			}
		}},
		{"daemon none and stale", func(t *testing.T, f *glanceFixture) {
			f.exec(`delete from meta where key = ?`, heartbeatKey)
			v := f.view()
			if v.Verdict != "unknown" || len(v.Attention) != 1 || v.Attention[0].Text != "the taskr daemon has no live Herdr connection; no heartbeat" {
				t.Fatalf("snapshot = %+v", v)
			}
			f.exec(`insert into meta(key, value) values (?, ?)`, heartbeatKey, stamp(f.at.Add(-time.Hour)))
			v = f.view()
			if v.Verdict != "unknown" || v.Attention[0].Text != "the taskr daemon's heartbeat stopped; Herdr events are not arriving" || v.Attention[0].AgeMS != time.Hour.Milliseconds() {
				t.Fatalf("snapshot = %+v", v)
			}
		}},
	} {
		t.Run(tc.name, func(t *testing.T) { tc.run(t, newGlanceFixture(t)) })
	}
}

func TestGlanceLeads(t *testing.T) {
	for _, tc := range []struct {
		name, status, verdict string
		kinds                 []string
		age                   time.Duration
		setup                 func(*glanceFixture, int64)
	}{
		{name: "working", status: "working", verdict: "rolling", kinds: []string{}},
		{name: "idle", status: "idle", verdict: "rolling", kinds: []string{}},
		{name: "done", status: "done", verdict: "rolling", kinds: []string{}},
		{name: "gone", status: "gone", verdict: "attention", kinds: []string{"lead_gone"}},
		{name: "blocked", status: "blocked", verdict: "attention", kinds: []string{"lead_blocked"}},
		{name: "stale client", status: "unknown", verdict: "unknown", kinds: []string{"host_stale", "lead_unknown"}, setup: func(f *glanceFixture, r int64) {
			f.exec(`update tasks set machine = 'mac' where id = ?`, r)
			f.lead(r, "working")
			f.exec(`insert into meta(key, value) values (?, ?)`, hostHeartbeatKey("mac"), stamp(f.at.Add(-time.Hour)))
		}},
		{name: "fresh client", status: "working", verdict: "rolling", kinds: []string{}, setup: func(f *glanceFixture, r int64) {
			f.exec(`update tasks set machine = 'mac' where id = ?`, r)
			f.lead(r, "working")
			f.exec(`insert into meta(key, value) values (?, ?)`, hostHeartbeatKey("mac"), stamp(f.at))
		}},
		{name: "quiet gone", status: "gone", verdict: "rolling", kinds: []string{}, age: 13 * time.Hour},
		{name: "waiting", status: "waiting", verdict: "rolling", kinds: []string{}, setup: func(f *glanceFixture, r int64) {
			f.exec(`update tasks set waiting_until = ? where id = ?`, stamp(f.at.Add(time.Minute)), r)
		}},
		{name: "never observed", status: "unknown", verdict: "unknown", kinds: []string{"lead_unknown"}, setup: func(f *glanceFixture, r int64) {
			f.exec(`update tasks set lead_status = null, lead_present = null, lead_observed_at = null where id = ?`, r)
		}},
		{name: "no pane", status: "unregistered", verdict: "rolling", kinds: []string{}, setup: func(f *glanceFixture, r int64) {
			f.exec(`update tasks set pane_id = null where id = ?`, r)
		}},
		{name: "no hub listing", status: "unknown", verdict: "unknown", kinds: []string{"lead_unknown"}, setup: func(f *glanceFixture, r int64) {
			f.lead(r, "working")
			f.exec(`delete from meta where key = ?`, leadListedKey)
		}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := newGlanceFixture(t)
			r := f.task("root", 0, "open")
			f.lead(r, tc.status)
			if tc.name == "waiting" {
				f.lead(r, "done")
			}
			f.event(r, 0, 0, "note", "progress", `{}`, tc.age)
			if tc.setup != nil {
				tc.setup(f, r)
			}
			v := f.view()
			if v.Verdict != tc.verdict || !reflect.DeepEqual(glanceKinds(v), tc.kinds) {
				t.Fatalf("snapshot = %+v", v)
			}
			if tc.age > glanceQuietAfter {
				if len(v.Campaigns) != 0 || v.Quiet.Count != 1 {
					t.Fatalf("quiet snapshot = %+v", v)
				}
				return
			}
			if len(v.Campaigns) != 1 || v.Campaigns[0].Lead != tc.status || v.Campaigns[0].LeadWaiting != (tc.name == "waiting") {
				t.Fatalf("campaigns = %+v", v.Campaigns)
			}
			raw, err := json.Marshal(v.Campaigns[0])
			if err != nil {
				t.Fatal(err)
			}
			if strings.Contains(string(raw), `"lead_waiting":true`) != (tc.name == "waiting") || strings.Contains(string(raw), `"lead_waiting":false`) {
				t.Fatalf("campaign JSON = %s", raw)
			}
			for _, a := range v.Attention {
				if !strings.HasPrefix(a.Kind, "lead_") {
					continue
				}
				pane, host, since := "root:p1", "", stamp(f.at.Add(-time.Minute))
				if tc.name == "stale client" {
					host = "mac"
				}
				if tc.name == "no pane" {
					pane = ""
				}
				if tc.name == "never observed" || tc.name == "no pane" {
					since = ""
				}
				text := map[string]string{
					"lead_gone":    "lead pane root:p1 is not in its host's agent list",
					"lead_blocked": "Herdr sees an approval or question dialog in the lead's pane",
					"lead_unknown": "lead liveness unknown: never observed or its host is not reporting",
				}[a.Kind]
				if a.Campaign != "root" || a.RootID != r || a.PaneID != pane || a.Host != host || a.Since != since || a.AgeMS != glanceAge(f.at, since) || a.Text != text {
					t.Fatalf("lead attention = %+v", a)
				}
			}
		})
	}
}

func TestGlanceLeadRanksAndPrecedence(t *testing.T) {
	for lead, lane := range map[string]string{"lead_blocked": "lane_blocked", "lead_gone": "lane_missing", "lead_unknown": "host_stale"} {
		if glanceRank(lead) != glanceRank(lane) {
			t.Errorf("%s rank differs from %s", lead, lane)
		}
	}
	f := newGlanceFixture(t)
	unknown := f.task("unknown", 0, "open")
	f.lead(unknown, "unknown")
	f.event(unknown, 0, 0, "note", "progress", `{}`, 0)
	gone := f.task("gone", 0, "open")
	f.lead(gone, "gone")
	f.event(gone, 0, 0, "note", "progress", `{}`, 0)
	if v := f.view(); v.Verdict != "attention" || !reflect.DeepEqual(glanceKinds(v), []string{"lead_gone", "lead_unknown"}) {
		t.Fatalf("actionable plus unknown = %+v", v)
	}
	f.event(gone, 0, 0, "ask", "approve", `{"owner":true}`, 0)
	if v := f.view(); v.Verdict != "needs_you" || len(v.Attention) != 2 {
		t.Fatalf("owner plus lead attention = %+v", v)
	}
}

func TestGlanceCLI(t *testing.T) {
	h := newHarness(t)
	for _, tc := range []struct {
		name string
		args []string
		env  map[string]string
		code int
	}{
		{"compact", []string{"glance"}, nil, 0},
		{"json", []string{"--json", "glance"}, nil, 0},
		{"json environment", []string{"glance"}, map[string]string{"TASKR_FORMAT": "json"}, 0},
		{"help", []string{"glance", "-h"}, nil, 0},
		{"watch fallback", []string{"glance", "--watch"}, nil, exitOK},
		{"unknown flag", []string{"glance", "--unknown"}, nil, exitUsage},
		{"positional refused", []string{"glance", "extra"}, nil, exitUsage},
	} {
		t.Run(tc.name, func(t *testing.T) {
			var out, errb bytes.Buffer
			if code := run(tc.args, h.getenv(tc.env), &out, &errb); code != tc.code {
				t.Fatalf("exit %d: %s %s", code, out.String(), errb.String())
			}
			if tc.code != 0 || tc.name == "help" || tc.name == "watch fallback" {
				return
			}
			raw := out.String()
			if tc.name == "compact" {
				if !strings.HasPrefix(raw, "j1 ") {
					t.Fatalf("compact = %q", raw)
				}
				raw = strings.TrimPrefix(raw, "j1 ")
			}
			var v map[string]any
			if err := json.Unmarshal([]byte(raw), &v); err != nil {
				t.Fatal(err)
			}
			for _, key := range []string{"needs_you", "attention", "campaigns"} {
				if _, ok := v[key].([]any); !ok {
					t.Fatalf("%s not an array: %s", key, raw)
				}
			}
		})
	}
}

func TestGlanceRPCFresh(t *testing.T) {
	r := newTwoHost(t)
	root := r.newTask("server-campaign", "orchestrator", 0)
	before := r.count(`select count(*) from requests`)
	first := r.want(0, "host-a", nil, "--request-key", "glance-fresh-key", "glance")
	if len(first["needs_you"].([]any)) != 0 {
		t.Fatalf("first = %v", first)
	}
	r.ok(nil, "ask", "approve server deployment", "--owner", "--as", id(root))
	second := r.want(0, "host-a", nil, "--request-key", "glance-fresh-key", "glance")
	needs := second["needs_you"].([]any)
	if len(needs) != 1 || needs[0].(map[string]any)["campaign"] != "server-campaign" || second["verdict"] != "needs_you" {
		t.Fatalf("second = %v", second)
	}
	if r.count(`select count(*) from requests`) != before {
		t.Fatal("glance stored an RPC request")
	}
	r.want(exitOK, "host-a", nil, "glance", "--watch")
	if err := filepath.WalkDir(r.homes["host-a"], func(path string, d os.DirEntry, err error) error {
		if err == nil && strings.HasSuffix(path, ".db") {
			t.Errorf("client opened a local DB: %s", path)
		}
		return err
	}); err != nil {
		t.Fatal(err)
	}
}

func TestGlanceSyntheticLatency(t *testing.T) {
	f := newGlanceFixture(t)
	lanes, roots := []int64{}, []int64{}
	for i := 0; i < 60; i++ {
		r := f.task(fmt.Sprintf("root-%02d", i), 0, "open")
		roots = append(roots, r)
		for j := 0; j < 10; j++ {
			lanes = append(lanes, f.task(fmt.Sprintf("lane-%02d-%02d", i, j), r, "open"))
		}
	}
	tx, err := f.db.Begin()
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	stmt, err := tx.Prepare(`insert into events(task_id, recipient_task_id, kind, summary, data, created_at) values (?, ?, ?, ?, '{}', ?)`)
	if err != nil {
		t.Fatal(err)
	}
	defer stmt.Close()
	for i := 0; i < 100000; i++ {
		lane := i % len(lanes)
		kind := "herdr"
		// Periodic reports on every lane, amid ordinary observation traffic.
		if (i/len(lanes))%10 == 0 {
			kind = "ready"
		}
		if _, err := stmt.Exec(lanes[lane], roots[lane/10], kind, "synthetic event", stamp(f.at.Add(-time.Duration(100000-i)*time.Second))); err != nil {
			t.Fatal(err)
		}
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
	start := time.Now()
	v := f.view()
	elapsed := time.Since(start)
	t.Logf("readGlance: 60 roots, 600 lanes, 100000 events: %s", elapsed)
	if len(v.Campaigns) != 60 {
		t.Fatalf("campaigns = %d", len(v.Campaigns))
	}
}
