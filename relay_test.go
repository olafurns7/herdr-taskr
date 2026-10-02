package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"
)

// The relay harness: the two-host harness with one fake Herdr per host.
// The fake keeps its state beside the socket herdr is told to use, so the
// server (its own socket) and the host-a (a socket of its own) answer
// separately, and both have a lane in pane w5N:p2.
type relay struct {
	*twoHost
	srvDir, hostADir, hostASock string
	hostA                       map[string]string // the host-a CLI's env
	dir                         string
}

const sharedPane = "w5N:p2"

func newRelay(t *testing.T) *relay {
	r := &relay{twoHost: newTwoHost(t), dir: t.TempDir()}
	r.write("herdr", strings.Replace(fakeHerdr, `d="$(dirname "$0")"`, `d="$(dirname "$HERDR_SOCKET_PATH")"`, 1), 0o755)
	r.srvDir, r.hostASock = filepath.Dir(r.herdrSock), liveHerdrSocket(t)
	r.hostADir = filepath.Dir(r.hostASock)
	r.hostA = map[string]string{"HERDR_SOCKET_PATH": r.hostASock}
	r.agents(r.srvDir)
	r.agents(r.hostADir)
	return r
}

// agents writes one host's fake `herdr agent list`: pane/status/seq triples.
func (r *relay) agents(dir string, specs ...string) {
	r.t.Helper()
	agents := []map[string]any{}
	for _, s := range specs {
		p := strings.Split(s, "/")
		var seq int
		fmt.Sscan(p[2], &seq)
		agents = append(agents, map[string]any{"name": "a", "agent_status": p[1], "state_change_seq": seq, "pane_id": p[0]})
	}
	b, _ := json.Marshal(map[string]any{"result": map[string]any{"agents": agents}})
	if err := os.WriteFile(filepath.Join(dir, "list.json"), b, 0o644); err != nil {
		r.t.Fatal(err)
	}
}

// prompts lists one host's herdr agent prompt calls.
func (r *relay) prompts(dir string) []string {
	b, _ := os.ReadFile(filepath.Join(dir, "calls.log"))
	var out []string
	for _, l := range strings.Split(string(b), "\n") {
		if strings.HasPrefix(l, "agent|prompt|") {
			out = append(out, l)
		}
	}
	return out
}

// beat writes machine's daemon heartbeat as of age ago.
func (r *twoHost) beat(machine string, age time.Duration) {
	r.t.Helper()
	if err := setMeta(r.openDB(), hostHeartbeatKey(machine), stamp(time.Now().Add(-age))); err != nil {
		r.t.Fatal(err)
	}
}

// lanes makes a server-host lane and a host-a lane, both in sharedPane.
func (r *relay) lanes() (srvTask, srvLaunch, hostATop, hostATask, hostALaunch int64) {
	r.t.Helper()
	top := r.newTask("srv-top", "orchestrator", 0)
	srvTask = r.newTask("srv-impl", "implementer", top, "--pane", sharedPane)
	srvLaunch = r.launch(srvTask)
	hostATop = num(r.want(0, "host-a", nil, "new", "host-a-top", "--role", "orchestrator", "--cwd", r.dir), "task_id")
	hostATask = num(r.want(0, "host-a", nil, "new", "host-a-impl", "--role", "implementer", "--parent", id(hostATop),
		"--cwd", r.dir, "--pane", sharedPane), "task_id")
	hostALaunch = num(r.want(0, "host-a", nil, "launch", id(hostATask), "--provider", "claude", "--model", "m", "--effort", "high"), "launch_id")
	return
}

type obsRow struct {
	version int64
	status  string
	present bool
}

func (r *relay) obs(launch int64) obsRow {
	r.t.Helper()
	var o obsRow
	if err := r.openDB().QueryRow(`select observed_version, coalesce(observed_status, ''), present from launches where id = ?`, launch).
		Scan(&o.version, &o.status, &o.present); err != nil {
		r.t.Fatal(err)
	}
	return o
}

// statusOf is task's line of `status --tree top` on the server.
func (r *relay) statusOf(top, task int64) map[string]any {
	r.t.Helper()
	_, lines := r.run(nil, "status", "--tree", id(top))
	for _, l := range lines {
		if num(l, "id") == task {
			return l
		}
	}
	r.t.Fatalf("task %d not in status --tree %d: %v", task, top, lines)
	return nil
}

