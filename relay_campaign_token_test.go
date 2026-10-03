package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"modernc.org/sqlite"
	sqlite3 "modernc.org/sqlite/lib"
)

func clientCampaignHarness(t *testing.T) (*relay, *harness, *hostRelay) {
	t.Helper()
	r := newRelay(t)
	script := strings.Replace(fakeHerdr, `d="$(dirname "$0")"`, `d="$(dirname "$HERDR_SOCKET_PATH")"`, 1)
	script = strings.Replace(script, `case "$1 $2" in`, `case "$1 $2" in
"workspace list") cat "$d/workspaces.json"; exit 0 ;;
"workspace report-metadata") exit 0 ;;`, 1)
	r.write("herdr", script, 0o755)
	client := &harness{t: t, bin: r.hostADir}
	h := &hostRelay{raw: r.url, sock: r.hostASock,
		statePath: filepath.Join(r.homes["host-a"], ".local", "state", "taskr", clientStateFile), log: &daemonLog{}}
	r.caller.Store("host-a")
	return r, client, h
}

func clientCampaignTask(r *relay, name string, parent int64, workspace, machine string) int64 {
	r.t.Helper()
	d := &daemon{db: r.openDB()}
	task := campaignTask(r.harness, d, name, parent, workspace, workspace+":p1")
	campaignExec(r.t, d, `update tasks set machine = ? where id = ?`, nullStr(machine), task)
	return task
}

func clientCampaignReply(r *relay, machine string) map[string]any {
	r.t.Helper()
	_, rep, _ := r.post(machine, rpcBody(r.dir, nil, "campaign-observe", "_host", "observe", "--agents", "[]"))
	if rep.Exit != exitOK {
		r.t.Fatalf("observe exit %d: %s", rep.Exit, rep.Stdout)
	}
	return lastJSON(rep.Stdout)
}

func TestRelayCampaignTokens(t *testing.T) {
	r, client, h := clientCampaignHarness(t)
	root := clientCampaignTask(r, "root-a", 0, "w1", "host-a")
	clientCampaignTask(r, "lane-a", root, "w2", "host-a")
	remote := clientCampaignTask(r, "root-b", 0, "w1", "")
	clientCampaignTask(r, "lane-b", remote, "w3", "host-a")
	clientCampaignTask(r, "lane-c", root, "w4", "host-b")
	campaignWorkspaces(client, map[string]map[string]string{"w1": nil, "w2": nil, "w3": nil})
	m := clientCampaignReply(r, "host-a")
	want := map[string]any{
		"w2": map[string]any{"campaign": "root-a", "parent": "w1"},
		"w3": map[string]any{"campaign": "root-b"},
	}
	if !reflect.DeepEqual(m["workspace_tokens"], want) {
		t.Fatalf("workspace_tokens = %#v, want %#v", m["workspace_tokens"], want)
	}
	if err := h.pass(); err != nil {
		t.Fatal(err)
	}
	campaignWrites(t, client, campaignWrite("w2", "root-a", "w1"), campaignWrite("w3", "root-b", ""))
	before := client.calls("workspace|")
	if err := h.pass(); err != nil {
		t.Fatal(err)
	}
	if got := client.calls("workspace|"); !reflect.DeepEqual(got, before) {
		t.Fatalf("unchanged pass called herdr workspace: %q", got)
	}
	if got := r.calls("workspace|report-metadata|"); len(got) != 0 {
		t.Fatalf("client pass wrote server workspaces: %q", got)
	}
	m = clientCampaignReply(r, "host-b")
	if want := map[string]any{"w4": map[string]any{"campaign": "root-a"}}; !reflect.DeepEqual(m["workspace_tokens"], want) {
		t.Fatalf("host-b workspace_tokens = %#v, want %#v", m["workspace_tokens"], want)
	}
}

