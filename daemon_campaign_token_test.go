package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"syscall"
	"testing"
	"time"
)

func campaignHarness(t *testing.T) (*harness, *daemon) {
	t.Helper()
	h := newHarness(t)
	script := strings.Replace(fakeHerdr, "case \"$1 $2\" in", `case "$1 $2" in
"workspace list")
  if [ -p "$d/workspaces.hold" ]; then read line < "$d/workspaces.hold"; fi
  cat "$d/workspaces.json"
  exit "$(cat "$d/workspaces.exit" 2>/dev/null || echo 0)" ;;
"workspace report-metadata")
  exit "$(cat "$d/workspace.$3.exit" 2>/dev/null || echo 0)" ;;`, 1)
	h.write("herdr", script, 0o755)
	d := &daemon{db: h.openDB(), log: &daemonLog{}, sock: h.herdrSock}
	d.connected.Store(true)
	return h, d
}

func campaignPass(d *daemon) int {
	return d.pass().tokens
}

func campaignWorkspaces(h *harness, tokens map[string]map[string]string) {
	h.t.Helper()
	workspaces := []map[string]any{}
	for workspace, pair := range tokens {
		workspaces = append(workspaces, map[string]any{"workspace_id": workspace, "tokens": pair})
	}
	b, err := json.Marshal(map[string]any{"result": map[string]any{"workspaces": workspaces}})
	if err != nil {
		h.t.Fatal(err)
	}
	h.write("workspaces.json", string(b), 0o644)
}

func campaignTask(h *harness, d *daemon, name string, parent int64, workspace, pane string) int64 {
	h.t.Helper()
	role := "implementer"
	if parent == 0 {
		role = "orchestrator"
	}
	task := h.newTask(name, role, parent)
	if _, err := d.db.Exec(`update tasks set workspace_id = ?, pane_id = ? where id = ?`, workspace, pane, task); err != nil {
		h.t.Fatal(err)
	}
	return task
}

func campaignExec(t *testing.T, d *daemon, query string, args ...any) {
	t.Helper()
	if _, err := d.db.Exec(query, args...); err != nil {
		t.Fatal(err)
	}
}

func campaignWrite(workspace, campaign, parent string) string {
	s := "workspace|report-metadata|" + workspace + "|--source|taskr|"
	for i, name := range []string{"taskr_campaign", "taskr_parent"} {
		value := []string{campaign, parent}[i]
		if value == "" {
			s += "--clear-token|" + name + "|"
		} else {
			s += "--token|" + name + "=" + value + "|"
		}
	}
	return s
}

func campaignWrites(t *testing.T, h *harness, want ...string) {
	t.Helper()
	if got := h.calls("workspace|report-metadata|"); !reflect.DeepEqual(got, want) {
		t.Fatalf("workspace writes = %q, want %q", got, want)
	}
}

func TestDaemonCampaignRootAndLanes(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	campaignTask(h, d, "lane-b", root, "w2", "w2:p2")
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil})
	if n := campaignPass(d); n != 1 {
		t.Fatalf("first writes = %d, want 1", n)
	}
	campaignWrites(t, h, campaignWrite("w2", "root-a", "w1"))
	if got := h.calls("pane|report-metadata|"); len(got) != 0 {
		t.Fatalf("first pass changed pane baseline: %q", got)
	}
}

func TestDaemonCampaignSameWorkspaceTab(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	lane := campaignTask(h, d, "lane-a", root, "w1", "w1:p2")
	campaignExec(t, d, `update tasks set tab_id = 'w1:t2' where id = ?`, lane)
	campaignWorkspaces(h, map[string]map[string]string{"w1": {"taskr_campaign": "root-a", "taskr_parent": "w9"}})
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w1", "", ""))
}

