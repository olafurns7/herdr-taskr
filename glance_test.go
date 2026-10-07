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

func TestGlanceOwnerNotes(t *testing.T) {
	long := strings.Repeat("á", 4100) + " approve the release"
	for _, tc := range []struct {
		name, text string
		age        time.Duration
		want       []string
	}{
		{"older than 48 hours", "OWNER: approve deploy", 72 * time.Hour, []string{"approve deploy"}},
		{"OWNER value past 4000 runes", "OWNER: " + long + " DONE: checks passed", time.Minute, []string{long}},
		{"nothing clears", "OWNER: nothing.", time.Minute, nil},
		{"empty OWNER does not clear", "OWNER: ", time.Minute, []string{""}},
		{"nothing until approval stays", "OWNER: nothing until you approve X", time.Minute, []string{"nothing until you approve X"}},
		{"numbered items", " OWNER: 1. approve X\n2) approve Y\nNOW: waiting", time.Minute, []string{"approve X", "approve Y"}},
		{"single item", "OWNER: approve X HAPPENED: built", time.Minute, []string{"approve X"}},
		{"newest owner note without OWNER", "bookkeeping", time.Minute, []string{"old todo"}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := newGlanceFixture(t)
			r := f.task("root", 0, "open")
			old := f.event(r, 0, 0, "note", "OWNER: old todo", `{"owner":true}`, 100*time.Hour)
			note := f.event(r, 0, 0, "note", tc.text, `{"owner":true}`, tc.age)
			// A newer ordinary note does not replace the newest owner note.
			f.event(r, 0, 0, "note", "ordinary progress", `{}`, 0)
			v := f.view()
			if len(tc.want) == 0 {
				if len(v.NeedsYou) != 0 {
					t.Fatalf("needs_you = %+v", v.NeedsYou)
				}
				return
			}
			if len(v.NeedsYou) != 1 {
				t.Fatalf("needs_you = %+v", v.NeedsYou)
			}
			n := v.NeedsYou[0]
			age := tc.age
			if tc.name == "newest owner note without OWNER" {
				note, age = old, 100*time.Hour
			}
			if n.Kind != "owner_todo" || n.NoteID != note || !reflect.DeepEqual(n.Items, tc.want) || n.AgeMS != age.Milliseconds() || v.Verdict != "needs_you" {
				t.Fatalf("todo = %+v, want %v", n, tc.want)
			}
		})
	}
}

