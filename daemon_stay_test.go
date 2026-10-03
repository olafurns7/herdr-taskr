package main

import (
	"context"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
)

func TestDaemonStayOfflineRPCAndDashboard(t *testing.T) {
	setVar(t, &daemonHeartbeat, 50*time.Millisecond)
	setVar(t, &daemonFallback, 50*time.Millisecond)
	fakeTailnetHooks(t)
	h := newHarness(t)
	port := freePort(t)
	tailnet("::1", h)
	h.writeAddr("tailnet:" + port)
	sock := filepath.Join(h.dir, "absent.sock")
	t.Setenv("HERDR_SOCKET_PATH", sock)
	h.openDB() // initialize the ledger before startup and status open it concurrently
	cx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		_, _, err := cmdDaemon(&ctx{cx: cx, getenv: h.getenv(map[string]string{"HERDR_SOCKET_PATH": sock}), out: io.Discard, errw: io.Discard, json: true}, []string{"--stay"})
		done <- err
		close(done)
	}()
	t.Cleanup(func() { cancel(); <-done })
	t.Cleanup(func() {
		if t.Failed() {
			t.Logf("scratch daemon log: %s", h.daemonLog())
		}
	})
	eventually(t, "offline dashboard and hub", func() bool {
		select {
		case err := <-done:
			t.Fatalf("offline daemon ended at startup: %v", err)
		default:
		}
		st := h.ok(nil, "daemon", "--status")
		return st["dashboard"] == "up" && st["tailnet_url"] != nil && st["stay"] == true
	})
	r := &twoHost{harness: h, url: "http://[::1]:" + port}
	code, rep, raw := r.post("host-a", rpcBody(h.dir, nil, newRequestKey(), "status"))
	if code != http.StatusOK || rep.Exit != exitOK {
		t.Fatalf("offline RPC = %d %+v %s", code, rep, raw)
	}
	getJSON(t, r.url+"/api/state", &dashState{})
	select {
	case why := <-done:
		t.Fatalf("offline daemon exited: %s", why)
	case <-time.After(150 * time.Millisecond):
	}
	if calls := r.calls(""); strings.TrimSpace(strings.Join(calls, "")) != "" {
		t.Fatalf("offline Herdr calls: %q", calls)
	}
	// Local DB waits still wake on answers while the service has no Herdr connection.
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("worker", "implementer", top, "--pane", "w9:p1")
	l := h.launch(w)
	env := as(w, l)
	env["HERDR_SOCKET_PATH"] = sock
	ask := num(h.ok(env, "ask", "Which base?", "--blocking"), "ask_id")
	waited := h.goRun(env, "wait", "--as", id(w), "--timeout", "5000")
	eventually(t, "offline local wait", func() bool { return h.waitingUntil(w).Valid })
	h.ok(map[string]string{"HERDR_SOCKET_PATH": sock}, "answer", id(ask), "main")
	select {
	case result := <-waited:
		if result.code != exitOK || eventOf(result.out)["kind"] != "answer" {
			t.Fatalf("offline wait = %+v", result)
		}
	case <-time.After(6 * time.Second):
		t.Fatal("offline local wait did not wake")
	}
}

func TestDaemonStayAttachesAndReattaches(t *testing.T) {
	setVar(t, &daemonHeartbeat, 50*time.Millisecond)
	setVar(t, &daemonFallback, 50*time.Millisecond)
	h := newHarness(t)
	w := h.newTask("worker", "implementer", h.newTask("top", "orchestrator", 0), "--pane", "w9:p1")
	h.launch(w)
	h.setAgents("w9:p1/working/1")
	s := newFakeSocket(t)
	path := s.path
	s.shutdown()
	d := &daemon{db: h.openDB(), log: &daemonLog{}, stay: true}
	cx, cancel := context.WithCancel(context.Background())
	done := make(chan string, 1)
	go func() { done <- d.run(cx, path); close(done) }()
	t.Cleanup(func() { cancel(); <-done })
	// A stale path also skips Herdr commands until a server accepts there.
	before := len(h.calls("agent|list|"))
	if err := os.WriteFile(path, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	time.Sleep(150 * time.Millisecond)
	if len(h.calls("agent|list|")) != before {
		t.Fatal("Herdr command ran against a refused socket")
	}
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 2; i++ {
		s = newFakeSocketAt(t, path)
		c, _, _ := s.next()
		c.Write([]byte(ack))
		want := i + 1
		eventually(t, "pane token on attach", func() bool { return len(h.calls("pane|report-metadata|")) >= want })
		s.shutdown()
		select {
		case why := <-done:
			t.Fatalf("stay exited on removal: %s", why)
		case <-time.After(150 * time.Millisecond):
		}
	}
}

func TestDaemonStayListenerRetries(t *testing.T) {
	h := newHarness(t)
	busy, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { busy.Close() })
	h.writeAddr(busy.Addr().String())
	d := startDashboard(h.openDB(), &daemonLog{}, h.stateDir(), true)
	if d == nil {
		t.Fatal("stay dashboard did not retain retry state")
	}
	t.Cleanup(d.stop)
	busy.Close()
	eventually(t, "dashboard bind retry", func() bool {
		_, ok, _ := getMeta(h.openDB(), dashboardURLKey)
		return ok
	})
	resp, err := http.Get("http://" + busy.Addr().String() + "/")
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("dashboard = %d", resp.StatusCode)
	}
}

