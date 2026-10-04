package main

// OpenCode stdin contract: chat.message receives {"input": <input>, "output": <output>}; session.created, session.idle, and session.error receive the event object itself.
// OpenCode: write one JSON object, then close stdin; a payload whose stdin stays open is dropped at the deadline.
// pi stdin contract: {"type": <event>, "sessionId": ..., "sessionFile": <absolute or omitted>} from ctx.sessionManager, plus the
// event's own fields: session_start {"reason"}, input {"text", "source"}, agent_settled {"message": {"stopReason", "diagnostics"}}
// for the branch's last assistant message. Same one-object, close-stdin rule.
// The hook does nothing unless both TASKR_LAUNCH and HERDR_ENV=1 are set, so the plugin must pass that env through.

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"
)

const (
	hookDeadline    = 500 * time.Millisecond
	hookSendBudget  = 350 * time.Millisecond
	hookBusyTimeout = 150 * time.Millisecond
)

// hookExit ends a hook that outlives its deadline. In-process tests swap it
// for a no-op so a slow run never exits the test binary.
var hookExit = func() { os.Exit(exitOK) }

var (
	hookReceiptRE = regexp.MustCompile(`\AFirst taskr got ([0-9]+)[.;]`)
	hookCodeRE    = regexp.MustCompile(`^[A-Za-z0-9_.-]{1,96}$`)
)

type hookRecord struct {
	Event, Session, Transcript, Error string
	Attempt                           int64
}

func hookEventSupported(harness, event string) bool {
	switch harness {
	case "claude":
		return event == "SessionStart" || event == "UserPromptSubmit" || event == "Stop" || event == "StopFailure"
	case "codex":
		return event == "SessionStart" || event == "UserPromptSubmit" || event == "Stop"
	case "opencode":
		return event == "chat.message" || event == "session.created" || event == "session.idle" || event == "session.error"
	case "pi":
		return event == "session_start" || event == "input" || event == "agent_settled"
	}
	return false
}

func hookSessionStart(event string) bool {
	return event == "SessionStart" || event == "session.created" || event == "session_start"
}

func hookPromptEvent(event string) bool {
	return event == "UserPromptSubmit" || event == "chat.message" || event == "input"
}

func hookStallEvent(event string) bool {
	return event == "Stop" || event == "StopFailure" || event == "session.idle" || event == "session.error" || event == "agent_settled"
}

func hookErrorCode(v any) string {
	switch x := v.(type) {
	case string:
		if hookCodeRE.MatchString(x) {
			return x
		}
	case map[string]any:
		for _, k := range []string{"codex_error_info", "code", "type", "name"} {
			switch v := x[k].(type) {
			case string:
				if hookCodeRE.MatchString(v) {
					return v
				}
			case map[string]any:
				if len(v) == 1 {
					for code := range v {
						if hookCodeRE.MatchString(code) {
							return code
						}
					}
				}
			}
		}
	}
	return ""
}

// piErrorCode reads an errored assistant message: the newest diagnostic's
// error code (pi allows a string or a number), else that diagnostic's type.
func piErrorCode(message map[string]any) string {
	diagnostics, _ := message["diagnostics"].([]any)
	for i := len(diagnostics) - 1; i >= 0; i-- {
		d := hookObject(diagnostics[i])
		if d == nil {
			continue
		}
		switch code := hookObject(d["error"])["code"].(type) {
		case string:
			if hookCodeRE.MatchString(code) {
				return code
			}
		case float64:
			if code == float64(int64(code)) {
				return strconv.FormatInt(int64(code), 10)
			}
		}
		if typ := hookText(d, "type"); hookCodeRE.MatchString(typ) {
			return typ
		}
	}
	return "unknown"
}

func hookObject(v any) map[string]any {
	m, _ := v.(map[string]any)
	return m
}

func hookText(m map[string]any, key string) string {
	s, _ := m[key].(string)
	return s
}

