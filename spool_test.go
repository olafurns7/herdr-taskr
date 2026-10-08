package main

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func spoolClientHost(r *twoHost) string {
	for host := range r.homes {
		return host
	}
	return ""
}

func spoolDeadURL(t *testing.T, r *twoHost) string {
	t.Helper()
	url, ln, srv := retryEndpoint(t, r, func(http.ResponseWriter, *http.Request) {})
	_ = srv.Close()
	_ = ln.Close()
	return url
}

func spoolRunCLI(r *twoHost, host, home string, env map[string]string, args ...string) (int, string, string) {
	r.caller.Store(host)
	var out, errb bytes.Buffer
	code := contractCLIMain(r.t, args, clientEnv(home, env), &out, &errb)
	return code, out.String(), errb.String()
}

func spoolMakeWorker(t *testing.T, r *twoHost, host string, n int, extra ...string) (int64, int64, int64) {
	t.Helper()
	suffix := strconv.Itoa(n)
	top := num(r.want(0, host, nil, "new", "root-"+suffix, "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	wargs := []string{"new", "worker-" + suffix, "--role", "implementer", "--parent", id(top), "--cwd", t.TempDir(), "--pane", "pane-" + suffix}
	wargs = append(wargs, extra...)
	w := num(r.want(0, host, nil, wargs...), "task_id")
	l := num(r.want(0, host, nil, "launch", id(w), "--provider", "codex", "--model", "m", "--effort", "high"), "launch_id")
	return top, w, l
}

func spoolPrompt(t *testing.T, r *twoHost, task, launch int64) int64 {
	t.Helper()
	var attempt int64
	err := withTx(r.openDB(), func(tx *sql.Tx) error {
		var err error
		attempt, err = insertEvent(tx, event{TaskID: task, LaunchID: ptr(launch), Kind: "prompt", Summary: "prompt"})
		return err
	})
	if err != nil {
		t.Fatal(err)
	}
	return attempt
}

func spoolQueuePath(home string) string {
	return filepath.Join(home, ".local", "state", "taskr", spoolDirName, spoolQueueDir)
}

func spoolStateDir(home string) string {
	return filepath.Join(home, ".local", "state", "taskr")
}

func spoolOutputMap(out string) map[string]any {
	line := strings.TrimSpace(out)
	line = strings.TrimPrefix(line, "j1 ")
	var result map[string]any
	_ = json.Unmarshal([]byte(line), &result)
	return result
}

func TestSpoolRetryWindow(t *testing.T) {
	contractGuard(t)
	if got := rpcRetryWindow([]string{"got", "1"}); got != 3*time.Second {
		t.Fatalf("record retry window = %s, want 3s", got)
	}
	if got := rpcRetryWindow([]string{"start"}); got < 60*time.Second {
		t.Fatalf("non-record retry window = %s, want existing window", got)
	}
}

func TestSpoolRecordCommandsQueueAfterTransportWindow(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	deadURL := spoolDeadURL(t, r)
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	tests := []struct {
		name string
		args func(top, task, launch, attempt int64) []string
		env  func(top, task, launch int64) map[string]string
	}{
		{name: "got", args: func(_, _, _, attempt int64) []string { return []string{"got", id(attempt)} }, env: func(_, task, launch int64) map[string]string { return as(task, launch) }},
		{name: "ready", args: func(_, _, _, _ int64) []string { return []string{"ready", "ready"} }, env: func(_, task, launch int64) map[string]string { return as(task, launch) }},
		{name: "done", args: func(_, _, _, _ int64) []string { return []string{"done"} }, env: func(_, task, launch int64) map[string]string { return as(task, launch) }},
		{name: "fail", args: func(_, _, _, _ int64) []string { return []string{"fail", "reason"} }, env: func(_, task, launch int64) map[string]string { return as(task, launch) }},
		{name: "decide", args: func(top, _, _, _ int64) []string { return []string{"decide", "--as", id(top), "rule"} }, env: func(_, _, _ int64) map[string]string { return nil }},
		{name: "next", args: func(top, _, _, _ int64) []string { return []string{"next", id(top), "step"} }, env: func(_, _, _ int64) map[string]string { return nil }},
		{name: "note", args: func(top, _, _, _ int64) []string { return []string{"note", "note", "--as", id(top)} }, env: func(_, _, _ int64) map[string]string { return nil }},
		{name: "close", args: func(_, task, _, _ int64) []string { return []string{"close", id(task)} }, env: func(_, _, _ int64) map[string]string { return nil }},
	}
	for i, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			top, task, launch := spoolMakeWorker(t, r, host, i+1)
			attempt := int64(0)
			if tc.name == "got" {
				attempt = spoolPrompt(t, r, task, launch)
			}
			home := r.clientHome(deadURL)
			args := tc.args(top, task, launch, attempt)
			code, out, stderr := spoolRunCLI(r, host, home, tc.env(top, task, launch), args...)
			if code != exitOK || !strings.HasPrefix(out, "qd1 ") || strings.Count(out, "\n") != 1 ||
				stderr != "taskr: server unreachable; queued (1 waiting)\n" {
				t.Fatalf("queued %s = %d %q %q", tc.name, code, out, stderr)
			}
			key := strings.TrimSpace(strings.TrimPrefix(out, "qd1 "))
			if !requestKeyRe.MatchString(key) {
				t.Fatalf("queued key = %q", key)
			}
			files, err := readSpoolFiles(spoolQueuePath(home))
			if err != nil || len(files) != 1 {
				t.Fatalf("queue files = %d, %v", len(files), err)
			}
			record := files[0].record
			if record.Version != 1 || record.Seq != 1 || record.RequestKey != key || record.Request.RequestKey != key ||
				!validSpoolTime(record.QueuedAt) || record.Request.QueuedAt != "" ||
				hasCapability(record.Request.Capabilities, docUploadCapability) != rpcCarriesDocument(record.Request.Argv) {
				t.Fatalf("queued record = %+v", record)
			}
			info, err := os.Stat(files[0].path)
			if err != nil || info.Mode().Perm() != 0o600 {
				t.Fatalf("queue file mode = %v, %v", info, err)
			}
			b, err := os.ReadFile(files[0].path)
			if err != nil {
				t.Fatal(err)
			}
			var envelope map[string]any
			if json.Unmarshal(b, &envelope) != nil {
				t.Fatalf("queue file is not JSON: %s", b)
			}
			request := envelope["request"].(map[string]any)
			if _, hasHost := request["host"]; hasHost {
				t.Fatal("queued RPC body includes a host field")
			}
			if got, _ := rpcCommand(record.Request.Argv); got != tc.name {
				t.Fatalf("queued command = %q, want %q", got, tc.name)
			}
		})
	}
}

func TestSpoolHookQueuesSilently(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	deadURL := spoolDeadURL(t, r)
	_, task, launch := spoolMakeWorker(t, r, host, 20)
	home := r.clientHome(deadURL)
	env := as(task, launch)
	var out, errb bytes.Buffer
	c := &ctx{getenv: clientEnv(home, env), out: &out, errw: &errb, client: true, server: deadURL}
	sendHookRPC(c, hookRecord{Event: "session.idle", Session: "session", Transcript: filepath.Join(t.TempDir(), "trace.jsonl")}, time.Now())
	files, err := readSpoolFiles(spoolQueuePath(home))
	if err != nil || len(files) != 1 || out.Len() != 0 || errb.Len() != 0 {
		t.Fatalf("hook queue = %d, %v; output %q %q", len(files), err, out.String(), errb.String())
	}
	if command, _ := rpcCommand(files[0].record.Request.Argv); command != "_hook" {
		t.Fatalf("queued hook command = %q", command)
	}
}

