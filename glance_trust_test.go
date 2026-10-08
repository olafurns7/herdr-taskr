package main

import (
	"fmt"
	"reflect"
	"strings"
	"testing"
	"time"
)

func TestGlanceTrustNotes(t *testing.T) {
	for _, tc := range []struct {
		text    string
		pending int
	}{
		{"OWNER: merge X DONE: built", 1}, {"DONE: built OWNER: merge X", 1},
		{"OWNER: nothing.", 0}, {"OWNER: nothing yet (waiting)", 0},
		{"OWNER: nothing urgent", 1}, {"OWNER: nothing until approval", 1},
		{"OWNER: no decision needed", 1}, {"bookkeeping", 1},
	} {
		t.Run(tc.text, func(t *testing.T) {
			f := newGlanceFixture(t)
			r := f.task("root", 0, "open")
			f.task("lane", r, "open")
			f.event(r, 0, 0, "note", "OWNER: old action", `{"owner":true}`, 100*time.Hour)
			note := f.event(r, 0, 0, "note", tc.text, `{"owner":true}`, 72*time.Hour)
			f.event(r, 0, 0, "note", "ordinary bookkeeping", `{}`, 0)
			v := f.view()
			if len(v.NeedsYou) != 0 || len(v.Attention) != 0 || v.OwnerNotesPending != tc.pending || len(v.Campaigns) != 1 {
				t.Fatalf("view = %+v", v)
			}
			n := v.Campaigns[0].OwnerNote
			if n == nil || n.EventID != note || n.Text != tc.text || n.AgeMS != (72*time.Hour).Milliseconds() {
				t.Fatalf("note = %+v", n)
			}
			ask := f.event(r, 0, 0, "ask", "merge X", `{"owner":true}`, 0)
			v = f.view()
			if len(v.NeedsYou) != 1 || v.NeedsYou[0].AskID != ask || v.NeedsYou[0].Kind != "owner_ask" || v.OwnerNotesPending != 0 {
				t.Fatalf("duplicate context = %+v", v)
			}
			f.exec(`update events set answered_by = ? where id = ?`, note, ask)
			if v := f.view(); len(v.NeedsYou) != 0 || v.OwnerNotesPending != tc.pending {
				t.Fatalf("answered = %+v", v)
			}
		})
	}
}

func TestGlanceTrustParking(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("root", 0, "open")
	f.lead(r, "gone")
	w := f.task("lane", r, "failed")
	f.event(w, r, 0, "fail", "old result", `{}`, 4*time.Hour)
	f.event(r, 0, 0, "ask", "owner action", `{"owner":true}`, 3*time.Hour)
	park := f.event(r, 0, 0, "ref", "glance.state=parked", `{"key":"glance.state","value":"parked"}`, 2*time.Hour)
	v := f.view()
	if len(v.NeedsYou) != 1 || len(v.Attention) != 0 || !v.Campaigns[0].Parked || v.Campaigns[0].ParkedActive || v.Campaigns[0].ParkAgeMS != (2*time.Hour).Milliseconds() {
		t.Fatalf("park = %+v", v)
	}
	// IDs, not timestamp order, establish activity after parking.
	f.event(w, r, 0, "note", "new event with old clock", `{}`, 5*time.Hour)
	v = f.view()
	if !v.Campaigns[0].ParkedActive || !reflect.DeepEqual(glanceKinds(v), []string{"parked_active"}) || len(v.NeedsYou) != 1 {
		t.Fatalf("stale park = %+v", v)
	}
	f.event(r, 0, 0, "ref", "unpark", `{"key":"glance.state","value":""}`, 0)
	v = f.view()
	if v.Campaigns[0].Parked || !reflect.DeepEqual(glanceKinds(v), []string{"lead_gone"}) {
		t.Fatalf("unpark after %d = %+v", park, v)
	}
}

