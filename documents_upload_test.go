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

type uploadOrderWriter struct {
	mu     *sync.Mutex
	events *[]string
	step   string
	buf    bytes.Buffer
}

func (w *uploadOrderWriter) Write(p []byte) (int, error) {
	if len(p) > 0 {
		w.mu.Lock()
		*w.events = append(*w.events, w.step)
		w.mu.Unlock()
	}
	return w.buf.Write(p)
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
	serverLane := r.newTask("lane-server-prompt", "implementer", root, "--pane", "w1:p2")
	serverLaunch := num(r.one(0, nil, "launch", id(serverLane), "--provider", "codex", "--model", "m", "--effort", "high"), "launch_id")
	serverPrompt := docFile(t, r.dir, "server-prompt.md", "caller-owned prompt")
	r.write("prompt.stdout", `{"result":{"agent":{"agent_status":"working"}}}`, 0o644)
	serverEnv := uploadEnv(as(serverLane, serverLaunch), map[string]string{"HERDR_SOCKET_PATH": r.herdrSock})
	r.want(0, "host-a", serverEnv, "prompt", id(serverLane), "--file", serverPrompt, "--receipt-timeout", "0")
	if d := docLatest(t, r.openDB(), serverLane, "prompt", filepath.Base(serverPrompt)); !d.Captured || d.Host.String != "host-a" || d.Path.String != serverPrompt {
		t.Fatalf("client prompt for server lane = %+v", d)
	}
	if code, _, _ := r.cli("host-a", nil, "handover", "--as", id(root)); code != exitOK {
		t.Fatalf("handover = %d", code)
	}
	handover := docLatest(t, r.openDB(), root, "handover", "")
	if !handover.Captured || handover.Host.Valid || handover.Reason.Valid {
		t.Fatalf("handover document = %+v", handover)
	}
}

func TestDocUploadCallerOwnsBriefAndPrompt(t *testing.T) {
	r := newTwoHost(t)
	r.beat("host-b", 0)

	ledgerBrief := docFile(t, r.dir, "ledger-brief.md", "ledger caller")
	ledgerTask := num(r.one(0, nil, "new", "ledger-caller-brief", "--role", "orchestrator", "--cwd", r.dir,
		"--machine", "host-b", "--brief", ledgerBrief), "task_id")
	if d := docLatest(t, r.openDB(), ledgerTask, "goal", ""); !d.Captured || d.Host.Valid || d.Path.String != ledgerBrief {
		t.Fatalf("ledger caller brief = %+v", d)
	}

	serverBrief := docFile(t, r.dir, "client-server-brief.md", "client caller, ledger task")
	serverTask := num(r.want(0, "host-a", nil, "new", "client-caller-server-task", "--role", "orchestrator", "--cwd", r.dir,
		"--machine", localMachine(), "--brief", serverBrief), "task_id")
	if r.machineOf("tasks", serverTask) != "NULL" {
		t.Fatalf("server task machine = %s", r.machineOf("tasks", serverTask))
	}
	assertClientDocument(t, r, serverTask, "goal", "", serverBrief, nil)

	secondClientBrief := docFile(t, r.dir, "second-client-brief.md", "client caller, second client task")
	secondClientTask := num(r.want(0, "host-a", nil, "new", "client-caller-second-client-task", "--role", "orchestrator", "--cwd", r.dir,
		"--machine", "host-b", "--brief", secondClientBrief), "task_id")
	if r.machineOf("tasks", secondClientTask) != "host-b" {
		t.Fatalf("second client task machine = %s", r.machineOf("tasks", secondClientTask))
	}
	assertClientDocument(t, r, secondClientTask, "goal", "", secondClientBrief, nil)

	root := r.newTask("prompt-root", "orchestrator", 0)
	capture := func(name, caller, taskHost string, local bool) {
		t.Helper()
		task := r.newTask(name, "implementer", root)
		if taskHost != "" {
			docExec(t, r.openDB(), `update tasks set machine = ? where id = ?`, taskHost, task)
		}
		path := docFile(t, r.dir, name+".md", "prompt from "+caller)
		body, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		in := bodyDocument(body, path)
		machine := caller
		if local {
			machine = ""
		} else {
			in.Host, in.Reason = caller, "client"
		}
		c := &ctx{machine: machine, docUpload: true}
		var eventID int64
		if err := withTx(r.openDB(), func(tx *sql.Tx) error {
			var err error
			eventID, err = insertEvent(tx, event{TaskID: task, Kind: "prompt", Data: map[string]any{"file": path}})
			if err != nil {
				return err
			}
			return capturePrompt(tx, c, task, eventID, in)
		}); err != nil {
			t.Fatal(err)
		}
		if local {
			d := docLatest(t, r.openDB(), task, "prompt", filepath.Base(path))
			if !d.Captured || d.Host.Valid || len(c.docUploads) != 0 {
				t.Fatalf("local prompt capture = %+v wants=%v", d, c.docUploads)
			}
			return
		}
		if len(c.docUploads) != 1 || c.docUploads[0].Path != path {
			t.Fatalf("prompt upload wants = %+v", c.docUploads)
		}
		req := rpcBody(r.dir, nil, "prompt-capture-put-"+name, "_doc", "put")
		req.Capabilities = []string{docUploadCapability}
		req.Document = clientDocPayload(c.docUploads[0], fileDocument(path, ""), false, false)
		_, rep, _ := r.post(caller, req)
		if rep.Exit != exitOK {
			t.Fatalf("prompt upload = %+v", rep)
		}
		if d := docLatest(t, r.openDB(), task, "prompt", filepath.Base(path)); !d.Captured || d.Host.String != caller {
			t.Fatalf("uploaded prompt capture = %+v", d)
		}
	}
	capture("prompt-ledger-caller", "ledger", "host-b", true)
	capture("prompt-ledger-task", "host-a", "", false)
	capture("prompt-second-client-task", "host-a", "host-b", false)
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
	base.Capabilities = nil
	_, rep, raw = r.post("host-a", base)
	if rep.Exit != exitOK || strings.Contains(raw, `"upload"`) {
		t.Fatalf("replay without capability = %+v %s", rep, raw)
	}
}

