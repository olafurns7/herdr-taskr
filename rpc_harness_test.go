package main

import (
	"bufio"
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"
)

// The two-host harness: one scratch ledger served by a hub dashboard over
// real HTTP on [::1], and two client hosts, `host-a` and `host-b`, told apart
// by the fake whois. The server's own label is this machine's hostname; its
// local CLI (h.run) is the server host, with no machine.
type twoHost struct {
	*harness
	d           *dashboard
	url         string
	caller      atomic.Value // the whois identity of the next request: host-a, host-b, ...
	homes       map[string]string
	callerFile  string
	loopbackURL string
}

func newTwoHost(t *testing.T) *twoHost {
	t.Setenv("TASKR_CONTRACT_TAILNET", "1")
	fakeTailnetHooks(t)
	setVar(t, &whoisTTL, time.Nanosecond)
	h := newHarness(t)
	tailnet(hubIP, h)
	const sfx = ".example.ts.net."
	for k, v := range map[string]string{
		"host-a":   whoisJSON("nHOSTA", "host-a"+sfx, owner, nil),
		"host-b":   whoisJSON("nHOSTB", "host-b"+sfx, owner, nil),
		"self":     whoisJSON(hubNode, "hub"+sfx, owner, nil),
		"samename": whoisJSON("nSAME", localMachine()+sfx, owner, nil),
		"tagged":   whoisJSON("nTAGOWN", "build-box"+sfx, owner, []string{"tag:ci"}),
		"other":    whoisJSON("nOTHER", "eve-laptop"+sfx, "eve@example.com", nil),
	} {
		h.write("ts.whois.as-"+k, v, 0o644)
	}
	ln, err := net.Listen("tcp", "[::1]:0")
	if err != nil {
		t.Fatal(err)
	}
	_, port, _ := net.SplitHostPort(ln.Addr().String())
	self, err := tailscaleSelf()
	if err != nil {
		t.Fatal(err)
	}
	r := &twoHost{harness: h, url: "http://[::1]:" + port, homes: map[string]string{}}
	r.caller.Store("host-a")
	r.d = newDashboard(h.openDB(), openDaemonLog(filepath.Join(h.stateDir(), "daemon.log")), testAddr)
	hub := r.d.enableHub(self, port)
	hub.hosts["[::1]:"+port] = true
	hub.whois.arg = func(net.IP) string { return "as-" + r.caller.Load().(string) }
	r.d.serverEnv = h.getenv(nil)
	if binary := os.Getenv("TASKR_HUB_BIN"); binary != "" {
		ln.Close()
		r.startExternal(binary, self.NodeID)
	} else {
		r.d.serve(ln, func() {})
		t.Cleanup(r.d.stop)
	}
	for _, m := range []string{"host-a", "host-b"} {
		r.homes[m] = r.clientHome(r.url)
	}
	return r
}

// clientHome is a client HOME whose server.url is url.
func (r *twoHost) clientHome(url string) string {
	home := r.t.TempDir()
	dir := filepath.Join(home, ".local", "state", "taskr")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		r.t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, serverURLFile), []byte(url+"\n"), 0o644); err != nil {
		r.t.Fatal(err)
	}
	return home
}

func clientEnv(home string, env map[string]string) func(string) string {
	return func(k string) string {
		switch k {
		case "HOME":
			return home
		case "TASKR_DB":
			return ""
		}
		return env[k]
	}
}

// cli runs the client CLI on machine and returns its exit, last JSON line
// and stderr.
func (r *twoHost) cli(machine string, env map[string]string, args ...string) (int, map[string]any, string) {
	r.t.Helper()
	r.setCaller(machine)
	var out, errb bytes.Buffer
	code := contractCLIMain(r.t, append([]string{"--json"}, args...), clientEnv(r.homes[machine], env), &out, &errb)
	return code, lastJSON(out.String()), errb.String()
}

func lastJSON(s string) map[string]any {
	lines := strings.Split(strings.TrimSpace(s), "\n")
	var m map[string]any
	json.Unmarshal([]byte(lines[len(lines)-1]), &m)
	return m
}

func (r *twoHost) want(code int, machine string, env map[string]string, args ...string) map[string]any {
	r.t.Helper()
	got, m, stderr := r.cli(machine, env, args...)
	if got != code {
		r.t.Fatalf("%s: taskr %v = exit %d, want %d; %v %s", machine, args, got, code, m, stderr)
	}
	return m
}