func TestGlanceTrustIdleResults(t *testing.T) {
	for _, state := range []string{"idle", "done", "working", "unknown", "blocked", "gone"} {
		for _, lease := range []bool{false, true} {
			t.Run(fmt.Sprintf("%s/lease=%v", state, lease), func(t *testing.T) {
				f := newGlanceFixture(t)
				r := f.task("root", 0, "open")
				f.lead(r, state)
				w := f.task("worker", r, "done")
				sub := f.task("sub", r, "open")
				f.event(w, sub, 0, "fail", "sub backlog", `{}`, 3*time.Hour)
				result := f.event(w, r, 0, "ready", "old ready", `{}`, 31*time.Minute)
				f.event(w, r, 0, "done", "new done", `{}`, 29*time.Minute)
				if lease {
					f.exec(`update tasks set waiting_until = ? where id = ?`, stamp(f.at.Add(time.Minute)), r)
				}
				v := f.view()
				want := []string{}
				if !lease && (state == "idle" || state == "done") {
					want = append(want, "lead_idle_results")
				}
				if state == "unknown" || state == "blocked" || state == "gone" {
					want = append(want, "lead_"+state)
				}
				if !reflect.DeepEqual(glanceKinds(v), want) {
					t.Fatalf("view = %+v", v)
				}
				if len(want) > 0 && want[0] == "lead_idle_results" && (v.Attention[0].Count != 1 || v.Attention[0].AgeMS != (31*time.Minute).Milliseconds()) {
					t.Fatalf("results = %+v", v.Attention)
				}
				if lease && (state == "idle" || state == "done" || state == "working") && v.Campaigns[0].Lead != "waiting" {
					t.Fatalf("lease = %+v", v.Campaigns)
				}
				if lease && (state == "unknown" || state == "blocked" || state == "gone") && v.Campaigns[0].Lead != state {
					t.Fatalf("lease hid state: %+v", v.Campaigns)
				}
				f.exec(`update tasks set acked_event_id = ? where id = ?`, result, r)
				for _, a := range f.view().Attention {
					if a.Kind == "lead_idle_results" {
						t.Fatal("acked result still alarms")
					}
				}
			})
		}
	}
}

func TestGlanceTrustUnregistered(t *testing.T) {
	for _, age := range []time.Duration{119 * time.Minute, 2 * time.Hour, 3 * time.Hour} {
		for _, lanes := range []bool{false, true} {
			t.Run(fmt.Sprintf("%s/lanes=%v", age, lanes), func(t *testing.T) {
				f := newGlanceFixture(t)
				r := f.task("root", 0, "open")
				f.exec(`update tasks set pane_id = null where id = ?`, r)
				if lanes {
					f.task("worker", r, "open")
				}
				f.event(r, 0, 0, "note", "progress", `{}`, age)
				v := f.view()
				want := []string{}
				if lanes && age >= 2*time.Hour {
					want = []string{"lead_unregistered_silent"}
				}
				if !reflect.DeepEqual(glanceKinds(v), want) || v.Campaigns[0].Lead != "unregistered" {
					t.Fatalf("view = %+v", v)
				}
			})
		}
	}
}