func TestDaemonStayResubscribeWritesOnlyNewPane(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	h.launch(h.newTask("a", "implementer", top, "--pane", "w9:p1"))
	h.launch(h.newTask("b", "implementer", top, "--pane", "w9:p2"))
	h.setAgents("w9:p1/working/1", "w9:p2/working/1")
	s := newFakeSocket(t)
	reqs := s.serve()
	d := &daemon{db: h.openDB(), log: &daemonLog{}, stay: true}
	cx, cancel := context.WithCancel(context.Background())
	done := make(chan string, 1)
	go func() { done <- d.run(cx, s.path); close(done) }()
	t.Cleanup(func() { cancel(); <-done })
	nextPanes(t, reqs)
	eventually(t, "initial pane tokens", func() bool { return len(h.calls("pane|report-metadata|")) >= 2 })
	noMorePanes(t, reqs)
	before := len(h.calls("pane|report-metadata|"))
	h.launch(h.newTask("c", "implementer", top, "--pane", "w9:p3"))
	h.setAgents("w9:p1/working/1", "w9:p2/working/1", "w9:p3/working/1")
	s.poke()
	if got := nextPanes(t, reqs); len(got) != 3 {
		t.Fatalf("new pane subscription = %v", got)
	}
	eventually(t, "new pane token", func() bool { return len(h.calls("pane|report-metadata|")) > before })
	noMorePanes(t, reqs)
	if got := len(h.calls("pane|report-metadata|")) - before; got != 1 {
		t.Fatalf("adding one pane wrote %d tokens, want 1", got)
	}
}

func TestDaemonStayResubscribeBeforeAckWritesAllPanes(t *testing.T) {
	setVar(t, &daemonFallback, 50*time.Millisecond)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	h.launch(h.newTask("a", "implementer", top, "--pane", "w9:p1"))
	h.launch(h.newTask("b", "implementer", top, "--pane", "w9:p2"))
	h.setAgents("w9:p1/working/1", "w9:p2/working/1")
	s := newFakeSocket(t)
	d := &daemon{db: h.openDB(), log: &daemonLog{}, stay: true}
	cx, cancel := context.WithCancel(context.Background())
	done := make(chan string, 1)
	go func() { done <- d.run(cx, s.path); close(done) }()
	t.Cleanup(func() { cancel(); <-done })
	_, _, first := s.next() // keep the first request unacked
	if got := panesOf(first); len(got) != 2 {
		t.Fatalf("first pane subscription = %v", got)
	}
	h.launch(h.newTask("c", "implementer", top, "--pane", "w9:p3"))
	h.setAgents("w9:p1/working/1", "w9:p2/working/1", "w9:p3/working/1")
	c, _, second := s.next()
	if got := panesOf(second); len(got) != 3 {
		t.Fatalf("second pane subscription = %v", got)
	}
	if got := h.calls("pane|report-metadata|"); len(got) != 0 {
		t.Fatalf("tokens written before ack: %v", got)
	}
	if _, err := c.Write([]byte(ack)); err != nil {
		t.Fatal(err)
	}
	eventually(t, "all pane tokens after the first ack", func() bool { return len(h.calls("pane|report-metadata|")) >= 3 })
	for _, pane := range []string{"w9:p1", "w9:p2", "w9:p3"} {
		if got := h.calls("pane|report-metadata|" + pane + "|"); len(got) != 1 {
			t.Fatalf("token writes for %s = %v, want one", pane, got)
		}
	}
}

