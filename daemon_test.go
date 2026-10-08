package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"
)

// fakeSocket is a Herdr socket stand-in. It lives under a short
// os.MkdirTemp("", "tk") directory: macOS limits sun_path to about 104 bytes,
// which t.TempDir() paths overrun.
type fakeSocket struct {
	t     *testing.T
	path  string
	ln    *net.UnixListener
	conns chan net.Conn
	mu    sync.Mutex
	all   []net.Conn
	subs  []net.Conn // connections that sent a request line: the streams
	once  sync.Once
}

func newFakeSocket(t *testing.T) *fakeSocket {
	t.Helper()
	dir, err := os.MkdirTemp("", "tk")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	return newFakeSocketAt(t, filepath.Join(dir, "h.sock"))
}

func newFakeSocketAt(t *testing.T, path string) *fakeSocket {
	t.Helper()
	s := &fakeSocket{t: t, path: path, conns: make(chan net.Conn, 16)}
	var err error
	s.ln, err = net.ListenUnix("unix", &net.UnixAddr{Name: s.path, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	go func() {
		for {
			c, err := s.ln.Accept()
			if err != nil {
				return
			}
			s.mu.Lock()
			s.all = append(s.all, c)
			s.mu.Unlock()
			s.conns <- c
		}
	}()
	t.Cleanup(s.shutdown)
	return s
}

// subscriptionError applies Herdr 0.9.1's schema rule: a
// pane.agent_status_changed subscription requires pane_id; the other kinds
// take type alone. It returns the server's error line, or "" when valid.
func subscriptionError(req map[string]any) string {
	params, _ := req["params"].(map[string]any)
	subs, ok := params["subscriptions"].([]any)
	if req["method"] != "events.subscribe" || !ok {
		return `{"id":"taskr-daemon","error":{"code":"invalid_request","message":"expected events.subscribe"}}` + "\n"
	}
	for _, v := range subs {
		sub, _ := v.(map[string]any)
		if pane, ok := sub["pane_id"].(string); sub["type"] == "pane.agent_status_changed" && (!ok || pane == "") {
			return `{"id":"taskr-daemon","error":{"code":"invalid_request","message":"missing field pane_id"}}` + "\n"
		}
	}
	return ""
}

// panesOf lists the pane_id of each status subscription in req.
func panesOf(req map[string]any) []string {
	panes := []string{}
	params, _ := req["params"].(map[string]any)
	subs, _ := params["subscriptions"].([]any)
	for _, v := range subs {
		if sub, _ := v.(map[string]any); sub["type"] == "pane.agent_status_changed" {
			panes = append(panes, sub["pane_id"].(string))
		}
	}
	return panes
}

// next returns the next accepted connection and its first line. An invalid
// request gets Herdr's error line and a closed connection, then fails the test.
func (s *fakeSocket) next() (net.Conn, *bufio.Reader, map[string]any) {
	s.t.Helper()
	deadline := time.After(10 * time.Second)
	for {
		var c net.Conn
		select {
		case c = <-s.conns:
		case <-deadline:
			s.t.Fatal("daemon did not connect")
			return nil, nil, nil
		}
		{
			r := bufio.NewReader(c)
			c.SetReadDeadline(time.Now().Add(10 * time.Second))
			line, err := r.ReadBytes('\n')
			if err == io.EOF && len(line) == 0 {
				continue // the daemon's liveness dial before a pass: connect, close
			}
			if err != nil {
				s.t.Fatalf("reading subscription request: %v", err)
			}
			s.stream(c)
			var req map[string]any
			if err := json.Unmarshal(line, &req); err != nil {
				s.t.Fatalf("subscription request is not JSON: %q", line)
			}
			if e := subscriptionError(req); e != "" {
				c.Write([]byte(e))
				c.Close()
				s.t.Fatalf("server rejected %s: %s", line, e)
			}
			return c, r, req
		}
	}
}

// stream records c as a subscription stream, not a liveness dial.
func (s *fakeSocket) stream(c net.Conn) {
	s.mu.Lock()
	s.subs = append(s.subs, c)
	s.mu.Unlock()
}

// noStream fails if the daemon opens a stream (a connection that sends a
// request) within d. Liveness dials, which connect and close, are ignored.
func (s *fakeSocket) noStream(d time.Duration, why string) {
	s.t.Helper()
	for end := time.After(d); ; {
		select {
		case c := <-s.conns:
			c.SetReadDeadline(time.Now().Add(time.Second))
			if line, _ := bufio.NewReader(c).ReadBytes('\n'); len(line) > 0 {
				s.t.Fatal(why)
			}
		case <-end:
			return
		}
	}
}

// shutdown removes the socket file, then drops the listener and every
// connection: what a Herdr server exit looks like to a client.
func (s *fakeSocket) shutdown() {
	s.once.Do(func() {
		os.Remove(s.path)
		s.ln.Close()
		s.mu.Lock()
		for _, c := range s.all {
			c.Close()
		}
		s.mu.Unlock()
	})
}

const ack = `{"id":"taskr-daemon","result":{"type":"subscription_started"}}` + "\n"

func eventLines(n int) string {
	var b strings.Builder
	for i := 0; i < n; i++ {
		b.WriteString(`{"event":"pane.agent_status_changed","data":{"pane_id":"w9:p1","agent_status":"idle"}}` + "\n")
	}
	return b.String()
}

type daemonProc struct {
	pid  int
	done chan struct{}
	code int
	out  bytes.Buffer
}

// startDaemon runs `taskr daemon` in-process against sock. Cleanup shuts the
// socket down and waits for the daemon to exit.
func (h *harness) startDaemon(s *fakeSocket) *daemonProc {
	p := &daemonProc{done: make(chan struct{}), pid: os.Getpid()}
	env := h.getenv(map[string]string{"HERDR_SOCKET_PATH": s.path})
	if os.Getenv("TASKR_BIN") != "" {
		cmd := contractCommand([]string{"--json", "daemon"}, env, &p.out, io.Discard)
		if err := cmd.Start(); err != nil {
			h.t.Fatal(err)
		}
		p.pid = cmd.Process.Pid
		go func() {
			defer close(p.done)
			err := cmd.Wait()
			p.code = 0
			if err != nil {
				if ex, ok := err.(*exec.ExitError); ok {
					p.code = ex.ExitCode()
				} else {
					p.code = exitHerdr
				}
			}
		}()
		h.t.Cleanup(func() {
			if cmd.ProcessState == nil {
				cmd.Process.Kill()
			}
		})
	} else {
		go func() {
			defer close(p.done)
			p.code = contractRun(h.t, []string{"--json", "daemon"}, env, &p.out, io.Discard)
		}()
	}
	h.t.Cleanup(func() {
		s.shutdown()
		p.wait(h.t)
	})
	return p
}

func (p *daemonProc) wait(t *testing.T) {
	t.Helper()
	select {
	case <-p.done:
		if p.code == contractNotImplemented {
			contractMissing.Store(strings.SplitN(t.Name(), "/", 2)[0], true)
			t.Skip("adapter: daemon not implemented")
		}
	case <-time.After(15 * time.Second):
		t.Fatal("daemon did not exit")
	}
}

func (p *daemonProc) lines(t *testing.T) []map[string]any {
	t.Helper()
	var out []map[string]any
	for _, l := range strings.Split(strings.TrimSpace(p.out.String()), "\n") {
		var m map[string]any
		if err := json.Unmarshal([]byte(l), &m); err != nil {
			t.Fatalf("daemon stdout line is not JSON: %q", l)
		}
		out = append(out, m)
	}
	return out
}

// eventually polls cond until it holds or 10 s pass.
func eventually(t *testing.T, what string, cond func() bool) {
	t.Helper()
	for end := time.Now().Add(10 * time.Second); time.Now().Before(end); time.Sleep(20 * time.Millisecond) {
		if cond() {
			return
		}
	}
	t.Fatalf("timed out waiting for %s", what)
}

func (h *harness) sock(s *fakeSocket) map[string]string {
	return map[string]string{"HERDR_SOCKET_PATH": s.path}
}

func TestDaemonSubscriptionShapeAndWriteSilence(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	s := newFakeSocket(t)
	p := h.startDaemon(s)
	c, r, req := s.next()
	want := []any{}
	for _, k := range []string{"pane.exited", "pane.closed", "pane.agent_detected"} {
		want = append(want, map[string]any{"type": k})
	}
	params, _ := req["params"].(map[string]any)
	if req["method"] != "events.subscribe" || req["id"] == "" || !reflect.DeepEqual(params["subscriptions"], want) || len(req) != 3 {
		t.Fatalf("subscription request = %v", req)
	}
	if r.Buffered() != 0 {
		t.Fatalf("bytes after the request line: %d", r.Buffered())
	}
	c.Write([]byte(ack + eventLines(3)))
	c.SetReadDeadline(time.Now().Add(1500 * time.Millisecond))
	if n, err := r.Read(make([]byte, 1)); n != 0 || !os.IsTimeout(err) {
		t.Fatalf("client wrote after the request: n=%d err=%v", n, err)
	}
	eventually(t, "heartbeat", func() bool {
		st := h.ok(h.sock(s), "daemon", "--status")
		return st["daemon"] == "fresh"
	})
	st := h.ok(h.sock(s), "daemon", "--status")
	if st["running"] != true || num(st, "pid") != int64(p.pid) || st["socket"] != s.path {
		t.Fatalf("daemon --status = %v", st)
	}
	if _, lines := h.run(nil, "status"); lines[len(lines)-1]["daemon"] != "fresh" {
		t.Fatalf("status daemon line = %v", lines[len(lines)-1])
	}

	s.shutdown()
	p.wait(t)
	out := p.lines(t)
	if p.code != exitOK || len(out) != 1 || out[0]["ok"] != true || out[0]["socket"] != s.path || num(out[0], "pid") != int64(p.pid) {
		t.Fatalf("daemon exit %d, stdout %v", p.code, out)
	}
	if st := h.ok(nil, "daemon", "--status"); st["daemon"] != "none" || st["running"] != false {
		t.Fatalf("status after exit = %v", st)
	}
	logb, _ := os.ReadFile(filepath.Join(h.dir, ".local", "state", "taskr", "daemon.log"))
	if !strings.Contains(string(logb), "subscribed") || !strings.Contains(string(logb), "exit: socket removed") {
		t.Fatalf("daemon.log = %q", logb)
	}
}

func TestDaemonCoalescesBurst(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	h.launch(w)
	h.ok(nil, "prompt", id(w), "--text", "go", "--receipt-timeout", "0") // an owed result, no receipt deadline: the hints emit
	h.setAgents("w9:p1/idle/3")
	s := newFakeSocket(t)
	h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(ack + eventLines(50)))
	lists := func() int { return len(h.calls("agent|list|")) }
	eventually(t, "first pass", func() bool { return lists() >= 1 })
	time.Sleep(1500 * time.Millisecond)
	if n := lists(); n != 1 || h.countHerdr(top) != 1 {
		t.Fatalf("ack plus 50 lines: agent list calls = %d, herdr events = %d; want 1 and 1", n, h.countHerdr(top))
	}

	// A steady trickle is throttled to one pass per 500 ms.
	h.setAgents("w9:p1/done/4")
	began := time.Now()
	for i := 0; i < 30; i++ {
		c.Write([]byte(eventLines(1)))
		time.Sleep(50 * time.Millisecond)
	}
	elapsed := time.Since(began)
	time.Sleep(1 * time.Second)
	n := lists() - 1
	if limit := int(elapsed/daemonMinGap) + 2; n < 1 || n > limit {
		t.Fatalf("trickle over %v: %d passes, want 1..%d", elapsed, n, limit)
	}
	if h.countHerdr(top) != 2 {
		t.Fatalf("herdr events = %d, want 2 (idle, done)", h.countHerdr(top))
	}
}

func TestDaemonReconnects(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	s := newFakeSocket(t)
	h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(ack))
	c.Close()
	c2, _, req := s.next()
	if req["method"] != "events.subscribe" {
		t.Fatalf("reconnect request = %v", req)
	}
	c2.Write([]byte(ack))
	c2.Close() // a failed stream backs off, then connects again
	if _, _, req := s.next(); req["method"] != "events.subscribe" {
		t.Fatalf("second reconnect request = %v", req)
	}
}