func TestRelayMachineFilter(t *testing.T) {
	r := newRelay(t)
	_, sl, mtop, mw, ml := r.lanes()
	// The server daemon observes its own lane only, although the host-a lane
	// has the same pane id.
	r.agents(r.srvDir, sharedPane+"/idle/1")
	r.ok(nil, "daemon", "--once")
	if o := r.obs(sl); o != (obsRow{1, "idle", true}) {
		t.Fatalf("server lane after the server pass = %+v", o)
	}
	if o := r.obs(ml); o.version != 0 {
		t.Fatalf("the server daemon wrote the host-a launch: %+v", o)
	}
	// The host-a relay applies its listing to its own lane only and returns
	// its watch list.
	r.agents(r.hostADir, sharedPane+"/working/3")
	m := r.want(0, "host-a", r.hostA, "daemon", "--once")
	if w, _ := m["watch"].([]any); len(w) != 1 || w[0] != sharedPane {
		t.Fatalf("host-a relay pass = %v", m)
	}
	if o := r.obs(ml); o != (obsRow{1, "working", true}) {
		t.Fatalf("host-a lane after the relay = %+v", o)
	}
	if o := r.obs(sl); o != (obsRow{1, "idle", true}) {
		t.Fatalf("the host-a relay wrote the server launch: %+v", o)
	}
	if h, _ := r.statusOf(mtop, mw)["observed"].(map[string]any); h["agent_status"] != "working" {
		t.Fatalf("host-a lane status with a fresh relay = %v", h)
	}
	// The server's pane goes away: its lane is missing, the host-a lane is not.
	r.agents(r.srvDir)
	r.ok(nil, "daemon", "--once")
	if o := r.obs(sl); o.present || o.version != 2 {
		t.Fatalf("server lane after its agent left = %+v", o)
	}
	if o := r.obs(ml); o != (obsRow{1, "working", true}) {
		t.Fatalf("the server daemon wrote the host-a launch: %+v", o)
	}
	// The host-a daemon stops: after 30 s its lane is unknown, and nothing marks
	// it missing although its agent is gone too.
	r.agents(r.hostADir)
	r.beat("host-a", 31*time.Second)
	r.ok(nil, "daemon", "--once")
	if h, _ := r.statusOf(mtop, mw)["observed"].(map[string]any); h["agent_status"] != "unknown" || h["host"] != "host-a" {
		t.Fatalf("host-a lane status with a stale relay = %v", h)
	}
	if o := r.obs(ml); o != (obsRow{1, "working", true}) {
		t.Fatalf("host-a lane with a stale relay = %+v", o)
	}
	// The relay comes back: the agent is missing, through the relay only.
	// The host-a lane owes a result without a receipt deadline, so the hint emits.
	r.want(0, "host-a", r.hostA, "prompt", id(mw), "--text", "go", "--receipt-timeout", "0")
	r.want(0, "host-a", r.hostA, "daemon", "--once")
	if o := r.obs(ml); o.present || o.version != 2 {
		t.Fatalf("host-a lane after its agent left = %+v", o)
	}
	if n := r.count(`select count(*) from events where kind = 'herdr' and task_id = ? and summary = 'missing from herdr agent list'`, mw); n != 1 {
		t.Fatalf("host-a lane missing events = %d", n)
	}
	if o := r.obs(sl); o.version != 2 {
		t.Fatalf("the host-a relay wrote the server launch: %+v", o)
	}
	// Without a Herdr server the relay sends nothing: no heartbeat, no change.
	r.beat("host-a", time.Minute)
	code, m, _ := r.cli("host-a", map[string]string{"HERDR_SOCKET_PATH": filepath.Join(r.dir, "none.sock")}, "daemon", "--once")
	if code != exitHerdr || m["ok"] != false {
		t.Fatalf("relay pass without Herdr = %d %v", code, m)
	}
	if fresh, _ := hostFresh(r.openDB(), "host-a"); fresh {
		t.Fatal("a relay pass without Herdr refreshed the heartbeat")
	}
}

func TestReviewMacOwnerNotification(t *testing.T) {
	r := newRelay(t)
	_, _, _, mw, ml := r.lanes()
	r.agents(r.hostADir, sharedPane+"/working/1")
	summary := "Need owner decision"
	ask := num(r.want(0, "host-a", as(mw, ml), "ask", summary, "--owner"), "ask_id")
	r.ok(nil, "daemon", "--once")
	r.want(0, "host-a", r.hostA, "daemon", "--once")
	r.want(0, "host-a", r.hostA, "daemon", "--once")
	b, _ := os.ReadFile(filepath.Join(r.hostADir, "calls.log"))
	want := "notification|show|taskr: decision needed|--body|" + summary + "|--sound|request|"
	n := strings.Count(string(b), want)
	claimed := r.count(`select count(*) from meta where key = ?`, fmt.Sprintf("notified:%d", ask))
	if n != 1 || claimed != 1 {
		t.Fatalf("host-a owner ask: notifications=%d claims=%d; want one each", n, claimed)
	}
	b, _ = os.ReadFile(filepath.Join(r.srvDir, "calls.log"))
	if strings.Contains(string(b), "notification|") {
		t.Fatal("server notified a host-a ask")
	}
}