func TestSpoolBadFilesQuarantineAndNotify(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top, task, launch := spoolMakeWorker(t, r, host, 22)
	home := r.clientHome(r.url)
	dir := spoolStateDir(home)
	queueDir := filepath.Join(spoolPath(dir), spoolQueueDir)
	queued := rpcClientRequest([]string{"--json", "note", "before hook", "--as", id(top)}, t.TempDir(), "bad-queue-note-1", nil, nil)
	if _, err := queueSpoolRecord(dir, queued, nil); err != nil {
		t.Fatal(err)
	}
	if err := ensureSpoolDirs(dir); err != nil {
		t.Fatal(err)
	}
	badDir := filepath.Join(spoolPath(dir), spoolBadDir)
	garbageName, foreignName := "000099-garbage.json", "000100-foreign.json"
	garbage, foreign := []byte("{truncated"), []byte(`{"v":2,"seq":100,"request_key":"foreign-key-100","request":{"request_key":"foreign-key-100"},"queued_at":"2026-10-04T00:00:00Z"}`)
	for name, body := range map[string][]byte{garbageName: garbage, foreignName: foreign} {
		if err := os.WriteFile(filepath.Join(queueDir, name), body, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	env := as(task, launch)
	c := &ctx{getenv: clientEnv(home, env), client: true, server: r.url}
	sendHookRPC(c, hookRecord{Event: "session.idle", Session: "session", Transcript: filepath.Join(t.TempDir(), "trace.jsonl")}, time.Now())
	queuedFiles, err := readSpoolEntries(queueDir)
	if err != nil || len(queuedFiles) != 4 || queuedFiles[1].parseErr == "" || queuedFiles[2].parseErr == "" {
		t.Fatalf("hook did not queue behind the note: files=%+v err=%v", queuedFiles, err)
	}
	if command, _ := rpcCommand(queuedFiles[0].record.Request.Argv); command != "note" {
		t.Fatalf("queue head = %q", command)
	}
	if queuedFiles[3].parseErr != "" {
		t.Fatalf("hook queue tail is invalid: %+v", queuedFiles[3])
	}
	if command, _ := rpcCommand(queuedFiles[3].record.Request.Argv); command != "_hook" {
		t.Fatalf("hook queue tail = %q", command)
	}
	r.caller.Store(host)
	code, out, stderr := spoolRunCLI(r, host, home, nil, "--json", "spool", "send")
	if code != exitOK || stderr != "" || num(spoolOutputMap(out), "sent") != 2 || countSpoolFiles(queueDir) != 0 {
		t.Fatalf("send after quarantine = %d %q %q; queued %d", code, out, stderr, countSpoolFiles(queueDir))
	}
	if got := r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top); got != 1 {
		t.Fatalf("queued note events = %d", got)
	}
	for name, want := range map[string][]byte{garbageName: garbage, foreignName: foreign} {
		got, err := os.ReadFile(filepath.Join(badDir, name))
		if err != nil || !bytes.Equal(got, want) {
			t.Fatalf("bad file %s changed: %q, %v", name, got, err)
		}
	}
	code, out, stderr = spoolRunCLI(r, host, home, nil, "--json", "spool", "ls")
	listing := lastJSON(out)
	bad, _ := listing["bad"].([]any)
	if code != exitOK || stderr != "" || len(bad) != 2 {
		t.Fatalf("spool ls with bad files = %d %v %q", code, listing, stderr)
	}
	for _, raw := range bad {
		item := raw.(map[string]any)
		if item["kind"] != "bad" || item["error"] == "" {
			t.Fatalf("bad listing item = %v", item)
		}
	}
	statusCode, statusOut, statusErr := spoolRunCLI(r, host, home, nil, "--json", "daemon", "--status")
	status := lastJSON(statusOut)
	counts, _ := status["spool"].(map[string]any)
	if statusCode != exitOK || statusErr != "" || num(counts, "bad") != 2 {
		t.Fatalf("daemon status bad count = %d %v %q", statusCode, status, statusErr)
	}
	for i := 0; i < 2; i++ {
		code, _, stderr = spoolRunCLI(r, host, home, map[string]string{"HERDR_SOCKET_PATH": r.herdrSock}, "daemon", "--once")
		if code != exitOK || stderr != "" {
			t.Fatalf("daemon pass = %d %q", code, stderr)
		}
	}
	if calls := r.calls("notification|"); len(calls) != 2 {
		t.Fatalf("bad file notifications = %q", calls)
	}
	for _, name := range []string{garbageName, foreignName} {
		info, err := os.Stat(filepath.Join(badDir, name) + ".shown")
		if err != nil || info.Size() != 0 {
			t.Fatalf("bad notification marker %s = %v, %v", name, info, err)
		}
	}
	code, _, stderr = spoolRunCLI(r, host, home, nil, "spool", "rm", "99")
	if code != exitOK || stderr != "" {
		t.Fatalf("rm malformed bad file = %d %q", code, stderr)
	}
	if _, err := os.Stat(filepath.Join(badDir, garbageName)); !os.IsNotExist(err) {
		t.Fatalf("rm did not remove bad file without parsing: %v", err)
	}
	gotForeign, err := os.ReadFile(filepath.Join(badDir, foreignName))
	if err != nil || !bytes.Equal(gotForeign, foreign) {
		t.Fatalf("foreign file changed after rm: %q, %v", gotForeign, err)
	}
}

func TestSpoolStallAgeUsesClientClock(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	oldClock := spoolNow
	defer func() { spoolNow = oldClock }()
	for _, tc := range []struct {
		name   string
		offset time.Duration
		age    time.Duration
		events int
	}{
		{name: "client behind, fresh stall", offset: -15 * time.Minute, age: time.Minute, events: 1},
		{name: "client ahead, expired stall", offset: 15 * time.Minute, age: 20 * time.Minute, events: 0},
	} {
		t.Run(tc.name, func(t *testing.T) {
			workerNum := int(tc.age.Minutes() + 90)
			_, task, launch := spoolMakeWorker(t, r, host, workerNum)
			spoolPrompt(t, r, task, launch)
			path := filepath.Join(t.TempDir(), "trace.jsonl")
			if _, err := r.openDB().Exec(`update launches set session_ref = ?, session_source = ?, transcript_path = ? where id = ?`,
				"session", "hook:SessionStart", path, launch); err != nil {
				t.Fatal(err)
			}
			fakeNow := time.Now().UTC().Truncate(time.Second).Add(tc.offset)
			spoolNow = func() time.Time { return fakeNow }
			queuedAt := fakeNow.Add(-tc.age).Format(time.RFC3339)
			env := as(task, launch)
			env["HERDR_PANE_ID"] = fmt.Sprintf("pane-%d", workerNum)
			key := fmt.Sprintf("client-clock-record-%03d", int(tc.age.Minutes()))
			req := rpcClientRequest([]string{"--json", "_hook", "session.idle", "--session", "session", "--transcript", path},
				t.TempDir(), key, env, nil)
			dir := t.TempDir()
			if err := ensureSpoolDirs(dir); err != nil {
				t.Fatal(err)
			}
			record := spoolRecord{Version: 1, Seq: 1, RequestKey: key, Request: req, QueuedAt: queuedAt}
			if err := writeSpoolAtomic(filepath.Join(spoolPath(dir), spoolQueueDir), "000001-"+key+".json", record); err != nil {
				t.Fatal(err)
			}
			r.caller.Store(host)
			sent, err := sendSpool(dir, r.url, nil)
			got := r.count(`select count(*) from events where kind = 'herdr' and task_id = ?`, task)
			if err != nil || got != tc.events || (tc.events == 1 && sent != 1) || countSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir)) != 0 {
				t.Fatalf("send=%d events=%d queued=%d err=%v", sent, got, countSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir)), err)
			}
		})
	}
}

func TestSpoolQueuedBehindSaysWhy(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(exitOK, host, nil, "new", "root-queue-reason", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	home := r.clientHome(r.url)
	first := rpcClientRequest([]string{"note", "first", "--as", id(top)}, t.TempDir(), "queue-reason-first-1", nil, nil)
	if _, err := queueSpoolRecord(spoolStateDir(home), first, nil); err != nil {
		t.Fatal(err)
	}
	requests := r.count(`select count(*) from requests`)
	code, out, stderr := spoolRunCLI(r, host, home, nil, "note", "second", "--as", id(top))
	if code != exitOK || !strings.HasPrefix(out, "qd1 ") || stderr != "taskr: earlier records wait in the spool; queued (2 waiting)\n" {
		t.Fatalf("queued behind = %d %q %q", code, out, stderr)
	}
	if r.count(`select count(*) from requests`) != requests {
		t.Fatal("record reached the server ahead of the spool")
	}
	files, err := readSpoolFiles(spoolQueuePath(home))
	if err != nil || len(files) != 2 || files[0].record.Request.Argv[1] != "first" || files[1].record.Request.Argv[1] != "second" {
		t.Fatalf("queued order = %+v, %v", files, err)
	}
}

func TestSpoolRecordQueuesWhileSenderOwnsSendLock(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-send-lock", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	home := r.clientHome(r.url)
	dir := spoolStateDir(home)
	first := rpcClientRequest([]string{"--json", "note", "first", "--as", id(top)}, t.TempDir(), "send-lock-first-1", nil, nil)
	if _, err := queueSpoolRecord(dir, first, nil); err != nil {
		t.Fatal(err)
	}
	sendLock, ok, err := lockSpoolSender(dir)
	if err != nil || !ok {
		t.Fatalf("take send lock = %v, %v", ok, err)
	}
	defer unlockSpool(sendLock)
	started := time.Now()
	code, _, stderr := spoolRunCLI(r, host, home, nil, "note", "second", "--as", id(top))
	if elapsed := time.Since(started); code != exitOK || stderr != "taskr: earlier records wait in the spool; queued (2 waiting)\n" || elapsed > 250*time.Millisecond {
		t.Fatalf("record while sender locked = %d after %s, %q", code, elapsed, stderr)
	}
	files, err := readSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir))
	if err != nil || len(files) != 2 {
		t.Fatalf("queue while sender locked = %+v, %v", files, err)
	}
}

func TestSpoolHookQueueLockPollingAndDirectFallback(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top, task, launch := spoolMakeWorker(t, r, host, 23)
	queuedHome := r.clientHome(r.url)
	queuedDir := spoolStateDir(queuedHome)
	queuedReq := rpcClientRequest([]string{"--json", "note", "before hook", "--as", id(top)}, t.TempDir(), "hook-lock-note-1", nil, nil)
	if _, err := queueSpoolRecord(queuedDir, queuedReq, nil); err != nil {
		t.Fatal(err)
	}
	queueLock, ok, err := lockSpool(queuedDir, true)
	if err != nil || !ok {
		t.Fatalf("take queue lock = %v, %v", ok, err)
	}
	released := make(chan struct{})
	go func() {
		time.Sleep(100 * time.Millisecond)
		unlockSpool(queueLock)
		close(released)
	}()
	c := &ctx{getenv: clientEnv(queuedHome, as(task, launch)), client: true, server: r.url}
	sendHookRPC(c, hookRecord{Event: "session.idle", Session: "session", Transcript: filepath.Join(t.TempDir(), "queued.jsonl")}, time.Now())
	<-released
	files, err := readSpoolFiles(filepath.Join(spoolPath(queuedDir), spoolQueueDir))
	if err != nil || len(files) != 2 {
		t.Fatalf("hook did not queue after lock release: %+v, %v", files, err)
	}

	var calls atomic.Int32
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		calls.Add(1)
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	directHome := r.clientHome(url)
	directDir := spoolStateDir(directHome)
	if err := ensureSpoolDirs(directDir); err != nil {
		t.Fatal(err)
	}
	queueLock, ok, err = lockSpool(directDir, true)
	if err != nil || !ok {
		t.Fatalf("take empty queue lock = %v, %v", ok, err)
	}
	released = make(chan struct{})
	go func() {
		time.Sleep(hookBusyTimeout + 50*time.Millisecond)
		unlockSpool(queueLock)
		close(released)
	}()
	c = &ctx{getenv: clientEnv(directHome, as(task, launch)), client: true, server: url}
	r.caller.Store(host)
	sendHookRPC(c, hookRecord{Event: "session.idle", Session: "session", Transcript: filepath.Join(t.TempDir(), "direct.jsonl")}, time.Now())
	<-released
	if calls.Load() != 1 || countSpoolFiles(filepath.Join(spoolPath(directDir), spoolQueueDir)) != 0 {
		t.Fatalf("hook direct fallback = calls %d, queued %d", calls.Load(), countSpoolFiles(filepath.Join(spoolPath(directDir), spoolQueueDir)))
	}
}

func TestSpoolConcurrentSendersUseSendLock(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-two-senders", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	dir := t.TempDir()
	req := rpcClientRequest([]string{"--json", "note", "one sender", "--as", id(top)}, t.TempDir(), "one-sender-record-1", nil, nil)
	if _, err := queueSpoolRecord(dir, req, nil); err != nil {
		t.Fatal(err)
	}
	entered, release := make(chan struct{}), make(chan struct{})
	var calls atomic.Int32
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		if calls.Add(1) == 1 {
			close(entered)
			<-release
		}
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	r.caller.Store(host)
	first := make(chan struct {
		sent int
		err  error
	}, 1)
	go func() {
		sent, err := sendSpool(dir, url, nil)
		first <- struct {
			sent int
			err  error
		}{sent, err}
	}()
	select {
	case <-entered:
	case <-time.After(2 * time.Second):
		close(release)
		t.Fatal("first sender did not reach the server")
	}
	secondSent, secondErr := sendSpool(dir, url, nil)
	if secondErr != nil || secondSent != 0 || calls.Load() != 1 {
		close(release)
		t.Fatalf("second sender = %d, %v, server calls %d", secondSent, secondErr, calls.Load())
	}
	close(release)
	result := <-first
	if result.err != nil || result.sent != 1 || calls.Load() != 1 {
		t.Fatalf("first sender = %d, %v, server calls %d", result.sent, result.err, calls.Load())
	}
}