// post sends one raw RPC request as caller and returns the HTTP status and
// the reply.
func (r *twoHost) post(caller string, body any, mods ...func(*http.Request)) (int, rpcReply, string) {
	r.t.Helper()
	r.setCaller(caller)
	b, ok := body.(string)
	if !ok {
		raw, _ := json.Marshal(body)
		b = string(raw)
	}
	req, _ := http.NewRequest(http.MethodPost, r.url+rpcPath, strings.NewReader(b))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set(rpcHeader, "1")
	for _, m := range mods {
		m(req)
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		r.t.Fatal(err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	var rep rpcReply
	json.Unmarshal(raw, &rep)
	return resp.StatusCode, rep, string(raw)
}

func rpcBody(cwd string, env map[string]string, key string, argv ...string) rpcRequest {
	return rpcRequest{Argv: append([]string{"--json"}, argv...), Cwd: cwd, Env: env, RequestKey: key}
}

func (r *twoHost) machineOf(table string, id int64) string {
	r.t.Helper()
	var m *string
	if err := r.openDB().QueryRow(`select machine from `+table+` where id = ?`, id).Scan(&m); err != nil {
		r.t.Fatal(err)
	}
	if m == nil {
		return "NULL"
	}
	return *m
}

func (r *twoHost) count(q string, args ...any) int {
	r.t.Helper()
	var n int
	if err := r.openDB().QueryRow(q, args...).Scan(&n); err != nil {
		r.t.Fatal(err)
	}
	return n
}

func TestHarnessRPCAdmission(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	dir := t.TempDir()
	ok := rpcBody(dir, nil, "admission-key-1", "status")
	for _, c := range []struct {
		name, caller string
		want         int
		mod          func(*http.Request)
	}{
		{"tagged node", "tagged", 403, nil},
		{"another user's node", "other", 403, nil},
		{"the server's own node", "self", 403, nil},
		{"a node named like the server", "samename", 403, nil},
		{"Origin", "host-a", 403, func(q *http.Request) { q.Header.Set("Origin", "http://[::1]") }},
		{"Sec-Fetch-Site", "host-a", 403, func(q *http.Request) { q.Header.Set("Sec-Fetch-Site", "same-origin") }},
		{"wrong content type", "host-a", 415, func(q *http.Request) { q.Header.Set("Content-Type", "text/plain") }},
		{"missing taskr header", "host-a", 400, func(q *http.Request) { q.Header.Del(rpcHeader) }},
		{"admitted", "host-a", 200, nil},
	} {
		var mods []func(*http.Request)
		if c.mod != nil {
			mods = append(mods, c.mod)
		}
		if code, _, raw := r.post(c.caller, ok, mods...); code != c.want {
			t.Errorf("%s: HTTP %d, want %d: %s", c.name, code, c.want, raw)
		}
	}
	// Loopback carries no whois identity: the RPC route refuses it.
	w, _ := serve(r.d, req("POST", rpcPath, `{"argv":["status"],"cwd":"/","request_key":"admission-key-2"}`, func(q *http.Request) {
		q.Header.Del("Origin")
		q.Header.Del("Sec-Fetch-Site")
		q.Header.Set(rpcHeader, "1")
	}))
	if r.loopbackURL != "" {
		q, _ := http.NewRequest("POST", r.loopbackURL+rpcPath, strings.NewReader(`{"argv":["status"],"cwd":"/","request_key":"admission-loopback"}`))
		q.Header.Set("Content-Type", "application/json")
		q.Header.Set(rpcHeader, "1")
		response, err := http.DefaultClient.Do(q)
		if err != nil {
			t.Fatal(err)
		}
		w.Code = response.StatusCode
		response.Body.Close()
	}
	if w.Code != 403 {
		t.Fatalf("loopback rpc = %d %s", w.Code, w.Body)
	}
	for name, body := range map[string]string{
		"a body field naming a machine": `{"argv":["status"],"cwd":"/","request_key":"admission-key-3","machine":"host-b"}`,
		"relative cwd":                  `{"argv":["status"],"cwd":"rel","request_key":"admission-key-4"}`,
		"short key":                     `{"argv":["status"],"cwd":"/","request_key":"k"}`,
		"empty argv":                    `{"argv":[],"cwd":"/","request_key":"admission-key-5"}`,
		"trailing data":                 `{"argv":["status"],"cwd":"/","request_key":"admission-key-6"} {}`,
	} {
		if code, _, raw := r.post("host-a", body); code != 400 {
			t.Errorf("%s: HTTP %d, want 400: %s", name, code, raw)
		}
	}
	big := `{"argv":["note","` + strings.Repeat("x", rpcBodyMax) + `"],"cwd":"/","request_key":"admission-key-7"}`
	if code, _, _ := r.post("host-a", big); code != 413 {
		t.Errorf("oversized body: HTTP %d, want 413", code)
	}
	// An env key outside the allowlist is dropped; the machine is whois's.
	_, rep, _ := r.post("host-a", rpcBody(dir, map[string]string{"MACHINE": "host-b", "TASKR_DB": "/elsewhere.db"},
		"admission-key-8", "new", "lane", "--role", "gate"))
	if rep.Exit != 0 {
		t.Fatalf("new over rpc = %+v", rep)
	}
	created := num(lastJSON(rep.Stdout), "task_id")
	if m := r.machineOf("tasks", created); m != "host-a" {
		t.Fatalf("task machine = %s, want host-a (whois), not a body field", m)
	}
	// daemon never runs over RPC; each request leaves one audit line without argv text.
	_, rep, _ = r.post("host-a", rpcBody(dir, nil, "admission-key-9", "daemon", "--once"))
	if rep.Exit != exitUsage {
		t.Fatalf("daemon over rpc = %+v", rep)
	}
	logText, _ := os.ReadFile(filepath.Join(r.stateDir(), "daemon.log"))
	if !strings.Contains(string(logText), "rpc: machine=host-a cmd=new key=admission-key-8 exit=0") ||
		!strings.Contains(string(logText), "rpc: machine=host-a cmd=daemon key=admission-key-9 exit=2") ||
		strings.Contains(string(logText), "lane") {
		t.Fatalf("audit log:\n%s", logText)
	}
}

func (r *twoHost) taskEventSnapshot() string {
	db := r.openDB()
	tasks := dumpTable(r.t, db, "tasks", "id, parent_id, name, agent_name, role, status, workspace_id, tab_id, pane_id, cwd, brief_path, report_path, current_launch_id, acked_event_id, pending_event_id, last_poll_at, waiting_until, machine, created_at, updated_at, closed_at")
	events := dumpTable(r.t, db, "events", "id, task_id, recipient_task_id, launch_id, kind, summary, data, related_event_id, answered_by, event_key, created_at")
	return fmt.Sprint(tasks, events)
}

func TestHarnessRPCDuplicateFlags(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	dir := t.TempDir()
	path := filepath.Join(dir, "present")
	if err := os.WriteFile(path, []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	for i, tc := range []struct {
		name, flag string
		args       []string
	}{
		{"cwd", "cwd", []string{"new", "duplicate", "--role", "gate", "--cwd", dir, "--cwd", dir}},
		{"brief", "brief", []string{"new", "duplicate", "--role", "gate", "--brief", path, "--brief", path}},
		{"report", "report", []string{"new", "duplicate", "--role", "gate", "--report", path, "--report", path}},
		{"file", "file", []string{"prompt", "1", "--text", "x", "--file", path, "--file", path}},
		{"out", "out", []string{"handover", "1", "--out", path, "--out", path}},
		{"machine", "machine", []string{"new", "duplicate", "--role", "gate", "--machine", "host-a", "--machine", "host-b"}},
		{"role", "role", []string{"new", "duplicate", "--role", "gate", "--role", "implementer"}},
		{"planned", "planned", []string{"new", "duplicate", "--role", "gate", "--planned", "--planned"}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			want := "repeated RPC flag --" + tc.flag
			before := r.taskEventSnapshot()
			_, rep, _ := r.post("host-a", rpcBody(dir, nil, fmt.Sprintf("dup-%s-%d", tc.name, i), tc.args...))
			if rep.Exit != exitUsage || !strings.Contains(rep.Stderr, want) {
				t.Fatalf("raw RPC %v = exit %d, stderr %q; want %q", tc.args, rep.Exit, rep.Stderr, want)
			}
			if after := r.taskEventSnapshot(); after != before {
				t.Fatalf("raw RPC %v changed tasks or events", tc.args)
			}

			var out, errb bytes.Buffer
			code := 0
			if os.Getenv("TASKR_BIN") != "" {
				// An unreadable server.url is the executable version of readErr.
				home := t.TempDir()
				dir := filepath.Join(home, ".local", "state", "taskr")
				if err := os.MkdirAll(filepath.Join(dir, serverURLFile), 0700); err != nil {
					t.Fatal(err)
				}
				code = contractCLIMain(t, append([]string{"--json"}, tc.args...), clientEnv(home, nil), &out, &errb)
			} else {
				code = clientMain("http://192.168.1.5:7788", fmt.Errorf("sentinel server URL error"), append([]string{"--json"}, tc.args...), clientEnv(r.homes["host-a"], nil), &out, &errb)
			}
			if code != exitUsage || !strings.Contains(out.String()+errb.String(), want) {
				t.Fatalf("client %v = exit %d, output %q %q; want %q", tc.args, code, out.String(), errb.String(), want)
			}
			if after := r.taskEventSnapshot(); after != before {
				t.Fatalf("client %v changed tasks or events", tc.args)
			}
		})
	}
}

func TestHarnessWaitAfterAdopt(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	dir := t.TempDir()
	top := num(r.want(0, "host-a", nil, "new", "top", "--role", "orchestrator", "--cwd", dir), "task_id")
	child := num(r.want(0, "host-a", nil, "new", "child", "--role", "gate", "--parent", id(top)), "task_id")
	done := make(chan rpcReply, 1)
	go func() {
		if r.callerFile != "" {
			_, rep, _ := r.post("host-a", rpcBody(dir, nil, "wait-adopt-01", "wait", "--as", id(top), "--timeout", "5000"))
			done <- rep
			return
		}
		done <- r.d.rpcRun(context.Background(), "host-a", rpcBody(dir, nil, "wait-adopt-01", "wait", "--as", id(top), "--timeout", "5000"))
	}()
	deadline := time.Now().Add(3 * time.Second)
	for r.count(`select count(*) from tasks where id = ? and waiting_until is not null`, top) == 0 {
		if time.Now().After(deadline) {
			t.Fatal("wait did not block")
		}
		time.Sleep(10 * time.Millisecond)
	}
	var pendingBefore sql.NullInt64
	if err := r.openDB().QueryRow(`select pending_event_id from tasks where id = ?`, top).Scan(&pendingBefore); err != nil {
		t.Fatal(err)
	}
	r.want(0, "host-b", map[string]string{"HERDR_PANE_ID": "w1:p1"}, "adopt", id(top))
	r.want(0, "host-a", map[string]string{"TASKR_TASK": id(child)}, "ready", "after move")
	var rep rpcReply
	select {
	case rep = <-done:
	case <-time.After(3 * time.Second):
		t.Fatal("old host wait did not return after adopt")
	}
	if rep.Exit != exitReject {
		t.Fatalf("old host-a wait = exit %d, want %d: %s", rep.Exit, exitReject, rep.Stdout)
	}
	var pendingAfter sql.NullInt64
	if err := r.openDB().QueryRow(`select pending_event_id from tasks where id = ?`, top).Scan(&pendingAfter); err != nil {
		t.Fatal(err)
	}
	if pendingAfter != pendingBefore {
		t.Fatalf("old host wait changed pending_event_id from %v to %v", pendingBefore, pendingAfter)
	}
	m := r.want(0, "host-b", nil, "wait", "--as", id(top), "--timeout", "0")
	ev := eventOf(m)
	if ev["task_id"] != float64(child) {
		t.Fatalf("adopter received the wrong event: %v", ev)
	}
	if err := r.openDB().QueryRow(`select pending_event_id from tasks where id = ?`, top).Scan(&pendingAfter); err != nil {
		t.Fatal(err)
	}
	if !pendingAfter.Valid || pendingAfter.Int64 != int64(ev["id"].(float64)) {
		t.Fatalf("adopter did not receive the event: pending=%v event=%v", pendingAfter, ev)
	}
}

func TestHarnessHostIdentity(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	dir := t.TempDir()
	top := num(r.want(0, "host-a", nil, "new", "top", "--role", "orchestrator", "--cwd", dir), "task_id")
	w := num(r.want(0, "host-a", nil, "new", "impl", "--role", "implementer", "--parent", id(top), "--cwd", dir), "task_id")
	l := num(r.want(0, "host-a", nil, "launch", id(w), "--provider", "claude", "--model", "m", "--effort", "high"), "launch_id")
	if r.machineOf("tasks", top) != "host-a" || r.machineOf("tasks", w) != "host-a" || r.machineOf("launches", l) != "host-a" {
		t.Fatalf("machines: top %s, task %s, launch %s", r.machineOf("tasks", top), r.machineOf("tasks", w), r.machineOf("launches", l))
	}
	// A host-a worker writes with its launch; the same env from host-b, or the
	// server's local CLI, is another host.
	r.want(0, "host-a", as(w, l), "note", "on host-a")
	m := r.want(exitReject, "host-b", as(w, l), "note", "from host-b")
	if !strings.Contains(fmt.Sprint(m["error"]), "launch is on host-a, caller is host-b") {
		t.Fatalf("host-b write = %v", m)
	}
	m = r.one(exitReject, as(w, l), "note", "from the server's CLI")
	if !strings.Contains(fmt.Sprint(m["error"]), "launch is on host-a, caller is "+localMachine()) {
		t.Fatalf("server CLI write = %v", m)
	}
	// A stale launch stays rejected.
	l2 := num(r.want(0, "host-a", nil, "launch", id(w), "--provider", "claude", "--model", "m", "--effort", "high"), "launch_id")
	r.want(exitReject, "host-a", as(w, l), "note", "stale")
	r.want(0, "host-a", as(w, l2), "note", "current")
	// An old host-a id that collides with a central id: the server's own lane.
	stop := r.newTask("central", "orchestrator", 0)
	cw := r.newTask("central-impl", "implementer", stop)
	cl := r.launch(cw)
	r.want(exitReject, "host-a", as(cw, cl), "ready", "collides", "--report", filepath.Join(dir, "r.md"))
	r.ok(as(cw, cl), "note", "the server's CLI owns it")
	// --as checks the root's host: writes, answers, waits and acks.
	r.want(0, "host-a", nil, "note", "root note", "--as", id(top))
	r.want(exitReject, "host-b", nil, "note", "wrong host", "--as", id(top))
	r.one(exitReject, nil, "note", "wrong host", "--as", id(top))
	ask := num(r.want(0, "host-a", as(w, l2), "ask", "which?"), "ask_id")
	r.want(exitReject, "host-b", nil, "wait", "--as", id(top), "--timeout", "0")
	pending := num(eventOf(r.want(exitOK, "host-a", nil, "wait", "--as", id(top), "--timeout", "0")), "id")
	r.want(exitReject, "host-b", nil, "ack", id(pending), "--as", id(top))
	r.want(exitReject, "host-b", nil, "answer", id(ask), "this", "--as", id(top))
	r.want(0, "host-a", nil, "answer", id(ask), "this", "--as", id(top))
	r.want(0, "host-a", nil, "ack", id(pending), "--as", id(top))
	// --machine: the server's label is stored as NULL; an unknown label lists the known ones.
	srv := num(r.want(0, "host-a", nil, "new", "on-server", "--role", "gate", "--machine", localMachine()), "task_id")
	if r.machineOf("tasks", srv) != "NULL" {
		t.Fatalf("--machine %s stored %s", localMachine(), r.machineOf("tasks", srv))
	}
	m = r.want(exitUsage, "host-a", nil, "new", "nowhere", "--role", "gate", "--machine", "nowhere")
	if e := fmt.Sprint(m["error"]); !strings.Contains(e, localMachine()) || !strings.Contains(e, "host-a") {
		t.Fatalf("unknown --machine = %v", m)
	}
	r.beat("host-a", 0)
	r.one(0, nil, "new", "for-host-a", "--role", "gate", "--machine", "host-a") // a host with a fresh daemon
	// The task moves with its launch; adopt moves a root to the adopter.
	l3 := num(r.want(0, "host-a", nil, "launch", id(w), "--provider", "claude", "--model", "m", "--effort", "high",
		"--machine", localMachine()), "launch_id")
	if r.machineOf("tasks", w) != "NULL" || r.machineOf("launches", l3) != "NULL" {
		t.Fatalf("launch --machine server: task %s launch %s", r.machineOf("tasks", w), r.machineOf("launches", l3))
	}
	r.want(0, "host-b", map[string]string{"HERDR_PANE_ID": "w1:p1"}, "adopt", id(top))
	r.want(exitReject, "host-a", nil, "note", "moved", "--as", id(top))
	r.want(0, "host-b", nil, "note", "adopted", "--as", id(top))
	// A prompt to a lane on another host exits 5 before any attempt row.
	mw := num(r.want(0, "host-a", nil, "new", "host-a-impl", "--role", "implementer", "--parent", id(top), "--cwd", dir, "--pane", "w5:p2"), "task_id")
	r.want(0, "host-a", nil, "launch", id(mw), "--provider", "claude", "--model", "m", "--effort", "high")
	m = r.want(exitHerdr, "host-b", nil, "prompt", id(mw), "--text", "go")
	if !strings.Contains(fmt.Sprint(m["error"]), "lane host-a-impl is on host-a; prompt it from that host") ||
		r.count(`select count(*) from events where kind = 'prompt' and task_id = ?`, mw) != 0 || len(r.calls("agent|prompt|")) != 0 {
		t.Fatalf("cross-host prompt = %v", m)
	}
}

// promptTarget is a lane on the server host, which an RPC caller may prompt.
func (r *twoHost) promptTarget() int64 {
	top := r.newTask("srv-top", "orchestrator", 0)
	w := r.newTask("srv-impl", "implementer", top, "--pane", "w9:p4")
	r.launch(w)
	return w
}

func TestHarnessRetry(t *testing.T) {
	contractGuard(t)
	window := 120 * time.Millisecond
	if os.Getenv("TASKR_HUB_BIN") != "" {
		window = 2 * time.Second
	} // subprocess dispatch includes process startup
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return window })
	r := newTwoHost(t)
	dir := t.TempDir()
	top := num(r.want(0, "host-a", nil, "new", "top", "--role", "orchestrator", "--cwd", dir), "task_id")
	notes := func() int { return r.count(`select count(*) from events where kind = 'note' and task_id = ?`, top) }
	_, first, _ := r.cli("host-a", nil, "--request-key", "retry-key-1", "note", "once", "--as", id(top))
	_, again, _ := r.cli("host-a", nil, "--request-key", "retry-key-1", "note", "once", "--as", id(top))
	if notes() != 1 || fmt.Sprint(first) != fmt.Sprint(again) || num(again, "event_id") == 0 {
		t.Fatalf("same key twice: %d notes, %v then %v", notes(), first, again)
	}
	m := r.want(exitReject, "host-a", nil, "--request-key", "retry-key-1", "note", "other text", "--as", id(top))
	if !strings.Contains(fmt.Sprint(m["error"]), "belongs to another command") || notes() != 1 {
		t.Fatalf("same key, other argv = %v (%d notes)", m, notes())
	}
	r.want(exitReject, "host-b", nil, "--request-key", "retry-key-1", "note", "once", "--as", id(top))
	// A running key: the outcome is unknown.
	argv := []string{"--json", "note", "stuck", "--as", id(top)}
	if _, err := r.openDB().Exec(`insert into requests (key, machine, argv_sha, state, created_at) values (?, 'host-a', ?, 'running', ?)`,
		"retry-key-2", argvSHA(argv), now()); err != nil {
		t.Fatal(err)
	}
	m = r.want(exitHerdr, "host-a", nil, "--request-key", "retry-key-2", "note", "stuck", "--as", id(top))
	if !strings.Contains(fmt.Sprint(m["error"]), "outcome unknown") || notes() != 1 {
		t.Fatalf("running key = %v", m)
	}
	// Week-old keys are pruned on the next insert.
	r.openDB().Exec(`update requests set created_at = ? where key = 'retry-key-1'`, stamp(time.Now().Add(-8*24*time.Hour)))
	r.want(0, "host-a", nil, "note", "later", "--as", id(top))
	if r.count(`select count(*) from requests where key = 'retry-key-1'`) != 0 {
		t.Fatal("an 8-day-old request key was not pruned")
	}
	// Fresh commands are never stored.
	before := r.count(`select count(*) from requests`)
	r.want(0, "host-a", nil, "--request-key", "retry-key-3", "status")
	r.want(0, "host-a", nil, "--request-key", "retry-key-3", "log", id(top))
	if r.count(`select count(*) from requests`) != before {
		t.Fatal("a fresh command was stored")
	}

	// The client drops mid-command: the prompt runs to the end and a retry
	// returns its stored result without a second send.
	w := r.promptTarget()
	started, release := make(chan struct{}), make(chan struct{})
	var once sync.Once
	if r.callerFile == "" {
		setVar(t, &receiptPolled, func(int64) { once.Do(func() { close(started); <-release }) })
	} else {
		go func() {
			deadline := time.Now().Add(20 * time.Second)
			for time.Now().Before(deadline) {
				if r.count(`select count(*) from events where task_id=? and kind='prompt_outcome' and summary='activity_observed'`, w) > 0 {
					close(started)
					return
				}
				time.Sleep(10 * time.Millisecond)
			}
		}()
	}
	cx, cancel := context.WithCancel(context.Background())
	body, _ := json.Marshal(rpcBody(dir, nil, "retry-key-4", "prompt", id(w), "--text", "go", "--confirm", "--confirm-timeout", "300"))
	q, _ := http.NewRequestWithContext(cx, http.MethodPost, r.url+rpcPath, bytes.NewReader(body))
	q.Header.Set("Content-Type", "application/json")
	q.Header.Set(rpcHeader, "1")
	r.caller.Store("host-a")
	dropped := make(chan error, 1)
	go func() {
		resp, err := http.DefaultClient.Do(q)
		if err == nil {
			resp.Body.Close()
		}
		dropped <- err
	}()
	select {
	case <-started:
	case <-time.After(20 * time.Second):
		t.Fatal("prompt never reached its receipt wait")
	}
	cancel()
	if err := <-dropped; err == nil {
		t.Fatal("the client did not drop")
	}
	close(release)
	deadline := time.Now().Add(20 * time.Second)
	for r.count(`select count(*) from requests where key = 'retry-key-4' and state = 'done'`) == 0 {
		if time.Now().After(deadline) {
			t.Fatal("the dropped prompt never finished")
		}
		time.Sleep(50 * time.Millisecond)
	}
	m = r.want(exitHerdr, "host-a", nil, "--request-key", "retry-key-4", "prompt", id(w), "--text", "go", "--confirm", "--confirm-timeout", "300")
	if m["outcome"] != "no_receipt" || len(r.calls("agent|prompt|")) != 1 ||
		r.count(`select count(*) from events where kind = 'prompt' and task_id = ?`, w) != 1 {
		t.Fatalf("retry after drop = %v, %d herdr prompts", m, len(r.calls("agent|prompt|")))
	}
}

