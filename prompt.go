package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

// herdrPromptDeadline bounds the herdr subprocess; herdr's own --timeout is 20 s.
var herdrPromptDeadline = 30 * time.Second

func cmdPrompt(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("prompt", flag.ContinueOnError)
	file := fs.String("file", "", "prompt file; the agent is told to read it")
	text := fs.String("text", "", "literal prompt text")
	confirm := fs.Bool("confirm", false, "after observed activity, wait for the worker's taskr got receipt")
	confirmTimeout := fs.Int64("confirm-timeout", 60000, "milliseconds to wait for the receipt")
	receiptTimeout := fs.Int64("receipt-timeout", defaultReceiptTimeout.Milliseconds(),
		"without --confirm: milliseconds until a missing receipt alarms the parent; 0 disarms")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	id, err := parseID(pos[0], "task id")
	if err != nil {
		return nil, 0, err
	}
	if (*file == "") == (*text == "") {
		return nil, 0, usageErr("prompt needs exactly one of --file and --text")
	}
	if *confirmTimeout < 0 {
		return nil, 0, usageErr("--confirm-timeout must not be negative")
	}
	if err := checkReceiptTimeout(*receiptTimeout, flagWasSet(fs, "receipt-timeout"), *confirm); err != nil {
		return nil, 0, err
	}
	data, abs := map[string]any{}, ""
	var fileBody []byte
	if *file != "" {
		var err error
		fileBody, err = os.ReadFile(*file)
		if err != nil {
			return nil, 0, usageErr("read prompt file: %v", err)
		}
		abs, err = filepath.Abs(*file)
		if err != nil {
			return nil, 0, usageErr("prompt file path: %v", err)
		}
		sum := sha256.Sum256(fileBody)
		data = map[string]any{"file": abs, "sha256": hex.EncodeToString(sum[:]), "bytes": len(fileBody)}
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	d := promptDelivery(*text, abs, data, *confirm, time.Duration(*confirmTimeout)*time.Millisecond)
	if abs != "" {
		in := bodyDocument(fileBody, abs)
		d.document = &in
	}
	d.receiptTimeout = time.Duration(*receiptTimeout) * time.Millisecond
	return deliver(c, db, id, d)
}

// promptDelivery is a prompt command's delivery: the text, or with abs a
// file the agent is told to read.
func promptDelivery(text, abs string, data map[string]any, confirm bool, timeout time.Duration) delivery {
	compose := func(attempt int64) string { return fmt.Sprintf("First taskr got %d. %s", attempt, text) }
	if abs != "" {
		compose = func(attempt int64) string {
			return fmt.Sprintf("First taskr got %d; read %s; execute exactly.", attempt, abs)
		}
	}
	in := bodyDocument([]byte(text), "")
	if abs != "" {
		in = documentInput{Path: abs, Reason: "client"}
		in.Hash, _ = data["sha256"].(string)
		if n, ok := data["bytes"].(int64); ok {
			in.Bytes = ptr(n)
		}
	}
	return delivery{document: &in, compose: compose, body: text, data: data, reopen: true, confirm: confirm, confirmTimeout: timeout,
		receiptTimeout: defaultReceiptTimeout}
}

// delivery is one prompt to send. compose builds the sent text from the
// attempt id; body is the caller's text, hashed unless data already has a
// file's hash. Without confirm, a positive receiptTimeout arms a receipt
// deadline whose alarm goes to receiptRecipient, or else the target's parent.
type delivery struct {
	document         *documentInput
	compose          func(attempt int64) string
	body             string
	data             map[string]any
	related          *int64
	reopen           bool
	confirm          bool
	confirmTimeout   time.Duration
	receiptTimeout   time.Duration
	receiptRecipient *int64
}

// deliver records a prompt attempt, runs herdr agent prompt once, and records
// the observed outcome. It never retries. With confirm, observed activity is
// followed by a wait for the worker's got receipt; none by the deadline is
// recorded as the outcome no_receipt.
func deliver(c *ctx, db *sql.DB, taskID int64, d delivery) (any, int, error) {
	sock := socketPath(c)
	// No server, no attempt: herdr would start one from this shell.
	a, err := beginAttempt(c, db, taskID, d, false, func() error {
		if !serverUp(sock) {
			return herdrErr("Herdr server not reachable at %s; not delivering", sock)
		}
		return nil
	})
	if err != nil {
		return nil, 0, err
	}
	if a.launchID != nil {
		bindCapacityBeforePrompt(db, sock, taskID, *a.launchID)
	}
	outcome, detail := runHerdrPrompt(c, sock, a.target, a.text)
	return finishDelivery(db, a, outcome, detail, d.confirm, d.confirmTimeout)
}

// attempt is a recorded prompt attempt and what to send.
type attempt struct {
	taskID, id   int64
	launchID     *int64
	target, text string
	agent        string
}

// errServerLane is beginAttempt's answer to a caller that would deliver on
// its own host when the lane is on the server host: nothing was written.
var errServerLane = &exitErr{exitOK, "route", "the lane is on the server host"}

// beginAttempt checks the lane and records the attempt. Only the host that
// owns the pane can deliver: here is false for the server's own Herdr and
// true for the RPC caller's, which delivers itself. ready runs last, before
// the write.
func beginAttempt(c *ctx, db *sql.DB, taskID int64, d delivery, here bool, ready func() error) (attempt, error) {
	a := attempt{taskID: taskID}
	data := d.data
	if _, ok := data["sha256"]; !ok {
		sum := sha256.Sum256([]byte(d.body))
		data["sha256"], data["bytes"] = hex.EncodeToString(sum[:]), len(d.body)
	}
	err := withTx(db, func(tx *sql.Tx) error {
		t, err := loadTask(tx, taskID)
		if err != nil {
			return err
		}
		if t.Status == "closed" {
			return rejectErr("task %d is closed", taskID)
		}
		if t.Status == "planned" {
			return rejectErr("task %d is planned; run `taskr launch %d` before prompting it", taskID, taskID)
		}
		if pa, err := plannedAncestor(tx, taskID); err != nil {
			return err
		} else if pa != 0 {
			return rejectErr("task %d is under planned task %d; launch task %d first", taskID, pa, pa)
		}
		var lpane, host sql.NullString
		if t.CurrentLaunchID.Valid {
			a.launchID = ptr(t.CurrentLaunchID.Int64)
			if err := tx.QueryRow(`select pane_id, machine from launches where id = ?`, *a.launchID).Scan(&lpane, &host); err != nil {
				return err
			}
		} else if err := tx.QueryRow(`select machine from tasks where id = ?`, taskID).Scan(&host); err != nil {
			return err
		}
		switch {
		case !host.Valid && here:
			return errServerLane
		case !host.Valid, here && host.String == c.machine:
		case host.String == c.machine:
			return herdrErr("lane %s is on %s, this caller's host; send it with `taskr prompt %d` there", t.Name, host.String, taskID)
		default:
			return herdrErr("lane %s is on %s; prompt it from that host (start a sub-orchestrator there)", t.Name, host.String)
		}
		a.target = firstNonEmpty(lpane.String, t.PaneID.String, t.AgentName.String)
		if a.target == "" {
			return usageErr("task %d has no pane or agent name to prompt", taskID)
		}
		a.agent = t.AgentName.String
		data["target"] = a.target
		if err := ready(); err != nil {
			return err
		}
		a.id, err = insertEvent(tx, event{TaskID: taskID, LaunchID: a.launchID, Kind: "prompt", Data: data, RelatedEventID: d.related})
		if err != nil {
			return err
		}
		if d.reopen {
			if _, err = tx.Exec(`update tasks set status = 'open', updated_at = ? where id = ?`, now(), taskID); err != nil {
				return err
			}
		}
		// Armed here, not after herdr returns: a hook's got can land first.
		recip := d.receiptRecipient
		if recip == nil {
			recip = parentRecipient(t)
		}
		if !d.confirm && d.receiptTimeout > 0 && t.ParentID.Valid && recip != nil {
			if err := armReceipt(tx, a.id, taskID, a.launchID, *recip, d.receiptTimeout); err != nil {
				return err
			}
		}
		if d.document != nil {
			return capturePrompt(tx, taskID, a.id, *d.document)
		}
		return nil
	})
	if err != nil {
		return a, err
	}
	a.text = d.compose(a.id)
	return a, nil
}

// finishDelivery records the outcome of attempt a, then with confirm waits
// for the receipt after observed activity.
func finishDelivery(db *sql.DB, a attempt, outcome string, detail map[string]any, confirm bool, timeout time.Duration) (any, int, error) {
	out := map[string]any{"task_id": a.taskID, "attempt_id": a.id, "target": a.target, "outcome": outcome}
	odata := map[string]any{"outcome": outcome}
	for k, v := range detail {
		odata[k], out[k] = v, v
	}
	err := withTx(db, func(tx *sql.Tx) error {
		id, err := insertEvent(tx, event{TaskID: a.taskID, LaunchID: a.launchID, Kind: "prompt_outcome",
			Summary: outcome, Data: odata, RelatedEventID: ptr(a.id)})
		out["outcome_event_id"] = id
		if err != nil {
			return err
		}
		// rejected: nothing reached the agent. --confirm keeps its own
		// synchronous receipt, even for an attempt an older client armed.
		if outcome == "rejected" || confirm {
			return disarmReceipt(tx, a.id)
		}
		// delivery_unknown keeps its deadline; its error output stays as it was.
		if outcome != "activity_observed" {
			return nil
		}
		window, armed, err := receiptWindow(tx, a.id)
		if armed {
			out["receipt_due_ms"] = window
		}
		return err
	})
	if err != nil {
		return out, 0, err
	}
	if outcome != "activity_observed" {
		out["ok"] = false
		msg := fmt.Sprintf("prompt attempt %d: %s; inspect the agent before any resend", a.id, outcome)
		return out, 0, &exitErr{exitHerdr, "herdr", msg}
	}
	if confirm {
		return awaitReceipt(db, a.taskID, a.launchID, a.id, timeout, out)
	}
	out["ok"] = true
	return out, exitOK, nil
}

// receiptPolled runs after each receipt query that found nothing; tests hook it.
var receiptPolled = func(attempt int64) {}

// awaitReceipt polls the ledger for the got event of attempt until timeout.
// It never resends.
func awaitReceipt(db *sql.DB, taskID int64, launchID *int64, attempt int64, timeout time.Duration, out map[string]any) (any, int, error) {
	deadline := time.Now().Add(timeout)
	for {
		var id int64
		var data sql.NullString
		err := db.QueryRow(`select id, data from events where kind = 'got' and related_event_id = ? order by id limit 1`, attempt).
			Scan(&id, &data)
		if err == nil {
			var g struct {
				Round int64 `json:"round"`
			}
			json.Unmarshal([]byte(data.String), &g)
			out["ok"], out["receipt"], out["receipt_event_id"], out["round"] = true, true, id, g.Round
			return out, exitOK, nil
		} else if !errors.Is(err, sql.ErrNoRows) {
			return out, 0, dbErr(err)
		}
		receiptPolled(attempt)
		left := time.Until(deadline)
		if left <= 0 {
			break
		}
		time.Sleep(min(left, dbPollInterval))
	}
	out["ok"], out["receipt"], out["outcome"] = false, false, "no_receipt"
	out["delivery_outcome_event_id"] = out["outcome_event_id"]
	err := withTx(db, func(tx *sql.Tx) error {
		id, err := insertEvent(tx, event{TaskID: taskID, LaunchID: launchID, Kind: "prompt_outcome", Summary: "no_receipt",
			Data: map[string]any{"outcome": "no_receipt", "confirm_timeout_ms": timeout.Milliseconds()}, RelatedEventID: ptr(attempt)})
		out["outcome_event_id"] = id
		return err
	})
	if err != nil {
		return out, 0, err
	}
	msg := fmt.Sprintf("prompt attempt %d: activity observed but no taskr got receipt within %d ms; inspect the agent before any resend",
		attempt, timeout.Milliseconds())
	return out, 0, &exitErr{exitHerdr, "no_receipt", msg}
}

// runHerdrPrompt returns activity_observed, rejected (Herdr refused before
// sending input, or could not be run) or delivery_unknown (anything else).
func runHerdrPrompt(c *ctx, sock, target, text string) (string, map[string]any) {
	cx, cancel := context.WithTimeout(context.Background(), herdrPromptDeadline)
	defer cancel()
	cmd, err := herdrCommand(cx, sock, "agent", "prompt", target, text,
		"--wait", "--until", "working", "--until", "blocked", "--timeout", "20000")
	if err != nil { // the server went away after the attempt was recorded
		return "rejected", map[string]any{"herdr_error": "no_server"}
	}
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	err = cmd.Run()
	detail := map[string]any{}
	if err == nil {
		var ok struct {
			Result struct {
				Agent struct {
					Status string `json:"agent_status"`
				} `json:"agent"`
			} `json:"result"`
		}
		if json.Unmarshal(stdout.Bytes(), &ok) == nil && ok.Result.Agent.Status != "" {
			detail["agent_status"] = ok.Result.Agent.Status
		}
		return "activity_observed", detail
	}
	if stderr.Len() > 0 && c.json {
		fmt.Fprintf(c.errw, "herdr: %s\n", strings.TrimSpace(stderr.String()))
	}
	// Herdr CLI errors: {"id": ..., "error": {"code": ..., "message": ...}} on stderr.
	var env struct {
		Error struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	parseErr := json.Unmarshal(stderr.Bytes(), &env)
	if !c.json {
		message := env.Error.Message
		if parseErr != nil {
			message = strings.TrimSpace(stderr.String())
		}
		if message != "" {
			detail["herdr_message"] = message
		}
	}
	var ee *exec.ExitError
	if !errors.As(err, &ee) {
		detail["herdr_error"] = "not_run"
		return "rejected", detail
	}
	if cx.Err() != nil {
		detail["herdr_error"] = "deadline"
		return "delivery_unknown", detail
	}
	detail["herdr_exit"] = ee.ExitCode()
	code := env.Error.Code
	if code != "" {
		detail["herdr_error"] = code
	}
	if ee.ExitCode() == 2 || code == "agent_blocked" || strings.Contains(code, "not_found") {
		return "rejected", detail
	}
	return "delivery_unknown", detail
}