func TestDaemonExitsWhenSocketRemoved(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	s := newFakeSocket(t)
	p := h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(ack))
	eventually(t, "heartbeat", func() bool { return h.ok(nil, "daemon", "--status")["daemon"] == "fresh" })
	os.Remove(s.path)
	c.Close()
	p.wait(t)
	if p.code != exitOK {
		t.Fatalf("daemon exit %d", p.code)
	}
	if st := h.ok(nil, "daemon", "--status"); st["daemon"] != "none" {
		t.Fatalf("heartbeat not cleared: %v", st)
	}
	// No socket at start: print the start line, then exit 0.
	q := h.startDaemon(s)
	q.wait(t)
	if out := q.lines(t); q.code != exitOK || len(out) != 1 || out[0]["ok"] != true || out[0]["already_running"] != nil {
		t.Fatalf("start without socket: exit %d %v", q.code, out)
	}
}

func TestDaemonLockRefusesSecondInstance(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	s := newFakeSocket(t)
	p := h.startDaemon(s)
	s.next() // the first instance holds the lock before it connects
	out := h.ok(h.sock(s), "daemon")
	if out["ok"] != true || out["already_running"] != true || num(out, "pid") != int64(p.pid) {
		t.Fatalf("second daemon = %v", out)
	}
	s.shutdown()
	p.wait(t)
	// The lock dies with its holder.
	s2 := newFakeSocket(t)
	q := h.startDaemon(s2)
	s2.next()
	s2.shutdown()
	q.wait(t)
	if out := q.lines(t); out[0]["already_running"] != nil {
		t.Fatalf("restart after exit = %v", out)
	}
}