func TestSpoolHookNeverAnswerProcessHelper(t *testing.T) {
	contractGuard(t)
	if os.Getenv("TASKR_SPOOL_HOOK_HELPER") != "1" {
		return
	}
	fakeTailnetHooks(t)
	stateDir := filepath.Join(os.Getenv("HOME"), ".local", "state", "taskr")
	raw, found, err := readServerURL(stateDir)
	if err != nil || !found {
		_, statErr := os.Stat(filepath.Join(stateDir, serverURLFile))
		t.Fatalf("hook helper server URL: HOME=%q found=%v err=%v stat=%v", os.Getenv("HOME"), found, err, statErr)
	}
	if _, err := newRPCClient(raw); err != nil {
		t.Fatalf("hook helper RPC client: %v", err)
	}
	var out, errb bytes.Buffer
	code := contractCLIMain(t, []string{"hook", "claude", "Stop"}, os.Getenv, &out, &errb)
	if code != exitOK || out.Len() != 0 || errb.Len() != 0 {
		t.Fatalf("hook helper = %d %q %q", code, out.String(), errb.String())
	}
}

func TestSpoolHookTimeoutWritesBeforeProcessExit(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	_, task, launch := spoolMakeWorker(t, r, host, 24)
	never, entered := make(chan struct{}), make(chan struct{}, 1)
	url, ln, srv := retryEndpoint(t, r, func(http.ResponseWriter, *http.Request) {
		entered <- struct{}{}
		<-never
	})
	go srv.Serve(ln)
	defer func() { close(never); _ = srv.Close() }()
	home := r.clientHome(url)
	if _, found, err := readServerURL(spoolStateDir(home)); err != nil || !found {
		t.Fatalf("test server URL not installed: found=%v err=%v", found, err)
	}
	cmd := exec.Command(os.Args[0], "-test.run=^TestSpoolHookNeverAnswerProcessHelper$")
	cmd.Env = []string{
		"TASKR_SPOOL_HOOK_HELPER=1",
		"HOME=" + home,
		"TASKR_DB=",
		"TASKR_TASK=" + id(task),
		"TASKR_LAUNCH=" + id(launch),
		"HERDR_ENV=1",
		"HERDR_PANE_ID=test-pane",
		"GOTELEMETRY=off",
		"PATH=" + r.bin + string(os.PathListSeparator) + os.Getenv("PATH"),
	}
	payload, err := os.ReadFile(filepath.Join("testdata", "hooks", "claude-stop.json"))
	if err != nil {
		t.Fatal(err)
	}
	cmd.Stdin = bytes.NewReader(payload)
	var out, errb bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &errb
	if err := cmd.Run(); err != nil || errb.Len() != 0 {
		t.Fatalf("hook process = %v, stdout %q, stderr %q", err, out.String(), errb.String())
	}
	files, err := readSpoolFiles(spoolQueuePath(home))
	if err != nil || len(files) != 1 {
		t.Fatalf("process exited without queued hook: %+v, %v", files, err)
	}
	// On a loaded host the 350 ms send budget can end before the request
	// connects. The record is queued either way, but the unanswered path
	// was not exercised, so say so instead of passing.
	select {
	case <-entered:
	default:
		t.Skip("queued before exit; the unanswered-request path was not exercised")
	}
}

func TestSpoolHookLargeQueueWritesBeforeExit(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	_, task, launch := spoolMakeWorker(t, r, host, 25)
	never, entered := make(chan struct{}), make(chan struct{}, 1)
	url, ln, srv := retryEndpoint(t, r, func(http.ResponseWriter, *http.Request) {
		entered <- struct{}{}
		<-never
	})
	go srv.Serve(ln)
	defer func() { close(never); _ = srv.Close() }()
	home := r.clientHome(url)
	dir := spoolStateDir(home)
	if err := ensureSpoolDirs(dir); err != nil {
		t.Fatal(err)
	}
	queueDir := filepath.Join(spoolPath(dir), spoolQueueDir)
	large := bytes.Repeat([]byte("x"), 40<<10)
	for seq := 1; seq < spoolFileMax; seq++ {
		name := fmt.Sprintf("large-key-%04d", seq)
		path := filepath.Join(queueDir, fmt.Sprintf("%06d-%s.json", seq, name))
		if err := os.WriteFile(path, large, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	cmd := exec.Command(os.Args[0], "-test.run=^TestSpoolHookNeverAnswerProcessHelper$")
	cmd.Env = []string{
		"TASKR_SPOOL_HOOK_HELPER=1",
		"HOME=" + home,
		"TASKR_DB=",
		"TASKR_TASK=" + id(task),
		"TASKR_LAUNCH=" + id(launch),
		"HERDR_ENV=1",
		"HERDR_PANE_ID=test-pane",
		"GOTELEMETRY=off",
		"PATH=" + r.bin + string(os.PathListSeparator) + os.Getenv("PATH"),
	}
	payload, err := os.ReadFile(filepath.Join("testdata", "hooks", "claude-stop.json"))
	if err != nil {
		t.Fatal(err)
	}
	cmd.Stdin = bytes.NewReader(payload)
	var out, errb bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &errb
	if err := cmd.Run(); err != nil || errb.Len() != 0 {
		t.Fatalf("large-queue hook process = %v, stdout %q, stderr %q", err, out.String(), errb.String())
	}
	select {
	case <-entered:
		t.Fatal("hook parsed or sent past the existing queue")
	default:
	}
	entries, err := os.ReadDir(queueDir)
	if err != nil || len(entries) != spoolFileMax || countSpoolFiles(filepath.Join(spoolPath(dir), spoolBadDir)) != 0 {
		t.Fatalf("large-queue hook left %d files, bad=%d, err=%v", len(entries), countSpoolFiles(filepath.Join(spoolPath(dir), spoolBadDir)), err)
	}
	added := false
	for _, entry := range entries {
		if !strings.HasPrefix(entry.Name(), "large-key-") {
			added = true
			break
		}
	}
	if !added {
		t.Fatal("hook record was not written behind the large queue")
	}
}

func TestSpoolOutcomeUnknownKeepsQueue(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-still-running", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	dir := t.TempDir()
	first := rpcClientRequest([]string{"--json", "note", "first", "--as", id(top)}, t.TempDir(), "still-running-record-1", nil, nil)
	second := rpcClientRequest([]string{"--json", "note", "second", "--as", id(top)}, t.TempDir(), "still-running-record-2", nil, nil)
	for _, req := range []rpcRequest{first, second} {
		if _, err := queueSpoolRecord(dir, req, nil); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := r.openDB().Exec(`insert into requests (key, machine, argv_sha, state, created_at) values (?, ?, ?, 'running', ?)`,
		first.RequestKey, host, rpcRequestSHA(first), now()); err != nil {
		t.Fatal(err)
	}
	r.caller.Store(host)
	sent, err := sendSpool(dir, r.url, nil)
	files, readErr := readSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir))
	if err != nil || sent != 0 || readErr != nil || len(files) != 2 || countSpoolFiles(filepath.Join(spoolPath(dir), spoolFailDir)) != 0 {
		t.Fatalf("still-running send = sent %d files %+v errors %v / %v", sent, files, err, readErr)
	}
	if got := r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top); got != 0 {
		t.Fatalf("later queued record ran after unknown outcome: %d", got)
	}
}

func TestSpoolOutcomeUnknownStaysStuckThenRefuses(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-stuck-expire", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	oldClock := spoolNow
	defer func() { spoolNow = oldClock }()
	started := time.Now().UTC().Truncate(time.Second)
	spoolNow = func() time.Time { return started }
	home := r.clientHome(r.url)
	dir := spoolStateDir(home)
	first := rpcClientRequest([]string{"--json", "note", "stuck first", "--as", id(top)}, t.TempDir(), "stuck-first-note-01", nil, nil)
	second := rpcClientRequest([]string{"--json", "note", "stuck second", "--as", id(top)}, t.TempDir(), "stuck-second-note-2", nil, nil)
	for _, req := range []rpcRequest{first, second} {
		if _, err := queueSpoolRecord(dir, req, nil); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := r.openDB().Exec(`insert into requests (key, machine, argv_sha, state, created_at) values (?, ?, ?, 'running', ?)`,
		first.RequestKey, host, rpcRequestSHA(first), now()); err != nil {
		t.Fatal(err)
	}
	r.caller.Store(host)
	for _, advance := range []time.Duration{0, 5 * time.Minute} {
		started = time.Now().UTC().Truncate(time.Second).Add(advance)
		sent, err := sendSpool(dir, r.url, nil)
		if err != nil || sent != 0 {
			t.Fatalf("stuck pass at %s = sent %d, err %v", advance, sent, err)
		}
	}
	files, err := readSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir))
	if err != nil || len(files) != 2 || files[0].record.StuckSince == "" ||
		files[0].record.StuckReason != spoolOutcomeUnknownReason {
		t.Fatalf("stuck queue head = %+v, %v", files, err)
	}
	code, out, stderr := spoolRunCLI(r, host, home, nil, "--json", "spool", "ls")
	listing := spoolOutputMap(out)
	queued, _ := listing["queued"].([]any)
	if code != exitOK || stderr != "" || len(queued) != 2 || queued[0].(map[string]any)["stuck_since"] == "" ||
		queued[0].(map[string]any)["stuck_reason"] != spoolOutcomeUnknownReason {
		t.Fatalf("stuck spool ls = %d %v %q", code, listing, stderr)
	}
	code, out, stderr = spoolRunCLI(r, host, home, nil, "--json", "daemon", "--status")
	status := spoolOutputMap(out)
	counts, _ := status["spool"].(map[string]any)
	if code != exitOK || stderr != "" || counts["stuck"] != true {
		t.Fatalf("stuck daemon status = %d %v %q", code, status, stderr)
	}
	started = started.Truncate(time.Second).Add(5 * time.Minute)
	sent, err := sendSpool(dir, r.url, nil)
	queuedCount, refusedCount, _ := spoolCounts(dir)
	if err != nil || sent != 1 || queuedCount != 0 || refusedCount != 1 {
		t.Fatalf("expired outcome unknown = sent %d, queue %d, refused %d, err %v", sent, queuedCount, refusedCount, err)
	}
	failed, err := readSpoolFiles(filepath.Join(spoolPath(dir), spoolFailDir))
	if err != nil || len(failed) != 1 || failed[0].record.Error != spoolOutcomeUnknownReason {
		t.Fatalf("refused stuck head = %+v, %v", failed, err)
	}
	if got := r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top); got != 1 {
		t.Fatalf("note behind refused head was not sent: %d", got)
	}
	notifySpoolRefused(dir, r.herdrSock, nil)
	notifySpoolRefused(dir, r.herdrSock, nil)
	if calls := r.calls("notification|"); len(calls) != 1 || !strings.Contains(calls[0], "outcome unknown: look for the record") {
		t.Fatalf("outcome unknown notifications = %q", calls)
	}
}