func TestOwnerNoteTrustWarning(t *testing.T) {
	h := newHarness(t)
	r := h.newTask("root", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", r)
	launch := h.launch(lane)
	for _, text := range []string{"OWNER: merge X", "DONE: built OWNER: merge X", "OWNER: nothing.", "plain context"} {
		code, _, stderr := h.compact(nil, "note", text, "--owner", "--as", id(r))
		want := strings.Contains(text, "merge X")
		if code != exitOK || strings.Contains(stderr, "owner actions must be asks: taskr ask --owner ...") != want || (want && strings.Count(stderr, "\n") != 1) {
			t.Fatalf("note %q = %d %q", text, code, stderr)
		}
	}
	h.ok(as(lane, launch), "ask", "merge X", "--owner")
	_, _, stderr := h.compact(nil, "note", "OWNER: merge X", "--owner", "--as", id(r))
	if stderr != "" {
		t.Fatalf("restating ask warned: %q", stderr)
	}
	h.ok(nil, "close", id(lane))
	_, _, stderr = h.compact(nil, "note", "OWNER: merge X", "--owner", "--as", id(r))
	if !strings.Contains(stderr, "owner actions must be asks") {
		t.Fatalf("closed asker suppressed warning: %q", stderr)
	}
}

func TestGlanceStateRootOnly(t *testing.T) {
	h := newHarness(t)
	r := h.newTask("root", "orchestrator", 0)
	w := h.newTask("worker", "implementer", r)
	l := h.launch(w)
	for _, value := range []string{"parked", ""} {
		h.one(exitReject, as(w, l), "set", id(r), "glance.state="+value)
		h.one(exitReject, as(r, 0), "set", id(r), "glance.state="+value)
		h.one(exitReject, nil, "set", id(w), "glance.state="+value)
	}
	h.one(exitUsage, nil, "set", id(r), "glance.state=other")
	h.ok(nil, "set", id(r), "glance.state=parked")
	h.ok(nil, "set", id(r), "glance.state=")
}

func TestRenderGlanceTrust(t *testing.T) {
	v := &glanceView{Verdict: "rolling", OwnerNotesPending: 2, Campaigns: []glanceCampaign{
		{Name: "held", Parked: true, ParkAgeMS: (3 * time.Hour).Milliseconds()},
		{Name: "unregistered", Lead: "unregistered"},
		{Name: "waiting", Lead: "waiting"},
		{Name: "context", OwnerNote: &glanceLast{Text: "OWNER: merge X", AgeMS: (72 * time.Hour).Milliseconds()}},
	}}
	rows := renderGlance(v, 80, 24, 0, "", true, watchTestNow)
	all := strings.Join(rows, "\n")
	for _, want := range []string{"no owner action", "2 notes still carry OWNER items", "parked · 3h", "unregistered", "lead waiting", "merge X", "3d"} {
		if !strings.Contains(all, want) {
			t.Fatalf("missing %q: %q", want, rows)
		}
	}
	if !strings.HasPrefix(rows[2], "\x1b[2m") || !strings.HasPrefix(rows[3], "\x1b[2m") || strings.Contains(all, "\x1b[31m") {
		t.Fatalf("context colors: %q", rows)
	}
	v.Campaigns[0].ParkedActive = true
	if all := strings.Join(renderGlance(v, 80, 24, 0, "", true, watchTestNow), "\n"); !strings.Contains(all, "parked but active") || !strings.Contains(all, "\x1b[33m· held") {
		t.Fatalf("active park: %q", all)
	}
	v.OwnerNotesPending = 0
	if all := strings.Join(renderGlance(v, 80, 24, 0, "", false, watchTestNow), "\n"); strings.Contains(all, "still carry") {
		t.Fatal("zero migration count is visible")
	}
}

func TestGlanceTrustUnregisteredQuietLanes(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("root", 0, "open")
	f.exec(`update tasks set pane_id = null where id = ?`, r)
	w := f.task("failed", r, "failed")
	f.event(w, r, 0, "fail", "old unclosed lane", `{}`, 13*time.Hour)
	v := f.view()
	if !reflect.DeepEqual(glanceKinds(v), []string{"lead_unregistered_silent"}) || len(v.Campaigns) != 1 {
		t.Fatalf("silent unregistered tree must not fold away: %+v", v)
	}
}

func TestGlanceBookkeepingKeepsMigration(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("quiet-root", 0, "open")
	f.event(r, 0, 0, "note", "OWNER: approve synthetic release", `{"owner":true}`, 25*time.Hour)
	f.event(r, 0, 0, "note", "LANES: inventory updated", `{"owner":true}`, 24*time.Hour)
	f.event(r, 0, 0, "note", "LANES: moved (OWNER: approved earlier)", `{"owner":true}`, 24*time.Hour)
	v := f.view()
	if v.OwnerNotesPending != 1 || !reflect.DeepEqual(v.OwnerNoteRootIDs, []int64{r}) || v.Quiet.Count != 1 {
		t.Fatalf("migration hidden: %+v", v)
	}
	f.event(r, 0, 0, "note", "OWNER: nothing", `{"owner":true}`, 23*time.Hour)
	if v := f.view(); v.OwnerNotesPending != 0 || len(v.OwnerNoteRootIDs) != 0 {
		t.Fatalf("not cleared: %+v", v)
	}
}

func TestGlanceReparkAndHostGuard(t *testing.T) {
	h := newHarness(t)
	r := h.newTask("root", "orchestrator", 0)
	h.one(exitReject, map[string]string{"TASKR_LAUNCH": "1"}, "set", id(r), "glance.state=parked")
	first := h.ok(nil, "set", id(r), "glance.state=parked")
	second := h.ok(nil, "set", id(r), "glance.state=parked")
	if len(first["event_ids"].([]any)) != 1 || len(second["event_ids"].([]any)) != 1 || reflect.DeepEqual(first["event_ids"], second["event_ids"]) {
		t.Fatalf("repark = %v / %v", first, second)
	}
	db := h.openDB()
	if _, err := db.Exec(`update tasks set machine = 'other-host' where id = ?`, r); err != nil {
		t.Fatal(err)
	}
	h.one(exitReject, nil, "set", id(r), "glance.state=parked")
}

func TestGlanceSyntheticOwnerParser(t *testing.T) {
	for _, tc := range []struct {
		text  string
		items bool
	}{
		{"OWNER: merge demo", true}, {"DONE: built OWNER: deploy demo NOW: checking", true},
		{"plain bookkeeping", false}, {"LANES: updated", false}, {"XOWNER: merge", false},
		{"OWNER: nothing", false}, {"OWNER: NOTHING.", false}, {"OWNER: nothing yet", false},
		{"OWNER: nothing new", false}, {"OWNER: nothing now", false}, {"OWNER: nothing (waiting)", false},
		{"OWNER: nothing yet (waiting)", false}, {"OWNER: nothing new.", false}, {"OWNER: nothing now.", false},
		{"OWNER: nothing urgent", true}, {"OWNER: nothing until approval", true}, {"OWNER: no decision needed", true},
		{"OWNER: no action", true}, {"OWNER: 1. approve 2. deploy", true}, {"OWNER: review 2.5", true},
		{"OWNER: review 2027", true}, {"OWNER:", true}, {"OWNER: none", true},
		{"OWNER: nothing\tDONE: built", false}, {"OWNER: nothing\nNOW: checking", false},
		{"HAPPENED: built OWNER: approve DONE: checked", true}, {"OWNER: nothing DONE: approve", false},
		{"OWNER: nothingness", true}, {"OWNER: Nothing new (waiting)", false}, {"OWNER: nothing; approve", true},
		{"\tOWNER: approve", true},
	} {
		if got := ownerNoteHasItems(tc.text); got != tc.items {
			t.Errorf("%q: %v", tc.text, got)
		}
	}
}

func TestGlanceBookkeepingGluedOwner(t *testing.T) {
	for _, newer := range []string{"LANES: moved (OWNER: approved earlier)", "XOWNER: nothing", "LANES: moved, see note"} {
		t.Run(newer, func(t *testing.T) {
			f := newGlanceFixture(t)
			r := f.task("demo-root", 0, "open")
			f.event(r, 0, 0, "note", "OWNER: approve demo", `{"owner":true}`, 25*time.Hour)
			f.event(r, 0, 0, "note", newer, `{"owner":true}`, 24*time.Hour)
			v := f.view()
			if v.OwnerNotesPending != 1 || !reflect.DeepEqual(v.OwnerNoteRootIDs, []int64{r}) || v.Quiet.Count != 1 {
				t.Fatalf("glued label hid migration: %+v", v)
			}
		})
	}
}

func TestGlanceOwnerSQLSegmentBoundaries(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("demo-root", 0, "open")
	f.event(r, 0, 0, "note", "OWNER: approve demo", `{"owner":true}`, time.Hour)
	// Every unicode.IsSpace boundary must select the newest clearing segment.
	for _, boundary := range []string{"", " ", "\t", "\n", "\r", "\v", "\f", "\u0085", "\u00a0", "\u1680", "\u2000", "\u200a", "\u2028", "\u2029", "\u202f", "\u205f", "\u3000"} {
		text := boundary + "OWNER: nothing"
		if boundary != "" {
			text = "DONE: demo" + text
		}
		f.event(r, 0, 0, "note", text, `{"owner":true}`, 0)
		if v := f.view(); v.OwnerNotesPending != 0 || v.Campaigns[0].OwnerNote.Text != clip(oneLine(text), 120) {
			t.Fatalf("boundary %q: %+v", boundary, v)
		}
		f.event(r, 0, 0, "note", "OWNER: approve demo", `{"owner":true}`, 0)
	}
}