// livenessFixture builds the same ledger in any harness: two watched
// children, a gate, a child in its own wait, and a closed child, first all
// working, then idle, missing, and a new working seq.
func livenessFixture(h *harness, poll func(top int64)) int64 {
	top := h.newTask("top", "orchestrator", 0)
	var ta, tb int64
	for i, c := range []struct{ name, role, pane string }{
		{"a", "implementer", "w9:p1"}, {"b", "reviewer", "w9:p2"}, {"c", "implementer", "w9:p3"},
		{"g", "gate", "w9:p4"}, {"wt", "implementer", "w9:p5"}, {"x", "implementer", "w9:p6"},
	} {
		t := h.newTask(c.name, c.role, top, "--pane", c.pane)
		h.launch(t)
		switch i {
		case 0:
			ta = t
		case 1:
			tb = t
		}
	}
	// a and b owe a result without a receipt deadline: their idle and missing hints still emit.
	h.ok(nil, "prompt", id(ta), "--text", "go", "--receipt-timeout", "0")
	h.ok(nil, "prompt", id(tb), "--text", "go", "--receipt-timeout", "0")
	h.setAgents("w9:p1/working/1", "w9:p2/working/1", "w9:p3/working/1", "w9:p4/working/1", "w9:p5/working/1", "w9:p6/working/1")
	poll(top)
	db := h.openDB()
	db.Exec(`update tasks set waiting_until = ? where name = 'wt'`, stamp(time.Now().Add(time.Hour)))
	db.Exec(`update tasks set status = 'closed' where name = 'x'`)
	h.setAgents("w9:p1/idle/3", "w9:p3/working/2", "w9:p4/done/2", "w9:p5/idle/2", "w9:p6/idle/2")
	poll(top)
	return top
}

