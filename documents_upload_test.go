package main

import (
	"bytes"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"testing"
)

func uploadEnv(base map[string]string, extra ...map[string]string) map[string]string {
	out := map[string]string{}
	for k, v := range base {
		out[k] = v
	}
	for _, values := range extra {
		for k, v := range values {
			out[k] = v
		}
	}
	return out
}

func uploadClient(t *testing.T, r *twoHost, home string, env map[string]string, args ...string) (int, string, string) {
	t.Helper()
	r.caller.Store("host-a")
	var out, errb bytes.Buffer
	code := cliMain(append([]string{"--json"}, args...), clientEnv(home, env), &out, &errb)
	return code, out.String(), errb.String()
}

func uploadRoot(t *testing.T, r *twoHost) int64 {
	t.Helper()
	return num(r.want(0, "host-a", nil, "new", "root-a", "--role", "orchestrator", "--cwd", r.dir), "task_id")
}

func uploadLane(t *testing.T, r *twoHost, root int64, name, report string) (int64, int64) {
	t.Helper()
	lane := num(r.want(0, "host-a", nil, "new", name, "--role", "implementer", "--parent", id(root), "--cwd", r.dir, "--report", report), "task_id")
	launch := num(r.want(0, "host-a", nil, "launch", id(lane), "--provider", "codex", "--model", "m", "--effort", "high"), "launch_id")
	return lane, launch
}

func assertClientDocument(t *testing.T, r *twoHost, task int64, kind, name, path string, eventID *int64) document {
	t.Helper()
	d := docLatest(t, r.openDB(), task, kind, name)
	if !d.Captured || d.Reason.Valid || d.Host.String != "host-a" || d.Path.String != path {
		t.Fatalf("document = %+v", d)
	}
	if eventID == nil && d.EventID.Valid || eventID != nil && (!d.EventID.Valid || d.EventID.Int64 != *eventID) {
		t.Fatalf("document event = %+v, want %v", d.EventID, eventID)
	}
	return d
}

func TestDocUploadCapturePoints(t *testing.T) {
	r := newTwoHost(t)
	rootBrief := docFile(t, r.dir, "root.md", "root brief from client")
	root := num(r.want(0, "host-a", nil, "new", "root-a", "--role", "orchestrator", "--cwd", r.dir, "--brief", rootBrief), "task_id")
	assertClientDocument(t, r, root, "goal", "", rootBrief, nil)

	for _, kind := range []string{"ready", "done", "fail", "close"} {
		t.Run(kind, func(t *testing.T) {
			path := docFile(t, r.dir, filepath.Join("reports", kind+".md"), "report "+kind)
			lane, launch := uploadLane(t, r, root, "lane-"+kind, path)
			var result map[string]any
			switch kind {
			case "ready":
				result = r.want(0, "host-a", as(lane, launch), "ready", "ready", "--report", path)
			case "done":
				result = r.want(0, "host-a", as(lane, launch), "done", "finished")
			case "fail":
				result = r.want(0, "host-a", as(lane, launch), "fail", "failed")
			case "close":
				result = r.want(0, "host-a", nil, "close", id(lane))
			}
			assertClientDocument(t, r, lane, "report", "", path, ptr(num(result, "event_id")))
		})
	}

	path := docFile(t, r.dir, "prompt.md", "prompt from client")
	lane, launch := uploadLane(t, r, root, "lane-prompt", "")
	r.write("prompt.stdout", `{"result":{"agent":{"agent_status":"working"}}}`, 0o644)
	env := uploadEnv(as(lane, launch), map[string]string{"HERDR_SOCKET_PATH": r.herdrSock})
	promptResult := r.want(0, "host-a", env, "prompt", id(lane), "--file", path, "--receipt-timeout", "0")
	promptDoc := docLatest(t, r.openDB(), lane, "prompt", filepath.Base(path))
	if !promptDoc.Captured || promptDoc.Host.String != "host-a" || promptDoc.Path.String != path ||
		!promptDoc.EventID.Valid || promptDoc.EventID.Int64 != num(promptResult, "attempt_id") {
		t.Fatalf("file prompt document = %+v; result %v", promptDoc, promptResult)
	}

	r.write("prompt.stdout", `{"result":{"agent":{"agent_status":"working"}}}`, 0o644)
	r.want(0, "host-a", env, "prompt", id(lane), "--text", "literal prompt", "--receipt-timeout", "0")
	textDoc := docLatest(t, r.openDB(), lane, "prompt", "")
	if !textDoc.Captured || textDoc.Host.Valid || textDoc.Reason.Valid {
		t.Fatalf("text prompt document = %+v", textDoc)
	}
	if code, _, _ := r.cli("host-a", nil, "handover", "--as", id(root)); code != exitOK {
		t.Fatalf("handover = %d", code)
	}
	handover := docLatest(t, r.openDB(), root, "handover", "")
	if !handover.Captured || handover.Host.Valid || handover.Reason.Valid {
		t.Fatalf("handover document = %+v", handover)
	}
}