func TestRelayPrompt(t *testing.T) {
	r := newRelay(t)
	sw, _, _, mw, ml := r.lanes()
	attempts := func(task int64) int {
		return r.count(`select count(*) from events where kind = 'prompt' and task_id = ?`, task)
	}
	// A host-a caller prompts a host-a lane: the host-a's Herdr delivers it once, and
	// the central ledger has the attempt and its outcome.
	m := r.want(0, "host-a", r.hostA, "prompt", id(mw), "--text", "go")
	at := num(m, "attempt_id")
	if p := r.prompts(r.hostADir); len(p) != 1 || !strings.HasPrefix(p[0], fmt.Sprintf("agent|prompt|%s|First taskr got %d. go|", sharedPane, at)) {
		t.Fatalf("host-a Herdr prompts = %q", p)
	}
	if len(r.prompts(r.srvDir)) != 0 || attempts(mw) != 1 || m["outcome"] != "activity_observed" ||
		r.count(`select count(*) from events where kind = 'prompt_outcome' and related_event_id = ? and summary = 'activity_observed'`, at) != 1 {
		t.Fatalf("host-a prompt = %v; server prompts %q", m, r.prompts(r.srvDir))
	}
	// --confirm gets the worker's receipt.
	var once sync.Once
	setVar(t, &receiptPolled, func(a int64) {
		once.Do(func() { r.cli("host-a", as(mw, ml), "got", id(a)) })
	})
	m = r.want(0, "host-a", r.hostA, "prompt", id(mw), "--text", "again", "--confirm", "--confirm-timeout", "5000")
	if m["receipt"] != true || len(r.prompts(r.hostADir)) != 2 {
		t.Fatalf("host-a prompt --confirm = %v", m)
	}
	// A host-a caller prompts a server lane: the server delivers it.
	r.want(0, "host-a", r.hostA, "prompt", id(sw), "--text", "srv")
	if len(r.prompts(r.srvDir)) != 1 || len(r.prompts(r.hostADir)) != 2 || attempts(sw) != 1 {
		t.Fatalf("server lane: server prompts %q, host-a prompts %q", r.prompts(r.srvDir), r.prompts(r.hostADir))
	}
	// A host-b caller, or the server's CLI, prompting the host-a lane: exit 5,
	// no attempt row, no herdr call.
	m = r.want(exitHerdr, "host-b", nil, "prompt", id(mw), "--text", "x")
	if !strings.Contains(fmt.Sprint(m["error"]), "lane host-a-impl is on host-a; prompt it from that host (start a sub-orchestrator there)") {
		t.Fatalf("host-b prompt = %v", m)
	}
	r.one(exitHerdr, nil, "prompt", id(mw), "--text", "x")
	if attempts(mw) != 2 || len(r.prompts(r.hostADir)) != 2 || len(r.prompts(r.srvDir)) != 1 {
		t.Fatalf("a refused prompt left an attempt or a herdr call: %d", attempts(mw))
	}
	// The host-a's Herdr is down: no attempt.
	r.want(exitHerdr, "host-a", map[string]string{"HERDR_SOCKET_PATH": filepath.Join(r.dir, "none.sock")}, "prompt", id(mw), "--text", "x")
	if attempts(mw) != 2 {
		t.Fatal("a prompt without the host-a's Herdr recorded an attempt")
	}
	// The hidden calls: not on the local CLI, and only for the caller's host.
	r.one(exitUsage, nil, "_prompt", "begin", id(mw), "--text", "x", "--local-herdr")
	_, rep, _ := r.post("host-b", rpcBody(r.dir, nil, "hidden-key-1", "_prompt", "begin", id(mw), "--text", "x", "--local-herdr"))
	if rep.Exit != exitHerdr || attempts(mw) != 2 {
		t.Fatalf("host-b _prompt begin = %+v", rep)
	}
	_, rep, _ = r.post("host-b", rpcBody(r.dir, nil, "hidden-key-2", "_prompt", "outcome", id(at), "--outcome", "rejected"))
	if rep.Exit != exitReject {
		t.Fatalf("host-b _prompt outcome = %+v", rep)
	}
	_, rep, _ = r.post("host-a", rpcBody(r.dir, nil, "hidden-key-3", "_prompt", "outcome", id(at), "--outcome", "rejected"))
	if rep.Exit != exitReject || !strings.Contains(rep.Stdout, "already has an outcome") {
		t.Fatalf("second _prompt outcome = %+v", rep)
	}
	_, rep, _ = r.post("host-b", rpcBody(r.dir, nil, "hidden-key-4", "_host", "observe", "--agents", "[]"))
	if rep.Exit != exitOK || r.obs(ml).version != 0 {
		t.Fatalf("host-b _host observe = %+v; host-a launch %+v", rep, r.obs(ml))
	}
	if n := r.count(`select count(*) from requests where key like 'hidden-key-%'`); n != 0 {
		t.Fatalf("%d hidden calls were stored", n)
	}
}