func TestSpoolOutcomeUnknownStoredResultClearsQueue(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-stuck-resolved", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	oldClock := spoolNow
	defer func() { spoolNow = oldClock }()
	started := time.Now().UTC().Truncate(time.Second)
	spoolNow = func() time.Time { return started }
	dir := t.TempDir()
	first := rpcClientRequest([]string{"--json", "note", "resolved first", "--as", id(top)}, t.TempDir(), "resolved-first-note-1", nil, nil)
	second := rpcClientRequest([]string{"--json", "note", "resolved second", "--as", id(top)}, t.TempDir(), "resolved-second-note-2", nil, nil)
	for _, req := range []rpcRequest{first, second} {
		if _, err := queueSpoolRecord(dir, req, nil); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := r.openDB().Exec(`insert into requests (key, machine, argv_sha, state, created_at) values (?, ?, ?, 'running', ?)`,
		first.RequestKey, host, rpcRequestSHA(first), now()); err != nil {
		t.Fatal(err)
	}
	r.caller.Store(host)
	if sent, err := sendSpool(dir, r.url, nil); err != nil || sent != 0 {
		t.Fatalf("initial unknown reply = sent %d, err %v", sent, err)
	}
	if _, err := r.openDB().Exec(`update requests set state = 'done', exit = 0, stdout = '', stderr = '', upload = 'null' where key = ?`, first.RequestKey); err != nil {
		t.Fatal(err)
	}
	started = started.Add(10 * time.Minute)
	sent, err := sendSpool(dir, r.url, nil)
	queued, refused, _ := spoolCounts(dir)
	if err != nil || sent != 2 || queued != 0 || refused != 0 {
		t.Fatalf("stored result after stuck window = sent %d, queued %d, refused %d, err %v", sent, queued, refused, err)
	}
	if got := r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top); got != 1 {
		t.Fatalf("note behind stored result was not sent: %d", got)
	}
}

func TestSpoolForbiddenHeadStaysQueuedAndNotifiesOnce(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-forbidden-stuck", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	oldClock := spoolNow
	defer func() { spoolNow = oldClock }()
	started := time.Now().UTC().Truncate(time.Second)
	spoolNow = func() time.Time { return started }
	first := rpcClientRequest([]string{"--json", "note", "forbidden first", "--as", id(top)}, t.TempDir(), "forbidden-first-01", nil, nil)
	second := rpcClientRequest([]string{"--json", "note", "forbidden second", "--as", id(top)}, t.TempDir(), "forbidden-second-2", nil, nil)
	dir := t.TempDir()
	for _, req := range []rpcRequest{first, second} {
		if _, err := queueSpoolRecord(dir, req, nil); err != nil {
			t.Fatal(err)
		}
	}
	var forbidden atomic.Bool
	forbidden.Store(true)
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		if forbidden.Load() {
			http.Error(w, "token rejected", http.StatusForbidden)
			return
		}
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	defer srv.Close()
	r.caller.Store(host)
	for _, advance := range []time.Duration{0, 30 * time.Minute} {
		started = time.Now().UTC().Truncate(time.Second).Add(advance)
		sent, err := sendSpool(dir, url, nil)
		queued, refused, _ := spoolCounts(dir)
		if err != nil || sent != 0 || queued != 2 || refused != 0 {
			t.Fatalf("403 pass at %s = sent %d, queued %d, refused %d, err %v", advance, sent, queued, refused, err)
		}
	}
	files, err := readSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir))
	if err != nil || len(files) != 2 || files[0].record.StuckSince == "" || files[0].record.StuckReason != "server answered 403: token rejected" {
		t.Fatalf("403 stuck head = %+v, %v", files, err)
	}
	if got := r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top); got != 0 {
		t.Fatalf("queue advanced during 403: %d", got)
	}
	notifySpoolStuck(dir, r.herdrSock, nil)
	notifySpoolStuck(dir, r.herdrSock, nil)
	files, err = readSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir))
	if err != nil || len(files) != 2 || !files[0].record.StuckShown {
		t.Fatalf("403 notification marker = %+v, %v", files, err)
	}
	if calls := r.calls("notification|"); len(calls) != 1 || !strings.Contains(calls[0], "server answered 403") {
		t.Fatalf("403 stuck notifications = %q", calls)
	}
	forbidden.Store(false)
	started = started.Add(time.Minute)
	sent, err := sendSpool(dir, url, nil)
	queued, refused, _ := spoolCounts(dir)
	if err != nil || sent != 2 || queued != 0 || refused != 0 {
		t.Fatalf("queue after 403 ends = sent %d, queued %d, refused %d, err %v", sent, queued, refused, err)
	}
	rows, err := r.openDB().Query(`select summary from events where task_id = ? and kind = 'note' order by id`, top)
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()
	var summaries []string
	for rows.Next() {
		var summary string
		if err := rows.Scan(&summary); err != nil {
			t.Fatal(err)
		}
		summaries = append(summaries, summary)
	}
	if fmt.Sprint(summaries) != "[forbidden first forbidden second]" {
		t.Fatalf("queue order after 403 = %v", summaries)
	}
}

func TestSpoolTransportFailureNeverStartsStuckClock(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-transport-clock", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	oldClock := spoolNow
	defer func() { spoolNow = oldClock }()
	started := time.Now().UTC().Truncate(time.Second)
	spoolNow = func() time.Time { return started }
	req := rpcClientRequest([]string{"--json", "note", "transport", "--as", id(top)}, t.TempDir(), "transport-clock-01", nil, nil)
	dir := t.TempDir()
	if _, err := queueSpoolRecord(dir, req, nil); err != nil {
		t.Fatal(err)
	}
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, _ *http.Request) { cutRetryReply(w) })
	go srv.Serve(ln)
	defer srv.Close()
	r.caller.Store(host)
	for _, advance := range []time.Duration{0, 30 * time.Minute} {
		started = time.Now().UTC().Truncate(time.Second).Add(advance)
		if sent, err := sendSpool(dir, url, nil); err != nil || sent != 0 {
			t.Fatalf("transport pass at %s = sent %d, err %v", advance, sent, err)
		}
		files, err := readSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir))
		if err != nil || len(files) != 1 || files[0].record.StuckSince != "" || files[0].record.StuckReason != "" {
			t.Fatalf("transport started stuck clock at %s: %+v, %v", advance, files, err)
		}
	}
}

func TestSpoolNonRecordAndServerRefusalDoNotQueue(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	deadURL := spoolDeadURL(t, r)
	_, task, launch := spoolMakeWorker(t, r, host, 21)
	home := r.clientHome(deadURL)
	retryWindow := rpcRetryWindow
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	code, _, stderr := spoolRunCLI(r, host, home, as(task, launch), "start")
	if code != exitHerdr || !strings.Contains(stderr, "retry with:") || !strings.Contains(stderr, "server unreachable") {
		t.Fatalf("non-record outage = %d %q", code, stderr)
	}
	if _, err := os.Stat(filepath.Join(spoolStateDir(home), spoolDirName)); !os.IsNotExist(err) {
		t.Fatalf("non-record command created spool path: %v", err)
	}

	rpcRetryWindow = retryWindow
	live := r.clientHome(r.url)
	code, out, _ := spoolRunCLI(r, host, live, as(task, launch), "got", "999999999")
	if code != exitReject || !strings.Contains(out, "x1 6 ") {
		t.Fatalf("live refusal = %d %q", code, out)
	}
	if _, err := os.Stat(filepath.Join(spoolStateDir(live), spoolDirName)); !os.IsNotExist(err) {
		t.Fatalf("server refusal created spool path: %v", err)
	}
}

func TestSpoolQueuedOrderAndManualSend(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	deadURL := spoolDeadURL(t, r)
	_, task, launch := spoolMakeWorker(t, r, host, 30)
	attempt := spoolPrompt(t, r, task, launch)
	home := r.clientHome(deadURL)
	env := as(task, launch)
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	code, _, stderr := spoolRunCLI(r, host, home, env, "got", id(attempt))
	if code != exitOK || !strings.Contains(stderr, "1 waiting") {
		t.Fatalf("first queued record = %d %q", code, stderr)
	}
	if err := os.WriteFile(filepath.Join(spoolStateDir(home), serverURLFile), []byte(r.url+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	code, out, stderr := spoolRunCLI(r, host, home, env, "done")
	if code != exitOK || !strings.HasPrefix(out, "qd1 ") || stderr != "taskr: earlier records wait in the spool; queued (2 waiting)\n" {
		t.Fatalf("queued done behind got = %d %q %q", code, out, stderr)
	}
	files, err := readSpoolFiles(spoolQueuePath(home))
	if err != nil || len(files) != 2 || files[0].record.Seq >= files[1].record.Seq {
		t.Fatalf("queued order = %+v, %v", files, err)
	}
	if r.count(`select count(*) from events where task_id = ? and kind in ('got', 'done')`, task) != 0 {
		t.Fatal("new done reached the server ahead of the queued got")
	}
	code, out, stderr = spoolRunCLI(r, host, home, nil, "spool", "send")
	if code != exitOK || stderr != "" || num(spoolOutputMap(out), "sent") != 2 || num(spoolOutputMap(out), "queued") != 0 {
		t.Fatalf("manual spool send = %d %q %q", code, out, stderr)
	}
	rows, err := r.openDB().Query(`select kind from events where task_id = ? and kind in ('got', 'done') order by id`, task)
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()
	var kinds []string
	for rows.Next() {
		var kind string
		if err := rows.Scan(&kind); err != nil {
			t.Fatal(err)
		}
		kinds = append(kinds, kind)
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	if fmt.Sprint(kinds) != "[got done]" {
		t.Fatalf("delivered event order = %v", kinds)
	}
}

func TestSpoolAppliedRequestIsNotAppliedTwice(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-applied", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	var acceptReply atomic.Bool
	var calls atomic.Int32
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		calls.Add(1)
		if !acceptReply.Load() {
			body, _ := io.ReadAll(q.Body)
			q.Body = io.NopCloser(bytes.NewReader(body))
			rec := httptest.NewRecorder()
			r.d.ServeHTTP(rec, q)
			cutRetryReply(w)
			return
		}
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	home := r.clientHome(url)
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	code, out, stderr := spoolRunCLI(r, host, home, nil, "note", "once", "--as", id(top))
	if code != exitOK || !strings.HasPrefix(out, "qd1 ") || !strings.Contains(stderr, "queued (1 waiting)") ||
		r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top) != 1 {
		t.Fatalf("lost reply queue = %d %q %q; calls=%d", code, out, stderr, calls.Load())
	}
	acceptReply.Store(true)
	sent, err := sendSpool(spoolStateDir(home), url, nil)
	if err != nil || sent != 1 || r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top) != 1 {
		t.Fatalf("replay applied twice: sent=%d err=%v", sent, err)
	}
}

func TestSpoolTransportErrorStopsInSequence(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-middle", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	var calls atomic.Int32
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		switch calls.Add(1) {
		case 2:
			cutRetryReply(w)
		default:
			r.d.ServeHTTP(w, q)
		}
	})
	go srv.Serve(ln)
	dir := t.TempDir()
	for i, text := range []string{"one", "two", "three"} {
		key := "spool-note-" + text
		req := rpcClientRequest([]string{"--json", "note", text, "--as", id(top)}, t.TempDir(), key, nil, nil)
		if _, err := queueSpoolRecord(dir, req, nil); err != nil {
			t.Fatal(err)
		}
		if i > 0 && key == "" {
			t.Fatal("empty key")
		}
	}
	r.caller.Store(host)
	sent, err := sendSpool(dir, url, nil)
	files, readErr := readSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir))
	if err != nil || sent != 1 || readErr != nil || len(files) != 2 || files[0].record.Seq != 2 || files[1].record.Seq != 3 {
		t.Fatalf("first send = sent %d, files %+v, errors %v / %v", sent, files, err, readErr)
	}
	if got := r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top); got != 1 {
		t.Fatalf("events after interrupted send = %d", got)
	}
	sent, err = sendSpool(dir, url, nil)
	if err != nil || sent != 2 || countSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir)) != 0 {
		t.Fatalf("second send = sent %d, err %v", sent, err)
	}
	rows, err := r.openDB().Query(`select summary from events where task_id = ? and kind = 'note' order by id`, top)
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()
	var summaries []string
	for rows.Next() {
		var summary string
		if err := rows.Scan(&summary); err != nil {
			t.Fatal(err)
		}
		summaries = append(summaries, summary)
	}
	if fmt.Sprint(summaries) != "[one two three]" {
		t.Fatalf("resumed order = %v", summaries)
	}
}

