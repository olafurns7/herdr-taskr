package main

import (
	"database/sql"
	"encoding/json"
	"errors"
	"flag"
	"path/filepath"
	"strconv"
	"strings"
)

// worker is the verified identity of the calling worker.
type worker struct {
	task     *task
	launchID *int64
}

// resolveWorker checks TASKR_TASK and TASKR_LAUNCH inside the write
// transaction: the task must exist and be open to writes, and the launch must
// be its current one. A task with no launch (a root orchestrator) accepts
// writes only without TASKR_LAUNCH.
func resolveWorker(c *ctx, tx queryer) (*worker, error) {
	ts := c.env("TASKR_TASK")
	if ts == "" {
		return nil, usageErr("TASKR_TASK is not set")
	}
	tid, err := parseID(ts, "TASKR_TASK")
	if err != nil {
		return nil, err
	}
	t, err := loadTask(tx, tid)
	if err != nil {
		return nil, err
	}
	if t.Status == "closed" {
		return nil, rejectErr("task %d is closed", tid)
	}
	if t.Status == "planned" {
		return nil, rejectErr("task %d is planned, not launched; its orchestrator runs `taskr launch %d` first", tid, tid)
	}
	if pa, err := plannedAncestor(tx, tid); err != nil {
		return nil, err
	} else if pa != 0 {
		return nil, rejectErr("task %d is under planned task %d, which has no inbox yet", tid, pa)
	}
	w := &worker{task: t}
	ls := c.env("TASKR_LAUNCH")
	switch {
	case ls == "" && t.CurrentLaunchID.Valid:
		return nil, rejectErr("TASKR_LAUNCH is not set but task %d has launch %d", tid, t.CurrentLaunchID.Int64)
	case ls == "":
		if err := checkTaskHost(c, tx, tid); err != nil {
			return nil, err
		}
		return w, nil
	}
	lid, err := strconv.ParseInt(ls, 10, 64)
	if err != nil {
		return nil, usageErr("TASKR_LAUNCH must be an integer, got %q", ls)
	}
	if !t.CurrentLaunchID.Valid || t.CurrentLaunchID.Int64 != lid {
		return nil, rejectErr("launch %d is not the current launch of task %d", lid, tid)
	}
	var host sql.NullString
	if err := tx.QueryRow(`select machine from launches where id = ?`, lid).Scan(&host); err != nil {
		return nil, err
	}
	if host != callerMachine(c) {
		return nil, rejectErr("launch is on %s, caller is %s", machineName(host), machineName(callerMachine(c)))
	}
	w.launchID = ptr(lid)
	return w, nil
}

// checkTaskHost rejects a caller on another host than the task. It guards
// the writes and inbox reads a root (or a worker with --as) makes by task id;
// NULL is the server host on both sides. Old ids from another host's ledger
// that collide with this ledger's ids fail here.
func checkTaskHost(c *ctx, q queryer, id int64) error {
	var host sql.NullString
	err := q.QueryRow(`select machine from tasks where id = ?`, id).Scan(&host)
	if errors.Is(err, sql.ErrNoRows) {
		return rejectErr("task %d does not exist", id)
	} else if err != nil {
		return err
	}
	if host != callerMachine(c) {
		return rejectErr("task %d is on %s, caller is %s", id, machineName(host), machineName(callerMachine(c)))
	}
	return nil
}

