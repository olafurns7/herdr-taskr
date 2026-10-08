package main

import (
	"bufio"
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func hookFixture(t *testing.T, name string) []byte {
	t.Helper()
	b, err := os.ReadFile(filepath.Join("testdata", "hooks", name))
	if err != nil {
		t.Fatal(err)
	}
	return b
}

func hookPromptFixture(t *testing.T, name string, attempt int64, suffix string) []byte {
	t.Helper()
	var root map[string]any
	if err := json.Unmarshal(hookFixture(t, name), &root); err != nil {
		t.Fatal(err)
	}
	prompt := fmt.Sprintf("First taskr got %d.%s", attempt, suffix)
	if _, ok := root["input"]; ok {
		output := root["output"].(map[string]any)
		parts := output["parts"].([]any)
		parts[0].(map[string]any)["text"] = prompt
	} else if root["type"] == "input" {
		root["text"] = prompt
	} else {
		root["prompt"] = prompt
	}
	b, err := json.Marshal(root)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

func hookUncodedStopFailureFixture(t *testing.T) []byte {
	t.Helper()
	var root map[string]any
	if err := json.Unmarshal(hookFixture(t, "claude-stop-failure.json"), &root); err != nil {
		t.Fatal(err)
	}
	root["error"] = "API Error: 529 overloaded"
	b, err := json.Marshal(root)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

// stubHookExit disarms the hook's deadline exit for in-process runs.
func stubHookExit(t *testing.T) {
	t.Helper()
	old := hookExit
	hookExit = func() {}
	t.Cleanup(func() { hookExit = old })
}

func runHookPayload(t *testing.T, h *harness, env map[string]string, harnessName, event string, payload []byte) {
	t.Helper()
	stubHookExit(t)
	old := os.Stdin
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := w.Write(payload); err != nil {
		t.Fatal(err)
	}
	w.Close()
	os.Stdin = r
	code, lines := h.run(env, "hook", harnessName, event)
	os.Stdin = old
	r.Close()
	if code != exitOK || len(lines) != 0 || h.lastStderr() != "" {
		t.Fatalf("taskr hook %s %s = exit %d, stdout %v, stderr %q", harnessName, event, code, lines, h.lastStderr())
	}
}

func hookLane(t *testing.T, h *harness, provider, pane string) (int64, int64, int64) {
	t.Helper()
	return hookLaneRole(t, h, provider, pane, "implementer")
}

func hookLaneRole(t *testing.T, h *harness, provider, pane, role string) (int64, int64, int64) {
	t.Helper()
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("impl", role, top, "--pane", pane)
	l := num(h.ok(nil, "launch", id(w), "--provider", provider, "--model", "test", "--effort", "medium"), "launch_id")
	return top, w, l
}

func hookLaneEnv(task, launch int64, pane string) map[string]string {
	e := as(task, launch)
	e["HERDR_PANE_ID"], e["HERDR_ENV"] = pane, "1"
	return e
}

func hookStall(t *testing.T, h *harness, attempt int64) map[string]any {
	t.Helper()
	var eventID int64
	if err := h.openDB().QueryRow(`select id from events where event_key = ?`, "stall:"+id(attempt)).Scan(&eventID); err != nil {
		t.Fatalf("stall for attempt %d: %v", attempt, err)
	}
	return eventByID(h, eventID)
}

func TestHookFixtureFlowsAndReceipts(t *testing.T) {
	contractGuard(t)
	cases := []struct {
		name, provider, startFile, startEvent, promptFile, promptEvent, stopFile, stopEvent, session, transcript, errorCode string
	}{
		{"claude", "claude", "claude-session-start.json", "SessionStart", "claude-user-prompt-submit.json", "UserPromptSubmit", "claude-stop-failure.json", "StopFailure", "claude-session", "/tmp/taskr-hooks/claude.jsonl", "rate_limit"},
		{"codex", "codex", "codex-session-start.json", "SessionStart", "codex-user-prompt-submit.json", "UserPromptSubmit", "codex-stop.json", "Stop", "codex-session", "/tmp/taskr-hooks/codex.jsonl", ""},
		{"opencode", "opencode", "opencode-session-created.json", "session.created", "opencode-chat-message.json", "chat.message", "opencode-session-error.json", "session.error", "opencode-session", "", "server_overloaded"},
		{"pi", "pi", "pi-session-start.json", "session_start", "pi-input.json", "input", "pi-agent-settled-error.json", "agent_settled", "pi-session", "/tmp/taskr-hooks/pi.jsonl", "ECONNRESET"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			h := newHarness(t)
			_, w, l := hookLane(t, h, tc.provider, "w9:p1")
			env := hookLaneEnv(w, l, "w9:p1")
			runHookPayload(t, h, env, tc.name, tc.startEvent, hookFixture(t, tc.startFile))
			var session, source string
			var transcript sql.NullString
			if err := h.openDB().QueryRow(`select session_ref, session_source, transcript_path from launches where id = ?`, l).
				Scan(&session, &source, &transcript); err != nil {
				t.Fatal(err)
			}
			if session != tc.session || source != "hook:"+tc.startEvent || transcript.String != tc.transcript {
				t.Fatalf("bound session = %q %q %q", session, source, transcript.String)
			}
			a1 := num(h.ok(nil, "prompt", id(w), "--text", "work"), "attempt_id")
			runHookPayload(t, h, env, tc.name, tc.promptEvent,
				hookPromptFixture(t, tc.promptFile, a1, " Continue."))
			if n := hookRelatedCount(t, h, "got", a1); n != 1 {
				t.Fatalf("hook-first got count = %d, want 1", n)
			}
			got := h.ok(as(w, l), "got", id(a1))
			if got["duplicate"] != true {
				t.Fatalf("model got after hook = %v, want duplicate", got)
			}
			gotID := num(got, "event_id")
			identity := eventByID(h, gotID)["data"].(map[string]any)["identity"].(map[string]any)
			if identity["session_ref"] != tc.session || identity["session_source"] != "hook:"+tc.promptEvent {
				t.Fatalf("hook receipt identity = %v", identity)
			}
			if tc.transcript == "" && identity["transcript_path"] != nil || tc.transcript != "" && identity["transcript_path"] != tc.transcript {
				t.Fatalf("hook receipt transcript = %v, want %q", identity["transcript_path"], tc.transcript)
			}
			runHookPayload(t, h, env, tc.name, tc.stopEvent, hookFixture(t, tc.stopFile))
			if tc.name == "opencode" {
				runHookPayload(t, h, env, tc.name, "session.idle", hookFixture(t, "opencode-session-idle.json"))
			}
			if tc.name == "pi" {
				runHookPayload(t, h, env, tc.name, "agent_settled", hookFixture(t, "pi-agent-settled.json"))
			}
			stall := hookStall(t, h, a1)["data"].(map[string]any)
			if stall["reason"] != "stall" || stall["error"] != nil && stall["error"] != tc.errorCode {
				t.Fatalf("stall data = %v", stall)
			}
			runHookPayload(t, h, env, tc.name, tc.stopEvent, hookFixture(t, tc.stopFile))
			if n := hookStallCount(t, h, a1); n != 1 {
				t.Fatalf("stall count after duplicate stop = %d, want one", n)
			}

			a2 := num(h.ok(nil, "prompt", id(w), "--text", "second turn", "--receipt-timeout", "0"), "attempt_id")
			first := h.ok(as(w, l), "got", id(a2))
			if first["duplicate"] != nil {
				t.Fatalf("model-first got = %v", first)
			}
			runHookPayload(t, h, env, tc.name, tc.promptEvent,
				hookPromptFixture(t, tc.promptFile, a2, " Continue."))
			if n := hookRelatedCount(t, h, "got", a2); n != 1 {
				t.Fatalf("model-first got count = %d, want 1", n)
			}
			report := filepath.Join(h.dir, "report.md")
			if err := os.WriteFile(report, []byte("ready"), 0o644); err != nil {
				t.Fatal(err)
			}
			h.ok(as(w, l), "ready", "slice", "--report", report)
			runHookPayload(t, h, env, tc.name, tc.stopEvent, hookFixture(t, tc.stopFile))
			stall2 := hookStall(t, h, a2)["data"].(map[string]any)
			if stall2["last"] != "r" || stall2["error"] != nil && stall2["error"] != tc.errorCode {
				t.Fatalf("stall after ready = %v", stall2)
			}
		})
	}
}

func TestHookSessionAndPaneBinding(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	_, w, l := hookLane(t, h, "claude", "w9:p1")
	env := hookLaneEnv(w, l, "w9:p1")
	runHookPayload(t, h, env, "claude", "SessionStart", hookFixture(t, "claude-session-start.json"))
	var nested map[string]any
	if err := json.Unmarshal(hookFixture(t, "claude-session-start.json"), &nested); err != nil {
		t.Fatal(err)
	}
	nested["session_id"] = "nested-session"
	b, _ := json.Marshal(nested)
	runHookPayload(t, h, env, "claude", "SessionStart", b)
	var bound string
	h.openDB().QueryRow(`select session_ref from launches where id = ?`, l).Scan(&bound)
	if bound != "claude-session" {
		t.Fatalf("session was rebound to %q", bound)
	}

	a1 := num(h.ok(nil, "prompt", id(w), "--text", "quoted"), "attempt_id")
	quoted, _ := json.Marshal(map[string]any{"session_id": bound, "transcript_path": "/tmp/taskr-hooks/claude.jsonl",
		"hook_event_name": "UserPromptSubmit", "prompt": "Please see First taskr got " + id(a1) + "."})
	runHookPayload(t, h, env, "claude", "UserPromptSubmit", quoted)
	if n := hookRelatedCount(t, h, "got", a1); n != 0 {
		t.Fatalf("quoted pointer created %d got rows", n)
	}

	a2 := num(h.ok(nil, "prompt", id(w), "--text", "nested"), "attempt_id")
	runHookPayload(t, h, env, "claude", "UserPromptSubmit", hookPromptFixture(t, "claude-user-prompt-submit.json", a2, ""))
	if n := hookRelatedCount(t, h, "got", a2); n != 1 {
		t.Fatalf("bound session receipt count = %d", n)
	}
	a3 := num(h.ok(nil, "prompt", id(w), "--text", "wrong pane"), "attempt_id")
	wrongPane := hookLaneEnv(w, l, "w9:p2")
	runHookPayload(t, h, wrongPane, "claude", "UserPromptSubmit", hookPromptFixture(t, "claude-user-prompt-submit.json", a3, ""))
	if n := hookRelatedCount(t, h, "got", a3); n != 0 {
		t.Fatalf("wrong pane created %d got rows", n)
	}

	a4 := num(h.ok(nil, "prompt", id(w), "--text", "stale launch"), "attempt_id")
	newLaunch := num(h.ok(nil, "launch", id(w), "--provider", "claude", "--model", "test", "--effort", "medium"), "launch_id")
	if newLaunch == l {
		t.Fatal("launch did not change")
	}
	runHookPayload(t, h, env, "claude", "UserPromptSubmit", hookPromptFixture(t, "claude-user-prompt-submit.json", a4, ""))
	if n := hookRelatedCount(t, h, "got", a4); n != 0 {
		t.Fatalf("stale launch created %d got rows", n)
	}
}

func TestHookNoStallAfterDoneFailOrAsk(t *testing.T) {
	contractGuard(t)
	for _, outcome := range []string{"done", "fail", "ask"} {
		t.Run(outcome, func(t *testing.T) {
			h := newHarness(t)
			_, w, l := hookLane(t, h, "claude", "w9:p1")
			env := hookLaneEnv(w, l, "w9:p1")
			runHookPayload(t, h, env, "claude", "SessionStart", hookFixture(t, "claude-session-start.json"))
			a := num(h.ok(nil, "prompt", id(w), "--text", "work", "--receipt-timeout", "0"), "attempt_id")
			switch outcome {
			case "done":
				h.ok(as(w, l), "done", "finished")
			case "fail":
				h.ok(as(w, l), "fail", "failed")
			case "ask":
				h.ok(as(w, l), "ask", "need input")
			}
			runHookPayload(t, h, env, "claude", "Stop", hookFixture(t, "claude-stop.json"))
			if n := hookStallCount(t, h, a); n != 0 {
				t.Fatalf("stall after %s = %d, want 0", outcome, n)
			}
		})
	}
}

func TestHookRoleStalls(t *testing.T) {
	contractGuard(t)
	for _, tc := range []struct {
		role       string
		plainStall int
		errorStall bool
	}{
		{"sub-orchestrator", 0, true},
		{"orchestrator", 0, true},
		{"reviewer", 1, false},
	} {
		t.Run(tc.role, func(t *testing.T) {
			h := newHarness(t)
			_, w, l := hookLaneRole(t, h, "claude", "w9:p1", tc.role)
			env := hookLaneEnv(w, l, "w9:p1")
			runHookPayload(t, h, env, "claude", "SessionStart", hookFixture(t, "claude-session-start.json"))
			a := num(h.ok(nil, "prompt", id(w), "--text", "work", "--receipt-timeout", "0"), "attempt_id")
			runHookPayload(t, h, env, "claude", "UserPromptSubmit", hookPromptFixture(t, "claude-user-prompt-submit.json", a, " Continue."))
			runHookPayload(t, h, env, "claude", "Stop", hookFixture(t, "claude-stop.json"))
			if n := hookStallCount(t, h, a); n != tc.plainStall {
				t.Fatalf("plain stall count = %d, want %d", n, tc.plainStall)
			}
			if tc.errorStall {
				a2 := num(h.ok(nil, "prompt", id(w), "--text", "error work", "--receipt-timeout", "0"), "attempt_id")
				runHookPayload(t, h, env, "claude", "UserPromptSubmit", hookPromptFixture(t, "claude-user-prompt-submit.json", a2, " Continue."))
				runHookPayload(t, h, env, "claude", "StopFailure", hookFixture(t, "claude-stop-failure.json"))
				if n := hookStallCount(t, h, a2); n != 1 {
					t.Fatalf("error stall count = %d, want 1", n)
				}
				data := hookStall(t, h, a2)["data"].(map[string]any)
				if data["error"] != "rate_limit" {
					t.Fatalf("error stall data = %v", data)
				}
			}
		})
	}
}

func TestHookUncodedStopFailureStalls(t *testing.T) {
	contractGuard(t)
	for _, role := range []string{"orchestrator", "sub-orchestrator", "implementer"} {
		t.Run(role, func(t *testing.T) {
			h := newHarness(t)
			_, w, l := hookLaneRole(t, h, "claude", "w9:p1", role)
			env := hookLaneEnv(w, l, "w9:p1")
			runHookPayload(t, h, env, "claude", "SessionStart", hookFixture(t, "claude-session-start.json"))
			a := num(h.ok(nil, "prompt", id(w), "--text", "work", "--receipt-timeout", "0"), "attempt_id")
			runHookPayload(t, h, env, "claude", "UserPromptSubmit", hookPromptFixture(t, "claude-user-prompt-submit.json", a, " Continue."))
			runHookPayload(t, h, env, "claude", "StopFailure", hookUncodedStopFailureFixture(t))
			if n := hookStallCount(t, h, a); n != 1 {
				t.Fatalf("uncoded error stall count = %d, want 1", n)
			}
			if data := hookStall(t, h, a)["data"].(map[string]any); data["error"] != "unknown" {
				t.Fatalf("uncoded error stall data = %v", data)
			}
		})
	}
}

func TestHookSilentNoopsAndPanic(t *testing.T) {
	contractGuard(t)
	stubHookExit(t)
	h := newHarness(t)
	_, w, l := hookLane(t, h, "claude", "w9:p1")
	env := hookLaneEnv(w, l, "w9:p1")
	before := hookTotalEvents(t, h)
	for _, tc := range []struct {
		env            map[string]string
		harness, event string
		payload        []byte
	}{
		{nil, "claude", "SessionStart", hookFixture(t, "claude-session-start.json")},
		{env, "claude", "SessionStart", []byte("{broken")},
		{env, "unknown", "SessionStart", hookFixture(t, "claude-session-start.json")},
		{env, "claude", "SessionEnd", hookFixture(t, "claude-session-start.json")},
		{env, "opencode", "session.status", hookFixture(t, "opencode-session-status.json")},
		{env, "pi", "agent_start", hookFixture(t, "pi-agent-start.json")},
		{env, "pi", "input", hookFixture(t, "pi-agent-start.json")},
	} {
		runHookPayload(t, h, tc.env, tc.harness, tc.event, tc.payload)
	}
	if after := hookTotalEvents(t, h); after != before {
		t.Fatalf("silent no-ops wrote events: %d -> %d", before, after)
	}

	var out bytes.Buffer
	c := &ctx{getenv: func(string) string { panic("injected") }, out: &out, errw: &out}
	if _, code, err := cmdHook(c, []string{"claude", "SessionStart"}); code != exitOK || err != nil || out.Len() != 0 {
		t.Fatalf("panic hook = code %d, err %v, output %q", code, err, out.String())
	}

	path := filepath.Join(t.TempDir(), "missing", "taskr.db")
	var silent bytes.Buffer
	old := os.Stdin
	r, wpipe, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	wpipe.Write(hookFixture(t, "claude-session-start.json"))
	wpipe.Close()
	os.Stdin = r
	code := contractRun(t, []string{"hook", "claude", "SessionStart"}, func(k string) string {
		return map[string]string{"TASKR_DB": path, "TASKR_LAUNCH": id(l), "TASKR_TASK": id(w), "HERDR_ENV": "1", "HERDR_PANE_ID": "w9:p1"}[k]
	}, &silent, &silent)
	os.Stdin, _ = old, r.Close()
	if code != exitOK || silent.Len() != 0 {
		t.Fatalf("missing ledger hook = %d %q", code, silent.String())
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("hook created missing ledger: %v", err)
	}
}

func TestHookOlderBinaryShellGuard(t *testing.T) {
	contractGuard(t)
	dir := t.TempDir()
	fake := filepath.Join(dir, "taskr")
	if err := os.WriteFile(fake, []byte("#!/bin/sh\necho unknown command >&2\nexit 2\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command("/bin/sh", "-c", "taskr hook claude Stop >/dev/null 2>&1; true")
	cmd.Env = []string{"PATH=" + dir}
	out, err := cmd.CombinedOutput()
	if err != nil || len(out) != 0 {
		t.Fatalf("older binary wrapper = %q, %v", out, err)
	}
}

func TestHookLockChild(t *testing.T) {
	contractGuard(t)
	if os.Getenv("TASKR_HOOK_LOCK_CHILD") != "1" {
		return
	}
	var out, errw bytes.Buffer
	c := &ctx{getenv: func(k string) string {
		if k == "TASKR_LAUNCH" {
			fmt.Fprintln(os.Stdout, "READY")
		}
		return os.Getenv(k)
	}, out: &out, errw: &errw}
	_, code, err := cmdHook(c, []string{"claude", "SessionStart"})
	if code != exitOK || err != nil || out.Len() != 0 || errw.Len() != 0 {
		fmt.Fprintln(os.Stdout, "HOOKOUTPUT")
	}
	os.Exit(exitOK)
}

func TestHookDeadlineWithHeldDBLock(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	_, w, l := hookLane(t, h, "claude", "w9:p1")
	tx, err := h.openDB().Begin()
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	if _, err := tx.Exec(`insert into meta (key, value) values ('hook-lock', 'held')`); err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command(os.Args[0], "-test.run=^TestHookLockChild$")
	cmd.Env = hookChildEnv(map[string]string{"TASKR_HOOK_LOCK_CHILD": "1", "TASKR_DB": h.db, "TASKR_TASK": id(w),
		"TASKR_LAUNCH": id(l), "HERDR_ENV": "1", "HERDR_PANE_ID": "w9:p1"})
	cmd.Stdin = bytes.NewReader(hookFixture(t, "claude-session-start.json"))
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	reader := bufio.NewReader(stdout)
	line, err := reader.ReadString('\n')
	if err != nil || line != "READY\n" {
		t.Fatalf("lock child readiness = %q, %v", line, err)
	}
	start := time.Now()
	out, outErr := io.ReadAll(reader)
	errout, errErr := io.ReadAll(stderr)
	waitErr := cmd.Wait()
	elapsed := time.Since(start)
	// The child runs under -race in the prescribed gate; allow its scheduler overhead.
	if outErr != nil || errErr != nil || waitErr != nil || len(out) != 0 || len(errout) != 0 || elapsed >= 2*time.Second {
		t.Fatalf("locked hook = stdout %q stderr %q err %v/%v/%v elapsed %v", out, errout, outErr, errErr, waitErr, elapsed)
	}
}

func TestHookDBBusyTimeout(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	h.openDB().Close()
	db, err := openHookDB(&ctx{getenv: h.getenv(nil)}, context.Background())
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	var ms int
	if err := db.QueryRow(`pragma busy_timeout`).Scan(&ms); err != nil {
		t.Fatal(err)
	}
	if ms != int(hookBusyTimeout.Milliseconds()) {
		t.Fatalf("hook busy_timeout = %d, want %d", ms, hookBusyTimeout.Milliseconds())
	}
}

func TestHookMigrationFromV010(t *testing.T) {
	contractGuard(t)
	path := filepath.Join(t.TempDir(), "v0.10.db")
	legacy := strings.Replace(schemaSQL, "session_ref  text, session_kind text, session_source text, transcript_path text,",
		"session_ref  text, session_kind text, session_source text,", 1)
	if legacy == schemaSQL {
		t.Fatal("could not construct a v0.10 schema")
	}
	db, err := sql.Open("sqlite", "file:"+path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(legacy); err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(`insert into tasks (id, name, role, status, created_at, updated_at) values (1, 'old', 'implementer', 'open', ?, ?)`, now(), now()); err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(`insert into launches (id, task_id, observed_version, recorded_at) values (1, 1, 19, ?)`, now()); err != nil {
		t.Fatal(err)
	}
	db.Close()

	c := &ctx{getenv: func(k string) string {
		if k == "TASKR_DB" {
			return path
		}
		return ""
	}}
	migrated, err := openDB(c)
	if err != nil {
		t.Fatal(err)
	}
	defer migrated.Close()
	var columns, version int
	if err := migrated.QueryRow(`select count(*) from pragma_table_info('launches') where name = 'transcript_path'`).Scan(&columns); err != nil {
		t.Fatal(err)
	}
	if err := migrated.QueryRow(`select observed_version from launches where id = 1`).Scan(&version); err != nil {
		t.Fatal(err)
	}
	if columns != 1 || version != 19 {
		t.Fatalf("migration columns=%d observed_version=%d", columns, version)
	}
}

func TestHookedLivenessRule(t *testing.T) {
	contractGuard(t)
	h, top, w, l := hintFixture(t)
	env := hookLaneEnv(w, l, "w9:p1")
	runHookPayload(t, h, env, "claude", "SessionStart", hookFixture(t, "claude-session-start.json"))
	h.ok(nil, "prompt", id(w), "--text", "work", "--receipt-timeout", "0")
	h.setAgents("w9:p1/idle/2")
	h.ok(nil, "daemon", "--once")
	h.setAgents("w9:p1/unknown/3")
	h.ok(nil, "daemon", "--once")
	h.setAgents("w9:p1/done/4")
	h.ok(nil, "daemon", "--once")
	if n := hintCounter(t, h.openDB(), "idle", "hooked"); n != 1 {
		t.Fatalf("hooked idle counter = %d", n)
	}
	if n := hintCounter(t, h.openDB(), "unknown", "hooked"); n != 1 {
		t.Fatalf("hooked unknown counter = %d", n)
	}
	if n := hintCounter(t, h.openDB(), "done", "hooked"); n != 1 {
		t.Fatalf("hooked done counter = %d", n)
	}
	if n := h.countHerdr(top); n != 0 {
		t.Fatalf("hooked done emitted a hint: %d", n)
	}
	h.ok(nil, "prompt", id(w), "--text", "armed receipt")
	h.setAgents()
	h.ok(nil, "daemon", "--once")
	h.setAgents("w9:p1/blocked/4")
	h.ok(nil, "daemon", "--once")
	var n int
	if err := h.openDB().QueryRow(`select count(*) from events where task_id = ? and kind = 'herdr'`, w).Scan(&n); err != nil {
		t.Fatal(err)
	}
	if n != 2 {
		t.Fatalf("hooked missing and blocked hints = %d, want 2 (parent %d)", n, top)
	}

	plain, parent, worker, launch := hintFixture(t)
	plain.ok(nil, "prompt", id(worker), "--text", "work", "--receipt-timeout", "0")
	plain.setAgents("w9:p1/done/1")
	plain.ok(nil, "daemon", "--once")
	if n := plain.countHerdr(parent); n != 1 {
		t.Fatalf("non-hooked done emitted %d hints, want 1 (launch %d)", n, launch)
	}
}

// codexHookLane binds a Codex lane through the hook's SessionStart, with a
// rollout path under CODEX_HOME/sessions, and returns the worker env a real
// Codex shell carries.
func codexHookLane(t *testing.T, h *harness, role string) (w, l int64, hookEnv, workerEnv map[string]string, session, path string) {
	t.Helper()
	_, w, l = hookLaneRole(t, h, "codex", "w9:p1", role)
	home := t.TempDir()
	session = "019a0f91-fbbd-7051-83dd-74a92bc55288"
	path = filepath.Join(home, "sessions", "2026", "10", "01", "rollout-2026-10-01T20-00-00-"+session+".jsonl")
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	hookEnv = hookLaneEnv(w, l, "w9:p1")
	workerEnv = as(w, l)
	workerEnv["HERDR_PANE_ID"], workerEnv["CODEX_HOME"], workerEnv["CODEX_THREAD_ID"] = "w9:p1", home, "env-thread"
	runHookPayload(t, h, hookEnv, "codex", "SessionStart", codexHookPayload(t, "codex-session-start.json", session, path, 0))
	return
}

func codexHookPayload(t *testing.T, name, session, path string, attempt int64) []byte {
	t.Helper()
	var root map[string]any
	if err := json.Unmarshal(hookFixture(t, name), &root); err != nil {
		t.Fatal(err)
	}
	root["session_id"], root["transcript_path"] = session, path
	if attempt > 0 {
		root["prompt"] = fmt.Sprintf("First taskr got %d. Continue.", attempt)
	}
	b, err := json.Marshal(root)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

func promptCreatedAt(t *testing.T, h *harness, attempt int64) time.Time {
	t.Helper()
	var at string
	if err := h.openDB().QueryRow(`select created_at from events where id = ?`, attempt).Scan(&at); err != nil {
		t.Fatal(err)
	}
	return parseTime(at)
}

func TestHookCodexBindingSurvivesWorkerIdentity(t *testing.T) {
	contractGuard(t)
	for _, tc := range []string{"start", "model-first got", "nested rebind"} {
		t.Run(tc, func(t *testing.T) {
			h := newHarness(t)
			w, l, hookEnv, workerEnv, session, path := codexHookLane(t, h, "implementer")
			a := num(h.ok(nil, "prompt", id(w), "--text", "work", "--receipt-timeout", "0"), "attempt_id")
			if tc == "model-first got" {
				h.ok(workerEnv, "got", id(a))
			} else {
				runHookPayload(t, h, hookEnv, "codex", "UserPromptSubmit", codexHookPayload(t, "codex-user-prompt-submit.json", session, path, a))
				h.ok(workerEnv, "start")
			}
			var ref, kind, source, home string
			if err := h.openDB().QueryRow(`select session_ref, session_kind, session_source, coalesce(native_home, '') from launches where id = ?`, l).
				Scan(&ref, &kind, &source, &home); err != nil {
				t.Fatal(err)
			}
			if ref != session || kind != "thread_id" || source != "hook:SessionStart" || home != workerEnv["CODEX_HOME"] {
				t.Fatalf("after %s: session %q %q %q native_home %q", tc, ref, kind, source, home)
			}
			if tc == "nested rebind" {
				runHookPayload(t, h, hookEnv, "codex", "SessionStart", codexHookPayload(t, "codex-session-start.json", "nested-session", path, 0))
				h.openDB().QueryRow(`select session_ref from launches where id = ?`, l).Scan(&ref)
				if ref != session {
					t.Fatalf("nested SessionStart rebound %q to %q", session, ref)
				}
				return
			}
			runHookPayload(t, h, hookEnv, "codex", "Stop", codexHookPayload(t, "codex-stop.json", session, path, 0))
			if n := hookStallCount(t, h, a); n != 1 {
				t.Fatalf("Stop after %s: stall count = %d, want 1", tc, n)
			}
		})
	}
}

func TestHookCodexRolloutFallback(t *testing.T) {
	contractGuard(t)
	user := func(text string) string {
		return `{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"` + text + `"}]}}`
	}
	errorTurn := func(at time.Time, timestamp bool) string {
		row := `{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","last_agent_message":null,` +
			`"completed_at":` + fmt.Sprint(at.Unix()) + `,"error":{"message":"Selected model is at capacity.","codex_error_info":"server_overloaded"}}}`
		if timestamp {
			row = `{"timestamp":"` + at.UTC().Format(time.RFC3339Nano) + `",` + row[1:]
		}
		return row
	}
	uncodedErrorTurn := func(at time.Time, timestamp bool) string {
		return strings.Replace(errorTurn(at, timestamp), `"error":{"message":"Selected model is at capacity.","codex_error_info":"server_overloaded"}`, `"error":"API Error: 529 overloaded"`, 1)
	}
	for _, tc := range []struct {
		name, role, wantError string
		rows                  func(prompt time.Time) []string
		want                  bool
	}{
		{"error after prompt", "implementer", "server_overloaded", func(p time.Time) []string {
			return []string{user("First taskr got 1."), errorTurn(p.Add(time.Second), true)}
		}, true},
		{"completed_at fallback", "implementer", "server_overloaded", func(p time.Time) []string {
			return []string{user("First taskr got 1."), errorTurn(p.Add(2*time.Second), false)}
		}, true},
		{"later user message", "implementer", "", func(p time.Time) []string {
			return []string{errorTurn(p.Add(time.Second), true), user("First taskr got 2. Try again.")}
		}, false},
		{"error older than prompt", "implementer", "", func(p time.Time) []string {
			return []string{user("First taskr got 1."), errorTurn(p.Add(-time.Second), true)}
		}, false},
		{"sub-orchestrator error after prompt", "sub-orchestrator", "server_overloaded", func(p time.Time) []string {
			return []string{user("First taskr got 1."), errorTurn(p.Add(time.Second), true)}
		}, true},
		{"sub-orchestrator uncoded error after prompt", "sub-orchestrator", "unknown", func(p time.Time) []string {
			return []string{user("First taskr got 1."), uncodedErrorTurn(p.Add(time.Second), true)}
		}, true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			h := newHarness(t)
			w, _, hookEnv, workerEnv, session, path := codexHookLane(t, h, tc.role)
			a := num(h.ok(nil, "prompt", id(w), "--text", "work", "--receipt-timeout", "0"), "attempt_id")
			runHookPayload(t, h, hookEnv, "codex", "UserPromptSubmit", codexHookPayload(t, "codex-user-prompt-submit.json", session, path, a))
			if got := h.ok(workerEnv, "got", id(a)); got["duplicate"] != true {
				t.Fatalf("model got after hook got = %v, want duplicate", got)
			}
			rows := tc.rows(promptCreatedAt(t, h, a))
			if err := os.WriteFile(path, []byte(strings.Join(rows, "\n")+"\n"), 0o644); err != nil {
				t.Fatal(err)
			}
			h.setAgents("w9:p1/idle/2")
			h.ok(nil, "daemon", "--once")
			n := hookStallCount(t, h, a)
			if tc.want && n != 1 || !tc.want && n != 0 {
				t.Fatalf("rollout fallback stalls = %d, want error=%v", n, tc.want)
			}
			if tc.want {
				data := hookStall(t, h, a)["data"].(map[string]any)
				if data["error"] != tc.wantError || data["reason"] != "stall" {
					t.Fatalf("rollout stall = %v", data)
				}
			}
		})
	}
}

// An error turn already reported for one attempt must not stall a re-prompt
// that Codex has not yet appended to the rollout.
func TestHookCodexRolloutReprompt(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	w, _, hookEnv, workerEnv, session, path := codexHookLane(t, h, "implementer")
	a1 := num(h.ok(nil, "prompt", id(w), "--text", "work", "--receipt-timeout", "0"), "attempt_id")
	runHookPayload(t, h, hookEnv, "codex", "UserPromptSubmit", codexHookPayload(t, "codex-user-prompt-submit.json", session, path, a1))
	h.ok(workerEnv, "got", id(a1))
	at := promptCreatedAt(t, h, a1)
	rollout := `{"timestamp":"` + at.Format(time.RFC3339Nano) + `","type":"event_msg","payload":{"type":"task_complete",` +
		`"error":{"message":"capacity","codex_error_info":"server_overloaded"}}}` + "\n"
	if err := os.WriteFile(path, []byte(rollout), 0o644); err != nil {
		t.Fatal(err)
	}
	h.setAgents("w9:p1/idle/2")
	h.ok(nil, "daemon", "--once")
	if n := hookStallCount(t, h, a1); n != 1 {
		t.Fatalf("error turn stall for attempt %d = %d, want 1", a1, n)
	}
	for !time.Now().UTC().Truncate(time.Millisecond).After(at) {
		time.Sleep(time.Millisecond)
	}
	a2 := num(h.ok(nil, "prompt", id(w), "--text", "retry", "--receipt-timeout", "0"), "attempt_id")
	h.setAgents("w9:p1/working/3")
	h.ok(nil, "daemon", "--once")
	if n := hookStallCount(t, h, a2); n != 0 {
		t.Fatalf("re-prompt %d got the previous error turn's stall (count %d)", a2, n)
	}
}

func TestHookClientRPCNormalizedFieldsAndHostCheck(t *testing.T) {
	contractGuard(t)
	stubHookExit(t)
	r := newTwoHost(t)
	top := r.newTask("top", "orchestrator", 0)
	w := num(r.want(exitOK, "host-a", nil, "new", "host-a-worker", "--role", "sub-orchestrator", "--parent", id(top),
		"--cwd", r.dir, "--pane", "w9:p4"), "task_id")
	l := num(r.want(exitOK, "host-a", nil, "launch", id(w), "--provider", "claude", "--model", "test", "--effort", "medium"), "launch_id")
	env := hookLaneEnv(w, l, "w9:p4")
	rpcHook := func(machine, event string, payload []byte) (int, string) {
		old := os.Stdin
		in, out, err := os.Pipe()
		if err != nil {
			t.Fatal(err)
		}
		if _, err := out.Write(payload); err != nil {
			t.Fatal(err)
		}
		out.Close()
		os.Stdin = in
		code, _, stderr := r.cli(machine, env, "hook", "claude", event)
		os.Stdin = old
		in.Close()
		return code, stderr
	}
	code, stderr := rpcHook("host-a", "SessionStart", hookFixture(t, "claude-session-start.json"))
	if code != exitOK || stderr != "" {
		t.Fatalf("client hook start = %d %q", code, stderr)
	}
	attempt := int64(0)
	if err := withTx(r.openDB(), func(tx *sql.Tx) error {
		var err error
		attempt, err = insertEvent(tx, event{TaskID: w, LaunchID: ptr(l), Kind: "prompt", Summary: "stored prompt"})
		return err
	}); err != nil {
		t.Fatal(err)
	}
	secret := "LOCAL_PROMPT_MUST_NOT_CROSS_RPC"
	payload := hookPromptFixture(t, "claude-user-prompt-submit.json", attempt, " "+secret)
	parsed, ok := parseHookPayload("claude", "UserPromptSubmit", bytes.NewReader(payload))
	if !ok || strings.Contains(strings.Join(hookRPCArgs(parsed), "\n"), secret) {
		t.Fatal("normalized RPC args contain full prompt text")
	}
	code, stderr = rpcHook("host-a", "UserPromptSubmit", payload)
	if code != exitOK || stderr != "" || hookRelatedCount(t, r.harness, "got", attempt) != 1 {
		t.Fatalf("client hook receipt = %d %q", code, stderr)
	}
	if code, stderr = rpcHook("host-a", "Stop", hookFixture(t, "claude-stop.json")); code != exitOK || stderr != "" {
		t.Fatalf("client plain Stop = %d %q", code, stderr)
	}
	if n := hookStallCount(t, r.harness, attempt); n != 0 {
		t.Fatalf("client sub-orchestrator plain stall count = %d, want 0", n)
	}
	if code, stderr = rpcHook("host-a", "StopFailure", hookUncodedStopFailureFixture(t)); code != exitOK || stderr != "" {
		t.Fatalf("client uncoded StopFailure = %d %q", code, stderr)
	}
	if n := hookStallCount(t, r.harness, attempt); n != 1 {
		t.Fatalf("client sub-orchestrator error stall count = %d, want 1", n)
	}
	if data := hookStall(t, r.harness, attempt)["data"].(map[string]any); data["error"] != "unknown" {
		t.Fatalf("client uncoded error stall data = %v", data)
	}
	var all string
	rows, err := r.openDB().Query(`select coalesce(summary, '') || coalesce(data, '') from events`)
	if err != nil {
		t.Fatal(err)
	}
	for rows.Next() {
		var s string
		if err := rows.Scan(&s); err != nil {
			t.Fatal(err)
		}
		all += s
	}
	rows.Close()
	if strings.Contains(all, secret) {
		t.Fatal("full prompt text was stored by the RPC path")
	}
	if code, stderr = rpcHook("host-b", "SessionStart", hookFixture(t, "claude-session-start.json")); code != exitOK || stderr != "" {
		t.Fatalf("wrong-host hook = %d %q", code, stderr)
	}
	var bound string
	if err := r.openDB().QueryRow(`select session_ref from launches where id = ?`, l).Scan(&bound); err != nil {
		t.Fatal(err)
	}
	if bound != "claude-session" {
		t.Fatalf("wrong-host hook changed binding to %q", bound)
	}
}

func TestHookOpenCodeUncodedErrorStalls(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	_, w, l := hookLane(t, h, "opencode", "w9:p1")
	env := hookLaneEnv(w, l, "w9:p1")
	runHookPayload(t, h, env, "opencode", "session.created", hookFixture(t, "opencode-session-created.json"))
	a := num(h.ok(nil, "prompt", id(w), "--text", "work", "--receipt-timeout", "0"), "attempt_id")
	runHookPayload(t, h, env, "opencode", "chat.message", hookPromptFixture(t, "opencode-chat-message.json", a, " Continue."))
	var root map[string]any
	if err := json.Unmarshal(hookFixture(t, "opencode-session-error.json"), &root); err != nil {
		t.Fatal(err)
	}
	root["properties"].(map[string]any)["error"] = "API Error: 529 overloaded"
	payload, err := json.Marshal(root)
	if err != nil {
		t.Fatal(err)
	}
	runHookPayload(t, h, env, "opencode", "session.error", payload)
	if n := hookStallCount(t, h, a); n != 1 {
		t.Fatalf("OpenCode uncoded error stall count = %d, want 1", n)
	}
	if data := hookStall(t, h, a)["data"].(map[string]any); data["error"] != "unknown" {
		t.Fatalf("OpenCode uncoded error stall data = %v", data)
	}
}

func TestHookPiErrorCodes(t *testing.T) {
	contractGuard(t)
	for _, tc := range []struct {
		name, message, want string
	}{
		{"code", `{"stopReason":"error","diagnostics":[{"type":"provider_transport_failure","error":{"name":"Error","code":"ECONNRESET"}}]}`, "ECONNRESET"},
		{"numeric code", `{"stopReason":"error","diagnostics":[{"type":"old","error":{"code":1}},{"type":"provider_transport_failure","error":{"code":529}}]}`, "529"},
		{"type", `{"stopReason":"error","diagnostics":[{"type":"bedrock_response_failure","details":{}}]}`, "bedrock_response_failure"},
		{"uncoded", `{"stopReason":"error","errorMessage":"API Error: 529 overloaded"}`, "unknown"},
		{"aborted", `{"stopReason":"aborted","diagnostics":[{"type":"provider_transport_failure"}]}`, ""},
		{"stop", `{"stopReason":"stop"}`, ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			payload := `{"type":"agent_settled","sessionId":"pi-session","message":` + tc.message + `}`
			h, ok := parseHookPayload("pi", "agent_settled", strings.NewReader(payload))
			if !ok || h.Session != "pi-session" || h.Error != tc.want {
				t.Fatalf("pi agent_settled = %+v %v, want error %q", h, ok, tc.want)
			}
		})
	}
	if _, ok := parseHookPayload("pi", "session_start", strings.NewReader(`{"type":"session_start","sessionId":"pi-session","sessionFile":"pi.jsonl"}`)); ok {
		t.Fatal("pi relative sessionFile was accepted")
	}
}

func TestHookPiUncodedErrorStalls(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	_, w, l := hookLane(t, h, "pi", "w9:p1")
	env := hookLaneEnv(w, l, "w9:p1")
	runHookPayload(t, h, env, "pi", "session_start", hookFixture(t, "pi-session-start.json"))
	a := num(h.ok(nil, "prompt", id(w), "--text", "work", "--receipt-timeout", "0"), "attempt_id")
	runHookPayload(t, h, env, "pi", "input", hookPromptFixture(t, "pi-input.json", a, " Continue."))
	var root map[string]any
	if err := json.Unmarshal(hookFixture(t, "pi-agent-settled-error.json"), &root); err != nil {
		t.Fatal(err)
	}
	root["message"] = map[string]any{"stopReason": "error", "errorMessage": "API Error: 529 overloaded"}
	payload, err := json.Marshal(root)
	if err != nil {
		t.Fatal(err)
	}
	runHookPayload(t, h, env, "pi", "agent_settled", payload)
	if n := hookStallCount(t, h, a); n != 1 {
		t.Fatalf("pi uncoded error stall count = %d, want 1", n)
	}
	if data := hookStall(t, h, a)["data"].(map[string]any); data["error"] != "unknown" {
		t.Fatalf("pi uncoded error stall data = %v", data)
	}
}

func TestPiWorkerIgnoresInheritedCodexThread(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	_, w, l := hookLane(t, h, "pi", "w9:p1")
	env := hookLaneEnv(w, l, "w9:p1")
	env["CODEX_THREAD_ID"] = "inherited-thread"
	if start := h.ok(env, "start"); start["session_ref"] != nil {
		t.Fatalf("pi start = %v, want no inherited Codex thread", start)
	}
	runHookPayload(t, h, env, "pi", "session_start", hookFixture(t, "pi-session-start.json"))
	var session, kind, source string
	if err := h.openDB().QueryRow(`select coalesce(session_ref, ''), coalesce(session_kind, ''), coalesce(session_source, '') from launches where id = ?`, l).
		Scan(&session, &kind, &source); err != nil {
		t.Fatal(err)
	}
	if session != "pi-session" || kind != "id" || source != "hook:session_start" {
		t.Fatalf("pi launch session = %q %q %q", session, kind, source)
	}
}

func hookRelatedCount(t *testing.T, h *harness, kind string, related int64) int {
	t.Helper()
	var n int
	if err := h.openDB().QueryRow(`select count(*) from events where kind = ? and related_event_id = ?`, kind, related).Scan(&n); err != nil {
		t.Fatal(err)
	}
	return n
}

func hookStallCount(t *testing.T, h *harness, attempt int64) int {
	t.Helper()
	var n int
	if err := h.openDB().QueryRow(`select count(*) from events where event_key = ?`, "stall:"+id(attempt)).Scan(&n); err != nil {
		t.Fatal(err)
	}
	return n
}

func hookTotalEvents(t *testing.T, h *harness) int {
	t.Helper()
	var n int
	if err := h.openDB().QueryRow(`select count(*) from events`).Scan(&n); err != nil {
		t.Fatal(err)
	}
	return n
}

func hookChildEnv(values map[string]string) []string {
	var env []string
	for _, s := range os.Environ() {
		k, _, _ := strings.Cut(s, "=")
		if _, replace := values[k]; !replace {
			env = append(env, s)
		}
	}
	for k, v := range values {
		env = append(env, k+"="+v)
	}
	return env
}