func parseHookPayload(harness, event string, r io.Reader) (hookRecord, bool) {
	if !hookEventSupported(harness, event) {
		return hookRecord{}, false
	}
	dec := json.NewDecoder(io.LimitReader(r, 8<<20))
	var root map[string]any
	if dec.Decode(&root) != nil || root == nil {
		return hookRecord{}, false
	}
	var extra any
	if dec.Decode(&extra) != io.EOF {
		return hookRecord{}, false
	}
	h := hookRecord{Event: event}
	switch harness {
	case "claude", "codex":
		if name := hookText(root, "hook_event_name"); name != "" && name != event {
			return hookRecord{}, false
		}
		h.Session = hookText(root, "session_id")
		h.Transcript = hookText(root, "transcript_path")
		if hookPromptEvent(event) {
			h.Attempt = receiptAttempt(hookText(root, "prompt"))
		}
		if event == "StopFailure" {
			h.Error = hookErrorCode(root["error"])
			if h.Error == "" {
				h.Error = "unknown"
			}
		}
	case "opencode":
		if event == "chat.message" {
			input, output := hookObject(root["input"]), hookObject(root["output"])
			h.Session = hookText(input, "sessionID")
			message := hookObject(output["message"])
			if h.Session == "" {
				h.Session = hookText(message, "sessionID")
			}
			if h.Attempt == 0 {
				if parts, ok := output["parts"].([]any); ok {
					for _, part := range parts {
						p := hookObject(part)
						if hookText(p, "type") == "text" {
							h.Attempt = receiptAttempt(hookText(p, "text"))
							break
						}
					}
				}
			}
		} else {
			if hookText(root, "type") != event {
				return hookRecord{}, false
			}
			props := hookObject(root["properties"])
			h.Session = hookText(props, "sessionID")
			if h.Session == "" {
				h.Session = hookText(hookObject(props["info"]), "id")
			}
			if event == "session.error" {
				h.Error = hookErrorCode(props["error"])
				if h.Error == "" {
					h.Error = "unknown"
				}
			}
		}
	case "pi":
		if hookText(root, "type") != event {
			return hookRecord{}, false
		}
		h.Session = hookText(root, "sessionId")
		h.Transcript = hookText(root, "sessionFile")
		switch event {
		case "input":
			h.Attempt = receiptAttempt(hookText(root, "text"))
		case "agent_settled":
			if message := hookObject(root["message"]); hookText(message, "stopReason") == "error" {
				h.Error = piErrorCode(message)
			}
		}
	}
	if h.Session == "" || (hookPromptEvent(event) && h.Attempt == 0) {
		if !hookSessionStart(event) && !hookStallEvent(event) {
			return hookRecord{}, false
		}
		if h.Session == "" {
			return hookRecord{}, false
		}
	}
	if !absoluteHookPath(h.Transcript) {
		return hookRecord{}, false
	}
	return h, true
}

func receiptAttempt(prompt string) int64 {
	m := hookReceiptRE.FindStringSubmatch(prompt)
	if len(m) != 2 {
		return 0
	}
	n, err := strconv.ParseInt(m[1], 10, 64)
	if err != nil || n <= 0 {
		return 0
	}
	return n
}

// cmdHook is a fire-and-forget harness entry point. Keep every failure silent:
// hooks run inside the harness turn and must never block it.
func cmdHook(c *ctx, args []string) (res any, code int, err error) {
	started := time.Now()
	if c != nil {
		c.lines = true
	}
	if len(args) == 1 && (args[0] == "--help" || args[0] == "-h") {
		fmt.Fprintln(c.out, commandUsageLine("hook"))
		return nil, exitOK, errHelp
	}
	exit := hookExit
	timer := time.AfterFunc(hookDeadline, exit)
	defer timer.Stop()
	defer func() {
		if recover() != nil {
			res, code, err = nil, exitOK, nil
		}
	}()
	if c.env("TASKR_LAUNCH") == "" || c.env("HERDR_ENV") != "1" || len(args) != 2 {
		return nil, exitOK, nil
	}
	if !hookEventSupported(args[0], args[1]) {
		return nil, exitOK, nil
	}
	parent := c.cx
	if parent == nil {
		parent = context.Background()
	}
	cx, cancel := context.WithTimeout(parent, hookDeadline)
	defer cancel()
	h, ok := parseHookPayload(args[0], args[1], os.Stdin)
	if !ok {
		return nil, exitOK, nil
	}
	if c.client {
		sendHookRPC(c, h, started)
		return nil, exitOK, nil
	}
	db := c.db
	if db == nil {
		db, err = openHookDB(c, cx)
		if err != nil || db == nil {
			return nil, exitOK, nil
		}
		defer db.Close()
	}
	_ = applyHookRecord(c, db, cx, h)
	return nil, exitOK, nil
}

func openHookDB(c *ctx, cx context.Context) (*sql.DB, error) {
	p, err := dbPath(c)
	if err != nil {
		return nil, err
	}
	if _, err := os.Stat(p); err != nil {
		return nil, nil
	}
	q := url.Values{}
	q.Add("_pragma", "busy_timeout("+strconv.FormatInt(hookBusyTimeout.Milliseconds(), 10)+")")
	q.Add("_pragma", "foreign_keys(1)")
	q.Add("_txlock", "immediate")
	q.Set("mode", "rw")
	u := url.URL{Scheme: "file", Path: p}
	db, err := sql.Open("sqlite", u.String()+"?"+q.Encode())
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	if err := db.PingContext(cx); err != nil {
		db.Close()
		return nil, err
	}
	if _, err := db.ExecContext(cx, "pragma busy_timeout = "+strconv.Itoa(int(hookBusyTimeout.Milliseconds()))); err != nil {
		db.Close()
		return nil, err
	}
	return db, nil
}

