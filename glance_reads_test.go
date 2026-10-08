package main

import (
	"encoding/json"
	"reflect"
	"testing"
	"time"
)

func TestGlanceReadTargets(t *testing.T) {
	f := newGlanceFixture(t)
	r := f.task("campaign", 0, "open")
	w := f.task("asker", r, "open")
	f.exec(`update tasks set machine = 'old-host',pane_id = 'old-pane' where id = ?`, w)
	launch := f.launch(w, "working", true, "host-a")
	f.exec(`update launches set pane_id = 'wDemo:p2' where id = ?`, launch)
	f.exec(`insert into meta(key,value) values (?,?)`, hostHeartbeatKey("host-a"), stamp(f.at))
	ask := f.event(w, r, 0, "ask", "approve demo", `{"owner":true,"blocking":true}`, time.Minute)
	last := f.event(w, r, 0, "ready", "demo ready", `{}`, 0)
	f.exec(`update tasks set waiting_until = ? where id = ?`, stamp(f.at.Add(time.Minute)), r)
	q := f.task("quiet", 0, "open")
	f.event(q, 0, 0, "note", "old", `{}`, 25*time.Hour)
	v := f.view()
	n := v.NeedsYou[0]
	if n.AskerTaskID != w || n.AskID != ask || n.Host != "host-a" || n.PaneID != "wDemo:p2" {
		t.Fatalf("ask target = %+v", n)
	}
	c := v.Campaigns[0]
	if c.ID != r || c.Host != "" || c.PaneID != "campaign:p1" || !c.LeadWaiting || c.Lead != "waiting" || c.Last.EventID != last {
		t.Fatalf("campaign target = %+v", c)
	}
	if !reflect.DeepEqual(v.Quiet.RootIDs, []int64{q}) {
		t.Fatal(v.Quiet)
	}
	if v.ServerHost != localMachine() || v.CallerHost != "" {
		t.Fatalf("local hosts = %+v", v)
	}
}

func TestGlanceReadHostsRPC(t *testing.T) {
	r := newTwoHost(t)
	for _, host := range []string{"host-a", "host-b"} {
		v := r.want(exitOK, host, nil, "glance")
		if v["server_host"] != localMachine() || v["caller_host"] != host {
			t.Fatalf("%s hosts = %v", host, v)
		}
	}
	code, lines := r.run(nil, "glance")
	if code != exitOK || lines[0]["server_host"] != localMachine() || lines[0]["caller_host"] != "" {
		t.Fatalf("server hosts = %v", lines)
	}
}

func TestGlanceSparkBuckets(t *testing.T) {
	f := newGlanceFixture(t)
	f.at = time.Date(2026, 10, 8, 12, 5, 0, 0, time.UTC)
	r := f.task("spark", 0, "open")
	closed := f.task("closed-parent", r, "closed")
	nested := f.task("nested", closed, "open")
	empty := f.task("empty", 0, "open")
	other := f.task("other", 0, "open")
	archive := f.task("archive", 0, "closed")
	start := f.at.Truncate(10 * time.Minute).Add(-230 * time.Minute)
	add := func(task int64, at time.Time) { f.event(task, 0, 0, "note", "synthetic", `{}`, f.at.Sub(at)) }
	add(r, start.Add(-time.Millisecond)) // outside
	add(r, start)
	add(nested, start.Add(10*time.Minute-time.Millisecond))
	add(closed, start.Add(10*time.Minute))
	add(r, f.at)
	add(r, f.at.Add(time.Millisecond)) // future
	add(other, start)
	add(archive, start)
	got, err := campaignSparks(f.db, f.at, 0)
	if err != nil {
		t.Fatal(err)
	}
	want := make([]int, 24)
	want[0], want[1], want[23] = 2, 1, 1
	if !reflect.DeepEqual(got[r], want) || !reflect.DeepEqual(got[empty], make([]int, 24)) || got[other][0] != 1 || got[archive] != nil {
		t.Fatalf("sparks = %v", got)
	}
	archived, err := campaignSparks(f.db, f.at, archive)
	if err != nil || archived[archive][0] != 1 {
		t.Fatalf("archive = %v %v", archived, err)
	}
	// Both zero-event and quiet campaigns keep 24 buckets on the glance.
	v := f.view()
	for _, c := range v.Campaigns {
		if len(c.Spark) != 24 {
			t.Fatalf("spark missing: %+v", c)
		}
	}
	raw, err := json.Marshal(v)
	if err != nil || len(raw) == 0 {
		t.Fatal(err)
	}
}