func TestRelayCampaignClosure(t *testing.T) {
	r, client, h := clientCampaignHarness(t)
	root := clientCampaignTask(r, "root-a", 0, "w1", "host-a")
	lane := clientCampaignTask(r, "lane-a", root, "w2", "host-a")
	campaignWorkspaces(client, map[string]map[string]string{"w1": nil, "w2": nil})
	if err := h.pass(); err != nil {
		t.Fatal(err)
	}
	campaignWorkspaces(client, map[string]map[string]string{
		"w1": nil, "w2": {"taskr_campaign": "root-a", "taskr_parent": "w1", "other": "keep"},
	})
	campaignExec(t, &daemon{db: r.openDB()}, `update tasks set status = 'closed' where id = ?`, lane)
	if err := h.pass(); err != nil {
		t.Fatal(err)
	}
	// Exact arguments prove that only taskr's two tokens are cleared.
	campaignWrites(t, client, campaignWrite("w2", "root-a", "w1"), campaignWrite("w2", "", ""))
	m := clientCampaignReply(r, "host-a")
	if want := map[string]any{}; !reflect.DeepEqual(m["workspace_tokens"], want) {
		t.Fatalf("empty host workspace_tokens = %#v, want {}", m["workspace_tokens"])
	}
}

func TestRelayCampaignPreserveWithoutReply(t *testing.T) {
	for _, mode := range []string{"older-server", "rejected", "rejected-malformed", "unreachable", "malformed"} {
		t.Run(mode, func(t *testing.T) {
			r, client, h := clientCampaignHarness(t)
			root := clientCampaignTask(r, "root-a", 0, "w1", "host-a")
			clientCampaignTask(r, "lane-a", root, "w2", "host-a")
			campaignWorkspaces(client, map[string]map[string]string{
				"w1": nil, "w2": {"taskr_campaign": "stale", "taskr_parent": "w9", "other": "keep"},
			})
			url, ln, srv := retryEndpoint(t, r.twoHost, func(w http.ResponseWriter, q *http.Request) {
				if mode == "unreachable" {
					w.WriteHeader(http.StatusServiceUnavailable)
					return
				}
				rec := httptest.NewRecorder()
				r.d.ServeHTTP(rec, q)
				var rep rpcReply
				if err := json.Unmarshal(rec.Body.Bytes(), &rep); err != nil {
					t.Error(err)
					return
				}
				switch mode {
				case "older-server":
					m := lastJSON(rep.Stdout)
					delete(m, "workspace_tokens")
					b, _ := json.Marshal(m)
					rep.Stdout = string(b)
				case "rejected":
					rep.Exit = exitReject
					rep.Stdout = `{"error":"rejected","workspace_tokens":{}}`
				case "rejected-malformed":
					rep.Exit = exitReject
					rep.Stdout = `not JSON`
				case "malformed":
					rep.Stdout = `{"workspace_tokens":{},broken}`
				}
				json.NewEncoder(w).Encode(rep)
			})
			go srv.Serve(ln)
			h.raw = url
			err := h.pass()
			if mode == "older-server" && err != nil {
				t.Fatal(err)
			}
			if mode != "older-server" && err == nil {
				t.Fatal("failed reply accepted")
			}
			if strings.HasPrefix(mode, "rejected") {
				var rejected *exitErr
				if !errors.As(err, &rejected) || rejected.code != exitReject || rejected.kind != "rejected" {
					t.Fatalf("rejected reply error = %v, want rejected exit %d", err, exitReject)
				}
			}
			if got := client.calls("workspace|"); len(got) != 0 {
				t.Fatalf("unavailable field caused workspace calls: %q", got)
			}
		})
	}
}