func applyHookRecord(c *ctx, db *sql.DB, cx context.Context, h hookRecord) error {
	tx, err := db.BeginTx(cx, nil)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	w, err := resolveWorker(c, tx)
	if err != nil || w.launchID == nil {
		return err
	}
	var provider, pane string
	var session, source sql.NullString
	err = tx.QueryRow(`select coalesce(l.provider, ''), coalesce(l.pane_id, t.pane_id, ''),
		l.session_ref, l.session_source from launches l join tasks t on t.id = l.task_id where l.id = ?`, *w.launchID).
		Scan(&provider, &pane, &session, &source)
	if err != nil {
		return err
	}
	herdrPane := c.env("HERDR_PANE_ID")
	if herdrPane == "" || herdrPane != pane || h.Session == "" {
		return nil
	}
	if hookSessionStart(h.Event) {
		if strings.HasPrefix(source.String, "hook:") {
			return nil
		}
		kind := "id"
		if provider == "codex" {
			kind = "thread_id"
		}
		if _, err := tx.Exec(`update launches set session_ref = ?, session_kind = ?, session_source = ?, transcript_path = ? where id = ?`,
			h.Session, kind, "hook:"+h.Event, nullStr(h.Transcript), *w.launchID); err != nil {
			return err
		}
		return tx.Commit()
	}
	if !strings.HasPrefix(source.String, "hook:") || !session.Valid || session.String != h.Session {
		return nil
	}
	if hookPromptEvent(h.Event) {
		identity := func(tx *sql.Tx, launch int64) (map[string]any, error) {
			return map[string]any{"session_ref": session.String, "session_source": "hook:" + h.Event,
				"transcript_path": nullStringValueForHook(tx, launch)}, nil
		}
		_, err := recordGot(c, tx, w, h.Attempt, identity)
		if err != nil {
			return err
		}
	} else if hookStallEvent(h.Event) {
		if err := writeHookStall(c, tx, w, *w.launchID, h.Error); err != nil {
			return err
		}
	} else {
		return nil
	}
	return tx.Commit()
}

func nullStringValueForHook(tx *sql.Tx, launch int64) any {
	var path sql.NullString
	if err := tx.QueryRow(`select transcript_path from launches where id = ?`, launch).Scan(&path); err != nil || !path.Valid {
		return nil
	}
	return path.String
}

func writeHookStall(c *ctx, tx *sql.Tx, w *worker, launch int64, errorCode string) error {
	if errorCode == "" {
		var role string
		if err := tx.QueryRow(`select role from tasks where id = ?`, w.task.ID).Scan(&role); err != nil {
			return err
		}
		if role == "orchestrator" || role == "sub-orchestrator" {
			return nil
		}
	}
	var prompt int64
	owed, err := owes(tx, launch, &prompt)
	if err != nil || !owed {
		return err
	}
	var ended bool
	if err := tx.QueryRow(`select exists (select 1 from events where launch_id = ? and id > ? and kind in ('done', 'fail', 'ask'))`,
		launch, prompt).Scan(&ended); err != nil {
		return err
	}
	if ended {
		return nil
	}
	key := "stall:" + strconv.FormatInt(prompt, 10)
	var exists bool
	if err := tx.QueryRow(`select exists (select 1 from events where event_key = ?)`, key).Scan(&exists); err != nil {
		return err
	}
	if exists {
		return nil
	}
	data := map[string]any{"reason": "stall"}
	if errorCode != "" {
		data["error"] = errorCode
	}
	var last sql.NullString
	if err := tx.QueryRow(`select kind from events where launch_id = ? and kind in ('ready', 'done', 'fail', 'ask') order by id desc limit 1`,
		launch).Scan(&last); err != nil && err != sql.ErrNoRows {
		return err
	}
	if last.Valid {
		if code := map[string]string{"ready": "r", "done": "d", "fail": "f", "ask": "q"}[last.String]; code != "" {
			data["last"] = code
		}
	}
	var insert func(*sql.Tx, event) (int64, error) = insertEvent
	if c != nil && c.rpc && c.machine != "" {
		insert = c.insertEvent
	}
	_, err = insert(tx, event{TaskID: w.task.ID, RecipientTaskID: parentRecipient(w.task), LaunchID: ptr(launch),
		Kind: "herdr", Summary: "worker turn stalled", Data: data, EventKey: key})
	return err
}