func TestDocUploadCapabilityGate(t *testing.T) {
	r := newTwoHost(t)
	root := uploadRoot(t, r)
	path := docFile(t, r.dir, "report.md", "report")
	lane, launch := uploadLane(t, r, root, "lane-cap", path)
	env := as(lane, launch)
	base := rpcBody(r.dir, env, "capability-old-01", "ready", "first", "--report", path)
	_, rep, raw := r.post("host-a", base)
	if rep.Exit != exitOK || strings.Contains(raw, `"upload"`) {
		t.Fatalf("reply without capability = %+v %s", rep, raw)
	}
	base.RequestKey = "capability-new-01"
	base.Capabilities = []string{docUploadCapability}
	_, rep, raw = r.post("host-a", base)
	if rep.Exit != exitOK || rep.Upload == nil || len(*rep.Upload) != 1 || (*rep.Upload)[0].Task != lane || (*rep.Upload)[0].Path != path ||
		strings.Contains(rep.Stdout, "upload") || !strings.Contains(raw, `"upload":[`) {
		t.Fatalf("reply with capability = %+v %s", rep, raw)
	}
}

func TestDocUploadClientSetAndRequestCap(t *testing.T) {
	r := newTwoHost(t)
	root := uploadRoot(t, r)
	goal := docFile(t, r.dir, "goal.md", "goal from caller")
	if got := r.want(0, "host-a", nil, "doc", "set", id(root), "goal", "--file", goal); got["same"] != false {
		t.Fatalf("goal set = %v", got)
	}
	doc := docLatest(t, r.openDB(), root, "goal", "")
	if !doc.Captured || doc.Host.String != "host-a" || doc.Path.String != goal || !doc.EventID.Valid {
		t.Fatalf("goal set document = %+v", doc)
	}

	lane, _ := uploadLane(t, r, root, "lane-plan", "")
	plan := docFile(t, r.dir, "plan.md", "named plan")
	r.want(0, "host-a", nil, "doc", "set", id(lane), "plan", "--name", "implementation", "--file", plan)
	doc = docLatest(t, r.openDB(), lane, "plan", "implementation")
	if !doc.Captured || doc.Host.String != "host-a" || doc.Path.String != plan || !doc.EventID.Valid {
		t.Fatalf("named plan document = %+v", doc)
	}

	largeNewlines := docFile(t, r.dir, "newlines.md", strings.Repeat("\n", documentCap))
	r.want(0, "host-a", nil, "doc", "set", id(lane), "plan", "--name", "newlines", "--file", largeNewlines)
	d := docLatest(t, r.openDB(), lane, "plan", "newlines")
	if !d.Captured || d.Bytes.Int64 != documentCap {
		t.Fatalf("document at request cap = %+v", d)
	}
	var body string
	if err := r.openDB().QueryRow(`select body from doc_blobs where sha256 = ?`, d.Hash.String).Scan(&body); err != nil || body != strings.Repeat("\n", documentCap) {
		t.Fatalf("request-cap body length=%d err=%v", len(body), err)
	}

	for _, tc := range []struct {
		name string
		body string
		want string
	}{
		{name: "missing", want: "document file is missing or unreadable"},
		{name: "too-large", body: strings.Repeat("x", documentCap+1), want: "too_large"},
		{name: "binary", body: "\xff", want: "binary"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			path := filepath.Join(r.dir, tc.name)
			if tc.name != "missing" {
				docFile(t, r.dir, tc.name, tc.body)
			}
			before := r.count(`select count(*) from requests`)
			clientCode, clientOut, clientErr := uploadClient(t, r, r.homes["host-a"], nil, "doc", "set", id(lane), "plan", "--file", path)
			localCode, localLines := r.run(nil, "doc", "set", id(lane), "plan", "--file", path)
			if clientCode != localCode || len(localLines) != 1 || !reflect.DeepEqual(lastJSON(clientOut), localLines[0]) || clientErr != r.lastStderr() || clientCode != exitUsage {
				t.Fatalf("client/local mismatch: client=%d %q %q local=%d %v %q", clientCode, clientOut, clientErr, localCode, localLines, r.lastStderr())
			}
			if tc.name != "missing" && !strings.Contains(clientOut+clientErr, tc.want) {
				t.Fatalf("missing %q in %q %q", tc.want, clientOut, clientErr)
			}
			if after := r.count(`select count(*) from requests`); after != before {
				t.Fatalf("invalid file sent to server: requests %d -> %d", before, after)
			}
		})
	}
}

