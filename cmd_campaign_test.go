package main

import (
	"encoding/json"
	"fmt"
	"os"
	"reflect"
	"strings"
	"testing"
	"time"
)

func TestCampaignReadContract(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("demo-campaign", "orchestrator", 0, "--pane", "wDemo:p1")
	lane := h.newTask("demo-worker", "implementer", root, "--pane", "wDemo:p2")
	launch := h.launch(lane)
	closed := h.newTask("demo-closed", "reviewer", root)
	db := h.openDB()
	goal := campaignDoc(t, db, root, "goal", "", "# Demo goal\nShip the sample.", nil)
	marker := num(h.ok(nil, "note", "plan checkpoint", "--as", id(root)), "event_id")
	plan := campaignDoc(t, db, root, "plan", "", "# Demo plan", ptr(marker))
	brief := campaignDoc(t, db, lane, "brief", "", "# Demo brief", nil)
	campaignDoc(t, db, lane, "report", "", "# Demo report", nil)
	h.ok(nil, "decide", "--as", id(root), "keep demo simple")
	retired := num(h.ok(nil, "decide", "--as", id(root), "retire this"), "event_id")
	h.ok(nil, "decide", "--as", id(root), "--revoke", id(retired))
	h.ok(nil, "close", id(closed), "--outcome", "accepted")
	h.ok(nil, "set", id(lane), "pr=123", "pr.backend=123", "pr.frontend=124", "pr.title=Demo PR", "pr.state=open", "pr.ci=pass", "pr.review=approved")
	h.ok(nil, "next", id(root), "review the sample")
	ask := num(h.ok(as(lane, launch), "ask", "approve sample", "--owner", "--blocking"), "ask_id")
	answered := num(h.ok(as(lane, launch), "ask", "which sample"), "ask_id")
	answer := num(h.ok(nil, "answer", id(answered), "sample A"), "answer_id")
	if _, err := db.Exec(`update launches set observed_status = 'idle', observed_at = ?, present = 1 where id = ?`, now(), launch); err != nil {
		t.Fatal(err)
	}
	before := docCount(t, db, `select count(*) from events`)
	code, raw, stderr := h.compact(nil, "campaign", id(root))
	if code != exitOK || stderr != "" || strings.Count(raw, "\n") != 1 {
		t.Fatalf("compact = %d %q %q", code, raw, stderr)
	}
	compact := decodeFrame(t, raw)
	code, lines := h.run(nil, "campaign", id(root))
	if code != exitOK || len(lines) != 1 {
		t.Fatalf("json = %d %v", code, lines)
	}
	out := lines[0]
	for _, m := range []map[string]any{compact, out} {
		r := m["root"].(map[string]any)
		if r["pane_id"] != "wDemo:p1" || r["host"] != "" || r["next"] != "review the sample" {
			t.Fatal(r)
		}
		if !reflect.DeepEqual(m["goal"], []any{"# Demo goal", "Ship the sample."}) {
			t.Fatal(m["goal"])
		}
		p := m["plan"].(map[string]any)
		if num(p, "id") != plan.ID || num(p, "version") != 1 || num(p, "decisions_since") != 2 || num(p, "closed_since") != 1 || p["age_ms"] == nil {
			t.Fatal(p)
		}
		lanes := m["lanes"].([]any)
		if len(lanes) != 1 {
			t.Fatal(lanes)
		}
		l := lanes[0].(map[string]any)
		if num(l, "id") != lane || l["state"] != "idle" || l["pane_id"] != "wDemo:p2" || l["model"] != "claude-opus-5-5" || l["brief"] != true || l["report"] != true || l["age_ms"] == nil {
			t.Fatal(l)
		}
		asks := m["asks"].([]any)
		if len(asks) != 3 || num(asks[0].(map[string]any), "id") != ask || asks[0].(map[string]any)["open"] != true || asks[0].(map[string]any)["blocking"] != true || num(asks[2].(map[string]any), "id") != answer {
			t.Fatal(asks)
		}
		if len(m["decisions"].([]any)) != 1 || len(m["spark"].([]any)) != 24 {
			t.Fatal(m)
		}
		docs := m["docs"].([]any)
		found := map[int64]bool{}
		for _, d := range docs {
			row := d.(map[string]any)
			found[num(row, "id")] = true
			if row["body"] != nil {
				t.Fatal("eager body", row)
			}
		}
		if !found[goal.ID] || !found[plan.ID] || !found[brief.ID] {
			t.Fatal(docs)
		}
		prs := m["prs"].([]any)
		if len(prs) != 3 {
			t.Fatal(prs)
		}
		byKey := map[string]map[string]any{}
		for _, value := range prs {
			row := value.(map[string]any)
			byKey[row["key"].(string)] = row
		}
		for key, want := range map[string]string{"pr": "123", "pr.backend": "123", "pr.frontend": "124"} {
			row := byKey[key]
			if row == nil || row["value"] != want || fmt.Sprint(row["number"]) != want || row["lane"] != "demo-worker" || num(row, "task_id") != lane || len(row["refs"].([]any)) != 7 {
				t.Fatal(key, row)
			}
			for field, want := range map[string]string{"title": "Demo PR", "state": "open", "ci": "pass", "review": "approved"} {
				if key == "pr" && row[field] != want || key != "pr" && row[field] != nil {
					t.Fatal(key, field, row)
				}
			}
		}
	}
	// Bodies retain the existing raw doc-get contract.
	if code, raw, _ := h.compact(nil, "doc", "get", id(brief.ID)); code != exitOK || raw != "# Demo brief" {
		t.Fatalf("doc = %d %q", code, raw)
	}
	_, all := h.run(nil, "campaign", id(root), "--all")
	lanes := all[0]["lanes"].([]any)
	if len(lanes) != 2 || lanes[1].(map[string]any)["outcome"] != "accepted" || lanes[1].(map[string]any)["state"] != "closed" {
		t.Fatal(lanes)
	}
	if after := docCount(t, db, `select count(*) from events`); after != before {
		t.Fatalf("read wrote events: %d -> %d", before, after)
	}
}

