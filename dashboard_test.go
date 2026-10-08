package main

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
	"time"
)

// dashFixture is two open campaigns, one closed one, and owner asks from a
// nested worker and from a root.
type dashFixture struct {
	a, sub, impl, rev, b, bw, c int64
	implL, bwL                  int64
	workerAsk, rootAsk, plain   int64
}

func seedDashboard(h *harness) dashFixture {
	h.t.Helper()
	var f dashFixture
	f.a = h.newTask("camp-a", "orchestrator", 0, "--workspace", "w1", "--tab", "w1:t1", "--pane", "w1:p1")
	h.ok(nil, "note", "--as", id(f.a), "phase 2: reviewing")
	f.sub = h.newTask("sub-a", "sub-orchestrator", f.a, "--pane", "w1:p2")
	h.launch(f.sub)
	f.impl = h.newTask("impl-a", "implementer", f.sub, "--pane", "w1:p3", "--report", "r/impl.md")
	f.implL = h.launch(f.impl)
	f.rev = h.newTask("rev-a", "reviewer", f.a, "--pane", "w1:p4")
	h.launch(f.rev)
	f.workerAsk = num(h.ok(as(f.impl, f.implL), "ask", "Ship <b>now</b>?\nOr wait?", "--owner", "--blocking"), "ask_id")
	f.plain = num(h.ok(as(f.impl, f.implL), "ask", "not for the owner"), "ask_id")
	h.ok(as(f.impl, f.implL), "note", "<script>alert(1)</script>")

	f.b = h.newTask("camp-b", "orchestrator", 0)
	f.bw = h.newTask("impl-b", "implementer", f.b, "--pane", "w2:p2")
	f.bwL = h.launch(f.bw)
	f.rootAsk = num(h.ok(nil, "ask", "--as", id(f.b), "Which branch?", "--owner"), "ask_id")

	f.c = h.newTask("camp-c", "orchestrator", 0)
	cw := h.newTask("impl-c", "implementer", f.c)
	h.ok(nil, "note", "--as", id(f.c), "all done")
	h.ok(nil, "close", id(cw))
	h.ok(nil, "close", id(f.c))
	return f
}

const testAddr = "127.0.0.1:7788"

func (h *harness) dash() *dashboard { return newDashboard(h.openDB(), &daemonLog{}, testAddr) }

// probePath is the one route; a GET of it answers admitted (it takes POST
// only) once a request has passed admission and the Host check.
const (
	probePath = rpcPath
	admitted  = http.StatusMethodNotAllowed
)

// req builds a request from this machine; mods change it.
func req(method, path, body string, mods ...func(*http.Request)) *http.Request {
	r := httptest.NewRequest(method, "http://"+testAddr+path, strings.NewReader(body))
	r.Host = testAddr
	r.RemoteAddr = "127.0.0.1:50000" // a process on this machine
	if method != http.MethodGet {
		r.Header.Set("Origin", "http://"+testAddr)
		r.Header.Set("Sec-Fetch-Site", "same-origin")
		r.Header.Set("Content-Type", "application/json")
	}
	for _, m := range mods {
		m(r)
	}
	return r
}

func serve(d *dashboard, r *http.Request) (*httptest.ResponseRecorder, map[string]any) {
	w := httptest.NewRecorder()
	d.ServeHTTP(w, r)
	var m map[string]any
	json.Unmarshal(w.Body.Bytes(), &m)
	return w, m
}

func answerEvent(t *testing.T, h *harness, ask int64) (int64, map[string]any) {
	t.Helper()
	var aid int64
	if err := h.openDB().QueryRow(`select answered_by from events where id = ?`, ask).Scan(&aid); err != nil {
		t.Fatalf("ask %d not answered: %v", ask, err)
	}
	ev, err := loadEvent(h.openDB(), aid)
	if err != nil {
		t.Fatal(err)
	}
	return aid, viaJSON(ev)
}

// viaJSON round-trips a ledger row so its numbers read like CLI output.
func viaJSON(m map[string]any) map[string]any {
	b, _ := json.Marshal(m)
	var out map[string]any
	json.Unmarshal(b, &out)
	return out
}

func ownerAnswers(t *testing.T, h *harness) []map[string]any {
	t.Helper()
	rows, err := h.openDB().Query(`select ` + eventCols + ` from events e join tasks t on t.id = e.task_id
		where e.kind = 'owner_answer' order by e.id`)
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()
	var out []map[string]any
	for rows.Next() {
		m, err := scanEvent(rows)
		if err != nil {
			t.Fatal(err)
		}
		out = append(out, viaJSON(m))
	}
	return out
}