func herdrEvents(h *harness) []map[string]any {
	h.t.Helper()
	rows, err := h.openDB().Query(`select ` + eventCols + ` from events e join tasks t on t.id = e.task_id where e.kind = 'herdr' order by e.id`)
	if err != nil {
		h.t.Fatal(err)
	}
	defer rows.Close()
	var out []map[string]any
	for rows.Next() {
		m, err := scanEvent(rows)
		if err != nil {
			h.t.Fatal(err)
		}
		delete(m, "created_at")
		out = append(out, m)
	}
	return out
}

func TestDaemonOnceMatchesWaitPoll(t *testing.T) {
	contractGuard(t)
	hw := newHarness(t)
	livenessFixture(hw, func(top int64) {
		hw.openDB().Exec(`update tasks set last_poll_at = null`)
		hw.run(nil, "wait", "--as", id(top), "--timeout", "300")
	})
	viaWait := herdrEvents(hw)

	hd := newHarness(t)
	top := livenessFixture(hd, func(int64) {
		if out := hd.ok(nil, "daemon", "--once"); out["once"] != true {
			t.Fatalf("daemon --once = %v", out)
		}
	})
	viaDaemon := herdrEvents(hd)
	if len(viaWait) != 2 || !reflect.DeepEqual(viaWait, viaDaemon) {
		t.Fatalf("herdr events differ:\nwait:   %v\ndaemon: %v", viaWait, viaDaemon)
	}
	if n := len(hd.calls("agent|list|")); n != 2 {
		t.Fatalf("daemon --once agent list calls = %d, want 2", n)
	}

	// Unlike one wait, the daemon covers every parent: a grandchild reports to its sub-orchestrator.
	sub := hd.newTask("sub", "sub-orchestrator", top, "--pane", "w9:p7")
	hd.launch(sub)
	hd.launch(hd.newTask("gc", "implementer", sub, "--pane", "w9:p8"))
	hd.setAgents("w9:p1/idle/3", "w9:p3/working/2", "w9:p7/working/1", "w9:p8/blocked/1")
	hd.ok(nil, "daemon", "--once")
	if hd.countHerdr(sub) != 1 {
		t.Fatalf("grandchild herdr events to sub = %d, want 1", hd.countHerdr(sub))
	}
	// --once is not a live daemon: it writes no heartbeat.
	if st := hd.ok(nil, "daemon", "--status"); st["daemon"] != "none" {
		t.Fatalf("--once wrote a heartbeat: %v", st)
	}
	// A failed listing is exit 5 with the pass result.
	hd.write("list.exit", "1", 0o644)
	if out := hd.one(exitHerdr, nil, "daemon", "--once"); out["kind"] != "herdr" || out["once"] != true {
		t.Fatalf("daemon --once with failing herdr = %v", out)
	}
}

func TestWaitHeartbeatGating(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	h.launch(w)
	h.ok(nil, "prompt", id(w), "--text", "go", "--receipt-timeout", "0") // an owed result, no receipt deadline: the stale-heartbeat hint emits
	h.setAgents("w9:p1/idle/3")
	db := h.openDB()
	if _, lines := h.run(nil, "status"); lines[len(lines)-1]["record"] != "daemon" || lines[len(lines)-1]["daemon"] != "none" {
		t.Fatalf("status without heartbeat: %v", lines[len(lines)-1])
	}
	setMeta(db, heartbeatKey, now())
	if _, lines := h.run(nil, "status"); lines[len(lines)-1]["daemon"] != "fresh" || lines[0]["name"] != "top" {
		t.Fatalf("status with fresh heartbeat: %v", lines)
	}
	h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "300")
	if n := len(h.calls("agent|")); n != 1 { // the prompt's own call; the wait adds none
		t.Fatalf("wait with a fresh heartbeat made %d agent calls", n)
	}

	setMeta(db, heartbeatKey, stamp(time.Now().Add(-time.Minute)))
	db.Exec(`update tasks set last_poll_at = null`)
	if _, lines := h.run(nil, "status"); lines[len(lines)-1]["daemon"] != "stale" {
		t.Fatalf("status with stale heartbeat: %v", lines[len(lines)-1])
	}
	got := h.ok(nil, "wait", "--as", id(top), "--timeout", "2000")
	if eventOf(got)["kind"] != "herdr" || len(h.calls("agent|list|")) != 1 {
		t.Fatalf("stale heartbeat: wait = %v, list calls = %d", got, len(h.calls("agent|list|")))
	}
	h.ok(nil, "ack", id(num(eventOf(got), "id")), "--as", id(top))

	// The quota scan stays with wait even while the daemon is fresh.
	setMeta(db, heartbeatKey, now())
	db.Exec(`update tasks set last_poll_at = null`)
	h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "300", "--scan-quota")
	if l, r := len(h.calls("agent|list|")), len(h.calls("agent|read|")); l != 1 || r != 1 {
		t.Fatalf("fresh heartbeat with --scan-quota: list %d read %d, want 1 and 1", l, r)
	}
}