func TestDaemonCampaignMixedRoots(t *testing.T) {
	for _, existing := range []bool{false, true} {
		t.Run(map[bool]string{false: "initial", true: "previously-written"}[existing], func(t *testing.T) {
			h, d := campaignHarness(t)
			root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
			other := campaignTask(h, d, "root-b", 0, "w3", "w3:p1")
			campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
			campaignWorkspaces(h, map[string]map[string]string{"w2": nil})
			if existing {
				campaignPass(d)
				campaignWorkspaces(h, map[string]map[string]string{"w2": {"taskr_campaign": "root-a", "taskr_parent": "w1"}})
			}
			campaignTask(h, d, "lane-b", other, "w2", "w2:p2")
			campaignPass(d)
			if existing {
				campaignWrites(t, h, campaignWrite("w2", "root-a", "w1"), campaignWrite("w2", "", ""))
			} else {
				campaignWrites(t, h)
			}
		})
	}
}

func TestDaemonCampaignClientRoot(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	campaignExec(t, d, `update tasks set machine = 'client' where id = ?`, root)
	campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil})
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w2", "root-a", ""))
}

func TestDaemonCampaignTopRoot(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	sub := campaignTask(h, d, "sub", root, "w2", "w2:p1")
	campaignTask(h, d, "lane-a", sub, "w3", "w3:p1")
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil, "w3": nil})
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w2", "root-a", "w1"), campaignWrite("w3", "root-a", "w1"))
}

func TestDaemonCampaignClosure(t *testing.T) {
	for _, closeRoot := range []bool{false, true} {
		t.Run(map[bool]string{false: "last-lane", true: "root"}[closeRoot], func(t *testing.T) {
			h, d := campaignHarness(t)
			root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
			lane := campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
			campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil})
			campaignPass(d)
			campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": {"taskr_campaign": "root-a", "taskr_parent": "w1"}})
			closed := lane
			if closeRoot {
				closed = root
			}
			campaignExec(t, d, `update tasks set status = 'closed' where id = ?`, closed)
			campaignPass(d)
			want := []string{campaignWrite("w2", "root-a", "w1")}
			if closeRoot {
				want = append(want, campaignWrite("w2", "root-a", ""))
			} else {
				want = append(want, campaignWrite("w2", "", ""))
			}
			campaignWrites(t, h, want...)
		})
	}
}

func TestDaemonCampaignRelaunch(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	lane := campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil, "w3": nil})
	campaignPass(d)
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": {"taskr_campaign": "root-a", "taskr_parent": "w1"}, "w3": nil})
	launch := h.launch(lane, "--pane", "w3:p1")
	h.setAgents("w3:p1/working/1")
	campaignExec(t, d, `update launches set workspace_id = 'w3' where id = ?`, launch)
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w2", "root-a", "w1"), campaignWrite("w2", "", ""), campaignWrite("w3", "root-a", "w1"))
	want := []string{"pane|report-metadata|w3:p1|--source|taskr|--token|taskr_state=open|--token|taskr_round=0|"}
	if got := h.calls("pane|report-metadata|"); !reflect.DeepEqual(got, want) {
		t.Fatalf("relaunch pane writes = %q, want %q", got, want)
	}
}

func TestDaemonCampaignUnchanged(t *testing.T) {
	h, d := campaignHarness(t)
	campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil})
	campaignPass(d)
	before := h.calls("")
	campaignPass(d)
	// The root's lead is observed from one agent listing per pass; nothing else runs.
	want := append(before[:len(before)-1:len(before)-1], "agent|list|", "")
	if got := h.calls(""); !reflect.DeepEqual(got, want) {
		t.Fatalf("unchanged pass called herdr: %q, want %q", got, want)
	}
}

func TestDaemonCampaignFirstReconcile(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	campaignWorkspaces(h, map[string]map[string]string{
		"w1": {"taskr_campaign": "root-a", "other": "keep"},
		"w2": {"taskr_campaign": "root-a", "taskr_parent": "w1"},
		"w3": {"taskr_campaign": "stale", "taskr_parent": "w4", "other": "keep"},
		"w4": {"taskr_parent": "w1"},
		"w5": nil,
	})
	campaignTask(h, d, "lane-b", root, "w5", "w5:p1")
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w1", "", ""), campaignWrite("w3", "", ""), campaignWrite("w4", "", ""), campaignWrite("w5", "root-a", "w1"))
	if n := len(h.calls("workspace|list|")); n != 1 {
		t.Fatalf("first reconcile list calls = %d, want 1", n)
	}
}