func TestCampaignReadPagingAndEmpty(t *testing.T) {
	contractGuard(t)
	f := newGlanceFixture(t)
	root := f.task("empty", 0, "open")
	out, err := readTUICampaign(f.db, root, 1, false, f.at)
	if err != nil {
		t.Fatal(err)
	}
	for _, key := range []string{"lanes", "asks", "docs", "decisions", "log", "prs"} {
		if len(out[key].([]map[string]any)) != 0 {
			t.Fatal(key, out[key])
		}
	}
	if !reflect.DeepEqual(out["spark"], make([]int, 24)) || len(out["goal"].([]string)) != 0 {
		t.Fatal(out)
	}
	// More than 100 lanes are not silently lost; --page affects only the log.
	f.task("closed-before-open", root, "closed")
	for i := 0; i < 101; i++ {
		f.task(fmt.Sprintf("lane-%03d", i), root, "open")
		f.event(root, 0, 0, "note", fmt.Sprint(i), `{}`, 0)
	}
	detail, err := readCampaignDetail(f.db, root, 1, true, false)
	if err != nil || len(detail["lanes"].([]map[string]any)) != 101 || detail["total"] != 101 {
		t.Fatalf("SQL lane filter = %v %v", detail, err)
	}
	detail, err = readCampaignDetail(f.db, root, 1, true, true)
	if err != nil || len(detail["lanes"].([]map[string]any)) != 102 {
		t.Fatalf("all lanes = %v %v", detail, err)
	}
	out, err = readTUICampaign(f.db, root, 2, false, f.at)
	if err != nil {
		t.Fatal(err)
	}
	log := out["log"].([]map[string]any)
	if len(out["lanes"].([]map[string]any)) != 101 || len(log) != 1 || log[0]["text"] != "0" || out["log_page"].(map[string]any)["page"] != 2 {
		t.Fatal(out)
	}
}

func TestCampaignReadRPCFresh(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	root := num(r.want(exitOK, "host-a", nil, "new", "demo-root", "--role", "orchestrator", "--pane", "wDemo:p1"), "task_id")
	body := rpcBody(r.dir, nil, "same-read-key", "campaign", id(root))
	for _, want := range []string{"", "new step"} {
		if want != "" {
			r.want(exitOK, "host-a", nil, "next", id(root), want)
		}
		status, rep, _ := r.post("host-b", body)
		var out map[string]any
		if status != 200 || rep.Exit != exitOK || json.Unmarshal([]byte(rep.Stdout), &out) != nil {
			t.Fatalf("rpc = %d %+v", status, rep)
		}
		root := out["root"].(map[string]any)
		if root["host"] != "host-a" || root["pane_id"] != "wDemo:p1" || root["next"] != want {
			t.Fatal(root)
		}
	}
	if !freshCommands["campaign"] || storedCommand(body.Argv) || docCount(t, r.openDB(), `select count(*) from requests where key = 'same-read-key'`) != 0 {
		t.Fatal("campaign read cached")
	}
}