func TestDaemonOwnerNotificationOnce(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	l := h.launch(w)
	h.setAgents("w9:p1/working/1")
	long := "Ship  the cart\nprice change " + strings.Repeat("é", 150)
	h.ok(as(w, l), "ask", long, "--owner", "--blocking")
	h.ok(as(w, l), "ask", "not for the owner")
	h.ok(as(w, l), "ask", "quiet owner action", "--owner")
	answered := num(h.ok(as(w, l), "ask", "already answered", "--owner"), "ask_id")
	h.ok(nil, "answer", id(answered), "yes")

	h.ok(nil, "daemon", "--once")
	h.ok(nil, "daemon", "--once")
	body := "Ship the cart price change " + strings.Repeat("é", 120-len([]rune("Ship the cart price change "))-1) + "…"
	want := []string{"notification|show|taskr: decision needed|--body|" + body + "|--sound|request|"}
	if got := h.calls("notification|"); !reflect.DeepEqual(got, want) {
		t.Fatalf("notification calls = %q, want %q", got, want)
	}

	// A failed notification is logged and not retried.
	h.write("notify.exit", "1", 0o644)
	h.ok(as(w, l), "ask", "second question", "--owner", "--blocking")
	h.ok(nil, "daemon", "--once")
	h.ok(nil, "daemon", "--once")
	if n := len(h.calls("notification|")); n != 2 {
		t.Fatalf("notification calls after a failure = %d, want 2", n)
	}
	logb, _ := os.ReadFile(filepath.Join(h.dir, ".local", "state", "taskr", "daemon.log"))
	if !strings.Contains(string(logb), "failed") || strings.Contains(string(logb), "second question") || strings.Contains(string(logb), "Ship") {
		t.Fatalf("daemon.log = %q", logb)
	}
}

func TestDaemonTokensOnlyOnChange(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	l := h.launch(w)
	h.setAgents("w9:p1/working/1")
	d := &daemon{db: h.openDB(), log: &daemonLog{}, sock: h.herdrSock}
	writes := func() []string { return h.calls("pane|report-metadata|") }
	d.pass()
	d.pass()
	if n := len(writes()); n != 0 {
		t.Fatalf("token writes before any change = %d", n)
	}
	h.ok(as(w, l), "ready", "slice 1")
	d.pass()
	d.pass()
	want := []string{"pane|report-metadata|w9:p1|--source|taskr|--token|taskr_state=ready|--token|taskr_round=0|"}
	if got := writes(); !reflect.DeepEqual(got, want) {
		t.Fatalf("token writes = %q, want %q", got, want)
	}
	ask := num(h.ok(as(w, l), "ask", "which base?", "--blocking"), "ask_id")
	h.openDB().Exec(`update tasks set waiting_until = ? where id = ?`, stamp(time.Now().Add(time.Hour)), w)
	d.pass()
	if got := writes(); len(got) != 2 || !strings.Contains(got[1], "taskr_state=waiting|") {
		t.Fatalf("token writes = %q, want a waiting write", got)
	}

	// A failed write is logged and retried on the next pass.
	h.write("meta.exit", "1", 0o644)
	h.openDB().Exec(`update tasks set waiting_until = null where id = ?`, w)
	d.pass()
	d.pass()
	h.write("meta.exit", "0", 0o644)
	d.pass()
	d.pass()
	if got := writes(); len(got) != 5 || !strings.Contains(got[4], "taskr_state=ask|") {
		t.Fatalf("token writes = %q, want two failed and one ask write", got)
	}
	h.ok(nil, "answer", id(ask), "main")
	h.ok(as(w, l), "done")
	d.pass()
	if got := writes(); len(got) != 6 || !strings.Contains(got[5], "taskr_state=done|") {
		t.Fatalf("token writes = %q, want done", got)
	}
}