func TestHarnessRPCWaits(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	dir := t.TempDir()
	top := num(r.want(0, "host-a", nil, "new", "top", "--role", "orchestrator", "--cwd", dir), "task_id")
	w := num(r.want(0, "host-a", nil, "new", "impl", "--role", "implementer", "--parent", id(top), "--cwd", dir), "task_id")
	l := num(r.want(0, "host-a", nil, "launch", id(w), "--provider", "claude", "--model", "m", "--effort", "high"), "launch_id")
	e := num(r.want(0, "host-a", as(w, l), "ready", "first", "--report", filepath.Join(dir, "r.md")), "event_id")
	r.want(0, "host-a", nil, "wait", "--as", id(top), "--timeout", "0")
	r.want(exitTimeout, "host-a", nil, "wait", "--as", id(top), "--ack", id(e), "--timeout", "0")
	// The response was lost; the retry continues instead of exit 6.
	m := r.want(exitTimeout, "host-a", nil, "wait", "--as", id(top), "--ack", id(e), "--timeout", "0")
	if m["timeout"] != true {
		t.Fatalf("retried wait --ack = %v", m)
	}
	// A never-pending event and another task's event stay rejected.
	later := num(r.want(0, "host-a", as(w, l), "note", "not in the inbox"), "event_id")
	next := num(r.want(0, "host-a", as(w, l), "ready", "second", "--report", filepath.Join(dir, "r.md")), "event_id")
	r.want(exitReject, "host-a", nil, "wait", "--as", id(top), "--ack", id(next), "--timeout", "0")
	r.want(exitReject, "host-a", nil, "wait", "--as", id(top), "--ack", id(later), "--timeout", "0")

	// A long-poll past the server's 15 s write timeout returns, and a write
	// from elsewhere completes while it holds no transaction.
	other := r.newTask("server-root", "orchestrator", 0)
	type result struct {
		code    int
		m       map[string]any
		elapsed time.Duration
	}
	done := make(chan result, 1)
	began := time.Now()
	go func() {
		var out, errb bytes.Buffer
		code := contractCLIMain(t, []string{"--json", "wait", "--as", id(top), "--for", "done", "--timeout", "20000"},
			clientEnv(r.homes["host-a"], nil), &out, &errb)
		done <- result{code, lastJSON(out.String()), time.Since(began)}
	}()
	time.Sleep(5 * time.Second)
	wrote := time.Now()
	r.ok(nil, "note", "during the long poll", "--as", id(other))
	if el := time.Since(wrote); el > 2*time.Second {
		t.Fatalf("a write during the long poll took %v", el)
	}
	select {
	case got := <-done:
		// `--for done` skips (and acks) the pending ready; nothing else arrives.
		if got.code != exitTimeout || got.m["timeout"] != true || got.m["interrupted"] == true || got.elapsed < 20*time.Second {
			t.Fatalf("long poll = exit %d %v after %v", got.code, got.m, got.elapsed)
		}
	case <-time.After(40 * time.Second):
		t.Fatal("the long poll never returned")
	}
}