func TestSpoolRefusalMovesOnAndNotifiesOnce(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-refusal", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	dir := t.TempDir()
	bad := rpcClientRequest([]string{"--json", "done"}, t.TempDir(), "refused-record-0001", map[string]string{"TASKR_TASK": "999999999"}, nil)
	good := rpcClientRequest([]string{"--json", "note", "after", "--as", id(top)}, t.TempDir(), "valid-record-00002", nil, nil)
	if _, err := queueSpoolRecord(dir, bad, nil); err != nil {
		t.Fatal(err)
	}
	if _, err := queueSpoolRecord(dir, good, nil); err != nil {
		t.Fatal(err)
	}
	r.caller.Store(host)
	sent, err := sendSpool(dir, r.url, nil)
	queued, refused, _ := spoolCounts(dir)
	if err != nil || sent != 1 || queued != 0 || refused != 1 {
		t.Fatalf("refusal send = sent %d, queued %d, refused %d, err %v", sent, queued, refused, err)
	}
	failed, err := readSpoolFiles(filepath.Join(spoolPath(dir), spoolFailDir))
	if err != nil || len(failed) != 1 || failed[0].record.Exit == 0 || !strings.Contains(failed[0].record.Error, "does not exist") {
		t.Fatalf("refused file = %+v, %v", failed, err)
	}
	if got := r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top); got != 1 {
		t.Fatalf("record after refusal was not sent: %d", got)
	}
	notifySpoolRefused(dir, r.herdrSock, nil)
	notifySpoolRefused(dir, r.herdrSock, nil)
	calls := r.calls("notification|")
	if len(calls) != 1 || !strings.Contains(calls[0], "taskr: queued done for task 999999999 was refused:") {
		t.Fatalf("refusal notifications = %q", calls)
	}
	failed, err = readSpoolFiles(filepath.Join(spoolPath(dir), spoolFailDir))
	if err != nil || len(failed) != 1 || !failed[0].record.Shown {
		t.Fatalf("notification marker = %+v, %v", failed, err)
	}
}