func TestDocUploadCapabilityOnlyOnDocumentCommands(t *testing.T) {
	for _, tc := range []struct {
		name string
		argv []string
		want bool
	}{
		{"new", []string{"--json", "new", "lane"}, true},
		{"prompt", []string{"--json", "prompt", "1", "--file", "/tmp/prompt.md"}, true},
		{"_prompt", []string{"--json", "_prompt", "begin", "1"}, true},
		{"ready", []string{"--json", "ready", "ready"}, true},
		{"done", []string{"--json", "done", "finished"}, true},
		{"fail", []string{"--json", "fail", "failed"}, true},
		{"close", []string{"--json", "close", "1"}, true},
		{"doc set", []string{"--json", "doc", "set", "1", "goal"}, true},
		{"doc backfill", []string{"--json", "doc", "backfill"}, true},
		{"_doc put", []string{"--json", "_doc", "put"}, true},
		{"hook", []string{"--json", "_hook"}, false},
		{"host", []string{"--json", "_host"}, false},
		{"wait", []string{"--json", "wait"}, false},
		{"note", []string{"--json", "note", "text"}, false},
		{"doc get", []string{"--json", "doc", "get", "1"}, false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			req := rpcClientRequest(tc.argv, "/", "capability-scope-01", nil, nil)
			if got := hasCapability(req.Capabilities, docUploadCapability); got != tc.want {
				t.Fatalf("capability = %v, want %v", got, tc.want)
			}
		})
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

func TestDocUploadReportStaysOnTaskHost(t *testing.T) {
	r := newTwoHost(t)
	root := r.newTask("report-root", "orchestrator", 0)
	path := docFile(t, r.dir, "task-report.md", "report from task host")
	task := r.newTask("report-on-host-b", "implementer", root, "--report", path)
	docExec(t, r.openDB(), `update tasks set machine = 'host-b' where id = ?`, task)
	c := &ctx{machine: "host-a", docUpload: true}
	report := prepareReport(r.openDB(), task, path, true)
	var eventID int64
	if err := withTx(r.openDB(), func(tx *sql.Tx) error {
		var err error
		eventID, err = insertEvent(tx, event{TaskID: task, Kind: "ready", Data: map[string]any{"report": path}})
		if err != nil {
			return err
		}
		return captureReport(tx, c, task, eventID, report)
	}); err != nil {
		t.Fatal(err)
	}
	d := docLatest(t, r.openDB(), task, "report", "")
	if d.Captured || d.Reason.String != "client" || d.Host.String != "host-b" || len(c.docUploads) != 0 {
		t.Fatalf("foreign caller report capture = %+v wants=%v", d, c.docUploads)
	}
	before := docCount(t, r.openDB(), `select count(*) from documents where task_id = ? and kind = 'report'`, task)
	put := func(caller, key string) rpcReply {
		t.Helper()
		want := rpcDocWant{Task: task, Kind: "report", Path: path, EventID: ptr(eventID)}
		req := rpcBody(r.dir, nil, key, "_doc", "put")
		req.Capabilities = []string{docUploadCapability}
		req.Document = clientDocPayload(want, fileDocument(path, ""), false, false)
		_, rep, _ := r.post(caller, req)
		return rep
	}
	if rep := put("host-a", "report-put-wrong-host-01"); rep.Exit != exitReject {
		t.Fatalf("foreign report put = %+v", rep)
	}
	if got := docCount(t, r.openDB(), `select count(*) from documents where task_id = ? and kind = 'report'`, task); got != before {
		t.Fatalf("foreign report put wrote a row: %d -> %d", before, got)
	}
	wrongPath := docFile(t, r.dir, "other-report.md", "wrong path")
	wrongWant := rpcDocWant{Task: task, Kind: "report", Path: wrongPath, EventID: ptr(eventID)}
	wrongReq := rpcBody(r.dir, nil, "report-put-wrong-path-01", "_doc", "put")
	wrongReq.Capabilities = []string{docUploadCapability}
	wrongReq.Document = clientDocPayload(wrongWant, fileDocument(wrongPath, ""), false, false)
	if _, rep, _ := r.post("host-b", wrongReq); rep.Exit != exitReject {
		t.Fatalf("wrong-path report put = %+v", rep)
	}
	if rep := put("host-b", "report-put-owner-host-01"); rep.Exit != exitOK {
		t.Fatalf("task-host report put = %+v", rep)
	}
	if d := docLatest(t, r.openDB(), task, "report", ""); !d.Captured || d.Host.String != "host-b" || d.Path.String != path {
		t.Fatalf("task-host report = %+v", d)
	}
}

func TestDocUploadDifferentHostMisses(t *testing.T) {
	r := newTwoHost(t)
	root := r.newTask("miss-root", "orchestrator", 0)
	task := r.newTask("miss-lane", "implementer", root)
	path := docFile(t, r.dir, "same-prompt.md", "prompt")
	name := filepath.Base(path)
	for _, host := range []string{"host-a", "host-b"} {
		in := documentInput{Path: path, Host: host, Reason: "client"}
		if err := withTx(r.openDB(), func(tx *sql.Tx) error {
			_, _, err := storeDocument(tx, task, "prompt", name, in, nil, 0)
			return err
		}); err != nil {
			t.Fatal(err)
		}
	}
	if got := docCount(t, r.openDB(), `select count(*) from documents where task_id = ? and kind = 'prompt' and name = ?`, task, name); got != 2 {
		t.Fatalf("same-path misses = %d, want 2", got)
	}
	if d := docLatest(t, r.openDB(), task, "prompt", name); d.Captured || d.Host.String != "host-b" || d.Version != 2 {
		t.Fatalf("latest miss = %+v", d)
	}
}

func TestDocUploadRepeatedReadyPreservesCapture(t *testing.T) {
	r := newTwoHost(t)
	root := uploadRoot(t, r)
	path := docFile(t, r.dir, filepath.Join("reports", "repeat.md"), "report one")
	lane, launch := uploadLane(t, r, root, "repeat-report", path)
	for _, summary := range []string{"first", "second"} {
		r.want(0, "host-a", as(lane, launch), "ready", summary, "--report", path)
	}
	if got := docCount(t, r.openDB(), `select count(*) from documents where task_id = ? and kind = 'report'`, lane); got != 2 {
		t.Fatalf("repeat-ready rows = %d, want miss plus capture", got)
	}

	var mu sync.Mutex
	failPut := false
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		raw, err := io.ReadAll(q.Body)
		if err != nil {
			t.Error(err)
			return
		}
		var req rpcRequest
		_ = json.Unmarshal(raw, &req)
		name, args := rpcCommand(req.Argv)
		if name == "_doc" && len(args) > 0 && args[0] == "put" {
			mu.Lock()
			fail := failPut
			failPut = false
			mu.Unlock()
			if fail {
				http.Error(w, "upload failed", http.StatusInternalServerError)
				return
			}
		}
		q.Body = io.NopCloser(bytes.NewReader(raw))
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	mu.Lock()
	failPut = true
	mu.Unlock()
	home := r.clientHome(url)
	code, out, stderr := uploadClient(t, r, home, as(lane, launch), "ready", "third", "--report", path)
	if code != exitOK || stderr != "" || len(strings.Split(strings.TrimSpace(out), "\n")) != 1 {
		t.Fatalf("failed upload changed ready output: %d %q %q", code, out, stderr)
	}
	if got := docCount(t, r.openDB(), `select count(*) from documents where task_id = ? and kind = 'report'`, lane); got != 2 {
		t.Fatalf("failed-repeat rows = %d, want 2", got)
	}
	if d := docLatest(t, r.openDB(), lane, "report", ""); !d.Captured || d.Version != 2 {
		t.Fatalf("failed upload hid the captured report: %+v", d)
	}

	docFile(t, r.dir, filepath.Join("reports", "repeat.md"), "report two")
	r.want(0, "host-a", as(lane, launch), "ready", "changed", "--report", path)
	d := docLatest(t, r.openDB(), lane, "report", "")
	if !d.Captured || d.Version != 3 || docCount(t, r.openDB(), `select count(*) from documents where task_id = ? and kind = 'report'`, lane) != 3 {
		t.Fatalf("changed report versions = %+v", d)
	}
	var body string
	if err := r.openDB().QueryRow(`select body from doc_blobs where sha256 = ?`, d.Hash.String).Scan(&body); err != nil || body != "report two" {
		t.Fatalf("changed report body = %q, err=%v", body, err)
	}
}

func TestDocUploadMissingFileKeepsCapture(t *testing.T) {
	r := newTwoHost(t)
	root := uploadRoot(t, r)
	path := docFile(t, r.dir, "report.md", "captured report")
	lane, launch := uploadLane(t, r, root, "missing-report", path)
	result := r.want(exitOK, "host-a", as(lane, launch), "ready", "first", "--report", path)
	db := r.openDB()
	before := assertClientDocument(t, r, lane, "report", "", path, ptr(num(result, "event_id")))
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	r.want(exitOK, "host-a", as(lane, launch), "ready", "file gone", "--report", path)
	if after := docLatest(t, db, lane, "report", ""); !reflect.DeepEqual(after, before) {
		t.Fatalf("client missing file replaced captured report: before=%+v after=%+v", before, after)
	}
	// Move the scratch lane to the ledger host: both host and path differ.
	docExec(t, db, `update launches set machine = null where id = ?`, launch)
	r.ok(as(lane, launch), "ready", "file gone", "--report", filepath.Join(r.dir, "absent.md"))
	if after := docLatest(t, db, lane, "report", ""); !reflect.DeepEqual(after, before) {
		t.Fatalf("missing file replaced captured report: before=%+v after=%+v", before, after)
	}
	if n := docCount(t, db, `select count(*) from documents where task_id = ? and kind = 'report'`, lane); n != 2 {
		t.Fatalf("missing file wrote a document row: %d", n)
	}
}

func TestDocUploadChangedReportPathCapturesLatest(t *testing.T) {
	r := newTwoHost(t)
	root := uploadRoot(t, r)
	pathA := docFile(t, r.dir, filepath.Join("reports", "report1.md"), "report one")
	pathB := docFile(t, r.dir, filepath.Join("reports", "report2.md"), "report two")
	lane, launch := uploadLane(t, r, root, "changed-report-path", pathA)
	r.want(0, "host-a", as(lane, launch), "ready", "first", "--report", pathA)
	r.want(0, "host-a", as(lane, launch), "ready", "second", "--report", pathB)

	db := r.openDB()
	if got := docCount(t, db, `select count(*) from documents where task_id = ? and kind = 'report'`, lane); got != 4 {
		t.Fatalf("changed-path rows = %d, want 4", got)
	}
	miss, err := scanDocument(db.QueryRow(`select `+documentCols+` from documents where task_id = ? and kind = 'report' and version = 3`, lane))
	if err != nil || miss.Captured || miss.Reason.String != "client" || miss.Host.String != "host-a" || miss.Path.String != pathB {
		t.Fatalf("changed-path miss = %+v, err=%v", miss, err)
	}
	d := docLatest(t, db, lane, "report", "")
	if !d.Captured || d.Version != 4 || d.Host.String != "host-a" || d.Path.String != pathB {
		t.Fatalf("changed-path capture = %+v", d)
	}
}

func TestDocUploadPromptSameNameNewPathCapturesLatest(t *testing.T) {
	r := newTwoHost(t)
	root := uploadRoot(t, r)
	lane, launch := uploadLane(t, r, root, "same-name-prompt", "")
	pathA := docFile(t, r.dir, filepath.Join("a", "prompt.md"), "prompt one")
	pathB := docFile(t, r.dir, filepath.Join("b", "prompt.md"), "prompt two")
	env := uploadEnv(as(lane, launch), map[string]string{"HERDR_SOCKET_PATH": r.herdrSock})
	for _, path := range []string{pathA, pathB} {
		r.write("prompt.stdout", `{"result":{"agent":{"agent_status":"working"}}}`, 0o644)
		r.want(0, "host-a", env, "prompt", id(lane), "--file", path, "--receipt-timeout", "0")
	}

	db := r.openDB()
	name := filepath.Base(pathA)
	if got := docCount(t, db, `select count(*) from documents where task_id = ? and kind = 'prompt' and name = ?`, lane, name); got != 4 {
		t.Fatalf("same-name prompt rows = %d, want 4", got)
	}
	miss, err := scanDocument(db.QueryRow(`select `+documentCols+` from documents where task_id = ? and kind = 'prompt' and name = ? and version = 3`, lane, name))
	if err != nil || miss.Captured || miss.Reason.String != "client" || miss.Host.String != "host-a" || miss.Path.String != pathB {
		t.Fatalf("same-name prompt miss = %+v, err=%v", miss, err)
	}
	d := docLatest(t, db, lane, "prompt", name)
	if !d.Captured || d.Version != 4 || d.Host.String != "host-a" || d.Path.String != pathB {
		t.Fatalf("same-name prompt capture = %+v", d)
	}
}

func TestDocUploadReportSamePathNewHostCapturesLatest(t *testing.T) {
	r := newTwoHost(t)
	root := uploadRoot(t, r)
	path := docFile(t, r.dir, filepath.Join("reports", "moved.md"), "report")
	lane, launch := uploadLane(t, r, root, "moved-report-host", path)
	r.want(0, "host-a", as(lane, launch), "ready", "first", "--report", path)
	docExec(t, r.openDB(), `update launches set machine = 'host-b' where id = ?`, launch)
	r.want(0, "host-b", as(lane, launch), "ready", "second", "--report", path)

	db := r.openDB()
	if got := docCount(t, db, `select count(*) from documents where task_id = ? and kind = 'report'`, lane); got != 4 {
		t.Fatalf("changed-host rows = %d, want 4", got)
	}
	miss, err := scanDocument(db.QueryRow(`select `+documentCols+` from documents where task_id = ? and kind = 'report' and version = 3`, lane))
	if err != nil || miss.Captured || miss.Reason.String != "client" || miss.Host.String != "host-b" || miss.Path.String != path {
		t.Fatalf("changed-host miss = %+v, err=%v", miss, err)
	}
	d := docLatest(t, db, lane, "report", "")
	if !d.Captured || d.Version != 4 || d.Host.String != "host-b" || d.Path.String != path {
		t.Fatalf("changed-host capture = %+v", d)
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

func TestDocUploadPromptOutputBeforeUpload(t *testing.T) {
	r := newTwoHost(t)
	root := uploadRoot(t, r)
	path := docFile(t, r.dir, "ordered-prompt.md", "prompt")
	lane, launch := uploadLane(t, r, root, "ordered-prompt", "")
	r.write("prompt.stdout", `{"result":{"agent":{"agent_status":"working"}}}`, 0o644)
	var mu sync.Mutex
	var events []string
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		raw, err := io.ReadAll(q.Body)
		if err != nil {
			t.Error(err)
			return
		}
		var req rpcRequest
		_ = json.Unmarshal(raw, &req)
		name, args := rpcCommand(req.Argv)
		if name == "_doc" && len(args) > 0 && args[0] == "put" {
			mu.Lock()
			events = append(events, "upload")
			mu.Unlock()
		}
		q.Body = io.NopCloser(bytes.NewReader(raw))
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	home := r.clientHome(url)
	env := uploadEnv(as(lane, launch), map[string]string{"HERDR_SOCKET_PATH": r.herdrSock})
	out := &uploadOrderWriter{mu: &mu, events: &events, step: "stdout"}
	var errOut bytes.Buffer
	r.caller.Store("host-a")
	code := cliMain([]string{"--json", "prompt", id(lane), "--file", path, "--receipt-timeout", "0"}, clientEnv(home, env), out, &errOut)
	if code != exitOK || errOut.Len() != 0 {
		t.Fatalf("prompt = %d stdout=%q stderr=%q", code, out.buf.String(), errOut.String())
	}
	mu.Lock()
	got := append([]string(nil), events...)
	mu.Unlock()
	if !reflect.DeepEqual(got, []string{"stdout", "upload"}) {
		t.Fatalf("prompt output/upload order = %v", got)
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

func TestClientBackfillUsesMissSourceHost(t *testing.T) {
	r := newTwoHost(t)
	r.beat("host-b", 0)
	root := r.newTask("backfill-root", "orchestrator", 0)
	path := docFile(t, r.dir, "caller-brief.md", "brief from caller")
	create := rpcBody(r.dir, nil, "backfill-source-new-01", "new", "client-brief-on-host-b", "--role", "implementer",
		"--parent", id(root), "--cwd", r.dir, "--machine", "host-b", "--brief", path)
	_, rep, _ := r.post("host-a", create)
	task := num(lastJSON(rep.Stdout), "task_id")
	if rep.Exit != exitOK || rep.Upload != nil {
		t.Fatalf("uncapable new = %+v", rep)
	}
	miss := docLatest(t, r.openDB(), task, "brief", "")
	if miss.Captured || miss.Reason.String != "client" || miss.Host.String != "host-a" {
		t.Fatalf("initial brief miss = %+v", miss)
	}
	wanted := func(caller, key string) []rpcDocWant {
		t.Helper()
		req := rpcBody(r.dir, nil, key, "_doc", "wanted", "--offset", "0")
		req.Capabilities = []string{docUploadCapability}
		_, reply, _ := r.post(caller, req)
		if reply.Exit != exitOK {
			t.Fatalf("wanted from %s = %+v", caller, reply)
		}
		var page struct {
			Documents []rpcDocWant `json:"documents"`
		}
		if err := json.Unmarshal([]byte(lastLine(reply.Stdout)), &page); err != nil {
			t.Fatal(err)
		}
		return page.Documents
	}
	if got := wanted("host-a", "backfill-wanted-a-01"); len(got) != 1 || got[0].Task != task || got[0].Path != path {
		t.Fatalf("caller-host wanted = %+v", got)
	}
	if got := wanted("host-b", "backfill-wanted-b-01"); len(got) != 0 {
		t.Fatalf("task-host wanted = %+v", got)
	}
	wrongHostPut := rpcBody(r.dir, nil, "backfill-put-wrong-host-01", "_doc", "put")
	wrongHostPut.Capabilities = []string{docUploadCapability}
	wrongHostPut.Document = clientDocPayload(rpcDocWant{Task: task, Kind: "brief", Path: path}, fileDocument(path, ""), true, false)
	before := docCount(t, r.openDB(), `select count(*) from documents where task_id = ? and kind = 'brief'`, task)
	if _, rep, _ := r.post("host-b", wrongHostPut); rep.Exit != exitReject {
		t.Fatalf("task-host backfill put = %+v", rep)
	}
	if got := docCount(t, r.openDB(), `select count(*) from documents where task_id = ? and kind = 'brief'`, task); got != before {
		t.Fatalf("wrong-host backfill put wrote a row: %d -> %d", before, got)
	}
	code, out, stderr := uploadClient(t, r, r.homes["host-a"], nil, "doc", "backfill")
	if code != exitOK || stderr != "" || num(lastJSON(out), "captured") != 1 {
		t.Fatalf("caller-host backfill = %d %q %q", code, out, stderr)
	}
	if d := docLatest(t, r.openDB(), task, "brief", ""); !d.Captured || d.Host.String != "host-a" || d.Backfill != 2 {
		t.Fatalf("caller-host backfill row = %+v", d)
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