func TestDocUploadPutValidationAndIdempotency(t *testing.T) {
	r := newTwoHost(t)
	root := uploadRoot(t, r)
	path := docFile(t, r.dir, "report.md", "uploaded report")
	first, _ := uploadLane(t, r, root, "lane-first", path)
	second, _ := uploadLane(t, r, root, "lane-second", path)
	capRequest := func(task int64, key string) (rpcDocWant, int64) {
		r.t.Helper()
		var launch int64
		if err := r.openDB().QueryRow(`select current_launch_id from tasks where id = ?`, task).Scan(&launch); err != nil {
			t.Fatal(err)
		}
		req := rpcBody(r.dir, as(task, launch), key, "ready", key, "--report", path)
		req.Capabilities = []string{docUploadCapability}
		_, rep, _ := r.post("host-a", req)
		if rep.Exit != exitOK || rep.Upload == nil || len(*rep.Upload) != 1 {
			t.Fatalf("capture response = %+v", rep)
		}
		return (*rep.Upload)[0], num(lastJSON(rep.Stdout), "event_id")
	}
	want, eventID := capRequest(first, "capture-first-01")
	_, _ = capRequest(second, "capture-second-01")
	in := fileDocument(path, "")
	payload := clientDocPayload(want, in, false, false)
	before := docCount(t, r.openDB(), `select count(*) from documents where task_id = ? and kind = 'report'`, first)
	tryPut := func(key string, p *rpcDocPayload) rpcReply {
		r.t.Helper()
		req := rpcBody(r.dir, nil, key, "_doc", "put")
		req.Capabilities = []string{docUploadCapability}
		req.Document = p
		_, rep, _ := r.post("host-a", req)
		if n := r.count(`select count(*) from requests where key = ?`, key); n != 0 {
			t.Fatalf("hidden request %s was stored", key)
		}
		return rep
	}

	badHash := *payload
	badHash.SHA256 = strings.Repeat("0", sha256.Size*2)
	if rep := tryPut("put-bad-hash-01", &badHash); rep.Exit != exitReject {
		t.Fatalf("bad hash accepted: %+v", rep)
	}
	wrongEvent := *payload
	wrongTask := second
	wrongEvent.Task = wrongTask
	wrongEvent.EventID = ptr(eventID)
	if rep := tryPut("put-wrong-event-01", &wrongEvent); rep.Exit != exitReject {
		t.Fatalf("foreign event accepted: %+v", rep)
	}
	unknown := *payload
	unknown.Task = 999999
	if rep := tryPut("put-unknown-task-01", &unknown); rep.Exit != exitReject {
		t.Fatalf("unknown task accepted: %+v", rep)
	}
	if got := docCount(t, r.openDB(), `select count(*) from documents where task_id = ? and kind = 'report'`, first); got != before {
		t.Fatalf("rejected upload wrote a document: %d -> %d", before, got)
	}
	if rep := tryPut("put-valid-one-01", payload); rep.Exit != exitOK || !strings.Contains(rep.Stdout, `"count":"captured"`) {
		t.Fatalf("valid upload = %+v", rep)
	}
	if rep := tryPut("put-valid-two-01", payload); rep.Exit != exitOK || !strings.Contains(rep.Stdout, `"count":"unchanged"`) {
		t.Fatalf("repeat upload = %+v", rep)
	}
	d := assertClientDocument(t, r, first, "report", "", path, ptr(eventID))
	if d.Version != 2 || docCount(t, r.openDB(), `select count(*) from documents where task_id = ? and kind = 'report'`, first) != before+1 {
		t.Fatalf("idempotent version = %+v", d)
	}
}