// Owner asks are answered with the CLI, in the orchestrator's pane: no via,
// no owner_answer notice.
func TestCLIAnswerUnchanged(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	f := seedDashboard(h)
	out := h.ok(nil, "answer", id(f.workerAsk), "from the CLI")
	if out["asker_waiting"] != false || num(out, "task_id") != f.impl {
		t.Fatalf("answer = %v", out)
	}
	_, ans := answerEvent(t, h, f.workerAsk)
	if data, _ := ans["data"].(map[string]any); data["via"] != nil || data["owner"] != true {
		t.Fatalf("CLI answer data = %v", data)
	}
	if oa := ownerAnswers(t, h); len(oa) != 0 {
		t.Fatalf("CLI answer wrote owner_answer: %v", oa)
	}
	h.ok(nil, "answer", id(f.plain), "non-owner asks still answer from the CLI")
}

// The server has no page and no write route but the RPC: the old routes are
// gone, and Host is still checked before anything is served.
func TestDashboardReadOnly(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	f := seedDashboard(h)
	d := h.dash()
	body := `{"text":"fine"}`
	cases := []struct {
		name string
		r    *http.Request
		code int
	}{
		{"old answer route", req("POST", fmt.Sprintf("/api/asks/%d/answer", f.workerAsk), body), 404},
		{"old answer route with a token", req("POST", fmt.Sprintf("/api/asks/%d/answer", f.rootAsk), body,
			func(r *http.Request) { r.Header.Set("X-Taskr-Token", strings.Repeat("0", 64)) }), 404},
		{"old hub relay route", req("POST", fmt.Sprintf("/api/peers/nX/asks/%d/answer", f.workerAsk), body), 404},
		{"old page", req("GET", "/", ""), 404},
		{"old state", req("GET", "/api/state", ""), 404},
		{"old campaigns", req("GET", "/api/campaigns", ""), 404},
		{"old campaign", req("GET", fmt.Sprintf("/api/campaign/%d", f.a), ""), 404},
		{"old document", req("GET", "/api/doc/1", ""), 404},
		{"old asset", req("GET", "/assets/app.js", ""), 404},
		{"old font", req("GET", "/fonts/OFL.txt", ""), 404},
		{"old peer push", req("POST", "/api/peer/push", `{"state":{},"now":"`+now()+`"}`), 404},
		{"wrong Host", req("GET", probePath, "", func(r *http.Request) { r.Host = "evil.example:7788" }), 421},
		{"wrong port in Host", req("GET", "/", "", func(r *http.Request) { r.Host = "127.0.0.1:9" }), 421},
	}
	for _, c := range cases {
		w, _ := serve(d, c.r)
		if w.Code != c.code || w.Header().Get("Access-Control-Allow-Origin") != "" {
			t.Errorf("%s: %d, want %d", c.name, w.Code, c.code)
		}
	}
	var answers int
	h.openDB().QueryRow(`select count(*) from events where kind in ('answer', 'owner_answer')`).Scan(&answers)
	if answers != 0 {
		t.Fatalf("%d answers written through the server", answers)
	}
	// localhost:<port> is still an accepted Host.
	if w, _ := serve(d, req("GET", probePath, "", func(r *http.Request) { r.Host = "localhost:7788" })); w.Code != admitted {
		t.Fatalf("GET as localhost = %d", w.Code)
	}
}

