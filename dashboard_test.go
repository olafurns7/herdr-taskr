package main

import (
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
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

// req builds a request from a browser on this machine; mods change it.
func req(method, path, body string, mods ...func(*http.Request)) *http.Request {
	r := httptest.NewRequest(method, "http://"+testAddr+path, strings.NewReader(body))
	r.Host = testAddr
	r.RemoteAddr = "127.0.0.1:50000" // a browser on this machine
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

func getState(t *testing.T, d *dashboard) (string, dashState) {
	t.Helper()
	w, _ := serve(d, req("GET", "/api/state", ""))
	if w.Code != 200 {
		t.Fatalf("GET /api/state = %d %s", w.Code, w.Body)
	}
	var s dashState
	if err := json.Unmarshal(w.Body.Bytes(), &s); err != nil {
		t.Fatal(err)
	}
	return w.Body.String(), s
}

func TestDashboardStateShape(t *testing.T) {
	h := newHarness(t)
	f := seedDashboard(h)
	raw, s := getState(t, h.dash())

	if len(s.OwnerAsks) != 2 {
		t.Fatalf("owner asks = %+v", s.OwnerAsks)
	}
	wa, ra := s.OwnerAsks[0], s.OwnerAsks[1]
	if wa.ID != f.workerAsk || wa.Text != "Ship <b>now</b>?\nOr wait?" || !wa.Blocking || wa.AskerIsRoot ||
		wa.Asker != (taskRef{f.impl, "impl-a", "implementer"}) || wa.Root != (taskRef{f.a, "camp-a", "orchestrator"}) ||
		wa.PaneID != "w1:p3" || wa.At == "" {
		t.Fatalf("worker owner ask = %+v", wa)
	}
	if ra.ID != f.rootAsk || !ra.AskerIsRoot || ra.Blocking || ra.Asker.ID != f.b || ra.Root.ID != f.b {
		t.Fatalf("root owner ask = %+v", ra)
	}

	if len(s.Orchestrators) != 2 || s.Orchestrators[0].ID != f.a || s.Orchestrators[1].ID != f.b {
		t.Fatalf("orchestrators = %+v", s.Orchestrators)
	}
	a := s.Orchestrators[0]
	if a.Name != "camp-a" || a.Status != "open" || a.WorkspaceID != "w1" || a.TabID != "w1:t1" || a.PaneID != "w1:p1" ||
		a.Note == nil || a.Note.Text != "phase 2: reviewing" || a.Unacked != 1 {
		t.Fatalf("camp-a = %+v (unacked: the worker's owner ask is in its inbox)", a)
	}
	var names []string
	for _, tk := range a.Tasks {
		names = append(names, fmt.Sprintf("%s/%d", tk.Name, tk.Depth))
	}
	if strings.Join(names, " ") != "sub-a/1 impl-a/2 rev-a/1" {
		t.Fatalf("camp-a tasks depth-first = %v", names)
	}
	impl := a.Tasks[1]
	if impl.ParentID != f.sub || impl.Role != "implementer" || impl.OpenAsks != 2 ||
		impl.ReportPath != filepath.Join(h.dir, "r/impl.md") || impl.LastEvent == nil ||
		impl.LastEvent.Kind != "note" || impl.LastEvent.Summary != "<script>alert(1)</script>" || impl.PaneID != "w1:p3" {
		t.Fatalf("impl-a = %+v last %+v", impl, impl.LastEvent)
	}
	b := s.Orchestrators[1]
	if b.Note != nil || len(b.Tasks) != 1 || b.Tasks[0].ID != f.bw || b.OpenAsks != 1 {
		t.Fatalf("camp-b = %+v", b)
	}

	if len(s.Activity) == 0 {
		t.Fatal("no activity")
	}
	for i, e := range s.Activity {
		if e.RootID == f.c || (i > 0 && e.ID > s.Activity[i-1].ID) {
			t.Fatalf("activity must be open trees only, newest first: %+v", s.Activity)
		}
	}
	if s.Activity[0].ID != f.rootAsk || s.Activity[0].RootName != "camp-b" {
		t.Fatalf("latest activity = %+v", s.Activity[0])
	}

	if len(s.Closed) != 1 || s.Closed[0].ID != f.c || s.Closed[0].Note == nil || s.Closed[0].Note.Text != "all done" ||
		s.Closed[0].ClosedAt == "" {
		t.Fatalf("closed = %+v", s.Closed)
	}
	// Every list is a JSON array, never null, even on an empty ledger.
	for _, k := range []string{`"owner_asks":[`, `"orchestrators":[`, `"activity":[`, `"closed":[`, `"now":"`} {
		if !strings.Contains(raw, k) {
			t.Fatalf("state JSON lacks %s: %s", k, raw)
		}
	}
	if raw, _ := getState(t, newHarness(t).dash()); !strings.Contains(raw, `"owner_asks":[]`) || !strings.Contains(raw, `"orchestrators":[]`) {
		t.Fatalf("empty ledger state = %s", raw)
	}
}

// A note holding markup comes back as JSON text, and the page never parses
// ledger text as HTML.
func TestDashboardNoHTMLInjection(t *testing.T) {
	h := newHarness(t)
	seedDashboard(h)
	raw, s := getState(t, h.dash())
	if strings.Contains(raw, "<script") || strings.Contains(raw, "<b>") {
		t.Fatalf("raw state JSON carries markup unescaped: %s", raw)
	}
	if s.Orchestrators[0].Tasks[1].LastEvent.Summary != "<script>alert(1)</script>" {
		t.Fatalf("decoded note = %q", s.Orchestrators[0].Tasks[1].LastEvent.Summary)
	}
}

// sinks parse strings as markup or code; ledger text must never reach one.
var sinks = regexp.MustCompile(`dangerouslySetInnerHTML|innerHTML|outerHTML|insertAdjacentHTML|document\.write|\beval\(|new Function|DOMParser|createContextualFragment`)

// The page's source and its built output render ledger text as text only.
// Preact's own runtime keeps its dangerouslySetInnerHTML path, which
// assigns innerHTML; it ships alone in the preact-*.js chunk (see
// web/vite.config.ts), no app code passes that prop (the web/src scan), and
// every other file holds no sink at all.
// preactRuntimeSHA256 pins the one file allowed to carry HTML sinks: the
// built Preact runtime chunk (web/dist/assets/preact-*.js), whose
// dangerouslySetInnerHTML path the app never reaches. The pin is its exact
// bytes, so a sink added to that file fails like one anywhere else. The chunk
// changes when Preact, Vite or the set of Preact exports the app imports
// changes: check the new chunk is Preact's own code, then record its sha256.
const preactRuntimeSHA256 = "92fd7803f886985b4350f416dd4b898d162f7046e3fef97d8d586cfe62cf748a"

// The parser's link normalisation names URL schemes and fetches nothing.
// The page still renders through the allow-list; connect-src stays 'self'.
// Only these exact vendor bytes may name remote schemes; every HTML/code
// sink and every remote-asset pattern remains forbidden in this chunk.
const markdownItSHA256 = "b29e31221c28a486c3b461971437ca350c5ef608c8ea4a4e9fbe31e082b5864b"

func TestWebNoHTMLSinks(t *testing.T) {
	scanned, pinned, parserPinned := 0, 0, 0
	for _, root := range []string{"web/src", "web/dist"} {
		err := filepath.WalkDir(root, func(path string, e os.DirEntry, err error) error {
			if err != nil || e.IsDir() || strings.HasSuffix(path, ".woff2") {
				return err
			}
			b, err := os.ReadFile(path)
			if err != nil {
				return err
			}
			scanned++
			text := string(b)
			found := sinks.FindAllString(text, -1)
			sum := sha256.Sum256(b)
			parserVendor := root == "web/dist" && hex.EncodeToString(sum[:]) == markdownItSHA256
			if parserVendor {
				parserPinned++
			} else if root == "web/dist" && strings.HasPrefix(filepath.Base(path), "markdown-it-") {
				t.Errorf("%s (sha256 %s) is not the pinned Markdown parser: review it and re-pin markdownItSHA256", path, hex.EncodeToString(sum[:]))
			}
			if sum := sha256.Sum256(b); root == "web/dist" && hex.EncodeToString(sum[:]) == preactRuntimeSHA256 {
				pinned++
				var other []string
				for _, f := range found {
					if f != "dangerouslySetInnerHTML" && f != "innerHTML" {
						other = append(other, f)
					}
				}
				found = other
			} else if len(found) > 0 && strings.HasPrefix(filepath.Base(path), "preact-") {
				t.Errorf("%s (sha256 %s) is not the pinned Preact runtime: review it and re-pin preactRuntimeSHA256", path, hex.EncodeToString(sum[:]))
			}
			if len(found) > 0 {
				t.Errorf("%s uses HTML or code sinks: %v", path, found)
			}
			// Nothing remote: a namespace URI is a name, not a fetch; the
			// font notes cite their source.
			if base := filepath.Base(path); base == "SOURCE.md" || base == "OFL.txt" {
				return nil
			}
			text = regexp.MustCompile(`http://www\.w3\.org/[\w/.]+`).ReplaceAllString(text, "") // namespace names
			for _, ext := range []string{"http://", "https://", "//cdn", "@import", "url(//"} {
				if parserVendor && (ext == "http://" || ext == "https://") {
					continue
				}
				if strings.Contains(text, ext) {
					t.Errorf("%s references %q; the page must be self-contained", path, ext)
				}
			}
			return nil
		})
		if err != nil {
			t.Fatal(err)
		}
	}
	if scanned < 10 || pinned != 1 || parserPinned != 1 {
		t.Fatalf("scanned %d files, %d pinned Preact runtimes, %d pinned Markdown parsers; is web/dist built?", scanned, pinned, parserPinned)
	}
	// The built page has no inline script or style for the CSP to refuse.
	for _, tag := range regexp.MustCompile(`<script[^>]*>`).FindAllString(dashboardPage, -1) {
		if !strings.Contains(tag, ` src="/assets/`) {
			t.Fatalf("web/dist/index.html has an inline script: %s", tag)
		}
	}
	if strings.Contains(dashboardPage, "<style") || regexp.MustCompile(`\sstyle=`).MatchString(dashboardPage) {
		t.Fatalf("web/dist/index.html has inline style:\n%s", dashboardPage)
	}
	if strings.Contains(dashboardPage, "taskr-token") {
		t.Fatal("web/dist/index.html still carries a token meta tag")
	}
}

func TestDashboardPage(t *testing.T) {
	h := newHarness(t)
	d := h.dash()
	w, _ := serve(d, req("GET", "/", ""))
	body := w.Body.String()
	if w.Code != 200 || body != dashboardPage || strings.Contains(body, "token") || strings.Contains(body, "__TASKR_") ||
		!strings.HasPrefix(w.Header().Get("Content-Type"), "text/html") || w.Header().Get("Cache-Control") != "no-store" {
		t.Fatalf("GET / = %d %q", w.Code, body)
	}
	if csp := w.Header().Get("Content-Security-Policy"); csp != "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; "+
		"img-src 'self' data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'" {
		t.Fatalf("CSP = %q", csp)
	}
	if w, _ := serve(d, req("GET", "/nope", "")); w.Code != 404 {
		t.Fatalf("GET /nope = %d", w.Code)
	}
	if w, _ := serve(d, req("POST", "/api/state", "{}")); w.Code != 405 {
		t.Fatalf("POST /api/state = %d", w.Code)
	}
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

// The dashboard is read-only: no route writes, however the request looks,
// and Host is still checked before anything is served.
func TestDashboardReadOnly(t *testing.T) {
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
		{"POST page", req("POST", "/", body), 405},
		{"POST state", req("POST", "/api/state", body), 405},
		{"PUT state", req("PUT", "/api/state", body), 405},
		{"DELETE state", req("DELETE", "/api/state", ""), 405},
		{"POST asset", req("POST", "/fonts/OFL.txt", body), 405},
		{"peer push, not a hub", req("POST", peerPushPath, `{"state":{},"now":"`+now()+`"}`), 404},
		{"wrong Host, GET state", req("GET", "/api/state", "", func(r *http.Request) { r.Host = "evil.example:7788" }), 421},
		{"wrong Host, GET page", req("GET", "/", "", func(r *http.Request) { r.Host = "127.0.0.1:9" }), 421},
	}
	for _, c := range cases {
		w, _ := serve(d, c.r)
		if w.Code != c.code || w.Header().Get("Access-Control-Allow-Origin") != "" {
			t.Errorf("%s: %d, want %d", c.name, w.Code, c.code)
		}
	}
	if _, s := getState(t, d); len(s.OwnerAsks) != 2 || s.OwnerAsks[0].ID != f.workerAsk {
		t.Fatalf("a refused request changed the open owner asks: %+v", s.OwnerAsks)
	}
	var answers int
	h.openDB().QueryRow(`select count(*) from events where kind in ('answer', 'owner_answer')`).Scan(&answers)
	if answers != 0 {
		t.Fatalf("%d answers written through the dashboard", answers)
	}
	// localhost:<port> is still an accepted Host.
	if w, _ := serve(d, req("GET", "/api/state", "", func(r *http.Request) { r.Host = "localhost:7788" })); w.Code != 200 {
		t.Fatalf("GET state as localhost = %d", w.Code)
	}
}

// The page offers nothing to answer with: no form, input, textarea or
// button in the app's source or its built page, and no call to a write
// route or token header in the built app.
func TestPageHasNoAnswerControls(t *testing.T) {
	// Local navigation, search, filters and details are read-only controls.
	// An answer form/editor or a write-capable request must never ship.
	tags := regexp.MustCompile(`<(form|textarea)\b`)
	writes := regexp.MustCompile(`method\s*:\s*["'](?:POST|PUT|PATCH|DELETE)["']`)
	check := func(path string, b []byte) {
		if m := tags.FindAllString(string(b), -1); m != nil {
			t.Errorf("%s has answer controls: %v", path, m)
		}
		for _, s := range []string{"/answer", "X-Taskr-Token", "taskr-token", `method:"POST"`, "createElement(\"textarea"} {
			if strings.Contains(string(b), s) {
				t.Errorf("%s contains %q", path, s)
			}
		}
		if writes.Match(b) {
			t.Errorf("%s contains a write request", path)
		}
	}
	scanned := 0
	for _, root := range []string{"web/src", "web/index.html", "web/dist/index.html"} {
		err := filepath.WalkDir(root, func(path string, e os.DirEntry, err error) error {
			if err != nil || e.IsDir() || !regexp.MustCompile(`\.(tsx?|html)$`).MatchString(path) {
				return err
			}
			b, err := os.ReadFile(path)
			if err != nil {
				return err
			}
			scanned++
			check(path, b)
			return nil
		})
		if err != nil {
			t.Fatal(err)
		}
	}
	for _, path := range []string{"web/src/App.tsx", "web/src/main.tsx", "web/src/api.ts", "web/src/poller.ts", "web/index.html", "web/dist/index.html"} {
		if _, err := os.Stat(path); err != nil {
			t.Fatal(err)
		}
	}
	apps, _ := filepath.Glob("web/dist/assets/app-*.js")
	if scanned < 6 || len(apps) != 1 {
		t.Fatalf("scanned %d source files, %d app chunks; is web/dist built?", scanned, len(apps))
	}
	b, err := os.ReadFile(apps[0])
	if err != nil {
		t.Fatal(err)
	}
	check(apps[0], b)
}

func TestDashboardAddr(t *testing.T) {
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

// The resident daemon serves the dashboard, --status reports it, and a
// clean exit takes it down and clears the URL.
func TestDashboardServedByDaemon(t *testing.T) {
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
	resp, err := http.Get(url)
	if err != nil {
		t.Fatal(err)
	}
	page, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if resp.StatusCode != 200 || string(page) != dashboardPage {
		t.Fatalf("GET / = %d", resp.StatusCode)
	}
	resp, err = http.Get(url + "api/state")
	if err != nil {
		t.Fatal(err)
	}
	var state dashState
	json.NewDecoder(resp.Body).Decode(&state)
	resp.Body.Close()
	if len(state.OwnerAsks) != 2 || len(state.Orchestrators) != 2 {
		t.Fatalf("state over HTTP = %+v", state)
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
	if _, err := http.Get(url + "api/state"); err == nil {
		t.Fatal("dashboard still serving after the daemon exited")
	}
	if l := dashLines(h.daemonLog()); len(l) != 1 || !strings.Contains(l[0], "dashboard at "+url) {
		t.Fatalf("dashboard log lines = %q", l)
	}
}

// A port in use costs one log line; the event bridge keeps passing.
func TestDashboardBindConflictDaemonCarriesOn(t *testing.T) {
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
	h.ok(as(w, l), "ask", "decide", "--owner")
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
	h := newHarness(t)
	f := seedDashboard(h)
	d := newDashboard(h.openDB(), &daemonLog{}, "[::1]:7788")
	if d.url != "http://[::1]:7788/" {
		t.Fatalf("url = %q", d.url)
	}
	w, _ := serve(d, req("GET", "/api/state", "", func(r *http.Request) { r.Host = "[::1]:7788" }))
	var s dashState
	json.Unmarshal(w.Body.Bytes(), &s)
	if w.Code != 200 || len(s.OwnerAsks) != 2 || s.OwnerAsks[1].ID != f.rootAsk {
		t.Fatalf("state over [::1] = %d %+v", w.Code, s.OwnerAsks)
	}
	if w, _ := serve(d, req("GET", "/api/state", "")); w.Code != 421 {
		t.Fatalf("Host 127.0.0.1 on a [::1] listener = %d", w.Code)
	}
}

// Closed history cannot use up the per-root cap: closed descendants are
// counted, open ones are listed, including open tasks under a closed parent.
func TestDashboardClosedHistoryDoesNotHideOpenTasks(t *testing.T) {
	h := newHarness(t)
	db := h.openDB()
	ts := now()
	err := withTx(db, func(tx *sql.Tx) error {
		ins := func(id, parent int64, status string) error {
			var p any
			if parent != 0 {
				p = parent
			}
			_, err := tx.Exec(`insert into tasks (id, parent_id, name, role, status, created_at, updated_at, closed_at)
				values (?, ?, ?, 'implementer', ?, ?, ?, ?)`, id, p, fmt.Sprintf("t%d", id), status, ts, ts,
				map[bool]any{true: ts, false: nil}[status == "closed"])
			return err
		}
		if err := ins(1, 0, "open"); err != nil {
			return err
		}
		for id := int64(2); id <= 201; id++ { // 200 closed children, oldest ids
			if err := ins(id, 1, "closed"); err != nil {
				return err
			}
		}
		for id := int64(202); id <= 301; id++ { // 100 open children
			if err := ins(id, 1, "open"); err != nil {
				return err
			}
		}
		for id := int64(302); id <= 401; id++ { // 100 open grandchildren under 301
			if err := ins(id, 301, "open"); err != nil {
				return err
			}
		}
		return ins(402, 2, "open") // open, under a closed child: first in depth-first order
	})
	if err != nil {
		t.Fatal(err)
	}
	_, s := getState(t, h.dash())
	if len(s.Orchestrators) != 1 {
		t.Fatalf("orchestrators = %d", len(s.Orchestrators))
	}
	o := s.Orchestrators[0]
	if o.ClosedTasks != 200 || len(o.Tasks) != stateTasksMax || o.TasksTruncated != 1 {
		t.Fatalf("closed_tasks %d, listed %d, truncated %d; want 200, %d, 1", o.ClosedTasks, len(o.Tasks), o.TasksTruncated, stateTasksMax)
	}
	nested := 0
	for _, tk := range o.Tasks {
		if tk.Status == "closed" {
			t.Fatalf("closed task %d listed", tk.ID)
		}
		if tk.Depth == 2 {
			nested++
		}
	}
	if o.Tasks[0].ID != 402 || o.Tasks[0].Depth != 2 || o.Tasks[1].ID != 202 || nested != 100 {
		t.Fatalf("first listed %+v, %+v; nested %d", o.Tasks[0], o.Tasks[1], nested)
	}
}

// viewAt is this machine's /api/state view.
func viewAt(t *testing.T, d *dashboard) stateView {
	t.Helper()
	w, _ := serve(d, req("GET", "/api/state", ""))
	if w.Code != 200 {
		t.Fatalf("GET /api/state = %d %s", w.Code, w.Body)
	}
	var v stateView
	if err := json.Unmarshal(w.Body.Bytes(), &v); err != nil {
		t.Fatal(err)
	}
	return v
}

// ago backdates an event.
func backdate(t *testing.T, db *sql.DB, event int64, d time.Duration) {
	t.Helper()
	if _, err := db.Exec(`update events set created_at = ? where id = ?`, stamp(time.Now().Add(-d)), event); err != nil {
		t.Fatal(err)
	}
}

func kindsOf(items []attentionItem) string {
	var k []string
	for _, it := range items {
		k = append(k, it.Kind)
	}
	return strings.Join(k, " ")
}

// Every attention kind this machine can raise, ordered by severity, each
// with its orchestrator, lane, text and action; the tallies follow.
func TestAttentionKinds(t *testing.T) {
	h := newHarness(t)
	db := h.openDB()
	top := h.newTask("orch-cart", "orchestrator", 0, "--pane", "w1:p1")
	// The oldest unacked report in the root's inbox, 11 minutes old.
	ready := h.newTask("impl-tax", "implementer", top, "--pane", "w1:p5")
	readyL := h.launch(ready)
	rid := num(h.ok(as(ready, readyL), "ready", "tax slice"), "event_id")
	backdate(t, db, rid, 11*time.Minute)
	failed := h.newTask("impl-cart", "implementer", top, "--pane", "w1:p2", "--report", "r/cart.md")
	failedL := h.launch(failed)
	h.ok(as(failed, failedL), "fail", "tests red on main\nsecond line")
	blocked := h.newTask("rev-cart", "reviewer", top, "--pane", "w1:p3")
	blockedL := h.launch(blocked)
	gone := h.newTask("rs-cart", "researcher", top, "--pane", "w1:p4")
	h.launch(gone)
	db.Exec(`update launches set observed_status = 'blocked', observed_at = ? where task_id = ?`, now(), blocked)
	db.Exec(`update launches set present = 0, observed_at = ? where task_id = ?`, now(), gone)
	h.ok(as(blocked, blockedL), "note", "reading the diff")
	asker := h.newTask("rev-docs", "reviewer", top, "--pane", "w1:p6")
	askID := num(h.ok(as(asker, h.launch(asker)), "ask", "Merge now?", "--owner"), "ask_id")
	quiet := h.newTask("orch-quiet", "orchestrator", 0)
	h.ok(nil, "note", "--as", id(quiet), "phase 1")
	db.Exec(`insert into meta (key, value) values (?, ?)`, heartbeatKey, stamp(time.Now().Add(-5*time.Minute)))

	v := viewAt(t, h.dash())
	if got := kindsOf(v.Attention); got != "owner_ask lane_blocked lane_failed lane_missing daemon_unhealthy work_waiting" {
		t.Fatalf("attention kinds = %s", got)
	}
	if kindsOf(v.Machines[0].State.Attention) != kindsOf(v.Attention) {
		t.Fatal("the machine's own attention list differs from the merged one on a single machine")
	}
	byKind := map[string]attentionItem{}
	for i, it := range v.Attention {
		byKind[it.Kind] = it
		if it.Severity != attentionRank[it.Kind] || it.Machine == "" || !it.Local || (i > 0 && it.Severity < v.Attention[i-1].Severity) {
			t.Fatalf("item %d = %+v", i, it)
		}
	}
	a := byKind[attentionOwnerAsk]
	if a.AskID != askID || a.Action != "Answer in orch-cart's pane w1:p1" || a.PaneID != "w1:p1" || a.Orchestrator.Name != "orch-cart" || a.Lane == nil || a.Lane.Name != "rev-docs" || a.Also != nil {
		t.Fatalf("owner ask cue = %+v", a)
	}
	f := byKind[attentionFailed]
	if f.Lane.ID != failed || f.Text != "tests red on main" || f.Report != filepath.Join(h.dir, "r/cart.md") ||
		!strings.HasPrefix(f.Command, "herdr agent read ") || f.Since == "" {
		t.Fatalf("failed cue = %+v", f)
	}
	if b := byKind[attentionBlocked]; b.Lane.ID != blocked || !strings.HasPrefix(b.Command, "herdr agent read ") || b.PaneID != "w1:p3" ||
		b.Text != "Herdr sees an approval or question dialog in its pane; last: reading the diff" {
		t.Fatalf("blocked cue = %+v", b)
	}
	if m := byKind[attentionMissing]; m.Lane.ID != gone || m.Command != fmt.Sprintf("taskr log %d", gone) || !strings.Contains(m.Text, "w1:p4") {
		t.Fatalf("missing cue = %+v", m)
	}
	w := byKind[attentionWorkWaits]
	if w.Lane.ID != ready || w.Orchestrator.ID != top || w.AgeMS < 10*60*1000 || w.Command != "herdr pane read w1:p1 --source recent-unwrapped --lines 60" ||
		!strings.Contains(w.Text, "orch-cart has not taken impl-tax's ready") {
		t.Fatalf("work waiting cue = %+v", w)
	}
	if d := byKind[attentionDaemon]; d.Command != "taskr daemon --status" || d.Orchestrator != nil || d.AgeMS < 4*60*1000 {
		t.Fatalf("daemon cue = %+v", d)
	}
	marks := map[string]string{}
	for _, tk := range v.Orchestrators[0].Tasks {
		marks[tk.Name] = tk.Mark
	}
	if fmt.Sprint(marks) != "map[impl-cart:failed impl-tax:ready rev-cart:blocked rev-docs:working rs-cart:missing]" {
		t.Fatalf("marks = %v", marks)
	}
	// Machine-wide cues light no tally: the quiet campaign's note is a
	// fresh milestone.
	if v.Orchestrators[0].Tally != tallyRed || v.Orchestrators[1].Tally != tallyGreen {
		t.Fatalf("tallies = %s %s", v.Orchestrators[0].Tally, v.Orchestrators[1].Tally)
	}
	// A healthy daemon raises nothing.
	db.Exec(`update meta set value = ? where key = ?`, now(), heartbeatKey)
	if v := viewAt(t, h.dash()); strings.Contains(kindsOf(v.Attention), attentionDaemon) {
		t.Fatalf("fresh heartbeat still unhealthy: %s", kindsOf(v.Attention))
	}
}

// A lane with an open owner ask shows as one cue: its blocked, failed or
// missing cue folds into the ask, and no cue repeats the ask's text. A
// blocked dialog always keeps its pane action; a wait lease only words it
// as a condition.
func TestAttentionFoldsIntoAsk(t *testing.T) {
	h := newHarness(t)
	db := h.openDB()
	db.Exec(`insert into meta (key, value) values (?, ?)`, heartbeatKey, now())
	top := h.newTask("orch", "orchestrator", 0, "--pane", "w1:p1")
	lane := func(name, pane string) (int64, int64) {
		id := h.newTask(name, "implementer", top, "--pane", pane)
		return id, h.launch(id)
	}
	waits, waitsL := lane("impl-waits", "w1:p2")
	dialog, dialogL := lane("impl-dialog", "w1:p3")
	failed, failedL := lane("impl-failed", "w1:p4")
	gone, goneL := lane("impl-gone", "w1:p5")
	const q = "Round half-up or banker's rounding?"
	for _, l := range [][2]int64{{waits, waitsL}, {dialog, dialogL}} {
		h.ok(as(l[0], l[1]), "ask", q, "--owner", "--blocking")
		db.Exec(`update launches set observed_status = 'blocked', observed_at = ? where task_id = ?`, now(), l[0])
	}
	db.Exec(`update tasks set waiting_until = ? where id = ?`, stamp(time.Now().Add(time.Hour)), waits)
	h.ok(as(failed, failedL), "ask", "Skip the flaky suite?", "--owner")
	h.ok(as(failed, failedL), "fail", "tests red")
	h.ok(as(gone, goneL), "ask", "Keep the old URLs?", "--owner")
	db.Exec(`update launches set present = 0, observed_at = ? where task_id = ?`, now(), gone)

	v := viewAt(t, h.dash())
	if got := kindsOf(v.Attention); got != "owner_ask owner_ask owner_ask owner_ask" {
		t.Fatalf("attention kinds = %s", got)
	}
	also := map[string]string{}
	for _, a := range v.Attention {
		var ks []string
		for _, s := range a.Also {
			ks = append(ks, s.Kind)
			if strings.Contains(s.Text, a.Text) || s.Action == "" {
				t.Fatalf("%s: folded %s = %+v", a.Lane.Name, s.Kind, s)
			}
		}
		also[a.Lane.Name] = strings.Join(ks, ",")
	}
	if fmt.Sprint(also) != "map[impl-dialog:lane_blocked impl-failed:lane_failed impl-gone:lane_missing impl-waits:lane_blocked]" {
		t.Fatalf("folded = %v", also)
	}
	for _, a := range v.Attention {
		want := map[int64]string{dialog: "Answer the dialog in its pane", waits: "If the dialog is not this question, answer it in its pane"}[a.Lane.ID]
		if want != "" && (a.Also[0].Action != want || !strings.HasPrefix(a.Also[0].Command, "herdr agent read ")) {
			t.Fatalf("%s pane action = %+v", a.Lane.Name, a.Also[0])
		}
	}
	// The lane marks and the tally still say what Herdr and the ledger see.
	marks := map[string]string{}
	for _, tk := range v.Orchestrators[0].Tasks {
		marks[tk.Name] = tk.Mark
	}
	if fmt.Sprint(marks) != "map[impl-dialog:blocked impl-failed:failed impl-gone:missing impl-waits:blocked]" || v.Orchestrators[0].Tally != tallyRed {
		t.Fatalf("marks = %v, tally %s", marks, v.Orchestrators[0].Tally)
	}
}

// Full owner questions reach the read-only page, whose two-line clamp and
// title attribute handle display without losing the rest of the question.
func TestOwnerAskKeepsFullQuestion(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("orch-question", "orchestrator", 0, "--pane", "w1:p1")
	question := strings.Repeat("Which option? ", 30) + "\nKeep this final detail."
	h.ok(nil, "ask", "--as", id(top), question, "--owner")
	v := viewAt(t, h.dash())
	if len(v.Attention) == 0 || v.Attention[0].Kind != attentionOwnerAsk || v.Attention[0].Text != question || v.Attention[0].Action != "Answer in orch-question's pane w1:p1" {
		t.Fatalf("owner question truncated or action lost: %+v", v.Attention)
	}
}

// The finish review's probe (round 2): a wait lease left on the asker, as
// a dead `taskr wait` leaves it, must not erase a dialog Herdr observes in
// the pane. The owner cue keeps it: tag, pane action and read command.
func TestWaitLeaseKeepsObservedDialog(t *testing.T) {
	h := newHarness(t)
	db := h.openDB()
	top := h.newTask("orch-review", "orchestrator", 0, "--pane", "w1:p1")
	lane := h.newTask("impl-review", "implementer", top, "--pane", "w1:p2")
	h.ok(as(lane, h.launch(lane)), "ask", "Publish the docs?", "--owner", "--blocking")
	db.Exec(`update tasks set waiting_until = ? where id = ?`, stamp(time.Now().Add(9*time.Minute)), lane)
	db.Exec(`update launches set observed_status = 'blocked', observed_at = ?, present = 1 where task_id = ?`, now(), lane)

	v := viewAt(t, h.dash())
	var cues []attentionItem
	for _, a := range v.Attention {
		if a.Lane != nil && a.Lane.ID == lane {
			cues = append(cues, a)
		}
	}
	if len(cues) != 1 || cues[0].Kind != attentionOwnerAsk || !cues[0].AskerWaiting || cues[0].Action != "Answer in orch-review's pane w1:p1" {
		t.Fatalf("lane cues = %+v", cues)
	}
	also := cues[0].Also
	if len(also) != 1 || also[0].Kind != attentionBlocked || !strings.Contains(also[0].Action, "answer it in its pane") ||
		also[0].Command != "herdr agent read impl-review --source recent-unwrapped --lines 60" || strings.Contains(also[0].Text, "Publish the docs?") {
		t.Fatalf("observed dialog lost from the owner cue: also = %+v, wall mark = %s", also, v.Orchestrators[0].Tasks[0].Mark)
	}
}

// Work waiting starts at workWaitingAfter and ends when the orchestrator
// acks; the tally goes amber only for it.
func TestWorkWaitingThreshold(t *testing.T) {
	h := newHarness(t)
	db := h.openDB()
	db.Exec(`insert into meta (key, value) values (?, ?)`, heartbeatKey, now())
	top := h.newTask("orch", "orchestrator", 0, "--pane", "w1:p1")
	sub := h.newTask("sub", "sub-orchestrator", top, "--pane", "w1:p2")
	subL := h.launch(sub)
	lane := h.newTask("impl", "implementer", sub, "--pane", "w1:p3")
	laneL := h.launch(lane)
	e1 := num(h.ok(as(lane, laneL), "ready", "slice 1"), "event_id")
	e2 := num(h.ok(as(lane, laneL), "done", "slice 2"), "event_id")
	backdate(t, db, e1, workWaitingAfter-time.Minute)
	backdate(t, db, e2, workWaitingAfter-2*time.Minute)
	v := viewAt(t, h.dash())
	o := v.Orchestrators[0]
	if len(v.Attention) != 0 || o.Tally != tallyGreen || len(o.Unread) != 1 || o.Unread[0].EventID != e1 || o.Unread[0].Count != 2 ||
		o.Unread[0].Recipient.ID != sub || o.Unread[0].From.ID != lane {
		t.Fatalf("under the threshold: attention %s, orch %+v", kindsOf(v.Attention), o)
	}
	backdate(t, db, e1, workWaitingAfter+time.Minute)
	backdate(t, db, e2, workWaitingAfter+time.Minute)
	v = viewAt(t, h.dash())
	if kindsOf(v.Attention) != attentionWorkWaits || v.Orchestrators[0].Tally != tallyAmber ||
		!strings.Contains(v.Attention[0].Text, "(2 unread)") || v.Attention[0].PaneID != "w1:p2" {
		t.Fatalf("over the threshold: %+v tally %s", v.Attention, v.Orchestrators[0].Tally)
	}
	for _, e := range []int64{e1, e2} {
		h.ok(as(sub, subL), "wait", "--as", id(sub), "--timeout", "1000")
		h.ok(as(sub, subL), "ack", id(e), "--as", id(sub))
	}
	if v = viewAt(t, h.dash()); len(v.Attention) != 0 || len(v.Orchestrators[0].Unread) != 0 {
		t.Fatalf("after ack: %+v %+v", v.Attention, v.Orchestrators[0].Unread)
	}
}

// Milestones are ready/done/fail/handover/adopt/decision events, root
// notes and pr/release/tag/merged refs, newest first, across open and
// closed campaigns, capped.
func TestMilestonesSelection(t *testing.T) {
	h := newHarness(t)
	db := h.openDB()
	top := h.newTask("orch-m", "orchestrator", 0)
	lane := h.newTask("rev-m", "reviewer", top)
	laneL := h.launch(lane)
	h.ok(nil, "note", "--as", id(top), "phase 2")
	h.ok(as(lane, laneL), "note", "lane chatter")
	h.ok(as(lane, laneL), "ready", "APPROVE")
	h.ok(nil, "set", id(lane), "pr=42", "commit=abc", "eas.run=1")
	h.ok(nil, "set", id(top), "release=v0.7.0")
	h.ok(nil, "decide", "--as", id(top), "App first.")
	h.ok(nil, "next", id(lane), "re-review")
	for _, k := range []string{"handover", "adopt"} {
		db.Exec(`insert into events (task_id, kind, summary, created_at) values (?, ?, ?, ?)`, top, k, k+" text", now())
	}
	h.ok(as(lane, laneL), "done", "all good")
	old := h.newTask("orch-old", "orchestrator", 0)
	h.ok(nil, "note", "--as", id(old), "shipped")
	h.ok(nil, "close", id(old))

	ms := viewAt(t, h.dash()).Milestones
	var got []string
	for _, m := range ms {
		got = append(got, m.Kind+":"+m.Orchestrator+"/"+m.Lane+":"+m.Text)
		if m.Machine == "" || m.AgeMS < 0 || m.At == "" {
			t.Fatalf("milestone %+v", m)
		}
	}
	want := []string{"note:orch-old/:shipped", "done:orch-m/rev-m:all good", "adopt:orch-m/:adopt text", "handover:orch-m/:handover text",
		"decision:orch-m/:App first.", "ref:orch-m/:release=v0.7.0", "ref:orch-m/rev-m:pr=42", "ready:orch-m/rev-m:APPROVE",
		"note:orch-m/:phase 2"}
	if strings.Join(got, "|") != strings.Join(want, "|") {
		t.Fatalf("milestones =\n%s\nwant\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	for i := 0; i < stateMilestonesMax+5; i++ {
		h.ok(nil, "note", "--as", id(top), fmt.Sprint("step ", i))
	}
	if ms = viewAt(t, h.dash()).Milestones; len(ms) != stateMilestonesMax || ms[0].Text != fmt.Sprint("step ", stateMilestonesMax+4) {
		t.Fatalf("capped milestones = %d, newest %+v", len(ms), ms[0])
	}
}

// Tally precedence and lane marks, derived from a state alone.
func TestTallyAndMarks(t *testing.T) {
	at := time.Now()
	yes, no := true, false
	ts := func(d time.Duration) string { return stamp(at.Add(-d)) }
	lane := func(id int64, status, agent string, present *bool) stateTask {
		return stateTask{ID: id, Name: fmt.Sprint("l", id), Role: "implementer", Status: status, AgentStatus: agent, Present: present}
	}
	overdue := func(root int64) []unreadWork {
		return []unreadWork{{EventID: root * 100, Kind: "ready", At: ts(workWaitingAfter + time.Second), Count: 1,
			Recipient: taskRef{ID: root, Name: "o"}, From: taskRef{ID: root*10 + 1, Name: "l"}}}
	}
	s := &dashState{
		OwnerAsks: []ownerAsk{{ID: 9, Text: "q", At: ts(time.Minute), Asker: taskRef{ID: 1, Name: "red"},
			Root: taskRef{ID: 1, Name: "red"}, AskerIsRoot: true}},
		Orchestrators: []orchestrator{
			{ID: 1, Name: "red", Tasks: []stateTask{lane(11, "done", "", &no), lane(12, "planned", "", nil), lane(13, "open", "working", &yes)}},
			{ID: 2, Name: "amber", Tasks: []stateTask{lane(21, "ready", "idle", &yes), lane(22, "ready", "idle", &no)}, Unread: overdue(2)},
			{ID: 3, Name: "green", Tasks: []stateTask{lane(31, "open", "idle", &yes)}},
			{ID: 4, Name: "off", Tasks: []stateTask{}},
			{ID: 5, Name: "blocked", Tasks: []stateTask{lane(51, "open", "blocked", &yes)}},
			{ID: 6, Name: "missing", Tasks: []stateTask{lane(61, "open", "working", &no)}},
			{ID: 7, Name: "failed-and-waiting", Tasks: []stateTask{lane(71, "failed", "done", &yes)}, Unread: overdue(7)},
		},
		Milestones: []milestone{{ID: 1, Kind: "done", RootID: 3, At: ts(5 * time.Minute)}, {ID: 2, Kind: "note", RootID: 4, At: ts(20 * time.Minute)}},
		Daemon:     &daemonHealth{State: "stale", At: ts(time.Hour)},
	}
	derive(s, machineCtx{Name: "host-a", Local: true}, at)
	var tallies, marks []string
	for _, o := range s.Orchestrators {
		tallies = append(tallies, o.Name+"="+o.Tally)
		for _, l := range o.Tasks {
			marks = append(marks, l.Name+"="+l.Mark)
		}
	}
	if got := strings.Join(tallies, " "); got != "red=red amber=amber green=green off=off blocked=red missing=red failed-and-waiting=red" {
		t.Fatalf("tallies = %s", got)
	}
	if got := strings.Join(marks, " "); got != "l11=done l12=planned l13=working l21=ready l22=ready l31=working l51=blocked l61=missing l71=failed" {
		t.Fatalf("marks = %s", got)
	}
	if got := kindsOf(s.Attention); got != "owner_ask lane_blocked lane_failed lane_missing daemon_unhealthy work_waiting work_waiting" {
		t.Fatalf("attention = %s", got)
	}
	for _, m := range s.Milestones {
		if m.Machine != "host-a" {
			t.Fatalf("milestone machine = %q", m.Machine)
		}
	}
	// A stale machine says so once, instead of its daemon's health.
	derive(s, machineCtx{Name: "host-a", Stale: true, LastPushAgeMS: 90000}, at)
	if got := kindsOf(s.Attention); got != "owner_ask lane_blocked lane_failed lane_missing machine_stale work_waiting work_waiting" {
		t.Fatalf("stale machine attention = %s", got)
	}
	for _, it := range s.Attention {
		if it.Kind == attentionStale && (it.AgeMS < 89000 || it.AgeMS > 91000 || it.Local) {
			t.Fatalf("machine_stale = %+v", it)
		}
	}
}

// The built app comes from the binary: every file under web/dist/assets is
// served long-cached with its type, the font licence beside it, and
// nothing else; the embedded stylesheet is nonempty (the UI uses system fonts).
// Renderer filenames may occur only at the dynamic import or in Vite's
// leading preload map; the parser filename must never occur in the app.
func lazyMarkdownImports(app, renderer, parser string) bool {
	if strings.Contains(app, parser) {
		return false
	}
	dynamic := regexp.MustCompile("import\\(\\s*[\"'\x60]\\./" + regexp.QuoteMeta(renderer) + "[\"'\x60]\\s*\\)")
	if !dynamic.MatchString(app) {
		return false
	}
	remaining := dynamic.ReplaceAllString(app, "")
	preload := regexp.MustCompile(`^(?:const|var)\s+__vite__mapDeps=[^;]+;`)
	remaining = preload.ReplaceAllStringFunc(remaining, func(code string) string { return strings.ReplaceAll(code, renderer, "") })
	return !strings.Contains(remaining, renderer)
}

func TestDashboardAssetImportMutations(t *testing.T) {
	entries, err := webFS.ReadDir("web/dist/assets")
	if err != nil {
		t.Fatal(err)
	}
	var app, renderer, parser string
	for _, entry := range entries {
		switch {
		case strings.HasPrefix(entry.Name(), "app-"):
			body, err := webFS.ReadFile("web/dist/assets/" + entry.Name())
			if err != nil {
				t.Fatal(err)
			}
			app = string(body)
		case strings.HasPrefix(entry.Name(), "markdown-it-"):
			parser = entry.Name()
		case strings.HasPrefix(entry.Name(), "markdown-"):
			renderer = entry.Name()
		}
	}
	if renderer == "" || parser == "" || !lazyMarkdownImports(app, renderer, parser) {
		t.Fatal("baseline lazy imports fail")
	}
	for _, mutation := range []struct{ name, code string }{
		{"m8 parser side-effect import", `import"./` + parser + `";`},
		{"m9 renderer side-effect import", `import"./` + renderer + `";`},
		{"renderer static import", `import { Markdown } from "./` + renderer + `";`},
		{"renderer outside allowed contexts", `const filename="` + renderer + `";`},
	} {
		t.Run(mutation.name, func(t *testing.T) {
			if lazyMarkdownImports(app+mutation.code, renderer, parser) {
				t.Fatal("eager/orphaned import escaped the guard")
			}
		})
	}
}

func TestDashboardAssets(t *testing.T) {
	h := newHarness(t)
	d := h.dash()
	entries, err := webFS.ReadDir("web/dist/assets")
	if err != nil || len(entries) < 3 {
		t.Fatalf("web/dist/assets = %v, %v", entries, err)
	}
	var css string
	js := map[string]string{}
	var app, renderer, parser string
	for _, e := range entries {
		w, _ := serve(d, req("GET", "/assets/"+e.Name(), ""))
		want, _ := webFS.ReadFile("web/dist/assets/" + e.Name())
		if w.Code != 200 || w.Header().Get("Content-Type") != assetTypes[filepath.Ext(e.Name())] ||
			w.Header().Get("Cache-Control") != assetsCache || w.Body.Len() != len(want) {
			t.Fatalf("GET /assets/%s = %d %q %q", e.Name(), w.Code, w.Header().Get("Content-Type"), w.Header().Get("Cache-Control"))
		}
		if filepath.Ext(e.Name()) == ".css" {
			css += w.Body.String()
		}
		lazy := strings.HasPrefix(e.Name(), "markdown-") || strings.HasPrefix(e.Name(), "markdown-it-")
		if lazy {
			if strings.Contains(dashboardPage, e.Name()) {
				t.Errorf("index.html eagerly loads or preloads %s", e.Name())
			}
		} else if filepath.Ext(e.Name()) != ".woff2" && !strings.Contains(dashboardPage, "/assets/"+e.Name()) {
			t.Errorf("index.html does not load %s", e.Name())
		}
		if filepath.Ext(e.Name()) == ".js" {
			js[e.Name()] = string(want)
			switch {
			case strings.HasPrefix(e.Name(), "app-"):
				app = e.Name()
			case strings.HasPrefix(e.Name(), "markdown-it-"):
				if parser != "" {
					t.Fatal("more than one parser chunk")
				}
				parser = e.Name()
			case strings.HasPrefix(e.Name(), "markdown-"):
				if renderer != "" {
					t.Fatal("more than one renderer chunk")
				}
				renderer = e.Name()
			}
		}
	}
	// The app dynamically imports the renderer; its vendor dependency follows
	// that lazy boundary. Neither Markdown chunk is in the initial page.
	if app == "" || renderer == "" || parser == "" {
		t.Fatalf("missing import-chain chunks: app=%q renderer=%q parser=%q", app, renderer, parser)
	}
	parserDependency := regexp.MustCompile("from[\"'\x60]\\./" + regexp.QuoteMeta(parser) + "[\"'\x60]")
	if !lazyMarkdownImports(js[app], renderer, parser) || !parserDependency.MatchString(js[renderer]) {
		t.Fatal("Markdown chunks are not isolated on the app -> renderer -> parser lazy import chain")
	}
	if strings.TrimSpace(css) == "" {
		t.Error("the embedded stylesheet is empty")
	}
	if w, _ := serve(d, req("GET", "/fonts/OFL.txt", "")); w.Code != 200 || !strings.Contains(w.Body.String(), "SIL OPEN FONT LICENSE") ||
		w.Header().Get("Cache-Control") != licenceCache {
		t.Fatalf("GET /fonts/OFL.txt = %d %q", w.Code, w.Header().Get("Cache-Control"))
	}
	for _, p := range []string{"/assets/nope.js", "/assets/..%2fdashboard.go", "/assets/.hidden.js", "/assets/index.html",
		"/fonts/SOURCE.md", "/fonts/nope.txt", "/assets/", "/index.html"} {
		if w, _ := serve(d, req("GET", p, "")); w.Code != 404 || w.Header().Get("Cache-Control") != "no-store" {
			t.Fatalf("GET %s = %d cache %q", p, w.Code, w.Header().Get("Cache-Control"))
		}
	}
}

func TestDashboardUsageClassification(t *testing.T) {
	fakeTailnetHooks(t)
	h := newHarness(t)
	d := h.hubDash()
	serveCounted := func(r *http.Request) int {
		w := httptest.NewRecorder()
		d.srv.Handler.ServeHTTP(w, r)
		return w.Code
	}

	if code := serveCounted(req("GET", "/", "")); code != http.StatusOK {
		t.Fatalf("GET / = %d", code)
	}
	if code := serveCounted(req("GET", "/api/state", "")); code != http.StatusOK {
		t.Fatalf("loopback GET /api/state = %d", code)
	}
	if code := serveCounted(req("GET", "/api/state", "", from(hostAIP, "hub.example.ts.net:7788"))); code != http.StatusOK {
		t.Fatalf("tailnet GET /api/state = %d", code)
	}

	at := time.Now()
	body, err := json.Marshal(push(peerState(at), at))
	if err != nil {
		t.Fatal(err)
	}
	pushReq := req("POST", peerPushPath, string(body), from(hostAIP, "hub.example.ts.net:7788"))
	pushReq.Header.Del("Origin")
	pushReq.Header.Del("Sec-Fetch-Site")
	if code := serveCounted(pushReq); code != http.StatusOK {
		t.Fatalf("accepted peer push = %d", code)
	}
	badPush := req("POST", peerPushPath, "{", from(hostAIP, "hub.example.ts.net:7788"))
	badPush.Header.Del("Origin")
	badPush.Header.Del("Sec-Fetch-Site")
	if code := serveCounted(badPush); code != http.StatusBadRequest {
		t.Fatalf("malformed peer push = %d, want 400", code)
	}

	refused := req("GET", "/api/state", "", from(otherIP, "hub.example.ts.net:7788"))
	if code := serveCounted(refused); code != http.StatusForbidden {
		t.Fatalf("whois refusal = %d, want 403 before routing", code)
	}
	badHost := req("GET", "/api/state", "", func(r *http.Request) { r.Host = "wrong.example" })
	if code := serveCounted(badHost); code != http.StatusMisdirectedRequest {
		t.Fatalf("Host mismatch = %d, want 421 before routing", code)
	}
	if code := serveCounted(req("GET", "/assets/missing.js", "")); code != http.StatusNotFound {
		t.Fatalf("missing asset = %d", code)
	}

	d.usage.mu.Lock()
	got := map[string]int64{}
	for _, counts := range d.usage.pending {
		for class, count := range counts {
			got[class] += count
		}
	}
	want := map[string]int64{
		usagePage: 1, usageStateLoopback: 1, usageStateTailnet: 1,
		usagePushAccepted: 1, usageRefused: 2, usageOther: 2,
	}
	for class, count := range want {
		if got[class] != count {
			t.Errorf("%s count = %d, want %d (all counts %v)", class, got[class], count, got)
		}
	}
	d.usage.mu.Unlock()
}

func TestDaemonStatusDashboardUsage(t *testing.T) {
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