func TestSpoolReadyReportCapturedAtQueueTime(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	deadURL := spoolDeadURL(t, r)
	_, task, launch := spoolMakeWorker(t, r, host, 40)
	report := filepath.Join(t.TempDir(), "report.md")
	const body = "captured before queueing\n"
	if err := os.WriteFile(report, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	home := r.clientHome(deadURL)
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	code, out, stderr := spoolRunCLI(r, host, home, as(task, launch), "ready", "ready", "--report", report)
	if code != exitOK || !strings.HasPrefix(out, "qd1 ") || !strings.Contains(stderr, "queued (1 waiting)") {
		t.Fatalf("ready queue = %d %q %q", code, out, stderr)
	}
	files, err := readSpoolFiles(spoolQueuePath(home))
	if err != nil || len(files) != 1 || files[0].record.Document == nil || files[0].record.Document.Body == nil {
		t.Fatalf("queued ready document = %+v, %v", files, err)
	}
	decoded, err := base64.StdEncoding.DecodeString(*files[0].record.Document.Body)
	if err != nil || string(decoded) != body {
		t.Fatalf("queued report body = %q, %v", decoded, err)
	}
	if err := os.Remove(report); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(spoolStateDir(home), serverURLFile), []byte(r.url+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	code, out, stderr = spoolRunCLI(r, host, home, nil, "spool", "send")
	if code != exitOK || stderr != "" || num(spoolOutputMap(out), "sent") != 1 {
		t.Fatalf("ready report send = %d %q %q", code, out, stderr)
	}
	var captured int
	var stored string
	err = r.openDB().QueryRow(`select d.captured, b.body from documents d join doc_blobs b on b.sha256 = d.sha256
		where d.task_id = ? and d.kind = 'report' order by d.version desc limit 1`, task).Scan(&captured, &stored)
	if err != nil || captured != 1 || stored != body {
		t.Fatalf("captured report = %q %q, %v", captured, stored, err)
	}
}

func TestSpoolDoneUploadsServerRequestedReport(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	deadURL := spoolDeadURL(t, r)
	_, task, launch := spoolMakeWorker(t, r, host, 41)
	report := filepath.Join(t.TempDir(), "report.txt")
	const body = "report read after done\n"
	if err := os.WriteFile(report, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := r.openDB().Exec(`update tasks set report_path = ? where id = ?`, report, task); err != nil {
		t.Fatal(err)
	}
	home := r.clientHome(deadURL)
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	code, out, stderr := spoolRunCLI(r, host, home, as(task, launch), "done")
	if code != exitOK || !strings.HasPrefix(out, "qd1 ") || !strings.Contains(stderr, "queued (1 waiting)") {
		t.Fatalf("done queue = %d %q %q", code, out, stderr)
	}
	files, err := readSpoolFiles(spoolQueuePath(home))
	if err != nil || len(files) != 1 || files[0].record.Document != nil {
		t.Fatalf("done carried text: %+v, %v", files, err)
	}
	if err := os.WriteFile(filepath.Join(spoolStateDir(home), serverURLFile), []byte(r.url+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	code, out, stderr = spoolRunCLI(r, host, home, nil, "spool", "send")
	if code != exitOK || stderr != "" || num(spoolOutputMap(out), "sent") != 1 {
		t.Fatalf("done send = %d %q %q", code, out, stderr)
	}
	var captured int
	var stored string
	err = r.openDB().QueryRow(`select d.captured, b.body from documents d join doc_blobs b on b.sha256 = d.sha256
		where d.task_id = ? and d.kind = 'report' order by d.version desc limit 1`, task).Scan(&captured, &stored)
	if err != nil || captured != 1 || stored != body {
		t.Fatalf("uploaded report = %q %q, %v", captured, stored, err)
	}
}

func TestSpoolUploadFailureKeepsReady(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	deadURL := spoolDeadURL(t, r)
	_, task, launch := spoolMakeWorker(t, r, host, 42)
	report := filepath.Join(t.TempDir(), "report.md")
	if err := os.WriteFile(report, []byte("captured ready report\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	home := r.clientHome(deadURL)
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	code, _, _ := spoolRunCLI(r, host, home, as(task, launch), "ready", "ready", "--report", report)
	if code != exitOK || countSpoolFiles(spoolQueuePath(home)) != 1 {
		t.Fatalf("ready did not queue: code %d", code)
	}
	var puts atomic.Int32
	var secondPut rpcReply
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		raw, err := io.ReadAll(q.Body)
		if err != nil {
			t.Error(err)
			return
		}
		var req rpcRequest
		_ = json.Unmarshal(raw, &req)
		name, _ := rpcCommand(req.Argv)
		q.Body = io.NopCloser(bytes.NewReader(raw))
		if name == "_doc" {
			rec := httptest.NewRecorder()
			r.d.ServeHTTP(rec, q)
			if puts.Add(1) == 1 {
				http.Error(w, "reply lost after upload", http.StatusBadGateway)
				return
			}
			if err := json.Unmarshal(rec.Body.Bytes(), &secondPut); err != nil {
				t.Error(err)
			}
			for key, values := range rec.Header() {
				w.Header()[key] = values
			}
			w.WriteHeader(rec.Code)
			_, _ = w.Write(rec.Body.Bytes())
			return
		}
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	r.caller.Store(host)
	dir := spoolStateDir(home)
	sent, err := sendSpool(dir, url, nil)
	if err == nil || sent != 0 || countSpoolFiles(spoolQueuePath(home)) != 1 || puts.Load() != 1 {
		t.Fatalf("failed upload = sent %d, queued %d, puts %d, err %v", sent, countSpoolFiles(spoolQueuePath(home)), puts.Load(), err)
	}
	sent, err = sendSpool(dir, url, nil)
	if err != nil || sent != 1 || countSpoolFiles(spoolQueuePath(home)) != 0 || puts.Load() != 2 {
		t.Fatalf("replayed upload = sent %d, queued %d, puts %d, err %v", sent, countSpoolFiles(spoolQueuePath(home)), puts.Load(), err)
	}
	if got := lastJSON(secondPut.Stdout)["count"]; got != "unchanged" {
		t.Fatalf("repeated put result = %q (%+v)", got, secondPut)
	}
}

func TestSpoolUploadRefusalDropsAndContinues(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	deadURL := spoolDeadURL(t, r)
	top, task, launch := spoolMakeWorker(t, r, host, 43)
	report := filepath.Join(t.TempDir(), "refused-report.txt")
	if err := os.WriteFile(report, []byte("report refused during upload\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := r.openDB().Exec(`update tasks set report_path = ? where id = ?`, report, task); err != nil {
		t.Fatal(err)
	}
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	home := r.clientHome(deadURL)
	code, _, _ := spoolRunCLI(r, host, home, as(task, launch), "done")
	if code != exitOK || countSpoolFiles(spoolQueuePath(home)) != 1 {
		t.Fatalf("done did not queue: code %d", code)
	}
	dir := spoolStateDir(home)
	note := rpcClientRequest([]string{"--json", "note", "after upload refusal", "--as", id(top)}, t.TempDir(), "after-upload-refusal-1", nil, nil)
	if _, err := queueSpoolRecord(dir, note, nil); err != nil {
		t.Fatal(err)
	}
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		raw, err := io.ReadAll(q.Body)
		if err != nil {
			t.Error(err)
			return
		}
		var req rpcRequest
		if err := json.Unmarshal(raw, &req); err != nil {
			t.Error(err)
			return
		}
		name, _ := rpcCommand(req.Argv)
		q.Body = io.NopCloser(bytes.NewReader(raw))
		if name == "_doc" {
			_ = json.NewEncoder(w).Encode(rpcReply{Exit: exitReject, Stderr: "upload refused"})
			return
		}
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	defer srv.Close()
	r.caller.Store(host)
	logPath := filepath.Join(t.TempDir(), "daemon.log")
	log := openDaemonLog(logPath)
	sent, err := sendSpool(dir, url, log)
	log.close()
	queued, refused, _ := spoolCounts(dir)
	if err != nil || sent != 1 || queued != 0 || refused != 0 {
		t.Fatalf("upload refusal pass = sent %d, queued %d, refused %d, err %v", sent, queued, refused, err)
	}
	if r.count(`select count(*) from events where task_id = ? and kind = 'done'`, task) != 1 ||
		r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top) != 1 {
		t.Fatal("refused upload stopped the pass or lost the applied record")
	}
	var captured int
	var reason string
	if err := r.openDB().QueryRow(`select captured, reason from documents where task_id = ? and kind = 'report'`, task).Scan(&captured, &reason); err != nil || captured != 0 || reason != "client" {
		t.Fatalf("refused report miss = captured %d, reason %q, err %v", captured, reason, err)
	}
	logText, err := os.ReadFile(logPath)
	if err != nil || !strings.Contains(string(logText), fmt.Sprintf("task=%d", task)) ||
		!strings.Contains(string(logText), report) || !strings.Contains(string(logText), "upload refused") {
		t.Fatalf("refused upload log = %q, %v", logText, err)
	}
}

func TestSpoolUpload429KeepsThenSendsInOrder(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	deadURL := spoolDeadURL(t, r)
	top, task, launch := spoolMakeWorker(t, r, host, 44)
	report := filepath.Join(t.TempDir(), "rate-limited-report.txt")
	const body = "report uploaded after rate limit\n"
	if err := os.WriteFile(report, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := r.openDB().Exec(`update tasks set report_path = ? where id = ?`, report, task); err != nil {
		t.Fatal(err)
	}
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	home := r.clientHome(deadURL)
	code, _, _ := spoolRunCLI(r, host, home, as(task, launch), "done")
	if code != exitOK || countSpoolFiles(spoolQueuePath(home)) != 1 {
		t.Fatalf("done did not queue: code %d", code)
	}
	dir := spoolStateDir(home)
	note := rpcClientRequest([]string{"--json", "note", "after upload rate limit", "--as", id(top)}, t.TempDir(), "after-upload-429-01", nil, nil)
	if _, err := queueSpoolRecord(dir, note, nil); err != nil {
		t.Fatal(err)
	}
	var uploadStatus atomic.Int32
	uploadStatus.Store(http.StatusTooManyRequests)
	var puts atomic.Int32
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		raw, err := io.ReadAll(q.Body)
		if err != nil {
			t.Error(err)
			return
		}
		var req rpcRequest
		if err := json.Unmarshal(raw, &req); err != nil {
			t.Error(err)
			return
		}
		name, _ := rpcCommand(req.Argv)
		if name == "_doc" {
			puts.Add(1)
			if int(uploadStatus.Load()) == http.StatusTooManyRequests {
				http.Error(w, "upload rate limited", http.StatusTooManyRequests)
				return
			}
		}
		q.Body = io.NopCloser(bytes.NewReader(raw))
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	defer srv.Close()
	r.caller.Store(host)
	sent, err := sendSpool(dir, url, nil)
	queued, refused, _ := spoolCounts(dir)
	files, readErr := readSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir))
	if err == nil || sent != 0 || queued != 2 || refused != 0 || readErr != nil || len(files) != 2 ||
		files[0].record.StuckSince == "" || files[0].record.StuckReason != "server answered 429: upload rate limited" {
		t.Fatalf("429 upload pass = sent %d, queued %d, refused %d, head %+v, err %v / %v", sent, queued, refused, files, err, readErr)
	}
	if r.count(`select count(*) from events where task_id = ? and kind = 'done'`, task) != 1 ||
		r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top) != 0 {
		t.Fatal("429 upload did not stop behind the applied done record")
	}
	uploadStatus.Store(http.StatusOK)
	sent, err = sendSpool(dir, url, nil)
	queued, refused, _ = spoolCounts(dir)
	if err != nil || sent != 2 || queued != 0 || refused != 0 || puts.Load() != 2 {
		t.Fatalf("send after 429 = sent %d, queued %d, refused %d, puts %d, err %v", sent, queued, refused, puts.Load(), err)
	}
	var captured int
	var stored string
	if err := r.openDB().QueryRow(`select d.captured, b.body from documents d join doc_blobs b on b.sha256 = d.sha256
		where d.task_id = ? and d.kind = 'report' order by d.version desc limit 1`, task).Scan(&captured, &stored); err != nil || captured != 1 || stored != body {
		t.Fatalf("report after 429 = captured %d, body %q, err %v", captured, stored, err)
	}
	if got := r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top); got != 1 {
		t.Fatalf("note behind uploaded report was not sent: %d", got)
	}
}

func TestSpoolQueueWriteFailurePrintsRetry(t *testing.T) {
	contractGuard(t)
	if os.Geteuid() == 0 {
		t.Skip("root can write to read-only directories")
	}
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-queue-write-error", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	home := r.clientHome(r.url)
	dir := spoolStateDir(home)
	first := rpcClientRequest([]string{"--json", "note", "first", "--as", id(top)}, t.TempDir(), "queue-write-first-1", nil, nil)
	if _, err := queueSpoolRecord(dir, first, nil); err != nil {
		t.Fatal(err)
	}
	queueDir := filepath.Join(spoolPath(dir), spoolQueueDir)
	if err := os.Chmod(queueDir, 0o500); err != nil {
		t.Fatal(err)
	}
	defer os.Chmod(queueDir, 0o700)
	code, _, stderr := spoolRunCLI(r, host, home, nil, "note", "second", "--as", id(top))
	if code != exitHerdr || !strings.Contains(stderr, "retry with:") || strings.Contains(stderr, "database") {
		t.Fatalf("queue write error = %d %q", code, stderr)
	}
}

func TestSpoolHTTPProxyErrorsKeepOrRefuse(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-proxy-errors", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	dir := t.TempDir()
	req := rpcClientRequest([]string{"--json", "note", "proxy", "--as", id(top)}, t.TempDir(), "proxy-error-record-1", nil, nil)
	if _, err := queueSpoolRecord(dir, req, nil); err != nil {
		t.Fatal(err)
	}
	var status atomic.Int32
	status.Store(http.StatusBadGateway)
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, _ *http.Request) {
		code := int(status.Load())
		body := "proxy unavailable"
		if code == http.StatusRequestEntityTooLarge {
			body = "payload too large"
		}
		w.WriteHeader(code)
		_, _ = io.WriteString(w, body)
	})
	go srv.Serve(ln)
	r.caller.Store(host)
	status.Store(http.StatusOK)
	sent, err := sendSpool(dir, url, nil)
	queued, refused, _ := spoolCounts(dir)
	if err != nil || sent != 0 || queued != 1 || refused != 0 {
		t.Fatalf("unparsed HTTP 200 = %d queued %d refused %d err %v", sent, queued, refused, err)
	}
	status.Store(http.StatusBadGateway)
	sent, err = sendSpool(dir, url, nil)
	queued, refused, _ = spoolCounts(dir)
	if err != nil || sent != 0 || queued != 1 || refused != 0 {
		t.Fatalf("502 send = %d queued %d refused %d err %v", sent, queued, refused, err)
	}
	for _, code := range []int{http.StatusUnauthorized, http.StatusForbidden, http.StatusRequestTimeout, http.StatusTooManyRequests, http.StatusInternalServerError} {
		status.Store(int32(code))
		sent, err = sendSpool(dir, url, nil)
		queued, refused, _ = spoolCounts(dir)
		if err != nil || sent != 0 || queued != 1 || refused != 0 {
			t.Fatalf("HTTP %d send = %d queued %d refused %d err %v", code, sent, queued, refused, err)
		}
	}
	status.Store(http.StatusRequestEntityTooLarge)
	sent, err = sendSpool(dir, url, nil)
	queued, refused, _ = spoolCounts(dir)
	files, readErr := readSpoolFiles(filepath.Join(spoolPath(dir), spoolFailDir))
	if err != nil || sent != 0 || queued != 0 || refused != 1 || readErr != nil || len(files) != 1 ||
		!strings.Contains(files[0].record.Error, "413") || !strings.Contains(files[0].record.Error, "payload too large") {
		t.Fatalf("413 send = %d queued %d refused %d file %+v err %v / %v", sent, queued, refused, files, err, readErr)
	}
}

func TestSpoolHTTPErrorUsesJSONErrorAndLimitsBody(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	var requests atomic.Int32
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, _ *http.Request) {
		switch requests.Add(1) {
		case 1:
			w.WriteHeader(http.StatusForbidden)
			_, _ = io.WriteString(w, `{"error":"this node is the server","ok":false}`)
		default:
			w.WriteHeader(http.StatusBadGateway)
			_, _ = io.WriteString(w, strings.Repeat("x", 250))
		}
	})
	go srv.Serve(ln)
	defer srv.Close()
	r.caller.Store(host)
	cl, err := newRPCClient(url)
	if err != nil {
		t.Fatal(err)
	}
	_, callErr := cl.call([]string{"--json", "status"}, t.TempDir(), "json-error-status-01", nil)
	if callErr == nil || callErr.code != exitReject || callErr.msg != "server answered 403: this node is the server" {
		t.Fatalf("403 JSON error = %+v", callErr)
	}
	_, callErr = cl.call([]string{"--json", "status"}, t.TempDir(), "json-error-status-02", nil)
	want := "server answered 502: " + strings.Repeat("x", 199) + "…"
	if callErr == nil || callErr.msg != want {
		t.Fatalf("long HTTP error = %+v, want %q", callErr, want)
	}
}

