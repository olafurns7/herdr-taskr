package main

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"fmt"
	"reflect"
	"strings"
	"testing"
)

func (h *harness) compact(env map[string]string, args ...string) (int, string, string) {
	h.t.Helper()
	var out, errb bytes.Buffer
	code := run(args, h.getenv(env), &out, &errb)
	return code, out.String(), errb.String()
}

func decodeFrame(t *testing.T, raw string) map[string]any {
	t.Helper()
	_, body, ok := strings.Cut(strings.TrimSpace(raw), " ")
	var m map[string]any
	if !ok || json.Unmarshal([]byte(body), &m) != nil {
		t.Fatalf("invalid frame %q", raw)
	}
	return m
}

func TestCompactDefaultAndJSONPin(t *testing.T) {
	h := newHarness(t)
	code, raw, diag := h.compact(nil, "new", "top", "--role", "orchestrator")
	if code != exitOK || raw != "n1 1\n" || diag != "taskr: no goal recorded for root 1; run `taskr doc set 1 goal --file PATH`\ntaskr: lead has no pane; run `taskr adopt 1 --pane <id>` as your first act\n" {
		t.Fatalf("new = %d %q %q", code, raw, diag)
	}
	for _, args := range [][]string{{"--json", "version"}, {"version", "--json"}} {
		code, out, _ := h.compact(nil, args...)
		if code != exitOK || out != fmt.Sprintf("{\"version\":%q,\"ok\":true}\n", version) {
			t.Fatalf("JSON contract = %d %q", code, out)
		}
	}

	if code, out, _ := h.compact(nil, "--json", "version", "extra"); code != exitUsage || out != "{\"error\":\"version: takes no arguments\",\"kind\":\"usage\"}\n" {
		t.Fatalf("version error contract = %d %q", code, out)
	}
	code, out, _ := h.compact(map[string]string{"TASKR_FORMAT": "json"}, "status", "--tree", "1")
	_, pinned, _ := h.compact(nil, "status", "--tree", "1", "--json")
	if code != exitOK || out != pinned || !strings.HasPrefix(out, `{"`) {
		t.Fatalf("TASKR_FORMAT=json = %d %q, flag %q", code, out, pinned)
	}
	for _, args := range [][]string{{"bogus"}, {"ready", "--no-such-flag"}, {"ack", "-2", "--as", "1"}} {
		code, out, diag := h.compact(nil, args...)
		if code != exitUsage || !strings.HasPrefix(out, "x1 2 ") || diag != "" || strings.Count(out, "\n") != 1 || strings.Contains(out, "usage: taskr") {
			t.Fatalf("single error = %d %q %q", code, out, diag)
		}
	}
}

func TestCompactWaitTimeoutAndJSONPin(t *testing.T) {
	for _, mode := range []struct {
		name          string
		env           map[string]string
		prefix, flags []string
		legacy        bool
	}{
		{name: "compact"},
		{name: "global-json", env: map[string]string{"TASKR_FORMAT": "compact"}, prefix: []string{"--json"}, legacy: true},
		{name: "command-json", env: map[string]string{"TASKR_FORMAT": "compact"}, flags: []string{"--json"}, legacy: true},
		{name: "env-json", env: map[string]string{"TASKR_FORMAT": "json"}, legacy: true},
		{name: "command-overrides-env", env: map[string]string{"TASKR_FORMAT": "json"}, flags: []string{"--json=false"}},
	} {
		for _, filtered := range []bool{false, true} {
			for _, timeout := range []string{"0", "20"} {
				t.Run(fmt.Sprintf("%s/filter=%t/timeout=%s", mode.name, filtered, timeout), func(t *testing.T) {
					h := newHarness(t)
					top := h.newTask("top", "orchestrator", 0)
					args := append(append([]string{}, mode.prefix...), "wait", "--as", id(top), "--timeout", timeout)
					if filtered {
						args = append(args, "--for", "ready")
					}
					args = append(args, mode.flags...)
					wantCode, wantOut := exitOK, "w1 {\"owed\":0,\"due\":0}\nx1 3 timeout\n"
					if mode.legacy {
						wantCode, wantOut = exitTimeout, fmt.Sprintf("{\"as\":%d,\"due\":0,\"owed\":0,\"timeout\":true}\n", top)
					}
					code, raw, diag := h.compact(mode.env, args...)
					if code != wantCode || raw != wantOut || diag != "" || h.waitingUntil(top).Valid {
						t.Fatalf("wait = %d %q %q; want %d %q, cleared waiting", code, raw, diag, wantCode, wantOut)
					}
					t.Logf("exit=%d stdout=%q stderr=%q", code, raw, diag)
				})
			}
		}
	}
}

