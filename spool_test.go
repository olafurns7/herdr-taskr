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
	code := cliMain(args, clientEnv(home, env), &out, &errb)
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
	if got := rpcRetryWindow([]string{"got", "1"}); got != 3*time.Second {
		t.Fatalf("record retry window = %s, want 3s", got)
	}
	if got := rpcRetryWindow([]string{"start"}); got < 60*time.Second {
		t.Fatalf("non-record retry window = %s, want existing window", got)
	}
}

func TestSpoolRecordCommandsQueueAfterTransportWindow(t *testing.T) {
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
				!validSpoolTime(record.QueuedAt) || record.Request.QueuedAt != "" || len(record.Request.Capabilities) == 0 {
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
	r := newTwoHost(t)
	host := spoolClientHost(r)
	deadURL := spoolDeadURL(t, r)
	_, task, launch := spoolMakeWorker(t, r, host, 20)
	home := r.clientHome(deadURL)
	env := as(task, launch)
	var out, errb bytes.Buffer
	c := &ctx{getenv: clientEnv(home, env), out: &out, errw: &errb, client: true, server: deadURL}
	sendHookRPC(c, hookRecord{Event: "session.idle", Session: "session", Transcript: filepath.Join(t.TempDir(), "trace.jsonl")})
	files, err := readSpoolFiles(spoolQueuePath(home))
	if err != nil || len(files) != 1 || out.Len() != 0 || errb.Len() != 0 {
		t.Fatalf("hook queue = %d, %v; output %q %q", len(files), err, out.String(), errb.String())
	}
	if command, _ := rpcCommand(files[0].record.Request.Argv); command != "_hook" {
		t.Fatalf("queued hook command = %q", command)
	}
}

func TestSpoolNonRecordAndServerRefusalDoNotQueue(t *testing.T) {
	r := newTwoHost(t)
	host := spoolClientHost(r)
	deadURL := spoolDeadURL(t, r)
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	_, task, launch := spoolMakeWorker(t, r, host, 21)
	home := r.clientHome(deadURL)
	code, _, stderr := spoolRunCLI(r, host, home, as(task, launch), "start")
	if code != exitHerdr || !strings.Contains(stderr, "retry with:") || !strings.Contains(stderr, "server unreachable") {
		t.Fatalf("non-record outage = %d %q", code, stderr)
	}
	if _, err := os.Stat(filepath.Join(spoolStateDir(home), spoolDirName)); !os.IsNotExist(err) {
		t.Fatalf("non-record command created spool path: %v", err)
	}

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
	if code != exitOK || !strings.HasPrefix(out, "qd1 ") || stderr != "taskr: server unreachable; queued (2 waiting)\n" {
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
	queued, refused := spoolCounts(dir)
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

func TestSpoolOlderServerIgnoresQueuedAt(t *testing.T) {
	r := newTwoHost(t)
	host := spoolClientHost(r)
	top := num(r.want(0, host, nil, "new", "root-old-server", "--role", "orchestrator", "--cwd", t.TempDir()), "task_id")
	var sawQueued, sawPlain atomic.Int32
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
	r.caller.Store(host)
	sent, err := sendSpool(dir, url, nil)
	if err != nil || sent != 1 || sawQueued.Load() != 1 || sawPlain.Load() != 1 {
		t.Fatalf("old server fallback = sent %d, queued %d, plain %d, err %v", sent, sawQueued.Load(), sawPlain.Load(), err)
	}
	if got := r.count(`select count(*) from events where task_id = ? and kind = 'note'`, top); got != 1 {
		t.Fatalf("old server did not apply request: %d", got)
	}
}

func TestSpoolQueuedAtCoversEveryRecordEventPath(t *testing.T) {
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
		Cwd: t.TempDir(), Env: env, RequestKey: "old-stall-record-0001", QueuedAt: old}
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
	if code != exitOK || stderr != "" || num(counts, "queued") != 1 || num(counts, "refused") != 1 {
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
	if code := cliMain([]string{"spool", "ls"}, h.getenv(nil), &localOut, &localErr); code != exitOK {
		t.Fatalf("local spool ls = %d %q", code, localErr.String())
	}
	if _, err := os.Stat(localSpool); !os.IsNotExist(err) {
		t.Fatalf("local spool ls created spool path: %v", err)
	}
}

func TestSpoolFileBound(t *testing.T) {
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