func TestDocUploadMissReasons(t *testing.T) {
	r := newTwoHost(t)
	root := uploadRoot(t, r)
	cases := []struct{ name, reason, body string }{
		{name: "missing", reason: "missing"},
		{name: "large", reason: "too_large", body: strings.Repeat("x", documentCap+1)},
		{name: "binary", reason: "binary", body: "\xff"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			path := filepath.Join(r.dir, "reports", tc.name+".md")
			if tc.body != "" {
				docFile(t, r.dir, filepath.Join("reports", tc.name+".md"), tc.body)
			}
			lane, launch := uploadLane(t, r, root, "lane-"+tc.name, path)
			r.want(0, "host-a", as(lane, launch), "ready", "ready", "--report", path)
			d := docLatest(t, r.openDB(), lane, "report", "")
			if d.Captured || d.Reason.String != tc.reason || d.Host.String != "host-a" || d.Path.String != path {
				t.Fatalf("miss = %+v", d)
			}
		})
	}
}

func TestDocUploadBestEffortFailures(t *testing.T) {
	for _, mode := range []string{"server-error", "timeout", "unknown-command"} {
		t.Run(mode, func(t *testing.T) {
			r := newTwoHost(t)
			var mu sync.Mutex
			var calls []string
			url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
				raw, err := io.ReadAll(q.Body)
				if err != nil {
					t.Error(err)
					return
				}
				var req rpcRequest
				_ = json.Unmarshal(raw, &req)
				name, _ := rpcCommand(req.Argv)
				if name == "_doc" {
					mu.Lock()
					calls = append(calls, name)
					mu.Unlock()
					switch mode {
					case "server-error":
						http.Error(w, "failed", http.StatusInternalServerError)
					case "timeout":
						<-q.Context().Done()
					case "unknown-command":
						httpJSON(w, http.StatusOK, rpcReply{Exit: exitUsage, Stdout: `{"error":"unknown command _doc","kind":"usage"}`})
					}
					return
				}
				q.Body = io.NopCloser(bytes.NewReader(raw))
				r.d.ServeHTTP(w, q)
			})
			go srv.Serve(ln)
			home := r.clientHome(url)
			path := docFile(t, r.dir, "brief.md", "brief")
			code, out, stderr := uploadClient(t, r, home, nil, "new", "root-a", "--role", "orchestrator", "--cwd", r.dir, "--brief", path)
			if code != exitOK || stderr != "" || len(strings.Split(strings.TrimSpace(out), "\n")) != 1 || strings.Contains(out, "upload") {
				t.Fatalf("primary result changed: %d %q %q", code, out, stderr)
			}
			mu.Lock()
			got := append([]string(nil), calls...)
			mu.Unlock()
			if !reflect.DeepEqual(got, []string{"_doc"}) {
				t.Fatalf("upload attempts = %v", got)
			}
			if d := docLatest(t, r.openDB(), int64(num(lastJSON(out), "task_id")), "goal", ""); d.Captured || d.Reason.String != "client" {
				t.Fatalf("primary capture changed after failed upload: %+v", d)
			}
		})
	}
}