func TestWaitTimeoutOutputForms(t *testing.T) {
	for _, tc := range []struct {
		name    string
		in      map[string]any
		compact string
		json    string
	}{
		{"quiet", map[string]any{"as": 1, "timeout": true, "owed": 0, "due": 0},
			"w1 {\"owed\":0,\"due\":0}\nx1 3 timeout\n",
			"{\"as\":1,\"due\":0,\"owed\":0,\"timeout\":true}\n"},
		{"interrupted", map[string]any{"as": 1, "timeout": true, "interrupted": true},
			"x1 3 timeout interrupted\n",
			"{\"as\":1,\"interrupted\":true,\"timeout\":true}\n"},
		{"unreachable", map[string]any{"as": 1, "timeout": true, "unreachable": true},
			"x1 3 timeout unreachable\n",
			"{\"as\":1,\"timeout\":true,\"unreachable\":true}\n"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			for _, jsonFormat := range []bool{false, true} {
				var out bytes.Buffer
				c := &ctx{cmd: "wait", out: &out, json: jsonFormat}
				c.emit(tc.in)
				want := tc.compact
				if jsonFormat {
					want = tc.json
				}
				if out.String() != want {
					t.Fatalf("json=%t output = %q, want %q", jsonFormat, out.String(), want)
				}
			}
		})
	}
}

func TestCompactWaitTimeoutCountsOwedAndDue(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("worker", "implementer", top)
	h.launch(w)
	h.ok(nil, "prompt", id(w), "--text", "Go.")
	code, raw, diag := h.compact(nil, "wait", "--as", id(top), "--timeout", "0")
	want := "w1 {\"owed\":1,\"due\":1}\nx1 3 timeout\n"
	if code != exitOK || raw != want || diag != "" {
		t.Fatalf("timeout counts = %d %q %q, want %q", code, raw, diag, want)
	}
	jsonResult := h.one(exitTimeout, nil, "wait", "--as", id(top), "--timeout", "0")
	if num(jsonResult, "owed") != 1 || num(jsonResult, "due") != 1 {
		t.Fatalf("JSON timeout counts = %v", jsonResult)
	}
}

func TestCompactEventEscapesAndOpaquePayload(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top)
	l := h.launch(w)
	summary := "tabs\tnewlines\nUnicode: Þ 😀 \"quote\" \\"
	eid := num(h.ok(as(w, l), "ready", summary, "--kv", "task_id=opaque", "--kv", "summary=retain"), "event_id")
	code, raw, _ := h.compact(nil, "wait", "--as", id(top), "--timeout", "0")
	cols := strings.Split(strings.TrimSuffix(raw, "\n"), "\t")
	if code != exitOK || len(cols) != 9 || cols[0] != "e1" || cols[1] != id(eid) || cols[4] != "r" || cols[5] != "0" || cols[6] != "-" {
		t.Fatalf("event = %d %q", code, raw)
	}
	var text string
	var data map[string]any
	if json.Unmarshal([]byte(cols[7]), &text) != nil || text != summary || json.Unmarshal([]byte(cols[8]), &data) != nil {
		t.Fatalf("escaped fields %q", raw)
	}
	if !reflect.DeepEqual(data["kv"], map[string]any{"task_id": "opaque", "summary": "retain"}) {
		t.Fatalf("opaque payload renamed: %v", data)
	}
	_, replay, _ := h.compact(nil, "wait", "--as", id(top), "--timeout", "0")
	if strings.Split(replay, "\t")[5] != "1" {
		t.Fatalf("matching event auto-acked: %q", replay)
	}
}