func TestGlanceOwnerItems(t *testing.T) {
	pending := "operator look at Pending cases d9ht12nonw5ztqkeah57jpaj (Dec 27-31 2026) and ugp8dnof985qay3seeq1111s (Apr 10-15 2027): no hotel booked."
	sync := "optional: Sync 9094875544961 (stale Refundable badge, check-in Oct 18)"
	for _, tc := range []struct {
		name, value string
		want        []string
	}{
		{"nothing", "nothing", nil},
		{"nothing period", "nothing.", nil},
		{"nothing yet", "nothing yet.", nil},
		{"nothing new live 51498", "nothing new. LANE PLAN (after your amendment): visual slices stay on Claude Opus high", nil},
		{"nothing now", "nothing now", nil},
		{"nothing answers live 50929", "nothing (your answers to ask 50880 are recorded as decision 50904).", nil},
		{"nothing install live 50850", "nothing yet (argent install is yours; I wait for it).", nil},
		{"nothing answers live 50723", "nothing (A1-A3 answered: in place on /chat; no freeze; nav controls in the rail, rename in the strip header).", nil},
		{"nothing comma asks", "nothing, but merge #4836 when green", []string{"nothing, but merge #4836 when green"}},
		{"nothing semicolon asks", "nothing yet; please approve X", []string{"nothing yet; please approve X"}},
		{"nothing semicolon", "nothing; waiting", []string{"nothing; waiting"}},
		{"nothing colon", "nothing: waiting", []string{"nothing: waiting"}},
		{"nothing dash", "nothing - waiting", []string{"nothing - waiting"}},
		{"trim and lowercase", " \tNOTHING YET. ", nil},
		{"nothing approval stays", "nothing until you approve X", []string{"nothing until you approve X"}},
		{"nothing yet approval stays", "nothing yet until you approve X", []string{"nothing yet until you approve X"}},
		{"no decision live 51793 stays", "no decision needed now.", []string{"no decision needed now."}},
		{"nothing prefix stays", "nothingness", []string{"nothingness"}},
		{"no first number", "PR 12. Then section 3) see (2026) here", []string{"PR 12. Then section 3) see (2026) here"}},
		{"sequential mixed markers", "1. approve PR 12. Then (2026) check\n2) deploy\t3. verify", []string{"approve PR 12. Then (2026) check", "deploy", "verify"}},
		{"stop at missing number", "1) approve X 3) later 4. still later", []string{"approve X 3) later 4. still later"}},
		{"later number before expected", "1) approve X 3) aside 2. approve Y 4) later", []string{"approve X 3) aside", "approve Y 4) later"}},
		{"marker boundary", "1)approve X 2)approve Y", []string{"approve X", "approve Y"}},
		{"decimal", "raise the disk to 1.5 TB", []string{"raise the disk to 1.5 TB"}},
		{"decimal in item", "1. bump the API to 2.1 and deploy", []string{"bump the API to 2.1 and deploy"}},
		{"next marker needs whitespace", "1)2) approval", []string{"2) approval"}},
		{"embedded number stays", "abc1) approve X x2) approve Y", []string{"abc1) approve X x2) approve Y"}},
		{"intro preserved", "merge choices: 1) approve X 2) approve Y", []string{"merge choices:", "approve X", "approve Y"}},
		{"51057 dates", "1) " + pending + " 2) " + sync + "; the new auto-sync fixes it only when an Expedia event arrives or the daily refresh reaches it.", []string{pending, sync + "; the new auto-sync fixes it only when an Expedia event arrives or the daily refresh reaches it."}},
		{"51010 dates", "1) PR #4833 (auto-sync on Expedia notifications) is green and merge-ready at 4f2b12ab3f; say yes to merge (a merge deploys trip-api). 2) " + pending + " 3) " + sync + ".", []string{"PR #4833 (auto-sync on Expedia notifications) is green and merge-ready at 4f2b12ab3f; say yes to merge (a merge deploys trip-api).", pending, sync + "."}},
		{"50875 dates", "1) PR #4833 (auto-sync on Expedia notifications) is open; review and say yes to merge when CI is green; I do not merge without it. 2) " + pending + " 3) " + sync + ".", []string{"PR #4833 (auto-sync on Expedia notifications) is open; review and say yes to merge when CI is green; I do not merge without it.", pending, sync + "."}},
		{"50730 dates", "1) have an operator look at 2 Pending manual cases with a future stay and no booked case for the same customer: d9ht12nonw5ztqkeah57jpaj (Dec 27-31 2026) and ugp8dnof985qay3seeq1111s (Apr 10-15 2027); no hotel is booked on them. 4 other future Pending cases look abandoned (a good booking covers the dates). 2) " + sync + ".", []string{"have an operator look at 2 Pending manual cases with a future stay and no booked case for the same customer: d9ht12nonw5ztqkeah57jpaj (Dec 27-31 2026) and ugp8dnof985qay3seeq1111s (Apr 10-15 2027); no hotel is booked on them. 4 other future Pending cases look abandoned (a good booking covers the dates).", sync + "."}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := glanceOwnerItems("OWNER: " + tc.value + " DONE: checks"); !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("items = %q, want %q", got, tc.want)
			}
		})
	}
}

func TestGlanceBookkeepingKeepsTodo(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("root", 0, "open")
	note := f.event(r, 0, 0, "note", "\t\n OWNER: merge #4836 when green. DONE: x", `{"owner":true}`, 5*time.Minute)
	f.event(r, 0, 0, "note", "LANES (corrected): visual -> Opus", `{"owner":true}`, time.Minute)
	v := f.view()
	if v.Verdict != "needs_you" || len(v.NeedsYou) != 1 || v.NeedsYou[0].NoteID != note || !reflect.DeepEqual(v.NeedsYou[0].Items, []string{"merge #4836 when green."}) {
		t.Fatalf("snapshot = %+v", v)
	}
	f.event(r, 0, 0, "note", "\u2003OWNER: nothing yet.", `{"owner":true}`, 0)
	f.event(r, 0, 0, "note", "bookkeeping after clear", `{"owner":true}`, 0)
	if v := f.view(); len(v.NeedsYou) != 0 {
		t.Fatalf("cleared snapshot = %+v", v)
	}
}