func TestSpoolRemoveBadFileByName(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	home := r.clientHome(r.url)
	badDir := filepath.Join(spoolPath(spoolStateDir(home)), spoolBadDir)
	if err := os.MkdirAll(badDir, 0o700); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(badDir, "noseq.json")
	if err := os.WriteFile(path, []byte("not a record"), 0o600); err != nil {
		t.Fatal(err)
	}
	code, out, stderr := spoolRunCLI(r, host, home, nil, "--json", "spool", "ls")
	listing := spoolOutputMap(out)
	bad, _ := listing["bad"].([]any)
	if code != exitOK || stderr != "" || len(bad) != 1 || bad[0].(map[string]any)["name"] != "noseq.json" {
		t.Fatalf("bad file listing = %d %v %q", code, listing, stderr)
	}
	code, _, stderr = spoolRunCLI(r, host, home, nil, "--json", "spool", "rm", "noseq.json")
	if code != exitOK || stderr != "" {
		t.Fatalf("spool rm by name = %d %q", code, stderr)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("bad file remains after removal: %v", err)
	}
}

func TestSpoolProxyFiveXXQueuesRecordOnly(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-proxy-five-xx", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusBadGateway)
		_, _ = io.WriteString(w, "upstream unavailable")
	})
	go srv.Serve(ln)
	defer srv.Close()
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	home := r.clientHome(url)
	code, out, stderr := spoolRunCLI(r, host, home, nil, "note", "proxy failure", "--as", id(top))
	if code != exitOK || !strings.HasPrefix(out, "qd1 ") || countSpoolFiles(spoolQueuePath(home)) != 1 ||
		!strings.Contains(stderr, "queued (1 waiting)") {
		t.Fatalf("record through 502 = %d %q %q", code, out, stderr)
	}
	code, _, _ = spoolRunCLI(r, host, home, nil, "status")
	if code != exitHerdr {
		t.Fatalf("status through 502 = %d, want %d", code, exitHerdr)
	}
}

func TestSpoolOlderServerIgnoresQueuedAt(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-old-server", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	var sawQueued, sawAge, sawPlain atomic.Int32
	url, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		body, _ := io.ReadAll(q.Body)
		var fields map[string]json.RawMessage
		_ = json.Unmarshal(body, &fields)
		if _, ok := fields["queued_at"]; ok {
			sawQueued.Add(1)
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(http.StatusBadRequest)
			io.WriteString(w, `{"error":"bad request: json: unknown field \"queued_at\""}`)
			return
		}
		if _, ok := fields["queued_age_ms"]; ok {
			sawAge.Add(1)
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(http.StatusBadRequest)
			io.WriteString(w, `{"error":"bad request: json: unknown field \"queued_age_ms\""}`)
			return
		}
		sawPlain.Add(1)
		q.Body = io.NopCloser(bytes.NewReader(body))
		r.d.ServeHTTP(w, q)
	})
	go srv.Serve(ln)
	dir := t.TempDir()
	req := rpcClientRequest([]string{"--json", "note", "old server", "--as", id(top)}, t.TempDir(), "old-server-record-1", nil, nil)
	if _, err := queueSpoolRecord(dir, req, nil); err != nil {
		t.Fatal(err)
	}
	files, err := readSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir))
	if err != nil || len(files) != 1 {
		t.Fatalf("old server queue = %+v, %v", files, err)
	}
	record := files[0].record
	record.QueuedAt = time.Now().UTC().Add(-time.Minute).Format(time.RFC3339)
	if err := writeSpoolAtomic(filepath.Join(spoolPath(dir), spoolQueueDir), filepath.Base(files[0].path), record); err != nil {
		t.Fatal(err)
	}
	r.caller.Store(host)
	sent, err := sendSpool(dir, url, nil)
	if err != nil || sent != 1 || sawQueued.Load() != 1 || sawAge.Load() != 1 || sawPlain.Load() != 1 {
		t.Fatalf("old server fallback = sent %d, queued %d, age %d, plain %d, err %v", sent, sawQueued.Load(), sawAge.Load(), sawPlain.Load(), err)
	}
	if got := r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top); got != 1 {
		t.Fatalf("old server did not apply request: %d", got)
	}
}

func TestSpoolQueuedAtCoversEveryRecordEventPath(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	queuedAt := time.Now().UTC().Add(-time.Minute).Format(time.RFC3339)
	cases := []string{"got", "ready", "done", "fail", "decide", "next", "note", "close", "hook"}
	for i, name := range cases {
		t.Run(name, func(t *testing.T) {
			top, task, launch := spoolMakeWorker(t, r, host, 50+i)
			env := as(task, launch)
			var argv []string
			switch name {
			case "got":
				argv = []string{"--json", "got", id(spoolPrompt(t, r, task, launch))}
			case "ready":
				argv = []string{"--json", "ready", "ready"}
			case "done":
				argv = []string{"--json", "done"}
			case "fail":
				argv = []string{"--json", "fail", "reason"}
			case "decide":
				argv, env = []string{"--json", "decide", "--as", id(top), "rule"}, nil
			case "next":
				argv, env = []string{"--json", "next", id(top), "step"}, nil
			case "note":
				argv, env = []string{"--json", "note", "note", "--as", id(top)}, nil
			case "close":
				argv, env = []string{"--json", "close", id(task)}, nil
			case "hook":
				prompt := spoolPrompt(t, r, task, launch)
				path := filepath.Join(t.TempDir(), "trace.jsonl")
				if _, err := r.openDB().Exec(`update launches set session_ref = ?, session_source = ?, transcript_path = ? where id = ?`,
					"session", "hook:SessionStart", path, launch); err != nil {
					t.Fatal(err)
				}
				_ = prompt
				env["HERDR_PANE_ID"] = "pane-" + strconv.Itoa(50+i)
				argv = []string{"--json", "_hook", "session.idle", "--session", "session", "--transcript", path}
			}
			var baseline int64
			if err := r.openDB().QueryRow(`select coalesce(max(id), 0) from events`).Scan(&baseline); err != nil {
				t.Fatal(err)
			}
			req := rpcRequest{Argv: argv, Cwd: t.TempDir(), Env: env, RequestKey: fmt.Sprintf("queued-at-%02d-key", i), QueuedAt: queuedAt}
			r.caller.Store(host)
			rep := r.d.rpcRun(context.Background(), host, req)
			if rep.Exit != exitOK {
				t.Fatalf("rpc result = %+v", rep)
			}
			rows, err := r.openDB().Query(`select data from events where id > ? order by id`, baseline)
			if err != nil {
				t.Fatal(err)
			}
			defer rows.Close()
			count := 0
			for rows.Next() {
				var raw sql.NullString
				if err := rows.Scan(&raw); err != nil {
					t.Fatal(err)
				}
				var data map[string]any
				if !raw.Valid || json.Unmarshal([]byte(raw.String), &data) != nil || data["queued_at"] != queuedAt {
					t.Fatalf("%s event data = %q", name, raw.String)
				}
				count++
			}
			if err := rows.Err(); err != nil {
				t.Fatal(err)
			}
			if count == 0 {
				t.Fatalf("%s inserted no event", name)
			}
		})
	}
}

func TestSpoolQueuedDoneTimeAndOrdinaryDone(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	_, task, launch := spoolMakeWorker(t, r, host, 70)
	queuedAt := time.Now().UTC().Add(-2 * time.Minute).Format(time.RFC3339)
	before := time.Now().UTC()
	req := rpcRequest{Argv: []string{"--json", "done"}, Cwd: t.TempDir(), Env: as(task, launch), RequestKey: "queued-done-time-0001", QueuedAt: queuedAt}
	r.caller.Store(host)
	if rep := r.d.rpcRun(context.Background(), host, req); rep.Exit != exitOK {
		t.Fatalf("queued done = %+v", rep)
	}
	var raw, created string
	if err := r.openDB().QueryRow(`select data, created_at from events where task_id = ? and kind = 'done'`, task).Scan(&raw, &created); err != nil {
		t.Fatal(err)
	}
	var data map[string]any
	if err := json.Unmarshal([]byte(raw), &data); err != nil || data["queued_at"] != queuedAt {
		t.Fatalf("queued done data = %q, %v", raw, err)
	}
	arrival := parseTime(created)
	if arrival.Before(before.Add(-time.Second)) || arrival.Before(spoolTime(queuedAt)) || arrival.After(time.Now().UTC().Add(time.Second)) {
		t.Fatalf("event time %s did not reflect server arrival after %s", created, queuedAt)
	}

	_, normalTask, normalLaunch := spoolMakeWorker(t, r, host, 71)
	normal := rpcRequest{Argv: []string{"--json", "done"}, Cwd: t.TempDir(), Env: as(normalTask, normalLaunch), RequestKey: "ordinary-done-time-001"}
	if rep := r.d.rpcRun(context.Background(), host, normal); rep.Exit != exitOK {
		t.Fatalf("ordinary done = %+v", rep)
	}
	var ordinary sql.NullString
	if err := r.openDB().QueryRow(`select data from events where task_id = ? and kind = 'done'`, normalTask).Scan(&ordinary); err != nil {
		t.Fatal(err)
	}
	if ordinary.Valid {
		var eventData map[string]any
		if err := json.Unmarshal([]byte(ordinary.String), &eventData); err != nil || eventData["queued_at"] != nil {
			t.Fatalf("ordinary done data contains queued_at: %q", ordinary.String)
		}
	}
}