func TestCompactReadsLossless(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top)
	l := h.launch(w)
	h.ok(as(w, l), "ask", "Which pin?", "--owner")
	h.ok(as(w, l), "start")
	for _, args := range [][]string{{"status", "--tree", id(top), "--all"}, {"asks", "--tree", id(top), "--limit", "0"}, {"log", id(top), "--tree", "--limit", "0"}} {
		_, original := h.run(nil, args...)
		code, raw, _ := h.compact(nil, args...)
		lines := strings.Split(strings.TrimSpace(raw), "\n")
		if code != exitOK || len(lines) != len(original) {
			t.Fatalf("read = %d %q", code, raw)
		}
		for i, line := range lines {
			want := aliases(original[i], readAliases)
			if obs, ok := original[i]["observed"].(map[string]any); ok {
				want["h"] = aliases(obs, map[string]string{"agent_status": "s", "state_change_seq": "seq"})
			}
			if got := decodeFrame(t, line); !reflect.DeepEqual(got, want) || len(got) != len(original[i]) {
				t.Fatalf("lossless read\ngot %v\nwant %v", got, want)
			}
		}
	}
}

func TestCompactReadFormat1Pinned(t *testing.T) {
	// Expected wire keys come from references/format.md, independently of
	// readAliases. Absent, empty, null and false fields must stay distinct.
	for _, tc := range []struct {
		cmd  string
		in   map[string]any
		want string
	}{
		{"log", map[string]any{"id": int64(9007199254740993), "summary": "", "data": nil},
			"j1 {\"d\":null,\"i\":9007199254740993,\"s\":\"\"}\n"},
		{"asks", map[string]any{"id": int64(3)}, "j1 {\"i\":3}\n"},
		{"status", map[string]any{"waiting": false, "observed": map[string]any{
			"agent_status": "", "state_change_seq": int64(0), "at": "", "present": false}},
			"j1 {\"h\":{\"at\":\"\",\"present\":false,\"s\":\"\",\"seq\":0},\"wait\":false}\n"},
	} {
		t.Run(tc.cmd, func(t *testing.T) {
			var out bytes.Buffer
			c := &ctx{cmd: tc.cmd, out: &out}
			c.emitCompact(tc.in)
			if out.String() != tc.want {
				t.Fatalf("format1 = %q, want %q", out.String(), tc.want)
			}
		})
	}
}