// A wait that connected times out; a record write queues when replies are lost.
func TestHarnessRPCDroppedConnection(t *testing.T) {
	contractGuard(t)
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 120 * time.Millisecond })
	r := newTwoHost(t)
	ln, err := net.Listen("tcp", "[::1]:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			c.Read(make([]byte, 4096))
			c.Close()
		}
	}()
	home := r.clientHome("http://" + ln.Addr().String())
	for _, args := range [][]string{{"wait", "--as", "1", "--timeout", "1000"}, {"note", "x", "--as", "1"}} {
		var out, errb bytes.Buffer
		code := contractCLIMain(t, append([]string{"--json", "--request-key", "drop-key-01"}, args...), clientEnv(home, nil), &out, &errb)
		m := lastJSON(out.String())
		if args[0] == "wait" {
			if code != exitTimeout || m["timeout"] != true || m["interrupted"] == true ||
				strings.Contains(errb.String(), "retry with:") {
				t.Fatalf("connected wait = %d %v %q", code, m, errb.String())
			}
			continue
		}
		errText := errb.String()
		if code != exitOK || m["queued"] != true || m["request_key"] != "drop-key-01" ||
			errText != "taskr: server unreachable; queued (1 waiting)\n" {
			t.Fatalf("%v over a dropped connection = %d %v %q", args, code, m, errb.String())
		}
	}
	// Nothing listening at all is the same.
	dead := r.clientHome("http://[::1]:1")
	var out, errb bytes.Buffer
	if code := contractCLIMain(t, []string{"status"}, clientEnv(dead, nil), &out, &errb); code != exitHerdr {
		t.Fatalf("unreachable server = %d %s", code, out.String())
	}
}