func TestSpoolOldStallExpiresAndOldSessionStartRecords(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	_, task, launch := spoolMakeWorker(t, r, host, 80)
	spoolPrompt(t, r, task, launch)
	path := filepath.Join(t.TempDir(), "trace.jsonl")
	if _, err := r.openDB().Exec(`update launches set session_ref = ?, session_source = ?, transcript_path = ? where id = ?`,
		"session", "hook:SessionStart", path, launch); err != nil {
		t.Fatal(err)
	}
	old := time.Now().UTC().Add(-11 * time.Minute).Format(time.RFC3339)
	env := as(task, launch)
	env["HERDR_PANE_ID"] = "pane-80"
	r.caller.Store(host)
	stall := rpcRequest{Argv: []string{"--json", "_hook", "session.idle", "--session", "session", "--transcript", path},
		Cwd: t.TempDir(), Env: env, RequestKey: "old-stall-record-0001", QueuedAt: old, QueuedAgeMS: (11 * time.Minute).Milliseconds()}
	baseline := r.count(`select count(*) from events where kind = 'herdr' and task_id = ?`, task)
	rep := r.d.rpcRun(context.Background(), host, stall)
	if rep.Exit != exitOK || strings.TrimSpace(rep.Stdout) != "expired" ||
		r.count(`select count(*) from events where kind = 'herdr' and task_id = ?`, task) != baseline {
		t.Fatalf("old stall = %+v; alert count %d", rep, baseline)
	}

	_, startTask, startLaunch := spoolMakeWorker(t, r, host, 81)
	startEnv := as(startTask, startLaunch)
	startEnv["HERDR_PANE_ID"] = "pane-81"
	startPath := filepath.Join(t.TempDir(), "start.jsonl")
	start := rpcRequest{Argv: []string{"--json", "_hook", "SessionStart", "--session", "older-session", "--transcript", startPath},
		Cwd: t.TempDir(), Env: startEnv, RequestKey: "old-start-record-0001", QueuedAt: old}
	rep = r.d.rpcRun(context.Background(), host, start)
	if rep.Exit != exitOK {
		t.Fatalf("old session start = %+v", rep)
	}
	var session string
	if err := r.openDB().QueryRow(`select session_ref from launches where id = ?`, startLaunch).Scan(&session); err != nil || session != "older-session" {
		t.Fatalf("old session start stored %q, %v", session, err)
	}
}

func TestSpoolCompactQueuedContract(t *testing.T) {
	contractGuard(t)
	key := "queued-contract-key-1"
	var compact bytes.Buffer
	(&ctx{cmd: "done", out: &compact}).emitCompact(map[string]any{"queued": true, "request_key": key})
	if compact.String() != "qd1 "+key+"\n" {
		t.Fatalf("compact queued result = %q", compact.String())
	}
	var legacy bytes.Buffer
	(&ctx{json: true, cmd: "done", out: &legacy}).emit(map[string]any{"queued": true, "request_key": key})
	if legacy.String() != `{"queued":true,"request_key":"`+key+`"}`+"\n" {
		t.Fatalf("JSON queued result = %q", legacy.String())
	}
}

func TestSpoolListRemoveAndStatusStayLocal(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-local", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	home := r.clientHome(spoolDeadURL(t, r))
	dir := spoolStateDir(home)
	req := rpcClientRequest([]string{"--json", "note", "queued", "--as", id(top)}, t.TempDir(), "local-queued-key-1", nil, nil)
	if _, err := queueSpoolRecord(dir, req, nil); err != nil {
		t.Fatal(err)
	}
	if err := ensureSpoolDirs(dir); err != nil {
		t.Fatal(err)
	}
	refusedReq := rpcClientRequest([]string{"--json", "done"}, t.TempDir(), "local-refused-key-1", map[string]string{"TASKR_TASK": "999999999"}, nil)
	refused := spoolRecord{Version: 1, Seq: 2, RequestKey: refusedReq.RequestKey, Request: refusedReq,
		QueuedAt: time.Now().UTC().Format(time.RFC3339), RefusedAt: time.Now().UTC().Format(time.RFC3339), Exit: exitReject, Error: "closed task"}
	if err := writeSpoolAtomic(filepath.Join(spoolPath(dir), spoolFailDir), "000002-"+refused.RequestKey+".json", refused); err != nil {
		t.Fatal(err)
	}
	code, out, stderr := spoolRunCLI(r, host, home, nil, "--json", "spool", "ls")
	if code != exitOK || stderr != "" {
		t.Fatalf("spool ls = %d %q %q", code, out, stderr)
	}
	listing := lastJSON(out)
	queued, _ := listing["queued"].([]any)
	failed, _ := listing["refused"].([]any)
	if len(queued) != 1 || len(failed) != 1 || num(queued[0].(map[string]any), "task") != top ||
		failed[0].(map[string]any)["error"] != "closed task" {
		t.Fatalf("spool listing = %v", listing)
	}
	code, out, stderr = spoolRunCLI(r, host, home, nil, "--json", "daemon", "--status")
	status := lastJSON(out)
	counts, _ := status["spool"].(map[string]any)
	if code != exitOK || stderr != "" || num(counts, "queued") != 1 || num(counts, "refused") != 1 || num(counts, "bad") != 0 {
		t.Fatalf("daemon status spool = %d %v %q", code, status, stderr)
	}
	for _, seq := range []string{"1", "2"} {
		code, _, stderr = spoolRunCLI(r, host, home, nil, "spool", "rm", seq)
		if code != exitOK || stderr != "" {
			t.Fatalf("spool rm %s = %d %q", seq, code, stderr)
		}
	}
	code, out, stderr = spoolRunCLI(r, host, home, nil, "spool", "send")
	if code != exitOK || stderr != "" || num(spoolOutputMap(out), "sent") != 0 {
		t.Fatalf("empty local spool send = %d %q %q", code, out, stderr)
	}

	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	worker := h.newTask("worker", "implementer", root)
	launch := h.launch(worker)
	h.ok(as(worker, launch), "done")
	localSpool := filepath.Join(h.dir, ".local", "state", "taskr", spoolDirName)
	if _, err := os.Stat(localSpool); !os.IsNotExist(err) {
		t.Fatalf("local ledger host created spool path: %v", err)
	}
	var localOut, localErr bytes.Buffer
	if code := contractCLIMain(t, []string{"spool", "ls"}, h.getenv(nil), &localOut, &localErr); code != exitOK {
		t.Fatalf("local spool ls = %d %q", code, localErr.String())
	}
	if _, err := os.Stat(localSpool); !os.IsNotExist(err) {
		t.Fatalf("local spool ls created spool path: %v", err)
	}
}

func TestSpoolFileBound(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-bound", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	deadURL := spoolDeadURL(t, r)
	home := r.clientHome(deadURL)
	dir := spoolStateDir(home)
	if err := ensureSpoolDirs(dir); err != nil {
		t.Fatal(err)
	}
	queueDir := filepath.Join(spoolPath(dir), spoolQueueDir)
	queuedAt := time.Now().UTC().Format(time.RFC3339)
	for i := 1; i <= spoolFileMax; i++ {
		key := fmt.Sprintf("bound-record-%06d", i)
		req := rpcClientRequest([]string{"--json", "note", "waiting", "--as", id(top)}, t.TempDir(), key, nil, nil)
		record := spoolRecord{Version: 1, Seq: int64(i), RequestKey: key, Request: req, QueuedAt: queuedAt}
		if err := writeSpoolAtomic(queueDir, fmt.Sprintf("%06d-%s.json", i, key), record); err != nil {
			t.Fatal(err)
		}
	}
	code, _, stderr := spoolRunCLI(r, host, home, nil, "note", "new", "--as", id(top))
	if code != exitHerdr || !strings.Contains(stderr, "spool full") || !strings.Contains(stderr, "retry with:") ||
		countSpoolFiles(queueDir) != spoolFileMax {
		t.Fatalf("full spool result = %d %q", code, stderr)
	}
}

func TestSpoolQueueProcessHelper(t *testing.T) {
	contractGuard(t)
	if os.Getenv("TASKR_SPOOL_HELPER") != "1" {
		return
	}
	gate := os.Getenv("TASKR_SPOOL_GATE")
	deadline := time.Now().Add(5 * time.Second)
	for {
		if _, err := os.Stat(gate); err == nil {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("queue gate was not opened")
		}
		time.Sleep(time.Millisecond)
	}
	key := os.Getenv("TASKR_SPOOL_KEY")
	req := rpcClientRequest([]string{"--json", "note", "parallel", "--as", "1"}, "/", key, nil, nil)
	if _, err := queueSpoolRecord(os.Getenv("TASKR_SPOOL_DIR"), req, nil); err != nil {
		t.Fatal(err)
	}
}

func TestSpoolClientDaemonSendsAfterSuccessfulObserve(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-daemon", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	url := r.url
	home := r.clientHome(url)
	req := rpcClientRequest([]string{"--json", "note", "daemon pass", "--as", id(top)}, t.TempDir(), "daemon-spool-record-1", nil, nil)
	if _, err := queueSpoolRecord(spoolStateDir(home), req, nil); err != nil {
		t.Fatal(err)
	}
	r.caller.Store(host)
	code, out, stderr := spoolRunCLI(r, host, home, map[string]string{"HERDR_SOCKET_PATH": r.herdrSock}, "daemon", "--once")
	if code != exitOK || stderr != "" {
		t.Fatalf("client daemon pass = %d %q %q", code, out, stderr)
	}
	if got := r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top); got != 1 ||
		countSpoolFiles(spoolQueuePath(home)) != 0 {
		t.Fatalf("daemon pass did not send queue: events=%d queued=%d", got, countSpoolFiles(spoolQueuePath(home)))
	}
}

func TestSpoolConcurrentProcessesAllocateOrderedSequences(t *testing.T) {
	contractGuard(t)
	dir := t.TempDir()
	gate := filepath.Join(dir, "go")
	keys := []string{"process-record-key-1", "process-record-key-2"}
	cmds := make([]*exec.Cmd, 0, len(keys))
	outputs := make([]*bytes.Buffer, 0, len(keys))
	for i, key := range keys {
		cmd := exec.Command(os.Args[0], "-test.run=^TestSpoolQueueProcessHelper$")
		output := &bytes.Buffer{}
		cmd.Env = []string{
			"TASKR_SPOOL_HELPER=1",
			"TASKR_SPOOL_DIR=" + dir,
			"TASKR_SPOOL_GATE=" + gate,
			"TASKR_SPOOL_KEY=" + key,
			"HOME=" + filepath.Join(dir, fmt.Sprintf("home-%d", i)),
			"TASKR_DB=" + filepath.Join(dir, fmt.Sprintf("ledger-%d.db", i)),
			"HERDR_SOCKET_PATH=" + filepath.Join(dir, fmt.Sprintf("socket-%d", i)),
			"PATH=" + os.Getenv("PATH"),
		}
		cmd.Stdout, cmd.Stderr = output, output
		if err := cmd.Start(); err != nil {
			t.Fatal(err)
		}
		cmds = append(cmds, cmd)
		outputs = append(outputs, output)
	}
	if err := os.WriteFile(gate, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	for i, cmd := range cmds {
		if err := cmd.Wait(); err != nil {
			t.Fatalf("queue subprocess failed: %v: %s", err, outputs[i].String())
		}
	}
	files, err := readSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir))
	if err != nil || len(files) != 2 || files[0].record.Seq != 1 || files[1].record.Seq != 2 ||
		files[0].record.RequestKey == files[1].record.RequestKey {
		t.Fatalf("process sequences = %+v, %v", files, err)
	}
}