func TestCompactTransportDiagnostics(t *testing.T) {
	for _, tc := range []struct {
		name, stderr, diagnostic, code, outcome string
	}{
		{"structured", `{"error":{"code":"agent_blocked","message":"Approval dialog requires owner decision: command xyz"}}`,
			"Approval dialog requires owner decision: command xyz", "agent_blocked", "rejected"},
		{"non-JSON", "Transport failed\nDetails: socket closed\n", "Transport failed\nDetails: socket closed", "", "delivery_unknown"},
	} {
		for _, command := range []string{"prompt", "answer"} {
			for _, legacy := range []bool{false, true} {
				t.Run(fmt.Sprintf("%s/%s/json=%t", tc.name, command, legacy), func(t *testing.T) {
					h := newHarness(t)
					top := h.newTask("top", "orchestrator", 0)
					w := h.newTask("lane", "implementer", top, "--pane", "w9:p1")
					l := h.launch(w)
					h.write("prompt.stderr", tc.stderr, 0o644)
					h.write("prompt.exit", "1", 0o644)
					args := []string{"prompt", id(w), "--text", "go"}
					var ask int64
					if command == "answer" {
						ask = num(h.ok(as(w, l), "ask", "which pin?"), "ask_id")
						args = []string{"answer", id(ask), "main", "--prompt"}
					}
					if legacy {
						args = append([]string{"--json"}, args...)
					}
					code, raw, diag := h.compact(nil, args...)
					if code != exitHerdr || strings.Count(raw, "\n") != 1 {
						t.Fatalf("transport = %d %q %q", code, raw, diag)
					}
					if !legacy {
						if !strings.HasPrefix(raw, "x1 5 ") || diag != "" {
							t.Fatalf("single compact error = %q %q", raw, diag)
						}
						m := decodeFrame(t, "j1 "+strings.TrimPrefix(raw, "x1 5 "))
						if m["herdr_message"] != tc.diagnostic || m["o"] != tc.outcome || num(m, "p") == 0 || num(m, "oe") == 0 {
							t.Fatalf("diagnostic lost: %v", m)
						}
						if tc.code != "" && m["herdr_error"] != tc.code {
							t.Fatalf("error code lost: %v", m)
						}
						if command == "answer" && (num(m, "a") == 0 || m["sent"] != false || m["w"] != false) {
							t.Fatalf("answer recovery data lost: %v", m)
						}
						return
					}
					var attempt, outcome, answer int64
					db := h.openDB()
					if err := db.QueryRow(`select id from events where kind = 'prompt'`).Scan(&attempt); err != nil {
						t.Fatal(err)
					}
					if err := db.QueryRow(`select id from events where kind = 'prompt_outcome'`).Scan(&outcome); err != nil {
						t.Fatal(err)
					}
					msg := fmt.Sprintf("prompt attempt %d: %s; inspect the agent before any resend", attempt, tc.outcome)
					want := map[string]any{"task_id": w, "attempt_id": attempt, "target": "w9:p1", "outcome": tc.outcome,
						"outcome_event_id": outcome, "herdr_exit": 1, "ok": false, "error": msg, "kind": "herdr"}
					if tc.code != "" {
						want["herdr_error"] = tc.code
					}
					if command == "answer" {
						if err := db.QueryRow(`select answered_by from events where id = ?`, ask).Scan(&answer); err != nil {
							t.Fatal(err)
						}
						want["ask_id"], want["answer_id"], want["delivered"], want["asker_waiting"] = ask, answer, false, false
					}
					if raw != jsonText(want)+"\n" || diag != "herdr: "+strings.TrimSpace(tc.stderr)+"\ntaskr "+command+": "+msg+"\n" {
						t.Fatalf("legacy diagnostic contract = %q %q", raw, diag)
					}
				})
			}
		}
	}
}

func TestCompactExceptionalMutations(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top, "--planned", "--pane", "w9:p1")
	code, raw, _ := h.compact(nil, "launch", id(w), "--provider", "codex", "--model", "m", "--effort", "e")
	m := decodeFrame(t, raw)
	if code != exitOK || m["was_planned"] != true || m["status"] != "open" {
		t.Fatalf("planned launch %q", raw)
	}
	l := num(m, "launch_id")
	_, raw, _ = h.compact(nil, "launch", id(w), "--provider", "codex", "--model", "m", "--effort", "e")
	m = decodeFrame(t, raw)
	if num(m, "replaced_launch_id") != l {
		t.Fatalf("replacement %q", raw)
	}
	l = num(m, "launch_id")
	_, first, _ := h.compact(as(w, l), "ready", "one", "--key", "unique")
	_, again, _ := h.compact(as(w, l), "ready", "one", "--key", "unique")
	if again != strings.TrimSuffix(first, "\n")+" dup\n" {
		t.Fatalf("duplicate %q %q", first, again)
	}
	_, cleared, _ := h.compact(nil, "next", id(w), "--clear")
	if !strings.HasSuffix(cleared, " clear\n") {
		t.Fatalf("clear %q", cleared)
	}
	_, decision, _ := h.compact(nil, "decide", "--as", id(top), "rule")
	_, revoked, _ := h.compact(nil, "decide", "--as", id(top), "--revoke", id(num(decodeFrame(t, decision), "e")))
	if decodeFrame(t, revoked)["revoked"] == nil {
		t.Fatalf("revoke %q", revoked)
	}
	h.write("prompt.stdout", `{"result":{"agent":{"agent_status":"working"}}}`, 0o644)
	ask := num(h.ok(as(w, l), "ask", "pin?"), "ask_id")
	code, raw, diag := h.compact(nil, "answer", id(ask), "main", "--prompt", "--confirm", "--confirm-timeout", "0")
	parts := strings.SplitN(strings.TrimSpace(raw), " ", 3)
	m = decodeFrame(t, "j1 "+parts[2])
	if code != exitHerdr || diag != "" || m["k"] != "no_receipt" || m["sent"] != true || m["w"] != false || m["gok"] != false || num(m, "a") == 0 || num(m, "p") == 0 || num(m, "oe") == num(m, "delivery_outcome_event_id") {
		t.Fatalf("partial answer failure = %d %q %q", code, raw, diag)
	}
}