// resolveWriter is resolveWorker, or with --as a root orchestrator naming
// itself: a task with no parent and no current launch. Orchestrators run in
// agent shells whose exports do not persist between tool calls, so they name
// themselves; a launched worker never does.
func resolveWriter(c *ctx, tx queryer, as int64) (*worker, error) {
	if as == 0 {
		return resolveWorker(c, tx)
	}
	if ts := c.env("TASKR_TASK"); ts != "" && ts != strconv.FormatInt(as, 10) {
		return nil, rejectErr("--as %d names a different task than TASKR_TASK=%s", as, ts)
	}
	if c.env("TASKR_LAUNCH") != "" {
		return nil, rejectErr("--as is for a root orchestrator, which has no launch; TASKR_LAUNCH is set")
	}
	t, err := loadTask(tx, as)
	if err != nil {
		return nil, err
	}
	switch {
	case t.Status == "closed":
		return nil, rejectErr("task %d is closed", as)
	case t.Status == "planned":
		return nil, rejectErr("task %d is planned, not launched", as)
	case t.ParentID.Valid:
		return nil, rejectErr("--as is only for a root orchestrator: task %d has parent %d", as, t.ParentID.Int64)
	case t.CurrentLaunchID.Valid:
		return nil, rejectErr("--as is only for a root orchestrator: task %d has launch %d; it writes with TASKR_TASK and TASKR_LAUNCH", as, t.CurrentLaunchID.Int64)
	}
	if err := checkTaskHost(c, tx, as); err != nil {
		return nil, err
	}
	return &worker{task: t}, nil
}

type workerWrite struct {
	kind, summary, key string
	data               map[string]any
	status             string // new task status, or "" to leave it
	owner              bool   // route to the root instead of the parent
	as                 int64  // a root orchestrator writing as itself (--as), or 0
}

// System event key prefixes; callers cannot use them with --key.
const (
	gotKeyPrefix         = "got:"
	noReceiptKeyPrefix   = "no_receipt:"
	lateReceiptKeyPrefix = "late_receipt:"
)

var systemKeyPrefixes = []string{gotKeyPrefix, noReceiptKeyPrefix, lateReceiptKeyPrefix, "quota:", "capacity:"}