func TestFakeSocketEnforcesPaneID(t *testing.T) {
	contractGuard(t)
	bad := map[string]any{"method": "events.subscribe", "params": map[string]any{"subscriptions": []any{
		map[string]any{"type": "pane.exited"}, map[string]any{"type": "pane.agent_status_changed"}}}}
	want := `{"id":"taskr-daemon","error":{"code":"invalid_request","message":"missing field pane_id"}}` + "\n"
	if got := subscriptionError(bad); got != want {
		t.Fatalf("type-only status subscription: %q", got)
	}
	var req map[string]any
	json.Unmarshal(subscribeRequest([]string{"w9:p1"}), &req)
	if got := subscriptionError(req); got != "" || !reflect.DeepEqual(panesOf(req), []string{"w9:p1"}) {
		t.Fatalf("daemon request %v rejected: %q", req, got)
	}
}

// serve answers every connection like Herdr: validate, ack, keep the stream
// open. Each accepted request's pane list goes to the returned channel.
func (s *fakeSocket) serve() <-chan []string {
	reqs := make(chan []string, 16)
	go func() {
		for c := range s.conns {
			r := bufio.NewReader(c)
			line, err := r.ReadBytes('\n')
			if err != nil {
				continue // a liveness dial
			}
			s.stream(c)
			var req map[string]any
			json.Unmarshal(line, &req)
			if e := subscriptionError(req); e != "" {
				c.Write([]byte(e))
				c.Close()
				reqs <- nil
				continue
			}
			c.Write([]byte(ack))
			reqs <- panesOf(req)
		}
	}()
	return reqs
}

// poke sends one global event line on the newest stream, as Herdr does on
// agent detection; it marks the daemon dirty.
func (s *fakeSocket) poke() {
	s.mu.Lock()
	c := s.subs[len(s.subs)-1]
	s.mu.Unlock()
	c.Write([]byte(`{"event":"pane.agent_detected","data":{"pane_id":"w9:p0"}}` + "\n"))
}

func nextPanes(t *testing.T, reqs <-chan []string) []string {
	t.Helper()
	select {
	case p := <-reqs:
		if p == nil {
			t.Fatal("server rejected the subscription")
		}
		return p
	case <-time.After(10 * time.Second):
		t.Fatal("no subscription")
	}
	return nil
}

func noMorePanes(t *testing.T, reqs <-chan []string) {
	t.Helper()
	select {
	case p := <-reqs:
		t.Fatalf("unexpected resubscribe with %v", p)
	case <-time.After(1500 * time.Millisecond):
	}
}

func TestDaemonPerPaneSubscriptions(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0, "--pane", "w9:p0")
	// A legacy root launch (written before launch rejected roots) is not watched.
	db := h.openDB()
	res, err := db.Exec(`insert into launches (task_id, provider, model, effort, pane_id, recorded_at) values (?, 'claude', 'm', 'high', 'w9:p0', ?)`, top, now())
	if err != nil {
		t.Fatal(err)
	}
	legacyLaunch, err := res.LastInsertId()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(`update tasks set current_launch_id = ? where id = ?`, legacyLaunch, top); err != nil {
		t.Fatal(err)
	}
	a := h.newTask("a", "implementer", top, "--pane", "w9:p1")
	h.launch(a)
	wt := h.newTask("wt", "implementer", top, "--pane", "w9:p2")
	h.launch(wt)
	h.openDB().Exec(`update tasks set waiting_until = ? where id = ?`, stamp(time.Now().Add(time.Hour)), wt)
	h.launch(h.newTask("g", "gate", top, "--pane", "w9:p3"))
	x := h.newTask("x", "implementer", top, "--pane", "w9:p4")
	h.launch(x)
	h.ok(nil, "close", id(x))
	h.newTask("nolaunch", "implementer", top, "--pane", "w9:p5")
	h.setAgents("w9:p1/working/1", "w9:p2/working/1")

	s := newFakeSocket(t)
	reqs := s.serve()
	h.startDaemon(s)
	if got := nextPanes(t, reqs); !reflect.DeepEqual(got, []string{"w9:p1", "w9:p2"}) {
		t.Fatalf("initial status panes = %v, want w9:p1 and w9:p2 (waiting included)", got)
	}
	noMorePanes(t, reqs) // the ack's pass finds the same set

	// A new launch with a new pane: exactly one resubscribe carrying it.
	b := h.newTask("b", "implementer", top, "--pane", "w9:p6")
	h.launch(b)
	h.setAgents("w9:p1/working/1", "w9:p2/working/1", "w9:p6/working/1")
	s.poke()
	if got := nextPanes(t, reqs); !reflect.DeepEqual(got, []string{"w9:p1", "w9:p2", "w9:p6"}) {
		t.Fatalf("after launch: status panes = %v", got)
	}
	noMorePanes(t, reqs)

	// Closing the task drops its pane.
	h.ok(nil, "close", id(b))
	s.poke()
	if got := nextPanes(t, reqs); !reflect.DeepEqual(got, []string{"w9:p1", "w9:p2"}) {
		t.Fatalf("after close: status panes = %v", got)
	}
	noMorePanes(t, reqs)
	logb, _ := os.ReadFile(filepath.Join(h.dir, ".local", "state", "taskr", "daemon.log"))
	if n := strings.Count(string(logb), "resubscribing with"); n != 2 ||
		!strings.Contains(string(logb), "resubscribing with 3 panes") {
		t.Fatalf("daemon.log resubscribe lines = %d:\n%s", n, logb)
	}
}