func TestCompactReceiptAndOwnerAnswerKeepRecoveryData(t *testing.T) {
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top, "--pane", "w9:p1")
	l := num(h.ok(nil, "launch", id(w), "--provider", "codex", "--model", "m", "--effort", "e"), "launch_id")
	attempt := num(h.ok(nil, "prompt", id(w), "--text", "Keep full payload."), "attempt_id")
	env := as(w, l)
	env["CODEX_HOME"] = "/home/user/.local/share/agent/provider/account-a/native"
	env["CODEX_THREAD_ID"] = "session-123"
	env["HERDR_PANE_ID"] = "w9:p1"
	code, got, _ := h.compact(env, "got", id(attempt))
	if code != exitOK || !strings.HasSuffix(got, " 1\n") {
		t.Fatalf("receipt %d %q", code, got)
	}
	_, dup, _ := h.compact(env, "got", id(attempt))
	if dup != strings.TrimSuffix(got, "\n")+" dup\n" {
		t.Fatalf("duplicate receipt %q", dup)
	}
	_, raw, _ := h.compact(nil, "wait", "--as", id(top), "--for", "got", "--timeout", "0")
	cols := strings.Split(strings.TrimSpace(raw), "\t")
	var data map[string]any
	json.Unmarshal([]byte(cols[8]), &data)
	if cols[6] != id(attempt) || cols[7] != `""` || data["identity"] != nil || data["round"] != float64(1) {
		t.Fatalf("got frame %q", raw)
	}
	_, log, _ := h.compact(nil, "log", id(w))
	if !strings.Contains(log, `"native_home":"/home/user/.local/share/agent/provider/account-a/native"`) || !strings.Contains(log, `"session_ref":"session-123"`) || !strings.Contains(log, `"sha256":`) {
		t.Fatalf("lost recovery data: %s", log)
	}
	h.ok(nil, "ack", cols[1], "--as", id(top))
	ask := num(h.ok(env, "ask", "owner pin?", "--owner"), "ask_id")
	db := h.openDB()
	var answer answerResult
	// Seed an old dashboard notice; v0.7 only writes CLI answers now.
	err := withTx(db, func(tx *sql.Tx) (err error) {
		answer, err = answerAsk(tx, ask, "main")
		if err != nil {
			return err
		}
		_, err = insertEvent(tx, event{TaskID: w, RecipientTaskID: &top, Kind: "owner_answer",
			Summary: "main", RelatedEventID: &ask, Data: map[string]any{"ask_id": ask, "answer_id": answer.answerID,
				"asker_task_id": w, "asker_waiting": answer.waiting}})
		return err
	})
	if err != nil {
		t.Fatal(err)
	}
	code, raw, _ = h.compact(nil, "wait", "--as", id(top), "--for", "owner_answer", "--timeout", "0")
	cols = strings.Split(strings.TrimSpace(strings.SplitN(raw, "\n", 2)[1]), "\t")
	json.Unmarshal([]byte(cols[8]), &data)
	if code != exitOK || !strings.HasPrefix(raw, "sk1 1\n") || cols[4] != "oa" || cols[6] != id(ask) || data["asker_waiting"] != false || data["answer_id"] != float64(answer.answerID) {
		t.Fatalf("owner answer %d %q", code, raw)
	}
}