func TestDashboardAddr(t *testing.T) {
	contractGuard(t)
	dir := t.TempDir()
	write := func(s string) {
		if err := os.WriteFile(filepath.Join(dir, dashboardAddrFile), []byte(s), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	if a, err := dashboardAddr(dir); err != nil || a != dashboardDefaultAddr {
		t.Fatalf("no file: %q %v", a, err)
	}
	ok := map[string]string{"": dashboardDefaultAddr, "off\n": "", " off ": "", "127.0.0.1:9999\n": "127.0.0.1:9999",
		"[::1]:9999": "[::1]:9999", "127.0.0.1:0": "127.0.0.1:0"}
	for in, want := range ok {
		write(in)
		if a, err := dashboardAddr(dir); err != nil || a != want {
			t.Errorf("%q: %q %v, want %q", in, a, err, want)
		}
	}
	for _, in := range []string{"0.0.0.0:7788", "192.168.1.5:7788", "[::]:7788", "localhost:7788", "example.com:80",
		"7788", "127.0.0.1", "127.0.0.1:99999", "127.0.0.2:7788", "127.0.0.1:x"} {
		write(in)
		if a, err := dashboardAddr(dir); err == nil {
			t.Errorf("%q accepted as %q", in, a)
		}
	}
	write("10.0.0.1:7788")
	if _, err := dashboardAddr(dir); err == nil || !strings.Contains(err.Error(), "127.0.0.1 or ::1") {
		t.Fatalf("non-loopback error = %v", err)
	}
}

func (h *harness) stateDir() string {
	d := filepath.Join(h.dir, ".local", "state", "taskr")
	if err := os.MkdirAll(d, 0o755); err != nil {
		h.t.Fatal(err)
	}
	return d
}

func (h *harness) writeAddr(s string) {
	h.t.Helper()
	if err := os.WriteFile(filepath.Join(h.stateDir(), dashboardAddrFile), []byte(s+"\n"), 0o644); err != nil {
		h.t.Fatal(err)
	}
}

func (h *harness) daemonLog() string {
	b, _ := os.ReadFile(filepath.Join(h.stateDir(), "daemon.log"))
	return string(b)
}

func dashLines(log string) []string {
	var out []string
	for _, l := range strings.Split(log, "\n") {
		if strings.Contains(l, "dashboard") {
			out = append(out, l)
		}
	}
	return out
}

// The resident daemon serves the RPC route, --status reports it, and a
// clean exit takes it down and clears the URL.
func TestDashboardServedByDaemon(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	f := seedDashboard(h)
	s := newFakeSocket(t)
	p := h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(ack))
	var st map[string]any
	eventually(t, "dashboard up", func() bool {
		st = h.ok(nil, "daemon", "--status")
		return st["dashboard"] == "up" && st["daemon"] == "fresh"
	})
	url, _ := st["dashboard_url"].(string)
	if !regexp.MustCompile(`^http://127\.0\.0\.1:\d+/$`).MatchString(url) || strings.HasSuffix(url, ":0/") {
		t.Fatalf("dashboard_url = %q", url)
	}
	for path, want := range map[string]int{"": 404, "api/state": 404, strings.TrimPrefix(probePath, "/"): admitted} {
		resp, err := http.Get(url + path)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		if resp.StatusCode != want {
			t.Fatalf("GET /%s = %d, want %d", path, resp.StatusCode, want)
		}
	}
	r, _ := http.NewRequest("POST", fmt.Sprintf("%sapi/asks/%d/answer", url, f.workerAsk), strings.NewReader(`{"text":"go"}`))
	r.Header.Set("Content-Type", "application/json")
	r.Header.Set("Origin", strings.TrimSuffix(url, "/"))
	if resp, err := http.DefaultClient.Do(r); err != nil || resp.StatusCode != 404 {
		t.Fatalf("POST an answer = %v %v", resp, err)
	} else {
		resp.Body.Close()
	}

	s.shutdown()
	p.wait(t)
	out := p.lines(t)
	if len(out) != 1 || out[0]["dashboard_url"] != url {
		t.Fatalf("daemon stdout = %v", out)
	}
	if st := h.ok(nil, "daemon", "--status"); st["dashboard"] != "down" || st["running"] != false {
		t.Fatalf("status after exit = %v", st)
	}
	if _, ok, _ := getMeta(h.openDB(), dashboardURLKey); ok {
		t.Fatal("dashboard_url left in meta after a clean exit")
	}
	if _, err := http.Get(url); err == nil {
		t.Fatal("still serving after the daemon exited")
	}
	if l := dashLines(h.daemonLog()); len(l) != 1 || !strings.Contains(l[0], "dashboard at "+url) {
		t.Fatalf("dashboard log lines = %q", l)
	}
}

// A port in use costs one log line; the event bridge keeps passing.
func TestDashboardBindConflictDaemonCarriesOn(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl-a", "implementer", top, "--pane", "w9:p1")
	l := h.launch(w)
	h.setAgents("w9:p1/working/1")
	busy, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer busy.Close()
	h.writeAddr(busy.Addr().String())
	s := newFakeSocket(t)
	p := h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(ack))
	eventually(t, "heartbeat", func() bool { return h.ok(nil, "daemon", "--status")["daemon"] == "fresh" })
	// A pass still runs on a wake: the owner ask is notified.
	h.ok(as(w, l), "ask", "decide", "--owner", "--blocking")
	c.Write([]byte(eventLines(1)))
	eventually(t, "owner notification", func() bool { return len(h.calls("notification|")) == 1 })
	st := h.ok(nil, "daemon", "--status")
	if st["running"] != true || st["dashboard"] != "down" || st["dashboard_url"] != "http://"+busy.Addr().String()+"/" {
		t.Fatalf("status with the port taken = %v", st)
	}
	s.shutdown()
	p.wait(t)
	if p.code != exitOK || p.lines(t)[0]["dashboard_url"] != nil {
		t.Fatalf("daemon exit %d stdout %v", p.code, p.lines(t))
	}
	lines := dashLines(h.daemonLog())
	if len(lines) != 1 || !strings.Contains(lines[0], "listen "+busy.Addr().String()+" failed") {
		t.Fatalf("dashboard log lines = %q", lines)
	}
}