func TestDaemonStayHerdrMissingDoesNotClaimOwnerAsk(t *testing.T) {
	setVar(t, &daemonFallback, 50*time.Millisecond)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("worker", "implementer", top, "--pane", "w9:p1")
	l := h.launch(w)
	ask := num(h.ok(as(w, l), "ask", "keep this decision pending", "--owner"), "ask_id")
	db := h.openDB()
	s := newFakeSocket(t)
	reqs := s.serve()
	t.Setenv("HERDR_SOCKET_PATH", s.path)
	t.Setenv("PATH", t.TempDir())
	h.writeAddr("127.0.0.1:0")
	cx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		_, _, err := cmdDaemon(&ctx{cx: cx, getenv: h.getenv(map[string]string{"HERDR_SOCKET_PATH": s.path}), out: io.Discard, errw: io.Discard, json: true}, []string{"--stay"})
		done <- err
		close(done)
	}()
	t.Cleanup(func() { cancel(); <-done })
	nextPanes(t, reqs)
	eventually(t, "missing Herdr status", func() bool {
		st := h.ok(nil, "daemon", "--status")
		return st["herdr_missing"] == true && st["dashboard"] == "up"
	})
	noMorePanes(t, reqs) // several fallback passes with an accepting socket
	if _, claimed, err := getMeta(db, "notified:"+id(ask)); err != nil || claimed {
		t.Fatalf("owner ask claimed with Herdr missing: %v (%v)", claimed, err)
	}
	if calls := h.calls(""); strings.TrimSpace(strings.Join(calls, "")) != "" {
		t.Fatalf("Herdr calls with executable missing: %v", calls)
	}
	if log := h.daemonLog(); strings.Contains(log, "observe failed") || strings.Contains(log, "notify ask") {
		t.Fatalf("Herdr commands attempted with executable missing: %s", log)
	}
}

func TestDaemonStayHerdrMissingStatus(t *testing.T) {
	for _, missing := range []bool{false, true} {
		t.Run(strconv.FormatBool(missing), func(t *testing.T) {
			r := newRestartRig(t)
			r.s.shutdown()
			if missing {
				r.env["PATH"] = t.TempDir()
			}
			r.startStay(t, false)
			eventually(t, "stay dashboard", func() bool {
				_, st := r.run(nil, "daemon", "--status")
				return st["stay"] == true && st["dashboard"] == "up"
			})
			_, st := r.run(nil, "daemon", "--status")
			value, present := st["herdr_missing"]
			if present != missing || (missing && value != true) {
				t.Fatalf("herdr_missing = %v (present %v), missing %v", value, present, missing)
			}
			rec, err := readClientDaemonRecord(filepath.Join(filepath.Dir(r.lock), localDaemonRecordFile))
			if err != nil {
				t.Fatal(err)
			}
			b, _ := json.Marshal(rec)
			if strings.Contains(string(b), `"herdr_missing":true`) != missing {
				t.Fatalf("identity missing flag = %s", b)
			}
			log := r.h.daemonLog()
			if missing && (strings.Count(log, "herdr missing") != 1 || !strings.Contains(log, "PATH="+r.env["PATH"])) {
				t.Fatalf("missing Herdr startup log = %s", log)
			}
			if !missing && strings.Contains(log, "herdr missing") {
				t.Fatalf("unexpected missing Herdr startup log = %s", log)
			}
		})
	}
}

func TestClientDaemonStayUsageError(t *testing.T) {
	r := newRestartRig(t)
	r.clientMode(t)
	cx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	cmd := exec.CommandContext(cx, r.bin, "--json", "daemon", "--stay")
	cmd.Env = []string{"HOME=" + r.h.dir, "PATH=" + r.env["PATH"], "HERDR_SOCKET_PATH=" + r.s.path}
	b, err := cmd.Output()
	if cx.Err() != nil {
		t.Fatal("client --stay did not exit within 5s; stopped scratch daemon")
	}
	if _, ok := err.(*exec.ExitError); !ok {
		t.Fatalf("client --stay error = %v", err)
	}
	code := cmd.ProcessState.ExitCode()
	var out map[string]any
	if err := json.Unmarshal(b, &out); err != nil {
		t.Fatalf("client --stay output: %s (%v)", b, err)
	}
	if code != exitUsage || !strings.Contains(out["error"].(string), "local") {
		t.Fatalf("client --stay = %d %v", code, out)
	}
	if _, err := os.Stat(r.lock); !os.IsNotExist(err) {
		t.Fatalf("client stay touched lock: %v", err)
	}
}