func writeWorkerEvent(c *ctx, ww workerWrite) (any, int, error) {
	for _, p := range systemKeyPrefixes {
		if strings.HasPrefix(ww.key, p) {
			return nil, 0, usageErr("--key prefix %q is reserved for events taskr writes itself", p)
		}
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	var report *preparedReport
	if ww.kind == "ready" || ww.kind == "done" || ww.kind == "fail" {
		w, err := resolveWriter(c, db, ww.as)
		if err != nil {
			return nil, 0, err
		}
		explicit, _ := ww.data["report"].(string)
		report = prepareReport(db, w.task.ID, explicit, ww.kind == "ready")
	}
	out := map[string]any{"ok": true, "kind": ww.kind}
	err = withTx(db, func(tx *sql.Tx) error {
		w, err := resolveWriter(c, tx, ww.as)
		if err != nil {
			return err
		}
		if ww.key != "" {
			var id, tid int64
			var kind string
			err := tx.QueryRow(`select id, task_id, kind from events where event_key = ?`, ww.key).Scan(&id, &tid, &kind)
			if err == nil {
				if tid != w.task.ID {
					return rejectErr("event key %q belongs to task %d", ww.key, tid)
				}
				if kind != ww.kind {
					return rejectErr("event key %q is a %s event, not %s", ww.key, kind, ww.kind)
				}
				out["event_id"], out["task_id"], out["status"], out["duplicate"] = id, tid, w.task.Status, true
				if ww.kind == "ask" {
					out["ask_id"] = id
				}
				return nil
			} else if !errors.Is(err, sql.ErrNoRows) {
				return err
			}
		}
		recip := parentRecipient(w.task)
		switch {
		case ww.kind == "note" || ww.kind == "start":
			recip = nil
		case ww.owner:
			root, err := rootOf(tx, w.task.ID)
			if err != nil {
				return err
			}
			recip = ptr(root)
			if root == w.task.ID {
				recip = nil
			}
		}
		if err := notPlannedRecipient(tx, recip); err != nil {
			return err
		}
		e := event{TaskID: w.task.ID, RecipientTaskID: recip, LaunchID: w.launchID,
			Kind: ww.kind, Summary: ww.summary, Data: ww.data, EventKey: ww.key}
		var id int64
		if ww.kind == "note" || ww.kind == "ready" || ww.kind == "done" || ww.kind == "fail" {
			id, err = c.insertEvent(tx, e)
		} else {
			id, err = insertEvent(tx, e)
		}
		if err != nil {
			return err
		}
		status := w.task.Status
		if ww.status != "" {
			status = ww.status
			if _, err := tx.Exec(`update tasks set status = ?, updated_at = ? where id = ?`, status, now(), w.task.ID); err != nil {
				return err
			}
		}
		out["event_id"], out["task_id"], out["status"] = id, w.task.ID, status
		if recip != nil {
			out["recipient_task_id"] = *recip
		}
		if ww.kind == "ask" {
			out["ask_id"] = id
		}
		return captureReport(tx, c, w.task.ID, id, report)
	})
	if err != nil {
		return nil, 0, err
	}
	return out, exitOK, nil
}

func cmdStart(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("start", flag.ContinueOnError)
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		return nil, 0, err
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	out := map[string]any{"ok": true, "kind": "start"}
	err = withTx(db, func(tx *sql.Tx) error {
		w, err := resolveWorker(c, tx)
		if err != nil {
			return err
		}
		if w.launchID == nil {
			return rejectErr("task %d has no launch to start", w.task.ID)
		}
		data, err := mergeIdentity(c, tx, *w.launchID)
		if err != nil {
			return err
		}
		for k, v := range data {
			out[k] = v
		}
		id, err := insertEvent(tx, event{TaskID: w.task.ID, LaunchID: w.launchID, Kind: "start", Data: data})
		out["event_id"], out["task_id"], out["launch_id"] = id, w.task.ID, *w.launchID
		return err
	})
	if err != nil {
		return nil, 0, err
	}
	return out, exitOK, nil
}

// mergeIdentity records the caller's native home, account, session and pane
// on its launch, keeping values already recorded where the caller has none,
// and returns the non-empty values it saw.
func mergeIdentity(c *ctx, tx *sql.Tx, launchID int64) (map[string]any, error) {
	var provider sql.NullString
	if err := tx.QueryRow(`select provider from launches where id = ?`, launchID).Scan(&provider); err != nil {
		return nil, err
	}
	codexHome, claudeHome := c.env("CODEX_HOME"), c.env("CLAUDE_CONFIG_DIR")
	home := firstNonEmpty(codexHome, claudeHome)
	switch provider.String {
	case "codex":
		home = codexHome
	case "claude":
		home = claudeHome
	}
	var sessRef, sessKind, sessSource string
	if thread := c.env("CODEX_THREAD_ID"); thread != "" && provider.String == "codex" {
		sessRef, sessKind, sessSource = thread, "thread_id", "env:CODEX_THREAD_ID"
	}
	acct, pane := accountLabel(home), c.env("HERDR_PANE_ID")
	if _, err := tx.Exec(`update launches set native_home = coalesce(?, native_home), account = coalesce(?, account),
		session_ref = case when session_source like 'hook:%' then session_ref else coalesce(?, session_ref) end,
		session_kind = case when session_source like 'hook:%' then session_kind else coalesce(?, session_kind) end,
		session_source = case when session_source like 'hook:%' then session_source else coalesce(?, session_source) end,
		pane_id = coalesce(pane_id, ?) where id = ?`,
		nullStr(home), nullStr(acct), nullStr(sessRef), nullStr(sessKind), nullStr(sessSource), nullStr(pane), launchID); err != nil {
		return nil, err
	}
	data := map[string]any{}
	for k, v := range map[string]string{"native_home": home, "account": acct, "session_ref": sessRef, "herdr_pane_id": pane} {
		if v != "" {
			data[k] = v
		}
	}
	return data, nil
}

// cmdGot records the worker's receipt of one prompt attempt.
func cmdGot(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("got", flag.ContinueOnError)
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	attempt, err := parseID(pos[0], "attempt event id")
	if err != nil {
		return nil, 0, err
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	out := map[string]any{"ok": true, "kind": "got", "attempt_id": attempt}
	err = withTx(db, func(tx *sql.Tx) error {
		w, err := resolveWorker(c, tx)
		if err != nil {
			return err
		}
		g, err := recordGot(c, tx, w, attempt, func(tx *sql.Tx, launch int64) (map[string]any, error) {
			return mergeIdentity(c, tx, launch)
		})
		if err != nil {
			return err
		}
		out["event_id"], out["round"] = g.eventID, g.round
		if g.duplicate {
			out["duplicate"] = true
		}
		return nil
	})
	if err != nil {
		return nil, 0, err
	}
	return out, exitOK, nil
}

type gotResult struct {
	eventID, round int64
	duplicate      bool
}

// recordGot records w's receipt of prompt attempt inside tx. The attempt
// must be a prompt on the worker's own task and current launch. A repeat
// returns the first receipt. If `start` has not recorded the launch's native
// home yet, identity supplies it. A first receipt deletes the attempt's
// receipt deadline and, after a no_receipt, writes late_receipt.
func recordGot(c *ctx, tx *sql.Tx, w *worker, attempt int64, identity func(tx *sql.Tx, launch int64) (map[string]any, error)) (gotResult, error) {
	var g gotResult
	if w.launchID == nil {
		return g, rejectErr("task %d has no launch to receive prompts", w.task.ID)
	}
	var tid int64
	var kind string
	var lid sql.NullInt64
	err := tx.QueryRow(`select task_id, kind, launch_id from events where id = ?`, attempt).Scan(&tid, &kind, &lid)
	switch {
	case errors.Is(err, sql.ErrNoRows):
		return g, rejectErr("event %d does not exist", attempt)
	case err != nil:
		return g, err
	case kind != "prompt":
		return g, rejectErr("event %d is a %s event, not a prompt attempt", attempt, kind)
	case tid != w.task.ID:
		return g, rejectErr("prompt attempt %d is for task %d, not task %d", attempt, tid, w.task.ID)
	case !lid.Valid || lid.Int64 != *w.launchID:
		return g, rejectErr("prompt attempt %d is not for the current launch %d of task %d", attempt, *w.launchID, w.task.ID)
	}
	key := gotKeyPrefix + strconv.FormatInt(attempt, 10)
	var id, etid int64
	var ekind string
	var elaunch, erelated sql.NullInt64
	var data sql.NullString
	err = tx.QueryRow(`select id, task_id, kind, launch_id, related_event_id, data from events where event_key = ?`, key).
		Scan(&id, &etid, &ekind, &elaunch, &erelated, &data)
	if err == nil {
		// A row written under this key by anything but this receipt (a
		// caller --key from before the prefix was reserved) is a collision.
		if ekind != "got" || etid != w.task.ID || !elaunch.Valid || elaunch.Int64 != *w.launchID ||
			!erelated.Valid || erelated.Int64 != attempt {
			return g, rejectErr("key collision: event key %q is already event %d (%s on task %d), not this receipt", key, id, ekind, etid)
		}
		var d struct {
			Round int64 `json:"round"`
		}
		json.Unmarshal([]byte(data.String), &d)
		return gotResult{eventID: id, round: d.Round, duplicate: true}, nil
	} else if !errors.Is(err, sql.ErrNoRows) {
		return g, err
	}
	if err := tx.QueryRow(`select count(*) from events where kind = 'prompt' and task_id = ? and launch_id = ?`,
		w.task.ID, *w.launchID).Scan(&g.round); err != nil {
		return g, err
	}
	edata := map[string]any{"round": g.round}
	var home sql.NullString
	if err := tx.QueryRow(`select native_home from launches where id = ?`, *w.launchID).Scan(&home); err != nil {
		return g, err
	}
	if !home.Valid {
		ident, err := identity(tx, *w.launchID)
		if err != nil {
			return g, err
		}
		if len(ident) > 0 {
			edata["identity"] = ident
		}
	}
	if err := notPlannedRecipient(tx, parentRecipient(w.task)); err != nil {
		return g, err
	}
	g.eventID, err = c.insertEvent(tx, event{TaskID: w.task.ID, RecipientTaskID: parentRecipient(w.task), LaunchID: w.launchID,
		Kind: "got", Summary: "prompt attempt " + strconv.FormatInt(attempt, 10) + " received", Data: edata,
		RelatedEventID: ptr(attempt), EventKey: key})
	if err != nil {
		return g, err
	}
	if err := disarmReceipt(tx, attempt); err != nil {
		return g, err
	}
	return g, lateReceipt(c, tx, w, attempt, g.eventID)
}

// accountLabel returns an optional account label when the agent home follows
// the <root>/<account>/native layout.
func accountLabel(home string) string {
	if home == "" {
		return ""
	}
	dir := filepath.Clean(home)
	for dir != "/" && dir != "." {
		if filepath.Base(dir) == "native" {
			parent := filepath.Base(filepath.Dir(dir))
			if parent != "/" && parent != "." {
				return parent
			}
			return ""
		}
		dir = filepath.Dir(dir)
	}
	return ""
}

func firstNonEmpty(s ...string) string {
	for _, v := range s {
		if v != "" {
			return v
		}
	}
	return ""
}

func cmdNote(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("note", flag.ContinueOnError)
	key := fs.String("key", "", "idempotency key")
	as := fs.Int64("as", 0, "a root orchestrator's own task id")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	if *as < 0 {
		return nil, 0, usageErr("--as must be a positive task id")
	}
	return writeWorkerEvent(c, workerWrite{kind: "note", summary: pos[0], key: *key, as: *as})
}

func cmdReady(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("ready", flag.ContinueOnError)
	report := fs.String("report", "", "report path")
	key := fs.String("key", "", "idempotency key")
	kv := kvFlag{}
	fs.Var(kv, "kv", "KEY=VALUE, repeatable")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	data := map[string]any{}
	if *report != "" {
		abs, err := filepath.Abs(*report)
		if err != nil {
			return nil, 0, usageErr("report path: %v", err)
		}
		data["report"] = abs
	}
	if len(kv) > 0 {
		data["kv"] = map[string]string(kv)
	}
	return writeWorkerEvent(c, workerWrite{kind: "ready", summary: pos[0], key: *key, data: data, status: "ready"})
}

func cmdAsk(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("ask", flag.ContinueOnError)
	blocking := fs.Bool("blocking", false, "the worker cannot continue without an answer")
	owner := fs.Bool("owner", false, "a question for the owner; routed to the root")
	key := fs.String("key", "", "idempotency key")
	as := fs.Int64("as", 0, "a root orchestrator's own task id")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	if *as < 0 {
		return nil, 0, usageErr("--as must be a positive task id")
	}
	data := map[string]any{"blocking": *blocking, "owner": *owner}
	return writeWorkerEvent(c, workerWrite{kind: "ask", summary: pos[0], key: *key, data: data, owner: *owner, as: *as})
}

func cmdDone(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("done", flag.ContinueOnError)
	key := fs.String("key", "", "idempotency key")
	pos, err := parseArgs(c, fs, args, 0, 1)
	if err != nil {
		return nil, 0, err
	}
	return writeWorkerEvent(c, workerWrite{kind: "done", summary: firstNonEmpty(pos...), key: *key, status: "done"})
}

func cmdFail(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("fail", flag.ContinueOnError)
	key := fs.String("key", "", "idempotency key")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	return writeWorkerEvent(c, workerWrite{kind: "fail", summary: pos[0], key: *key, status: "failed"})
}

// notPlannedRecipient refuses an event addressed to a planned task: a plan
// has no inbox. resolveWorker already refuses writers under a plan; this is
// the routing guard behind it.
func notPlannedRecipient(tx *sql.Tx, recip *int64) error {
	if recip == nil {
		return nil
	}
	var status string
	if err := tx.QueryRow(`select status from tasks where id = ?`, *recip).Scan(&status); err != nil {
		return err
	}
	if status == "planned" {
		return rejectErr("task %d is planned and has no inbox; launch it first", *recip)
	}
	return nil
}