func TestHarnessRPCPaths(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	dir, _ := filepath.EvalSymlinks(t.TempDir()) // os.Getwd reports the resolved path
	top := num(r.want(0, "host-a", nil, "new", "top", "--role", "orchestrator", "--cwd", dir), "task_id")
	// The server refuses a relative path and handover --out.
	for i, argv := range [][]string{
		{"new", "x", "--role", "gate", "--brief", "brief.md"},
		{"new", "x", "--role", "gate", "--cwd=rel"},
		{"ready", "r", "--report", "r.md"},
		{"prompt", "1", "--file", "p.md"},
		{"handover", "--as", id(top), "--out", filepath.Join(dir, "server.md")},
	} {
		_, rep, _ := r.post("host-a", rpcBody(dir, nil, fmt.Sprintf("paths-key-%d", i), argv...))
		if rep.Exit != exitUsage {
			t.Errorf("%v over rpc = %+v", argv, rep)
		}
	}
	if _, err := os.Stat(filepath.Join(dir, "server.md")); err == nil {
		t.Fatal("the server wrote handover --out")
	}
	// The client absolutizes: new's --brief against its --cwd.
	sub := filepath.Join(dir, "sub")
	os.Mkdir(sub, 0o755)
	os.WriteFile(filepath.Join(sub, "b.md"), []byte("brief"), 0o644)
	t.Chdir(dir)
	lane := num(r.want(0, "host-a", nil, "new", "lane", "--role", "implementer", "--parent", id(top), "--cwd", "sub", "--brief", "b.md"), "task_id")
	var cwd, brief string
	r.openDB().QueryRow(`select cwd, brief_path from tasks where id = ?`, lane).Scan(&cwd, &brief)
	if cwd != sub || brief != filepath.Join(sub, "b.md") {
		t.Fatalf("cwd %q brief %q", cwd, brief)
	}
	// Without --cwd, new uses the request's cwd, never the server's.
	_, rep, _ := r.post("host-a", rpcBody(sub, nil, "paths-key-cwd", "new", "here", "--role", "gate"))
	r.openDB().QueryRow(`select cwd from tasks where id = ?`, num(lastJSON(rep.Stdout), "task_id")).Scan(&cwd)
	if rep.Exit != 0 || cwd != sub {
		t.Fatalf("new without --cwd = %+v cwd %q", rep, cwd)
	}
	// --cwd belongs to the task's host, while --brief belongs to the caller.
	missing := filepath.Join(dir, "missing")
	before := r.count(`select count(*) from requests`)
	r.want(exitUsage, "host-a", nil, "new", "gone", "--role", "implementer", "--cwd", missing)
	if r.count(`select count(*) from requests`) != before {
		t.Fatal("the client sent a new whose --cwd does not exist here")
	}
	r.want(exitUsage, "host-a", nil, "new", "gone", "--role", "implementer", "--cwd", missing, "--machine", localMachine())
	r.beat("host-a", 0)
	r.one(0, nil, "new", "host-a-lane", "--role", "implementer", "--cwd", missing, "--machine", "host-a")
	r.beat("host-b", 0)
	callerBrief := filepath.Join(dir, "caller-brief.md")
	if err := os.WriteFile(callerBrief, []byte("caller brief"), 0o644); err != nil {
		t.Fatal(err)
	}
	before = r.count(`select count(*) from requests`)
	r.want(exitUsage, "host-a", nil, "new", "missing-third-host-brief", "--role", "orchestrator", "--cwd", dir,
		"--machine", "host-b", "--brief", filepath.Join(dir, "missing-brief.md"))
	if r.count(`select count(*) from requests`) != before {
		t.Fatal("the client sent a new whose --brief does not exist here")
	}
	thirdHostTask := num(r.want(0, "host-a", nil, "new", "third-host-brief", "--role", "orchestrator", "--cwd", dir,
		"--machine", "host-b", "--brief", callerBrief), "task_id")
	if d := docLatest(t, r.openDB(), thirdHostTask, "goal", ""); !d.Captured || d.Host.String != "host-a" || d.Path.String != callerBrief {
		t.Fatalf("third-host brief = %+v", d)
	}
	// handover --out is written by the client, from the server's stdout.
	out := filepath.Join(dir, "handover.md")
	code, _, stderr := r.cli("host-a", nil, "handover", "--as", id(top), "--out", "handover.md")
	b, err := os.ReadFile(out)
	if code != 0 || err != nil || !strings.Contains(string(b), "# ") || !strings.Contains(stderr, "wrote "+out) {
		t.Fatalf("handover --out = %d %q %v", code, stderr, err)
	}
	var data string
	r.openDB().QueryRow(`select data from events where kind = 'handover' and task_id = ?`, top).Scan(&data)
	if strings.Contains(data, `"out"`) {
		t.Fatalf("the server recorded an --out path: %s", data)
	}
}