func TestDaemonCampaignFailureRetry(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	campaignTask(h, d, "lane-b", root, "w3", "w3:p1")
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil, "w3": nil})
	log := openDaemonLog(filepath.Join(h.dir, "campaign.log"))
	t.Cleanup(log.close)
	d.log = log
	h.write("workspace.w2.exit", "1", 0o644)
	if n := campaignPass(d); n != 1 {
		t.Fatalf("successful writes with failure = %d, want 1", n)
	}
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil, "w3": {"taskr_campaign": "root-a", "taskr_parent": "w1"}})
	h.write("workspace.w2.exit", "0", 0o644)
	campaignPass(d)
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w2", "root-a", "w1"), campaignWrite("w3", "root-a", "w1"), campaignWrite("w2", "root-a", "w1"))
	b, err := os.ReadFile(filepath.Join(h.dir, "campaign.log"))
	if err != nil || !strings.Contains(string(b), "workspace token write for w2 failed") {
		t.Fatalf("failed write log = %q, error %v", b, err)
	}
}

func TestProbeUnlistedWorkspaceListsEveryPass(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	// Include a lane: under the new rule the root workspace wants no tokens.
	campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	campaignWorkspaces(h, map[string]map[string]string{"w9": nil})
	for i := 0; i < 4; i++ {
		campaignPass(d)
	}
	if n := len(h.calls("workspace|list|")); n != 1 {
		t.Fatalf("unlisted workspaces caused %d list calls, want 1", n)
	}
	campaignWrites(t, h)
}

func TestProbeLostTokensNotRepaired(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil, "w3": nil})
	campaignPass(d)
	// The next list has no tokens, simulating their loss in Herdr.
	campaignTask(h, d, "lane-b", root, "w3", "w3:p1")
	campaignPass(d)
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w2", "root-a", "w1"), campaignWrite("w2", "root-a", "w1"), campaignWrite("w3", "root-a", "w1"))
	if n := len(h.calls("workspace|list|")); n != 2 {
		t.Fatalf("lost-token repair list calls = %d, want 2", n)
	}
}

func TestProbeClientRootTwoLaneWorkspaces(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	campaignExec(t, d, `update tasks set machine = 'client' where id = ?`, root)
	campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	campaignTask(h, d, "lane-b", root, "w3", "w3:p1")
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil, "w3": nil})
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w2", "root-a", ""), campaignWrite("w3", "root-a", ""))
}

func TestDaemonCampaignPaneTokensBeforeWorkspaceCalls(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	lane := campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil})
	campaignPass(d)
	before := len(h.calls("")) - 1 // calls("") includes the log's trailing empty line
	campaignExec(t, d, `update tasks set status = 'ready' where id = ?`, lane)
	campaignExec(t, d, `update tasks set status = 'closed' where id = ?`, root)
	campaignPass(d)
	calls := h.calls("")[before:]
	if len(calls) < 2 || calls[0] != "pane|report-metadata|w2:p1|--source|taskr|--token|taskr_state=ready|--token|taskr_round=0|" || calls[1] != "workspace|list|" {
		t.Fatalf("pane write must precede workspace calls: %q", calls)
	}
}