func TestClientBackfillTwoPagesAndHostScope(t *testing.T) {
	r := newTwoHost(t)
	root := r.newTask("root-a", "orchestrator", 0)
	lane := r.newTask("lane-a", "implementer", root)
	pathDir := filepath.Join(r.dir, "client-files")
	if err := os.MkdirAll(pathDir, 0o755); err != nil {
		t.Fatal(err)
	}
	docExec(t, r.openDB(), `update tasks set machine = 'host-a' where id = ?`, lane)
	paths := make([]string, 201)
	events := make([]int64, 201)
	for i := range paths {
		body := fmt.Sprintf("prompt body %03d", i)
		paths[i] = filepath.Join(pathDir, fmt.Sprintf("p-%03d.md", i))
		if err := os.WriteFile(paths[i], []byte(body), 0o600); err != nil {
			t.Fatal(err)
		}
		sum := sha256.Sum256([]byte(body))
		if err := withTx(r.openDB(), func(tx *sql.Tx) error {
			var err error
			events[i], err = insertEvent(tx, event{TaskID: lane, Kind: "prompt", Data: map[string]any{
				"file": paths[i], "sha256": hex.EncodeToString(sum[:]),
			}})
			return err
		}); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.Remove(paths[200]); err != nil {
		t.Fatal(err)
	}
	other := r.newTask("lane-b", "implementer", root, "--report", docFile(t, r.dir, "other.md", "other"))
	docExec(t, r.openDB(), `update tasks set machine = 'host-b' where id = ?`, other)

	var mu sync.Mutex
	var offsets []int
	puts := 0
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		raw, err := io.ReadAll(q.Body)
		if err != nil {
			t.Error(err)
			return
		}
		var req rpcRequest
		_ = json.Unmarshal(raw, &req)
		name, args := rpcCommand(req.Argv)
		mu.Lock()
		if name == "_doc" && len(args) > 0 && args[0] == "wanted" {
			offset, _, _, _ := flagValue(args[1:], "offset")
			var n int
			fmt.Sscan(offset, &n)
			offsets = append(offsets, n)
		}
		if name == "_doc" && len(args) > 0 && args[0] == "put" {
			puts++
		}
		mu.Unlock()
		q.Body = io.NopCloser(bytes.NewReader(raw))
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	home := r.clientHome(url)
	code, out, stderr := uploadClient(t, r, home, nil, "doc", "backfill")
	if code != exitOK || stderr != "" {
		t.Fatalf("backfill = %d %q %q", code, out, stderr)
	}
	counts := lastJSON(out)
	if num(counts, "captured") != 200 || num(counts, "missing") != 1 || num(counts, "unchanged") != 0 {
		t.Fatalf("first backfill counts = %v", counts)
	}
	mu.Lock()
	firstOffsets, firstPuts := append([]int(nil), offsets...), puts
	offsets, puts = nil, 0
	mu.Unlock()
	if !reflect.DeepEqual(firstOffsets, []int{0, 200}) || firstPuts != 201 {
		t.Fatalf("first pages = %v, puts=%d", firstOffsets, firstPuts)
	}
	if got := docCount(t, r.openDB(), `select count(*) from documents where task_id = ?`, lane); got != 201 {
		t.Fatalf("backfill rows = %d", got)
	}
	for i := 0; i < 200; i++ {
		d := docLatest(t, r.openDB(), lane, "prompt", filepath.Base(paths[i]))
		if !d.Captured || d.Host.String != "host-a" || !d.EventID.Valid || d.EventID.Int64 != events[i] || d.Backfill != 1 {
			t.Fatalf("backfill %d = %+v", i, d)
		}
	}
	missing := docLatest(t, r.openDB(), lane, "prompt", filepath.Base(paths[200]))
	if missing.Captured || missing.Reason.String != "missing" || missing.Backfill != 2 || missing.Host.String != "host-a" {
		t.Fatalf("gone file = %+v", missing)
	}
	if n := docCount(t, r.openDB(), `select count(*) from documents where task_id = ?`, other); n != 0 {
		t.Fatalf("other host received %d documents", n)
	}

	code, out, stderr = uploadClient(t, r, home, nil, "doc", "backfill")
	if code != exitOK || stderr != "" || num(lastJSON(out), "unchanged") != 201 {
		t.Fatalf("second backfill = %d %q %q", code, out, stderr)
	}
	mu.Lock()
	secondOffsets, secondPuts := append([]int(nil), offsets...), puts
	mu.Unlock()
	if !reflect.DeepEqual(secondOffsets, []int{0, 200}) || secondPuts != 1 || docCount(t, r.openDB(), `select count(*) from documents where task_id = ?`, lane) != 201 {
		t.Fatalf("second pages = %v, puts=%d", secondOffsets, secondPuts)
	}
}

func TestDocUploadLegacyServerCapabilityFallback(t *testing.T) {
	r := newTwoHost(t)
	var mu sync.Mutex
	var caps []bool
	puts := 0
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		raw, err := io.ReadAll(q.Body)
		if err != nil {
			t.Error(err)
			return
		}
		var req rpcRequest
		_ = json.Unmarshal(raw, &req)
		name, _ := rpcCommand(req.Argv)
		mu.Lock()
		caps = append(caps, hasCapability(req.Capabilities, docUploadCapability))
		if name == "_doc" {
			puts++
		}
		mu.Unlock()
		if name != "_doc" && hasCapability(req.Capabilities, docUploadCapability) {
			httpJSON(w, http.StatusBadRequest, map[string]string{"error": `bad request: json: unknown field "capabilities"`})
			return
		}
		q.Body = io.NopCloser(bytes.NewReader(raw))
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	home := r.clientHome(url)
	path := docFile(t, r.dir, "brief.md", "brief")
	code, out, stderr := uploadClient(t, r, home, nil, "new", "root-a", "--role", "orchestrator", "--cwd", r.dir, "--brief", path)
	if code != exitOK || stderr != "" || strings.Contains(out, "upload") {
		t.Fatalf("legacy fallback = %d %q %q", code, out, stderr)
	}
	mu.Lock()
	gotCaps, gotPuts := append([]bool(nil), caps...), puts
	mu.Unlock()
	if !reflect.DeepEqual(gotCaps, []bool{true, false}) || gotPuts != 0 {
		t.Fatalf("capability fallback requests = %v, puts=%d", gotCaps, gotPuts)
	}
}