func TestDaemonEmptyPaneSetSubscribes(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	s := newFakeSocket(t)
	reqs := s.serve()
	h.startDaemon(s)
	if got := nextPanes(t, reqs); len(got) != 0 {
		t.Fatalf("empty ledger: status panes = %v", got)
	}
	eventually(t, "heartbeat", func() bool { return h.ok(nil, "daemon", "--status")["daemon"] == "fresh" })
	noMorePanes(t, reqs)
}

// The heartbeat handoff: a skip under a fresh heartbeat claims nothing, so
// the first wait after it goes stale observes, inside the same 15 s window.
func TestWaitObservesWhenHeartbeatGoesStale(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	h.launch(w)
	h.ok(nil, "prompt", id(w), "--text", "go", "--receipt-timeout", "0") // an owed result, no receipt deadline: the hint emits
	h.setAgents("w9:p1/idle/3")
	db := h.openDB()
	setMeta(db, heartbeatKey, stamp(time.Now().Add(-29*time.Second)))
	h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "300")
	if n := len(h.calls("agent|list|")); n != 0 {
		t.Fatalf("heartbeat 29 s old: agent list calls = %d", n)
	}
	setMeta(db, heartbeatKey, stamp(time.Now().Add(-31*time.Second)))
	got := h.ok(nil, "wait", "--as", id(top), "--timeout", "2000")
	if eventOf(got)["kind"] != "herdr" || len(h.calls("agent|list|")) != 1 {
		t.Fatalf("heartbeat 31 s old: wait = %v, agent list calls = %d", got, len(h.calls("agent|list|")))
	}
}

// run.sh starts the daemon detached and returns at once; a missing binary is
// exit 0. The fake daemon lives 10 s and the launcher must return within 2 s,
// so a foreground start fails; the fake is killed and reaped at cleanup.
func TestPluginRunShDetaches(t *testing.T) {
	contractGuard(t)
	home := t.TempDir()
	script, _ := filepath.Abs(filepath.Join("plugin", "run.sh"))
	cmd := exec.Command("sh", script)
	cmd.Env = []string{"HOME=" + home, "PATH=" + os.Getenv("PATH")}
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil || !strings.Contains(stderr.String(), "not found") {
		t.Fatalf("missing binary: err %v stderr %q", err, stderr.String())
	}
	bin := filepath.Join(home, ".local", "bin")
	os.MkdirAll(bin, 0o755)
	marker, pidFile := filepath.Join(home, "argv"), filepath.Join(home, "pid")
	fake := "#!/bin/sh\necho \"$@\" > " + marker + ".tmp\nmv " + marker + ".tmp " + marker + "\n" +
		"echo $$ > " + pidFile + ".tmp\nmv " + pidFile + ".tmp " + pidFile + "\nexec sleep 10\n"
	os.WriteFile(filepath.Join(bin, "taskr"), []byte(fake), 0o755)
	fakePID := func() int {
		b, _ := os.ReadFile(pidFile)
		n, _ := strconv.Atoi(strings.TrimSpace(string(b)))
		return n
	}
	t.Cleanup(func() {
		pid := fakePID()
		if pid <= 0 {
			return
		}
		syscall.Kill(pid, syscall.SIGKILL)
		// The fake is reparented once run.sh exits, so its reaper is init;
		// wait until the pid is gone.
		for end := time.Now().Add(5 * time.Second); time.Now().Before(end); time.Sleep(20 * time.Millisecond) {
			if syscall.Kill(pid, 0) != nil {
				return
			}
		}
		t.Errorf("fake daemon %d still exists after SIGKILL", pid)
	})

	cx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	began := time.Now()
	cmd = exec.CommandContext(cx, "sh", script)
	cmd.Env = []string{"HOME=" + home, "PATH=" + os.Getenv("PATH")}
	err := cmd.Run()
	if d := time.Since(began); d > 2*time.Second {
		t.Fatalf("run.sh took %v; the daemon must start detached (err %v)", d, err)
	}
	if err != nil {
		t.Fatalf("run.sh: %v", err)
	}
	eventually(t, "detached daemon start", func() bool {
		b, _ := os.ReadFile(marker)
		return string(b) == "daemon\n" && fakePID() > 0
	})
	if pid := fakePID(); syscall.Kill(pid, 0) != nil {
		t.Fatalf("fake daemon %d did not outlive run.sh", pid)
	}
}