func TestDaemonHeartbeatBeforeBlockedWorkspaceList(t *testing.T) {
	h, d := campaignHarness(t)
	campaignWorkspaces(h, map[string]map[string]string{})
	setVar(t, &herdrListDeadline, 2*time.Second)
	if err := syscall.Mkfifo(filepath.Join(h.bin, "workspaces.hold"), 0o600); err != nil {
		t.Fatal(err)
	}
	done := make(chan struct{})
	go func() {
		d.pass()
		close(done)
	}()
	t.Cleanup(func() {
		select {
		case <-done:
		case <-time.After(5 * time.Second):
			t.Error("pass did not finish after workspace list deadline")
		}
	})
	eventually(t, "blocked workspace list", func() bool { return len(h.calls("workspace|list|")) == 1 })
	if _, present, err := getMeta(d.db, heartbeatKey); err != nil || !present {
		t.Fatalf("workspace list began before the first heartbeat: present=%v error=%v", present, err)
	}
	select {
	case <-done:
		t.Fatal("workspace list returned instead of blocking")
	default:
	}
}

func TestDaemonFailedWorkspaceListRetryGate(t *testing.T) {
	for _, wanted := range []bool{false, true} {
		t.Run(map[bool]string{false: "nothing-wanted", true: "wanted"}[wanted], func(t *testing.T) {
			h, d := campaignHarness(t)
			var root int64
			if wanted {
				root = campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
				campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
			}
			campaignWorkspaces(h, map[string]map[string]string{})
			h.write("workspaces.exit", "1", 0o644)
			for i := 0; i < 4; i++ {
				campaignPass(d)
			}
			if n := len(h.calls("workspace|list|")); n != 1 {
				t.Fatalf("failed list calls over four unchanged passes = %d, want 1", n)
			}
			if !wanted {
				root = campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
			}
			campaignTask(h, d, "lane-b", root, "w3", "w3:p1")
			for i := 0; i < 4; i++ {
				campaignPass(d)
			}
			if n := len(h.calls("workspace|list|")); n != 2 {
				t.Fatalf("failed list calls after a pair changed = %d, want 2", n)
			}
		})
	}
}

func TestDaemonFailedWorkspaceListRetriesOnFallback(t *testing.T) {
	h, d := campaignHarness(t)
	campaignWorkspaces(h, map[string]map[string]string{})
	h.write("workspaces.exit", "1", 0o644)
	setVar(t, &daemonFallback, 100*time.Millisecond)
	s := newFakeSocket(t)
	cx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		d.run(cx, s.path)
		close(done)
	}()
	t.Cleanup(func() {
		cancel()
		select {
		case <-done:
		case <-time.After(5 * time.Second):
			t.Error("daemon did not stop")
		}
	})
	c, _, _ := s.next()
	c.Write([]byte(ack))
	eventually(t, "failed list retry on fallback without further events", func() bool {
		return len(h.calls("workspace|list|")) >= 2
	})
}

func TestDaemonCampaignMissingLaunchDoesNotCount(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	lane := campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	launch := h.launch(lane, "--pane", "w2:p1")
	campaignExec(t, d, `update launches set workspace_id = 'w2', present = 0 where id = ?`, launch)
	campaignWorkspaces(h, map[string]map[string]string{
		"w1": nil,
		"w2": {"taskr_campaign": "root-a", "taskr_parent": "w1"},
	})
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w2", "", ""))
}

func TestDaemonCampaignPlannedTaskDoesNotCount(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	lane := campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	campaignExec(t, d, `update tasks set status = 'planned' where id = ?`, lane)
	campaignWorkspaces(h, map[string]map[string]string{
		"w1": nil,
		"w2": {"taskr_campaign": "root-a", "taskr_parent": "w1"},
	})
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w2", "", ""))
}

func TestDaemonCampaignGoodListClearsRetryGate(t *testing.T) {
	h, d := campaignHarness(t)
	root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
	campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
	campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil})
	h.write("workspaces.exit", "1", 0o644)
	campaignPass(d)
	campaignWrites(t, h)

	campaignExec(t, d, `update tasks set name = 'root-b' where id = ?`, root)
	h.write("workspaces.exit", "0", 0o644)
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w2", "root-b", "w1"))

	campaignWorkspaces(h, map[string]map[string]string{
		"w1": nil,
		"w2": {"taskr_campaign": "root-b", "taskr_parent": "w1"},
	})
	campaignExec(t, d, `update tasks set name = 'root-a' where id = ?`, root)
	campaignPass(d)
	campaignPass(d)
	campaignWrites(t, h, campaignWrite("w2", "root-b", "w1"), campaignWrite("w2", "root-a", "w1"))
	if n := len(h.calls("workspace|list|")); n != 3 {
		t.Fatalf("list calls after returning to the failed pair = %d, want 3", n)
	}
}