func TestGlanceQuietBacklogVerdict(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("quiet", 0, "open")
	w := f.task("w", r, "done")
	f.event(w, r, 0, "fail", "unread failure", `{}`, 20*time.Hour)
	v := f.view()
	if v.Verdict != "attention" || len(v.Attention) != 0 || v.Quiet.Count != 1 || v.Quiet.WithBacklog != 1 || len(v.Campaigns) != 0 {
		t.Fatalf("quiet result snapshot = %+v", v)
	}
	r2 := f.task("quiet2", 0, "open")
	w2 := f.task("w2", r2, "failed")
	f.event(w2, r2, 0, "fail", "failed lane", `{}`, 20*time.Hour)
	f.exec(`update tasks set acked_event_id = (select max(id) from events) where id = ?`, r2)
	v = f.view()
	if v.Verdict != "attention" || len(v.NeedsYou) != 0 || len(v.Attention) != 0 || v.Quiet.Count != 2 || v.Quiet.WithBacklog != 2 || len(v.Campaigns) != 0 {
		t.Fatalf("quiet backlog snapshot = %+v", v)
	}
	f.exec(`delete from meta where key = ?`, heartbeatKey)
	if v := f.view(); v.Verdict != "attention" || !reflect.DeepEqual(glanceKinds(v), []string{"daemon_unhealthy"}) {
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
	if v.Verdict != "attention" || !reflect.DeepEqual(glanceKinds(v), []string{"lane_blocked"}) || v.Attention[0].LaneID != w || v.Attention[0].RootID != r || len(v.Campaigns) != 1 || v.Campaigns[0].Lanes.Open != 1 {
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
	if v.Verdict != "needs_you" || !reflect.DeepEqual(glanceKinds(v), []string{"host_stale", "lane_unknown"}) || len(v.NeedsYou) != 1 || len(v.NeedsYou[0].Also) != 0 || v.NeedsYou[0].Asker != "worker" {
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
				if !reflect.DeepEqual(n.Also, []string{"lane " + mark}) || n.Asker != mark || !*n.Blocking || !*n.AskerWaiting || n.PaneID != "root:p1" || len(n.Text) < 4100 {
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
			if v.Quiet.Count != 1 || v.Quiet.WithBacklog != 1 || !reflect.DeepEqual(v.Quiet.Names, []string{"quiet failed"}) || len(v.Attention) != 0 || len(v.NeedsYou) != 1 || len(v.Campaigns) != 1 || v.Campaigns[0].ID != ar {
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
			if !reflect.DeepEqual(glanceKinds(v), []string{"results_waiting", "results_waiting"}) || v.Verdict != "attention" || v.Campaigns[0].Lead != "working" || !v.Campaigns[0].LeadWaiting {
				t.Fatalf("snapshot = %+v", v)
			}
			if v.Attention[0].RecipientID != sub || v.Attention[0].AgeMS != time.Hour.Milliseconds() || v.Attention[1].Text != "worker ready: 31" {
				t.Fatalf("attention = %+v", v.Attention)
			}
			for _, a := range v.Attention {
				if a.Count != 1 || !*a.Waiting {
					t.Fatalf("attention = %+v", a)
				}
			}
		}},
		{"old launch stall excluded current included", func(t *testing.T, f *glanceFixture) {
			r := f.task("root", 0, "open")
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
			if !reflect.DeepEqual(glanceKinds(v), []string{"host_stale", "lead_unknown", "lane_unknown", "lane_unknown"}) || v.Verdict != "unknown" || v.Campaigns[0].Lanes.Working != 0 || v.Campaigns[0].Lanes.Ready != 0 || v.Campaigns[0].Host != "mac" {
				t.Fatalf("snapshot = %+v", v)
			}
		}},
		{"failed lane plus stale host is attention", func(t *testing.T, f *glanceFixture) {
			r := f.task("root", 0, "open")
			w := f.task("failed", r, "failed")
			f.launch(w, "working", true, "mac")
			f.event(w, r, 0, "fail", "recent failure", `{}`, time.Minute)
			v := f.view()
			if !reflect.DeepEqual(glanceKinds(v), []string{"lane_failed", "host_stale"}) || v.Verdict != "attention" {
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
		{name: "waiting", status: "working", verdict: "rolling", kinds: []string{}, setup: func(f *glanceFixture, r int64) {
			f.exec(`update tasks set waiting_until = ? where id = ?`, stamp(f.at.Add(time.Minute)), r)
		}},
		{name: "never observed", status: "unknown", verdict: "unknown", kinds: []string{"lead_unknown"}, setup: func(f *glanceFixture, r int64) {
			f.exec(`update tasks set lead_status = null, lead_present = null, lead_observed_at = null where id = ?`, r)
		}},
		{name: "no pane", status: "unknown", verdict: "unknown", kinds: []string{"lead_unknown"}, setup: func(f *glanceFixture, r int64) {
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
			f.event(r, 0, 0, "note", "progress", `{}`, tc.age)
			if tc.setup != nil {
				tc.setup(f, r)
			}
			v := f.view()
			if v.Verdict != tc.verdict || !reflect.DeepEqual(glanceKinds(v), tc.kinds) {
				t.Fatalf("snapshot = %+v", v)
			}
			if tc.age > glanceQuietAfter {
				if len(v.Campaigns) != 0 || v.Quiet.Count != 1 || v.Quiet.WithBacklog != 0 {
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
					"lead_unknown": "lead liveness unknown: no pane, never observed, or its host is not reporting",
				}[a.Kind]
				if a.Campaign != "root" || a.RootID != r || a.PaneID != pane || a.Host != host || a.Since != since || a.AgeMS != glanceAge(f.at, since) || a.Text != text || a.LaneID != 0 {
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
	r.ok(nil, "note", "OWNER: approve server deployment", "--owner", "--as", id(root))
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