func TestDashboardOffAndRefusedInDaemon(t *testing.T) {
	contractGuard(t)
	for _, c := range []struct{ addr, status, log string }{
		{"off", "off", "dashboard: off"},
		{"0.0.0.0:7788", "refused", "refused: the dashboard binds only 127.0.0.1 or ::1; not serving"},
	} {
		h := newHarness(t)
		h.writeAddr(c.addr)
		if st := h.ok(nil, "daemon", "--status"); st["dashboard"] != c.status || st["dashboard_url"] != nil {
			t.Fatalf("%s: status before start = %v", c.addr, st)
		}
		s := newFakeSocket(t)
		p := h.startDaemon(s)
		conn, _, _ := s.next()
		conn.Write([]byte(ack))
		eventually(t, "heartbeat", func() bool { return h.ok(nil, "daemon", "--status")["daemon"] == "fresh" })
		if st := h.ok(nil, "daemon", "--status"); st["dashboard"] != c.status {
			t.Fatalf("%s: status while running = %v", c.addr, st)
		}
		s.shutdown()
		p.wait(t)
		if out := p.lines(t); out[0]["dashboard_url"] != nil {
			t.Fatalf("%s: stdout = %v", c.addr, out)
		}
		if l := dashLines(h.daemonLog()); len(l) != 1 || !strings.Contains(l[0], c.log) {
			t.Fatalf("%s: dashboard log lines = %q", c.addr, l)
		}
	}
}

func TestDaemonOnceDoesNotServe(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	addr := ln.Addr().String()
	ln.Close()
	h.writeAddr(addr)
	h.ok(nil, "daemon", "--once")
	if l := dashLines(h.daemonLog()); len(l) != 0 {
		t.Fatalf("--once touched the dashboard: %q", l)
	}
	if _, ok, _ := getMeta(h.openDB(), dashboardURLKey); ok {
		t.Fatal("--once recorded a dashboard URL")
	}
	if c, err := net.DialTimeout("tcp", addr, 200*time.Millisecond); err == nil {
		c.Close()
		t.Fatal("something is listening after --once")
	}
}

// An IPv6 loopback listener accepts its own bracketed Host.
func TestDashboardIPv6Host(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	d := newDashboard(h.openDB(), &daemonLog{}, "[::1]:7788")
	if d.url != "http://[::1]:7788/" {
		t.Fatalf("url = %q", d.url)
	}
	if w, _ := serve(d, req("GET", probePath, "", func(r *http.Request) { r.Host = "[::1]:7788" })); w.Code != admitted {
		t.Fatalf("Host [::1] on a [::1] listener = %d", w.Code)
	}
	if w, _ := serve(d, req("GET", probePath, "")); w.Code != 421 {
		t.Fatalf("Host 127.0.0.1 on a [::1] listener = %d", w.Code)
	}
}

// ago backdates an event.
func backdate(t *testing.T, db *sql.DB, event int64, d time.Duration) {
	t.Helper()
	if _, err := db.Exec(`update events set created_at = ? where id = ?`, stamp(time.Now().Add(-d)), event); err != nil {
		t.Fatal(err)
	}
}