func TestRelayCampaignTokenFailureDeliversOwnerAsk(t *testing.T) {
	r, client, h := clientCampaignHarness(t)
	_, _, _, lane, launch := r.lanes()
	r.agents(r.hostADir, sharedPane+"/working/1")
	const summary = "Need owner decision"
	ask := num(r.want(0, "host-a", as(lane, launch), "ask", summary, "--owner"), "ask_id")
	logPath := filepath.Join(r.stateDir(), "daemon.log")
	campaignWorkspaces(client, map[string]map[string]string{"w5N": {"taskr_campaign": "stale", "taskr_parent": "w9"}})

	// Only the recursive token query uses a compound SELECT in this pass.
	// Restrict its real SQLite connection for one reply, then restore it.
	r.d.db.SetMaxOpenConns(1)
	setLimit := func(value int) int {
		t.Helper()
		conn, err := r.d.db.Conn(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		defer conn.Close()
		old, err := sqlite.Limit(conn, sqlite3.SQLITE_LIMIT_COMPOUND_SELECT, value)
		if err != nil {
			t.Fatal(err)
		}
		return old
	}
	old := setLimit(1)
	t.Cleanup(func() { setLimit(old) })
	replies := make(chan map[string]any, 2)
	url, ln, srv := retryEndpoint(t, r.twoHost, func(w http.ResponseWriter, q *http.Request) {
		rec := httptest.NewRecorder()
		r.d.ServeHTTP(rec, q)
		var rep rpcReply
		if err := json.Unmarshal(rec.Body.Bytes(), &rep); err != nil {
			t.Error(err)
			return
		}
		replies <- lastJSON(rep.Stdout)
		json.NewEncoder(w).Encode(rep)
	})
	go srv.Serve(ln)
	h.raw = url
	if err := h.pass(); err != nil {
		t.Fatal(err)
	}
	reply := <-replies
	if _, ok := reply["workspace_tokens"]; ok {
		t.Fatalf("failed token query included workspace_tokens: %v", reply)
	}
	if got := client.calls("notification|show|"); len(got) != 1 || !strings.Contains(got[0], "--body|"+summary+"|") {
		t.Fatalf("same-pass notifications = %q", got)
	}
	if got := client.calls("workspace|"); len(got) != 0 {
		t.Fatalf("failed token query caused workspace calls: %q", got)
	}
	if !reflect.DeepEqual(h.watch, []string{sharedPane}) {
		t.Fatalf("watch = %q", h.watch)
	}
	if fresh, err := hostFresh(r.d.db, "host-a"); err != nil || !fresh {
		t.Fatalf("heartbeat fresh = %v, err = %v", fresh, err)
	}
	if got := r.count(`select count(*) from meta where key = ?`, fmt.Sprintf("notified:%d", ask)); got != 1 {
		t.Fatalf("owner ask claims = %d, want 1", got)
	}
	log, err := os.ReadFile(logPath)
	if err != nil || strings.Count(string(log), "campaign token query failed:") != 1 {
		t.Fatalf("token failure log = %q, err = %v", log, err)
	}
	setLimit(old)
	if err := h.pass(); err != nil {
		t.Fatal(err)
	}
	reply = <-replies
	if _, ok := reply["workspace_tokens"]; !ok {
		t.Fatal("healthy reply omitted workspace_tokens")
	}
	if got := client.calls("notification|show|"); len(got) != 1 {
		t.Fatalf("owner ask notified again: %q", got)
	}
}

func TestRelayCampaignNotifiesBeforeTokenWrite(t *testing.T) {
	for _, failWrite := range []bool{false, true} {
		t.Run(fmt.Sprintf("fail-write=%v", failWrite), func(t *testing.T) {
			r, client, h := clientCampaignHarness(t)
			root := clientCampaignTask(r, "root-a", 0, "w1", "host-a")
			lane := clientCampaignTask(r, "lane-a", root, "w2", "host-a")
			r.want(0, "host-a", map[string]string{"TASKR_TASK": id(lane)}, "ask", "Need owner decision", "--owner")
			campaignWorkspaces(client, map[string]map[string]string{"w1": nil, "w2": nil})
			if failWrite {
				script, err := os.ReadFile(filepath.Join(r.bin, "herdr"))
				if err != nil {
					t.Fatal(err)
				}
				r.write("herdr", strings.Replace(string(script), `"workspace report-metadata") exit 0 ;;`, `"workspace report-metadata") exit 1 ;;`, 1), 0o755)
			}
			if err := h.pass(); err != nil {
				t.Fatal(err)
			}
			calls := client.calls("")
			notify, workspace := -1, -1
			for i, call := range calls {
				if strings.HasPrefix(call, "notification|show|") {
					notify = i
				}
				if workspace < 0 && strings.HasPrefix(call, "workspace|") {
					workspace = i
				}
			}
			if notify < 0 || workspace < 0 || notify >= workspace {
				t.Fatalf("notification must precede every workspace command: %q", calls)
			}
			campaignWrites(t, client, campaignWrite("w2", "root-a", "w1"))
			if failWrite && h.workspaceWriter.workspaceTokens["w2"].hasCampaign {
				t.Fatal("failed workspace write was accepted")
			}
		})
	}
}
