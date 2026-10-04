package main

import (
	"database/sql"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"
)

var roles = map[string]bool{"orchestrator": true, "sub-orchestrator": true, "implementer": true,
	"reviewer": true, "researcher": true, "gate": true}

func cmdNew(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("new", flag.ContinueOnError)
	parent := fs.Int64("parent", 0, "parent task id")
	role := fs.String("role", "", "orchestrator | sub-orchestrator | implementer | reviewer | researcher | gate")
	ws := fs.String("workspace", "", "Herdr workspace id")
	tab := fs.String("tab", "", "Herdr tab id")
	pane := fs.String("pane", "", "Herdr pane id")
	cwd := fs.String("cwd", "", "task working directory (default: current)")
	brief := fs.String("brief", "", "brief path")
	report := fs.String("report", "", "report path")
	planned := fs.Bool("planned", false, "a planned lane: no launch until `taskr launch`")
	machine := fs.String("machine", "", "the task's host (default: the caller's)")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	if err := validateName(pos[0], "name"); err != nil {
		return nil, 0, err
	}
	if *planned && *parent == 0 {
		return nil, 0, usageErr("--planned needs --parent: a plan is a lane of an orchestrator")
	}
	if !roles[*role] {
		return nil, 0, usageErr("--role must be one of orchestrator, sub-orchestrator, implementer, reviewer, researcher, gate")
	}
	if err := validateLocationFlags(fs, *ws, *tab, *pane); err != nil {
		return nil, 0, err
	}
	dir := *cwd
	if dir == "" && c.rpc {
		dir = c.cwd // the caller's directory, never the server's
	} else if dir == "" {
		if dir, err = os.Getwd(); err != nil {
			return nil, 0, usageErr("--cwd must resolve to an absolute existing directory: %v", err)
		}
	}
	dir, err = filepath.Abs(dir)
	if err != nil {
		return nil, 0, usageErr("--cwd must resolve to an absolute directory path: %v", err)
	}
	briefPath, err := absolutePath(dir, *brief, "--brief")
	if err != nil {
		return nil, 0, err
	}
	reportPath, err := absolutePath(dir, *report, "--report")
	if err != nil {
		return nil, 0, err
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	var briefInput *documentInput
	if briefPath != "" {
		in := fileDocument(briefPath, callerMachine(c).String)
		briefInput = &in
	}
	var id int64
	status := "open"
	if *planned {
		status = "planned"
	}
	err = withTx(db, func(tx *sql.Tx) error {
		host, err := resolveMachine(c, tx, *machine, flagWasSet(fs, "machine"), callerMachine(c))
		if err != nil {
			return err
		}
		if !*planned && *role != "gate" {
			if !host.Valid {
				if err := requireDirectory(dir); err != nil {
					return err
				}
			}
			if !c.rpc && briefPath != "" {
				if err := requireFile(briefPath); err != nil {
					return err
				}
			}
		}
		var parentID any
		if *parent != 0 {
			p, err := loadTask(tx, *parent)
			if err != nil {
				return err
			}
			if p.Status == "closed" {
				return rejectErr("parent task %d is closed", *parent)
			}
			parentID = *parent
		}
		ts := now()
		res, err := tx.Exec(`insert into tasks (parent_id, name, role, status, workspace_id, tab_id, pane_id, cwd,
			brief_path, report_path, machine, created_at, updated_at) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
			parentID, pos[0], *role, status, nullStr(*ws), nullStr(*tab), nullStr(*pane), dir,
			nullStr(briefPath), nullStr(reportPath), host, ts, ts)
		if err != nil {
			return err
		}
		id, err = res.LastInsertId()
		if err != nil {
			return err
		}
		return captureBrief(tx, c, id, briefInput)
	})
	if err != nil {
		return nil, 0, err
	}
	if *parent == 0 && briefPath == "" {
		fmt.Fprintf(c.errw, "taskr: no goal recorded for root %d; run `taskr doc set %d goal --file PATH`\n", id, id)
	}
	return map[string]any{"ok": true, "task_id": id, "name": pos[0], "status": status}, exitOK, nil
}

func cmdLaunch(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("launch", flag.ContinueOnError)
	provider := fs.String("provider", "", "claude | codex | ...")
	model := fs.String("model", "", "model id")
	effort := fs.String("effort", "", "effort level")
	ws := fs.String("workspace", "", "move the task to this Herdr workspace (default: keep)")
	tab := fs.String("tab", "", "move the task to this Herdr tab (default: keep)")
	pane := fs.String("pane", "", "move the task to this pane (default: the task's pane)")
	agent := fs.String("agent", "", "Herdr agent name (default: the task name)")
	scope := fs.String("herdr-scope", "", "Herdr server socket and session the pane id belongs to")
	machine := fs.String("machine", "", "the launch's host (default: the task's)")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	id, err := parseID(pos[0], "task id")
	if err != nil {
		return nil, 0, err
	}
	if *provider == "" || *model == "" || *effort == "" {
		return nil, 0, usageErr("launch needs --provider, --model and --effort")
	}
	if flagWasSet(fs, "agent") {
		if err := validateName(*agent, "--agent"); err != nil {
			return nil, 0, err
		}
	}
	if err := validateLocationFlags(fs, *ws, *tab, *pane); err != nil {
		return nil, 0, err
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	out := map[string]any{"ok": true, "task_id": id}
	err = withTx(db, func(tx *sql.Tx) error {
		t, err := loadTask(tx, id)
		if err != nil {
			return err
		}
		if t.Status == "closed" {
			return rejectErr("task %d is closed", id)
		}
		if !t.ParentID.Valid {
			return rejectErr("task %d is a root task; launch records a lane, so give it a task created with --parent", id)
		}
		// A lane under a planned task has nobody to report to yet.
		if pa, err := plannedAncestor(tx, id); err != nil {
			return err
		} else if pa != 0 {
			return rejectErr("task %d is under planned task %d; launch task %d first", id, pa, pa)
		}
		var ow, ot, om sql.NullString
		if err := tx.QueryRow(`select workspace_id, tab_id, machine from tasks where id = ?`, id).Scan(&ow, &ot, &om); err != nil {
			return err
		}
		host, err := resolveMachine(c, tx, *machine, flagWasSet(fs, "machine"), om)
		if err != nil {
			return err
		}
		// Given values move the task (a resume in a new tab); omitted ones keep it.
		w, tb := firstNonEmpty(*ws, ow.String), firstNonEmpty(*tab, ot.String)
		p := firstNonEmpty(*pane, t.PaneID.String)
		name := firstNonEmpty(*agent, t.Name)
		ts := now()
		res, err := tx.Exec(`insert into launches (task_id, provider, model, effort, herdr_scope, workspace_id, tab_id, pane_id, machine, recorded_at)
			values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`, id, *provider, *model, *effort, nullStr(*scope), nullStr(w), nullStr(tb), nullStr(p), host, ts)
		if err != nil {
			return err
		}
		lid, err := res.LastInsertId()
		if err != nil {
			return err
		}
		// The replaced launch's attempts can no longer get a receipt.
		if err := deleteTaskReceipts(tx, id); err != nil {
			return err
		}
		// Launching a planned lane opens it.
		// The task moves with its launch, host included.
		if _, err := tx.Exec(`update tasks set current_launch_id = ?, agent_name = ?, workspace_id = ?, tab_id = ?, pane_id = ?,
			machine = ?, updated_at = ?, status = case status when 'planned' then 'open' else status end where id = ?`,
			lid, name, nullStr(w), nullStr(tb), nullStr(p), host, ts, id); err != nil {
			return err
		}
		if *ws != "" || *tab != "" || *pane != "" {
			loc := func(w, t, p string) map[string]any {
				m := map[string]any{}
				for k, v := range map[string]string{"workspace_id": w, "tab_id": t, "pane_id": p} {
					if v != "" {
						m[k] = v
					}
				}
				return m
			}
			if _, err := insertEvent(tx, event{TaskID: id, LaunchID: ptr(lid), Kind: "launch", Summary: "launched at pane " + orNone(p),
				Data: map[string]any{"launch_id": lid, "old": loc(ow.String, ot.String, t.PaneID.String), "new": loc(w, tb, p)}}); err != nil {
				return err
			}
		}
		if t.Status == "planned" {
			out["status"], out["was_planned"] = "open", true
		}
		out["launch_id"], out["agent_name"] = lid, name
		if t.CurrentLaunchID.Valid {
			out["replaced_launch_id"] = t.CurrentLaunchID.Int64
		}
		if p != "" {
			out["pane_id"] = p
		}
		if w != "" {
			out["workspace_id"] = w
		}
		if tb != "" {
			out["tab_id"] = tb
		}
		return nil
	})
	if err != nil {
		return nil, 0, err
	}
	return out, exitOK, nil
}

func cmdClose(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("close", flag.ContinueOnError)
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	id, err := parseID(pos[0], "task id")
	if err != nil {
		return nil, 0, err
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	report := prepareReport(db, id, "", false)
	out := map[string]any{"ok": true, "task_id": id, "status": "closed"}
	err = withTx(db, func(tx *sql.Tx) error {
		t, err := loadTask(tx, id)
		if err != nil {
			return err
		}
		if t.Status == "closed" {
			out["already"] = true
			return nil
		}
		ts := now()
		if _, err = tx.Exec(`update tasks set status = 'closed', agent_name = null, closed_at = ?, updated_at = ? where id = ?`, ts, ts, id); err != nil {
			return err
		}
		if err := deleteTaskReceipts(tx, id); err != nil {
			return err
		}
		// The close's place in the event order: handover compares event ids,
		// never clocks.
		eid, err := c.insertEvent(tx, event{TaskID: id, Kind: "closed", Data: map[string]any{"from_status": t.Status}})
		out["event_id"] = eid
		if err != nil {
			return err
		}
		return captureReport(tx, c, id, eid, report)
	})
	if err != nil {
		return nil, 0, err
	}
	return out, exitOK, nil
}

func cmdAnswer(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("answer", flag.ContinueOnError)
	as := fs.Int64("as", 0, "the task that received the ask")
	doPrompt := fs.Bool("prompt", false, "also prompt the asker with the answer")
	confirm := fs.Bool("confirm", false, "with --prompt: wait for the asker's taskr got receipt")
	confirmTimeout := fs.Int64("confirm-timeout", 60000, "milliseconds to wait for the receipt")
	pos, err := parseArgs(c, fs, args, 2, 2)
	if err != nil {
		return nil, 0, err
	}
	askID, err := parseID(pos[0], "ask id")
	if err != nil {
		return nil, 0, err
	}
	text := pos[1]
	if *confirm && !*doPrompt {
		return nil, 0, usageErr("--confirm needs --prompt")
	}
	if *confirmTimeout < 0 {
		return nil, 0, usageErr("--confirm-timeout must not be negative")
	}
	asGiven := flagWasSet(fs, "as")
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	var ans answerResult
	err = withTx(db, func(tx *sql.Tx) (err error) {
		if asGiven {
			if err := checkTaskHost(c, tx, *as); err != nil {
				return err
			}
			ans, err = answerAsk(tx, askID, text, *as)
		} else {
			ans, err = answerAsk(tx, askID, text)
		}
		return err
	})
	if err != nil {
		return nil, 0, err
	}
	asker, answerID := ans.asker, ans.answerID
	out := map[string]any{"ok": true, "ask_id": askID, "answer_id": answerID, "task_id": asker, "delivered": false,
		"asker_waiting": ans.waiting}
	if !*doPrompt {
		return out, exitOK, nil
	}
	body := "taskr answer to ask " + pos[0] + ": " + text
	d := delivery{
		compose: func(attempt int64) string {
			return fmt.Sprintf("First taskr got %d. ask %s: %s", attempt, pos[0], text)
		},
		body: body, data: map[string]any{"answer_id": answerID}, related: ptr(answerID),
		confirm: *confirm, confirmTimeout: time.Duration(*confirmTimeout) * time.Millisecond,
		receiptTimeout: defaultReceiptTimeout}
	// An --owner ask's answerer is the root, not the asker's parent: the
	// alarm goes to whoever answered with --as.
	if asGiven {
		d.receiptRecipient = ptr(*as)
	}
	res, code, err := deliver(c, db, asker, d)
	if m, ok := res.(map[string]any); ok {
		for k, v := range m {
			out[k] = v
		}
		out["ok"] = err == nil
		out["delivered"] = m["outcome"] == "activity_observed" || m["outcome"] == "no_receipt"
	}
	return out, code, err
}

type answerResult struct {
	asker, answerID int64
	waiting         bool
}

// answerAsk records one answer to an open ask inside tx. The orchestrator
// writes it itself, so it needs no notice to the root. (Earlier versions let
// the dashboard answer owner asks, with via=dashboard and an owner_answer
// notice; those rows stay readable, but nothing writes them now.)
func answerAsk(tx *sql.Tx, askID int64, text string, expectedRecipient ...int64) (answerResult, error) {
	var r answerResult
	var kind string
	var recipient int64
	var answered sql.NullInt64
	var data sql.NullString
	err := tx.QueryRow(`select task_id, coalesce(recipient_task_id, task_id), kind, answered_by, data from events where id = ?`, askID).Scan(&r.asker, &recipient, &kind, &answered, &data)
	if err == sql.ErrNoRows || (err == nil && kind != "ask") {
		return r, rejectErr("event %d is not an ask", askID)
	} else if err != nil {
		return r, err
	}
	if len(expectedRecipient) > 0 && recipient != expectedRecipient[0] {
		return r, rejectErr("--as must match ask recipient task %d, got %d", recipient, expectedRecipient[0])
	}
	var ask map[string]any
	json.Unmarshal([]byte(data.String), &ask)
	owner := ask["owner"] == true
	if answered.Valid {
		return r, rejectErr("ask %d is already answered by event %d", askID, answered.Int64)
	}
	t, err := loadTask(tx, r.asker)
	if err != nil {
		return r, err
	}
	if t.Status == "closed" {
		return r, rejectErr("task %d is closed", r.asker)
	}
	r.waiting = t.WaitingUntil.Valid && t.WaitingUntil.String > now()
	adata := map[string]any{"owner": owner}
	r.answerID, err = insertEvent(tx, event{TaskID: r.asker, RecipientTaskID: ptr(r.asker), Kind: "answer",
		Summary: text, Data: adata, RelatedEventID: ptr(askID)})
	if err != nil {
		return r, err
	}
	res, err := tx.Exec(`update events set answered_by = ? where id = ? and answered_by is null`, r.answerID, askID)
	if err != nil {
		return r, err
	}
	if n, _ := res.RowsAffected(); n != 1 {
		return r, rejectErr("ask %d was answered concurrently", askID)
	}
	return r, nil
}

// callerMachine is the caller's host as stored: NULL for the server host.
func callerMachine(c *ctx) sql.NullString {
	return sql.NullString{String: c.machine, Valid: c.machine != ""}
}

// machineName is a stored host for messages: NULL is the server's label.
func machineName(m sql.NullString) string {
	if m.Valid {
		return m.String
	}
	return localMachine()
}

// resolveMachine returns the host a --machine flag names, or dflt without
// one. A host must be the server, the caller, or a client host whose daemon
// heartbeat is fresh; the server's label is stored as NULL.
func resolveMachine(c *ctx, tx queryer, m string, given bool, dflt sql.NullString) (sql.NullString, error) {
	if !given {
		return dflt, nil
	}
	server := localMachine()
	switch {
	case m == server:
		return sql.NullString{}, nil
	case m != "" && m == c.machine:
		return callerMachine(c), nil
	}
	fresh, err := freshHosts(tx)
	if err != nil {
		return sql.NullString{}, err
	}
	if m != "" && slices.Contains(fresh, m) {
		return sql.NullString{String: m, Valid: true}, nil
	}
	labels := []string{server}
	if c.machine != "" && c.machine != server {
		labels = append(labels, c.machine)
	}
	for _, k := range fresh {
		if !slices.Contains(labels, k) {
			labels = append(labels, k)
		}
	}
	return sql.NullString{}, usageErr("--machine %q is not this host, the server, or a host with a fresh daemon; hosts: %s",
		m, strings.Join(labels, ", "))
}

func listStrings(q queryer, query string, args ...any) ([]string, error) {
	rows, err := q.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []string
	for rows.Next() {
		var s string
		if err := rows.Scan(&s); err != nil {
			return nil, err
		}
		out = append(out, s)
	}
	return out, rows.Err()
}