func hookRPCArgs(h hookRecord) []string {
	args := []string{"--json", "_hook", h.Event, "--session", h.Session}
	if h.Transcript != "" {
		args = append(args, "--transcript", h.Transcript)
	}
	if h.Attempt > 0 {
		args = append(args, "--attempt", strconv.FormatInt(h.Attempt, 10))
	}
	if h.Error != "" {
		args = append(args, "--error", h.Error)
	}
	return args
}

func queueHookRecordUntil(dir string, req rpcRequest, deadline time.Time) {
	for {
		_, err := queueSpoolRecordMode(dir, req, nil, false)
		if err != errSpoolBusy || time.Until(deadline) <= 0 {
			return
		}
		time.Sleep(min(5*time.Millisecond, time.Until(deadline)))
	}
}

func queueHookIfWaitingUntil(dir string, req rpcRequest, deadline time.Time) (int, bool, bool, error) {
	for {
		count, queued, err := queueSpoolIfWaitingMode(dir, req, nil, false)
		if err != errSpoolBusy || time.Until(deadline) <= 0 {
			return count, queued, err == errSpoolBusy, err
		}
		time.Sleep(min(5*time.Millisecond, time.Until(deadline)))
	}
}

func sendHookRPC(c *ctx, h hookRecord, started time.Time) {
	cwd, err := os.Getwd()
	if err != nil {
		return
	}
	env := map[string]string{}
	for _, k := range []string{"TASKR_TASK", "TASKR_LAUNCH", "HERDR_PANE_ID"} {
		if v := c.env(k); v != "" {
			env[k] = v
		}
	}
	req := rpcClientRequest(hookRPCArgs(h), cwd, newRequestKey(), env, nil)
	dir, err := stateDir(c)
	if err != nil {
		return
	}
	sendDeadline := started.Add(hookSendBudget)
	processDeadline := started.Add(hookDeadline)
	count, queued, busy, queueErr := queueHookIfWaitingUntil(dir, req, started.Add(hookBusyTimeout))
	if queued || queueErr != nil && !busy {
		return
	}
	if busy {
		if countSpoolFiles(filepath.Join(spoolPath(dir), spoolQueueDir)) > 0 || count > 0 {
			queueHookRecordUntil(dir, req, processDeadline)
			return
		}
	}
	type clientResult struct {
		client *rpcClient
		err    *exitErr
	}
	clientCh := make(chan clientResult, 1)
	go func() {
		cl, e := newRPCClient(c.server)
		clientCh <- clientResult{client: cl, err: e}
	}()
	var cl *rpcClient
	select {
	case result := <-clientCh:
		if result.err != nil {
			if result.err.kind == "transport" {
				queueHookRecordUntil(dir, req, processDeadline)
			}
			return
		}
		cl = result.client
	case <-time.After(max(0, time.Until(sendDeadline))):
		queueHookRecordUntil(dir, req, processDeadline)
		return
	}
	cx, cancel := context.WithDeadline(context.Background(), sendDeadline)
	defer cancel()
	_, callErr, noReply, _ := cl.callOnceRequest(cx, req, true)
	if callErr != nil && (noReply || spoolTransportError(callErr)) {
		queueHookRecordUntil(dir, req, processDeadline)
	}
}

func cmdHookRPC(c *ctx, args []string) (res any, code int, err error) {
	if c != nil {
		c.lines = true
	}
	defer func() {
		if recover() != nil {
			res, code, err = nil, exitOK, nil
		}
	}()
	if hiddenOnly(c, "_hook") != nil || c.db == nil || len(args) < 3 {
		return nil, exitOK, nil
	}
	h := hookRecord{Event: args[0]}
	if !hookStallEvent(h.Event) && !hookSessionStart(h.Event) && !hookPromptEvent(h.Event) {
		return nil, exitOK, nil
	}
	for i := 1; i+1 < len(args); i += 2 {
		v := args[i+1]
		switch args[i] {
		case "--session":
			h.Session = v
		case "--transcript":
			h.Transcript = v
		case "--attempt":
			h.Attempt, _ = strconv.ParseInt(v, 10, 64)
		case "--error":
			h.Error = hookErrorCode(v)
		default:
			return nil, exitOK, nil
		}
	}
	if h.Session == "" || !absoluteHookPath(h.Transcript) || (hookPromptEvent(h.Event) && h.Attempt <= 0) {
		return nil, exitOK, nil
	}
	if hookStallEvent(h.Event) && c.queuedAgeMS > int64((10*time.Minute)/time.Millisecond) {
		fmt.Fprintln(c.out, "expired")
		return nil, exitOK, nil
	}
	parent := c.cx
	if parent == nil {
		parent = context.Background()
	}
	cx, cancel := context.WithTimeout(parent, hookDeadline)
	defer cancel()
	_ = applyHookRecord(c, c.db, cx, h)
	return nil, exitOK, nil
}

func absoluteHookPath(s string) bool { return s == "" || filepath.IsAbs(s) }