func TestRelayMachineFlagAndLimit(t *testing.T) {
	r := newTwoHost(t)
	m := r.want(exitUsage, "host-a", nil, "new", "x", "--role", "gate", "--machine", "laptop")
	if e := fmt.Sprint(m["error"]); !strings.Contains(e, localMachine()) || !strings.Contains(e, "host-a") || strings.Contains(e, "laptop,") {
		t.Fatalf("--machine of an unknown host = %v", m)
	}
	r.beat("laptop", 0)
	if x := num(r.want(0, "host-a", nil, "new", "x", "--role", "gate", "--machine", "laptop"), "task_id"); r.machineOf("tasks", x) != "laptop" {
		t.Fatalf("--machine laptop stored %s", r.machineOf("tasks", x))
	}
	r.beat("laptop", 31*time.Second)
	m = r.want(exitUsage, "host-a", nil, "new", "y", "--role", "gate", "--machine", "laptop")
	if strings.Contains(fmt.Sprint(m["error"]), ", laptop") {
		t.Fatalf("a stale host is listed: %v", m)
	}
	for _, args := range [][]string{{"log", "1", "--limit", "-1"}, {"asks", "--limit", "-1"}} {
		if m := r.one(exitUsage, nil, args...); m["error"] != "--limit must be >= 0, got -1" {
			t.Fatalf("%v = %v", args, m)
		}
	}
}

func TestRelayDaemonLoop(t *testing.T) {
	r := newRelay(t)
	setVar(t, &clientObserveEvery, 100*time.Millisecond)
	_, _, mtop, mw, ml := r.lanes()
	s := newFakeSocket(t)
	r.hostA = map[string]string{"HERDR_SOCKET_PATH": s.path}
	r.hostADir = filepath.Dir(s.path)
	r.agents(r.hostADir, sharedPane+"/working/2")
	r.caller.Store("host-a") // every request below is the host-a's
	var out bytes.Buffer
	done := make(chan int)
	go func() {
		done <- cliMain([]string{"--json", "daemon"}, clientEnv(r.homes["host-a"], r.hostA), &out, io.Discard)
	}()
	// The loop subscribes with the server's watch list.
	for {
		c, _, req := s.next()
		c.Write([]byte(ack))
		if slices.Equal(panesOf(req), []string{sharedPane}) {
			break
		}
	}
	eventually(t, "host-a lane observed", func() bool { return r.obs(ml).status == "working" })
	if fresh, _ := hostFresh(r.openDB(), "host-a"); !fresh {
		t.Fatal("no fresh host-a heartbeat")
	}
	if h, _ := r.statusOf(mtop, mw)["observed"].(map[string]any); h["agent_status"] != "working" {
		t.Fatalf("host-a lane status = %v", h)
	}
	m := r.want(0, "host-a", r.hostA, "daemon", "--status")
	if m["mode"] != "client" || m["server"] != r.url || m["running"] != true || m["last_call_at"] == nil || m["last_error"] != nil {
		t.Fatalf("client daemon --status = %v", m)
	}
	s.shutdown()
	select {
	case code := <-done:
		if code != 0 {
			t.Fatalf("client daemon exit %d: %s", code, out.String())
		}
	case <-time.After(15 * time.Second):
		t.Fatal("client daemon did not exit")
	}
	if m := r.want(0, "host-a", r.hostA, "daemon", "--status"); m["running"] != false {
		t.Fatalf("client daemon --status after exit = %v", m)
	}
}