// Herdr 0.9.2: a reader that falls behind gets an error line on the stream
// (keeping the request id). The daemon logs it once, resubscribes at once
// (no backoff), and runs a pass to recover what was lost.
func TestDaemonStreamErrorResubscribes(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	h.launch(w)
	h.ok(nil, "prompt", id(w), "--text", "go", "--receipt-timeout", "0") // an owed result, no receipt deadline: the hints emit
	h.setAgents("w9:p1/idle/3")
	s := newFakeSocket(t)
	h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(ack))
	lists := func() int { return len(h.calls("agent|list|")) }
	eventually(t, "first pass", func() bool { return lists() >= 1 })
	time.Sleep(700 * time.Millisecond)
	before := lists()
	h.setAgents("w9:p1/done/4")
	began := time.Now()
	c.Write([]byte(`{"id":"taskr-daemon","error":{"code":"events_lost","message":"reader fell behind"}}` + "\n"))
	c2, _, req := s.next()
	if req["method"] != "events.subscribe" || time.Since(began) >= daemonRetryBase {
		t.Fatalf("resubscribe after %v: %v", time.Since(began), req)
	}
	c2.Write([]byte(ack))
	eventually(t, "recovery pass", func() bool { return lists() > before && h.countHerdr(top) == 2 })
	log := h.daemonLog()
	if n := strings.Count(log, "stream error: events_lost reader fell behind; resubscribing"); n != 1 {
		t.Fatalf("stream error lines = %d:\n%s", n, log)
	}
	if strings.Contains(log, "disconnected; reconnect in") {
		t.Fatalf("an error line took the backoff path:\n%s", log)
	}
	// The first connection was closed by the daemon.
	c.SetReadDeadline(time.Now().Add(2 * time.Second))
	if _, err := c.Read(make([]byte, 1)); err == nil || os.IsTimeout(err) {
		t.Fatalf("old stream still open: %v", err)
	}
}

// A second error right after the first reconnects with backoff, not in a
// tight loop; an event payload that merely mentions "error" is not a reply.
func TestDaemonStreamErrorNoSpin(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	s := newFakeSocket(t)
	h.startDaemon(s)
	lost := `{"id":"taskr-daemon","error":{"code":"events_lost","message":"x"}}` + "\n"
	c, _, _ := s.next()
	c.Write([]byte(ack + `{"event":"pane.exited","data":{"error":{"code":"nope"}}}` + "\n"))
	s.noStream(300*time.Millisecond, "an event mentioning error closed the stream")
	c.Write([]byte(lost))
	c2, _, _ := s.next()
	c2.Write([]byte(ack + lost))
	s.next()
	if log := h.daemonLog(); !strings.Contains(log, "disconnected; reconnect in") || strings.Count(log, "stream error:") != 1 {
		t.Fatalf("second error within the gap should back off, logged once:\n%s", log)
	}
}

// A subscription setup error carrying the request id is still an ack-path
// rejection, not a stream error.
func TestDaemonSetupErrorWithID(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	s := newFakeSocket(t)
	h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(`{"id":"taskr-daemon","error":{"code":"invalid_request","message":"bad"}}` + "\n"))
	c.Close()
	s.next()
	log := h.daemonLog()
	if !strings.Contains(log, "subscribe rejected: invalid_request bad") || strings.Contains(log, "stream error") {
		t.Fatalf("daemon.log = %s", log)
	}
}

// A socket file with no server behind it: the daemon makes no herdr CLI call
// (one could start a server from here), logs once, and still exits when the
// path disappears.
func TestDaemonNoHerdrCallsWithoutServer(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	root := h.newTask("top", "orchestrator", 0)
	h.launch(h.newTask("worker", "implementer", root, "--pane", "w9:p1"))
	h.setAgents("w9:p1/idle/2")
	dir, err := os.MkdirTemp("", "tk")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	sock := filepath.Join(dir, "dead.sock")
	ln, err := net.ListenUnix("unix", &net.UnixAddr{Name: sock, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	ln.SetUnlinkOnClose(false)
	ln.Close()
	old := daemonFallback
	daemonFallback = 30 * time.Millisecond
	t.Cleanup(func() { daemonFallback = old })

	logf := filepath.Join(h.dir, "daemon.log")
	lf, err := os.Create(logf)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { lf.Close() })
	d := &daemon{db: h.openDB(), log: &daemonLog{f: lf}}
	cx, cancel := context.WithTimeout(context.Background(), 400*time.Millisecond)
	defer cancel()
	if why := d.run(cx, sock); why != "signal" {
		t.Fatalf("run ended with %q", why)
	}
	if calls := h.calls(""); len(calls) > 1 {
		t.Fatalf("herdr CLI calls without a server: %q", calls)
	}
	if b, _ := os.ReadFile(logf); strings.Count(string(b), "no Herdr server") != 1 {
		t.Fatalf("daemon.log = %q, want one no-server line", b)
	}

	// The path disappears: the daemon exits.
	done := make(chan string, 1)
	go func() { done <- d.run(context.Background(), sock) }()
	time.Sleep(100 * time.Millisecond)
	os.Remove(sock)
	select {
	case why := <-done:
		if why != "socket removed" {
			t.Fatalf("run ended with %q", why)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("daemon did not exit after the socket was removed")
	}
	if calls := h.calls(""); len(calls) > 1 {
		t.Fatalf("herdr CLI calls without a server: %q", calls)
	}
}