// startExternal replaces the server half, keeping the existing CLI and SQL assertions.
// The Rust fixture flag and loopback whois overrides are absent from default builds.
func (r *twoHost) startExternal(binary, nodeID string) {
	t := r.t
	r.callerFile = filepath.Join(r.bin, "hub-caller")
	r.setCaller("host-a")
	wrapper := strings.Replace(fakeTailscale, `f="$d/ts.whois.$3"`, `peer="$3"
  [ "$peer" = ::1 ] && peer="as-$(cat "$d/hub-caller")"
  f="$d/ts.whois.$peer"`, 1)
	r.write("tailscale", wrapper, 0755)
	cmd := exec.Command(binary, "--contract-hub", nodeID)
	cmd.Env = []string{"HOME=" + r.dir, "TASKR_DB=" + r.db, "PATH=" + os.Getenv("PATH"), "HERDR_SOCKET_PATH=" + r.herdrSock, "TASKR_CONTRACT_TAILNET=1", "TASKR_CONTRACT_ORACLE=1", "LANG=C.UTF-8", "TZ=UTC"}
	stderr, err := os.Create(filepath.Join(r.dir, "hub-stderr.log"))
	if err != nil {
		t.Fatal(err)
	}
	cmd.Stderr = stderr
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err = cmd.Start(); err != nil {
		t.Fatal(err)
	}
	done := make(chan struct{})
	var waitErr error
	go func() { waitErr = cmd.Wait(); close(done) }()
	t.Cleanup(func() {
		_ = cmd.Process.Signal(syscall.SIGTERM)
		select {
		case <-done:
		case <-time.After(5 * time.Second):
			_ = cmd.Process.Kill()
			<-done
		}
		stderr.Close()
	})
	lines := make(chan string, 2)
	go func() {
		scanner := bufio.NewScanner(stdout)
		for scanner.Scan() {
			lines <- scanner.Text()
		}
	}()
	for _, target := range []*string{&r.url, &r.loopbackURL} {
		select {
		case *target = <-lines:
		case <-done:
			t.Fatalf("external hub stopped: %v", waitErr)
		case <-time.After(10 * time.Second):
			t.Fatal("external hub did not announce listeners")
		}
	}
}
func (r *twoHost) setCaller(machine string) {
	r.caller.Store(machine)
	if r.callerFile != "" {
		if err := os.WriteFile(r.callerFile, []byte(machine), 0600); err != nil {
			r.t.Fatal(err)
		}
	}
}