// Pane-token compatibility is checked against exact CLI arguments across all
// campaign transitions above; the existing pane test covers asks and waits.
func TestDaemonCampaignPaneCompatibility(t *testing.T) {
	for _, change := range []string{"lanes", "same-workspace", "mixed", "client-root", "sub", "close-lane", "close-root", "relaunch", "unchanged", "reconcile", "failure"} {
		t.Run(change, func(t *testing.T) {
			h, d := campaignHarness(t)
			root := campaignTask(h, d, "root-a", 0, "w1", "w1:p1")
			lane := campaignTask(h, d, "lane-a", root, "w2", "w2:p1")
			campaignWorkspaces(h, map[string]map[string]string{"w1": nil, "w2": nil, "w3": nil})
			campaignPass(d)
			campaignExec(t, d, `update tasks set status = 'ready' where id = ?`, lane)
			switch change {
			case "lanes":
				campaignTask(h, d, "lane-b", root, "w2", "w2:p2")
			case "same-workspace":
				campaignExec(t, d, `update tasks set workspace_id = 'w1', tab_id = 'w1:t2' where id = ?`, lane)
			case "mixed":
				other := campaignTask(h, d, "root-b", 0, "w3", "w3:p1")
				campaignExec(t, d, `update tasks set parent_id = ? where id = ?`, other, lane)
			case "client-root":
				campaignExec(t, d, `update tasks set machine = 'client' where id = ?`, root)
			case "sub":
				sub := campaignTask(h, d, "sub", root, "w3", "w3:p1")
				campaignExec(t, d, `update tasks set parent_id = ? where id = ?`, sub, lane)
			case "close-lane":
				campaignExec(t, d, `update tasks set status = 'closed' where id = ?`, lane)
			case "close-root":
				campaignExec(t, d, `update tasks set status = 'closed' where id = ?`, root)
			case "relaunch":
				launch := h.launch(lane, "--pane", "w3:p1")
				h.setAgents("w3:p1/working/1")
				campaignExec(t, d, `update launches set workspace_id = 'w3' where id = ?`, launch)
				campaignExec(t, d, `insert into events(task_id, launch_id, kind, created_at) values (?, ?, 'prompt', ?)`, lane, launch, now())
			case "unchanged":
				campaignExec(t, d, `update tasks set status = 'open' where id = ?`, lane)
			case "reconcile":
				// A new daemon still records a pane baseline even while
				// reconciling stale workspace metadata.
				d.tokens = nil
			case "failure":
				h.write("workspace.w2.exit", "1", 0o644)
				campaignExec(t, d, `update tasks set status = 'closed' where id = ?`, root)
			}
			campaignPass(d)
			campaignPass(d)
			pane := "w2:p1"
			round := "0"
			if change == "relaunch" {
				pane, round = "w3:p1", "1"
			}
			var want []string
			if change != "close-lane" && change != "unchanged" && change != "reconcile" {
				want = append(want, "pane|report-metadata|"+pane+"|--source|taskr|--token|taskr_state=ready|--token|taskr_round="+round+"|")
			}
			// Newly selected tasks retain the existing first-seen pane write.
			if change == "lanes" || change == "mixed" || change == "sub" {
				pane = "w3:p1"
				if change == "lanes" {
					pane = "w2:p2"
				}
				want = append(want, "pane|report-metadata|"+pane+"|--source|taskr|--token|taskr_state=open|--token|taskr_round=0|")
			}
			if got := h.calls("pane|report-metadata|"); !reflect.DeepEqual(got, want) {
				t.Fatalf("pane writes = %q, want %q", got, want)
			}
		})
	}
}