func TestCampaignReadValidationAndHint(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	lane := h.newTask("lane", "implementer", root)
	for _, args := range [][]string{{"campaign"}, {"campaign", "0"}, {"campaign", "bad"}, {"campaign", id(root), "--page", "0"}, {"campaign", id(root), "--unknown"}, {"campaign", id(root), "extra"}} {
		h.one(exitUsage, nil, args...)
	}
	h.one(exitReject, nil, "campaign", id(lane))
	h.one(exitReject, nil, "campaign", "999999")
	for _, args := range [][]string{{"campaign", "--help"}, {"--json", "campaign", "--help"}, {"help", "campaign"}} {
		code, raw, stderr := h.compact(nil, args...)
		if code != exitOK || !strings.Contains(raw, "campaign ID [--page N] [--all]") || stderr != "" {
			t.Fatalf("help = %d %q %q", code, raw, stderr)
		}
	}
	for _, pane := range []string{"", "wDemo:p1"} {
		args := []string{"new", "lead", "--role", "orchestrator"}
		if pane != "" {
			args = append(args, "--pane", pane)
		}
		code, _, stderr := h.compact(nil, args...)
		if code != exitOK || strings.Contains(stderr, "as your first act") != (pane == "") {
			t.Fatalf("hint = %d %q", code, stderr)
		}
		if pane == "" && !strings.Contains(stderr, "--pane <id>` as your first act\n") {
			t.Fatal(stderr)
		}
	}
	code, _, stderr := h.compact(nil, "new", "child-orchestrator", "--role", "orchestrator", "--parent", id(root))
	if code != exitOK || stderr != "" {
		t.Fatalf("child adopt hint = %d %q", code, stderr)
	}
	// A root with a captured goal still gets exactly one pane-registration hint.
	goalPath := h.dir + "/goal.md"
	if err := os.WriteFile(goalPath, []byte("# Demo goal"), 0600); err != nil {
		t.Fatal(err)
	}
	code, _, stderr = h.compact(nil, "new", "with-goal", "--role", "orchestrator", "--brief", goalPath)
	if code != exitOK || strings.Count(stderr, "\n") != 1 || !strings.Contains(stderr, "taskr adopt") {
		t.Fatalf("goal hint = %d %q", code, stderr)
	}
}

func TestCampaignReadAsksRefsAndStaleHost(t *testing.T) {
	contractGuard(t)
	f := newGlanceFixture(t)
	root := f.task("demo", 0, "open")
	closed := f.task("old-sub", root, "closed")
	lane := f.task("nested", closed, "open")
	launch := f.launch(lane, "working", true, "host-a")
	f.exec(`insert into meta(key,value) values (?,?)`, hostHeartbeatKey("host-a"), stamp(f.at.Add(-time.Hour)))
	f.event(lane, root, launch, "ask", "open nested", `{"owner":true}`, 0)
	f.event(closed, root, 0, "ask", "closed asker", `{"owner":true}`, 0)
	for i := 0; i < 21; i++ {
		ask := f.event(root, 0, 0, "ask", fmt.Sprint(i), `{}`, 0)
		answer := f.event(root, root, 0, "answer", "done", `{}`, 0)
		f.exec(`update events set answered_by = ? where id = ?`, answer, ask)
	}
	f.event(lane, 0, 0, "ref", "", `{"key":"pr","value":"https://example.test/demo/pull/123"}`, 0)
	f.event(lane, 0, 0, "ref", "", `{"key":"pr.signed","value":"+124"}`, 0)
	f.event(lane, 0, 0, "ref", "", `{"key":"pr.zero","value":"0"}`, 0)
	f.event(lane, 0, 0, "ref", "", `{"key":"pr.review","value":"45"}`, 0)
	f.event(root, 0, 0, "ref", "", `{"key":"pr","value":"2"}`, 0)
	f.event(root, 0, 0, "ref", "", `{"key":"pr","value":""}`, 0)
	f.event(root, 0, 0, "ref", "", `{"key":"pr.review","value":"metadata-only"}`, 0)
	out, err := readTUICampaign(f.db, root, 999, false, f.at)
	if err != nil {
		t.Fatal(err)
	}
	asks := out["asks"].([]map[string]any)
	if len(asks) != 41 || asks[0]["text"] != "open nested" || asks[1]["text"] != "20" {
		t.Fatal(asks)
	}
	lanes := out["lanes"].([]map[string]any)
	if len(lanes) != 1 || lanes[0]["state"] != "unknown" || lanes[0]["host"] != "host-a" || lanes[0]["depth"] != 2 {
		t.Fatal(lanes)
	}
	prs := out["prs"].([]map[string]any)
	if len(prs) != 3 || prs[0]["value"] != "https://example.test/demo/pull/123" || prs[0]["number"] != nil || prs[1]["value"] != "+124" || prs[1]["number"] != nil || prs[2]["number"] != uint64(0) {
		t.Fatal(prs)
	}
	for _, row := range prs {
		if row["key"] == "pr" && row["review"] != "45" || row["key"] != "pr" && row["review"] != nil || len(row["refs"].([]planRef)) != 4 {
			t.Fatalf("metadata misapplied: %v", row)
		}
	}
	all, err := readTUICampaign(f.db, root, 1, true, f.at)
	if err != nil || len(all["asks"].([]map[string]any)) != 42 {
		t.Fatalf("all = %v %v", all, err)
	}
	f.exec(`update tasks set status = 'closed' where id = ?`, root)
	archived, err := readTUICampaign(f.db, root, 1, false, f.at)
	if err != nil || len(archived["asks"].([]map[string]any)) != 40 || archived["root"].(map[string]any)["lead"] != "closed" {
		t.Fatalf("archive = %v %v", archived, err)
	}
}