func TestDashboardUsageClassification(t *testing.T) {
	contractGuard(t)
	fakeTailnetHooks(t)
	h := newHarness(t)
	d := h.hubDash()
	serveCounted := func(r *http.Request) int {
		w := httptest.NewRecorder()
		d.srv.Handler.ServeHTTP(w, r)
		return w.Code
	}

	// Nothing is counted as a page view, a state poll or a push any more.
	if code := serveCounted(req("GET", "/", "")); code != http.StatusNotFound {
		t.Fatalf("GET / = %d", code)
	}
	if code := serveCounted(req("GET", "/api/state", "", from(hostAIP, "hub.example.ts.net:7788"))); code != http.StatusNotFound {
		t.Fatalf("tailnet GET /api/state = %d", code)
	}
	if code := serveCounted(req("GET", probePath, "")); code != admitted {
		t.Fatalf("GET %s = %d", probePath, code)
	}
	refused := req("GET", probePath, "", from(otherIP, "hub.example.ts.net:7788"))
	if code := serveCounted(refused); code != http.StatusForbidden {
		t.Fatalf("whois refusal = %d, want 403 before routing", code)
	}
	badHost := req("GET", probePath, "", func(r *http.Request) { r.Host = "wrong.example" })
	if code := serveCounted(badHost); code != http.StatusMisdirectedRequest {
		t.Fatalf("Host mismatch = %d, want 421 before routing", code)
	}

	d.usage.mu.Lock()
	got := map[string]int64{}
	for _, counts := range d.usage.pending {
		for class, count := range counts {
			got[class] += count
		}
	}
	want := map[string]int64{
		usagePage: 0, usageStateLoopback: 0, usageStateTailnet: 0,
		usagePushAccepted: 0, usageRefused: 2, usageOther: 3,
	}
	for class, count := range want {
		if got[class] != count {
			t.Errorf("%s count = %d, want %d (all counts %v)", class, got[class], count, got)
		}
	}
	d.usage.mu.Unlock()
}

func TestDaemonStatusDashboardUsage(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	db := h.openDB()
	at := time.Now().UTC()
	seed := func(hour time.Time, value string) {
		t.Helper()
		key := "usage:" + hour.UTC().Format("2006-01-02T15")
		if err := setMeta(db, key, value); err != nil {
			t.Fatal(err)
		}
	}
	seed(at, `{"page":1,"state_loopback":20,"state_tailnet":20,"push_accepted":2,"other":1}`)
	seed(at.Add(-23*time.Hour), `{"page":3,"state_loopback":10,"state_tailnet":10,"refused":1}`)
	seed(at.Add(-25*time.Hour), `{"page":5,"state_loopback":5,"state_tailnet":5,"other":4}`)
	seed(at.Add(-167*time.Hour), `{"page":7,"state_loopback":2,"state_tailnet":2}`)
	seed(at.Add(-169*time.Hour), `{"page":100,"state_loopback":100,"state_tailnet":100,"refused":100}`)

	c := &ctx{getenv: h.getenv(nil), out: io.Discard, errw: io.Discard}
	result, code, err := daemonStatus(c, h.stateDir(), filepath.Join(h.stateDir(), "daemon.lock"))
	if err != nil || code != exitOK {
		t.Fatalf("daemonStatus = %v, %d, %v", result, code, err)
	}
	status := result.(map[string]any)["dashboard_usage"].(map[string]any)
	h24, d7 := status["h24"].(map[string]int64), status["d7"].(map[string]int64)
	want24 := map[string]int64{usagePage: 4, usageStateLoopback: 30, usageStateTailnet: 30,
		usagePushAccepted: 2, usageRefused: 1, usageOther: 1}
	want7 := map[string]int64{usagePage: 16, usageStateLoopback: 37, usageStateTailnet: 37,
		usagePushAccepted: 2, usageRefused: 1, usageOther: 5}
	for class := range emptyUsageCounts() {
		if h24[class] != want24[class] || d7[class] != want7[class] {
			t.Errorf("%s: h24=%d, d7=%d; want %d, %d", class, h24[class], d7[class], want24[class], want7[class])
		}
	}
	if status["visible_minutes_h24"] != 3.0 || status["visible_minutes_d7"] != 3.7 {
		t.Fatalf("visible minutes = %v, %v", status["visible_minutes_h24"], status["visible_minutes_d7"])
	}
}

func TestDaemonStatusInvalidDashboardUsage(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	db := h.openDB()
	key := "usage:" + time.Now().UTC().Format("2006-01-02T15")
	if err := setMeta(db, key, "not json"); err != nil {
		t.Fatal(err)
	}
	c := &ctx{getenv: h.getenv(nil), out: io.Discard, errw: io.Discard}
	result, code, err := daemonStatus(c, h.stateDir(), filepath.Join(h.stateDir(), "daemon.lock"))
	if err != nil || code != exitOK {
		t.Fatalf("daemonStatus = %v, %d, %v", result, code, err)
	}
	status, ok := result.(map[string]any)
	if !ok {
		t.Fatalf("daemonStatus result = %T, want a map", result)
	}
	usage, ok := status["dashboard_usage"].(map[string]any)
	message, _ := usage["error"].(string)
	if !ok || message == "" {
		t.Fatalf("dashboard_usage = %v, want a non-empty error", status["dashboard_usage"])
	}
}
