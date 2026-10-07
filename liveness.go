package main

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"regexp"
	"strconv"
	"time"
)

var herdrListDeadline = 10 * time.Second

// herdrWaitDelay bounds how long a canceled or exited herdr subprocess may
// hold its stdout pipe open through a descendant; wait can overrun its
// deadline by at most this grace.
const herdrWaitDelay = 2 * time.Second

type agentObs struct {
	Name   string `json:"name"`
	Status string `json:"agent_status"`
	Seq    int64  `json:"state_change_seq"`
	PaneID string `json:"pane_id"`
}

// herdrAgentList runs `herdr agent list` and indexes the agents by pane id.
func herdrAgentList(sock string, deadline time.Time) (map[string]agentObs, error) {
	cx, cancel := context.WithTimeout(context.Background(), min(time.Until(deadline), herdrListDeadline))
	defer cancel()
	var stdout bytes.Buffer
	cmd, err := herdrCommand(cx, sock, "agent", "list")
	if err != nil {
		return nil, err
	}
	cmd.Stdout = &stdout
	cmd.WaitDelay = herdrWaitDelay
	if err := cmd.Run(); err != nil {
		return nil, herdrErr("herdr agent list failed: %v", err)
	}
	var resp struct {
		Result *struct {
			Agents []agentObs `json:"agents"`
		} `json:"result"`
	}
	if err := json.Unmarshal(stdout.Bytes(), &resp); err != nil {
		return nil, herdrErr("herdr agent list returned malformed JSON: %v", err)
	}
	if resp.Result == nil || resp.Result.Agents == nil {
		return nil, herdrErr("herdr agent list returned no result.agents")
	}
	byPane := map[string]agentObs{}
	for _, a := range resp.Result.Agents {
		if a.PaneID == "" || a.Status == "" {
			return nil, herdrErr("herdr agent list entry without pane_id or agent_status")
		}
		byPane[a.PaneID] = a
	}
	return byPane, nil
}

// watched is a child task's current launch and its stored observation.
type watched struct {
	TaskID, ParentID, LaunchID, Version int64
	Pane                                string
	Status                              sql.NullString
	Seq                                 sql.NullInt64
	Present                             bool
}

// watchedChildren skips gate tasks and tasks in a blocking wait: a waiting
// agent is legitimately working until its waiting_until passes.
// Only the server host's launches are this host's to observe.
func watchedChildren(q queryer, parent int64) ([]watched, error) {
	return watchedTasks(q, &parent)
}

// serverHost is the stored machine of the server host's launches.
var serverHost = sql.NullString{}

// watchedTasks is the liveness watch list: the children of parent, or with a
// nil parent every task that has one, under the same filter wait uses.
func watchedTasks(q queryer, parent *int64) ([]watched, error) {
	return watchedOn(q, parent, serverHost)
}

// watchedOn is watchedTasks for host's launches only: pane ids repeat
// across hosts, so a host reads only its own.
func watchedOn(q queryer, parent *int64, host sql.NullString) ([]watched, error) {
	rows, err := q.Query(`select t.id, t.parent_id, l.id, l.observed_version, coalesce(l.pane_id, t.pane_id),
		l.observed_status, l.observed_seq, l.present
		from tasks t join launches l on l.id = t.current_launch_id
		where t.parent_id is not null and (? is null or t.parent_id = ?)
		and t.status not in ('closed', 'planned') and t.role != 'gate' and coalesce(l.pane_id, t.pane_id) is not null
		and (t.waiting_until is null or t.waiting_until <= ?) and l.machine is ?
		order by t.id`, nullInt(parent), nullInt(parent), now(), host)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var ws []watched
	for rows.Next() {
		var w watched
		if err := rows.Scan(&w.TaskID, &w.ParentID, &w.LaunchID, &w.Version, &w.Pane, &w.Status, &w.Seq, &w.Present); err != nil {
			return nil, err
		}
		ws = append(ws, w)
	}
	return ws, rows.Err()
}

// change is one observation to write, guarded by the version it was computed from.
type change struct {
	w     watched
	agent agentObs
	found bool
	hint  bool // the observation is a hint candidate; applyChange applies the emission rules
}

// planChanges compares the listing with stored observations. A changed
// (status, seq) pair, or a reappearance, is written; a working agent is
// never a hint. A present-to-missing transition is written and is a hint
// candidate. Unchanged absence is nothing. applyChange applies the
// emission rules and writes the event or the suppressed counter.
func planChanges(ws []watched, agents map[string]agentObs) []change {
	var cs []change
	for _, w := range ws {
		a, ok := agents[w.Pane]
		switch {
		case ok && (!w.Status.Valid || w.Status.String != a.Status || w.Seq.Int64 != a.Seq || !w.Present):
			cs = append(cs, change{w: w, agent: a, found: true, hint: a.Status != "working"})
		case !ok && w.Present:
			cs = append(cs, change{w: w, hint: true})
		}
	}
	return cs
}

// current limits an observation update to a launch that is still the current
// launch of a task that is not closed.
const current = ` and exists (select 1 from tasks t where t.id = launches.task_id
	and t.status != 'closed' and t.current_launch_id = launches.id)`

// applyChange writes one observation with a compare-and-swap on
// observed_version, inserting its herdr event in the same transaction. The
// emission rules decide whether the hint is emitted or counted as
// suppressed; the observation is written either way.
func applyChange(db *sql.DB, ch change) (bool, error) {
	var wrote bool
	err := withTx(db, func(tx *sql.Tx) error {
		ts := now()
		var res sql.Result
		var err error
		if ch.found {
			res, err = tx.Exec(`update launches set observed_status = ?, observed_seq = ?, present = 1, observed_at = ?,
				observed_version = observed_version + 1 where id = ? and observed_version = ?`+current,
				ch.agent.Status, ch.agent.Seq, ts, ch.w.LaunchID, ch.w.Version)
		} else {
			res, err = tx.Exec(`update launches set present = 0, observed_at = ?, observed_version = observed_version + 1
				where id = ? and observed_version = ?`+current, ts, ch.w.LaunchID, ch.w.Version)
		}
		if err != nil {
			return err
		}
		if n, _ := res.RowsAffected(); n != 1 {
			return nil
		}
		wrote = true
		if !ch.hint {
			return nil
		}
		status := "missing"
		if ch.found {
			status = ch.agent.Status
		}
		emit, phase, err := hintRule(tx, ch.w.LaunchID, status)
		if err != nil {
			return err
		}
		if !emit {
			return bumpSuppressed(tx, status, phase)
		}
		data := map[string]any{"pane_id": ch.w.Pane, "present": ch.found}
		summary := "missing from herdr agent list"
		if ch.found {
			data["agent_status"], data["state_change_seq"] = ch.agent.Status, ch.agent.Seq
			if ch.agent.Name != "" {
				data["agent_name"] = ch.agent.Name
			}
			summary = fmt.Sprintf("agent_status %s (seq %d)", ch.agent.Status, ch.agent.Seq)
		}
		_, err = insertEvent(tx, event{TaskID: ch.w.TaskID, RecipientTaskID: ptr(ch.w.ParentID), LaunchID: ptr(ch.w.LaunchID),
			Kind: "herdr", Summary: summary, Data: data})
		return err
	})
	return wrote, err
}

// hintRule decides whether an observation of status on launch emits a herdr
// hint and, when it does not, names the phase recorded in the
// hint_suppressed counter (spec I4-A rules 1-5). Rule 1: a blocked lane
// always emits. Rule 2's capacity and quota observations emit on their own
// paths and never reach here. Rules 3 and 4: with a prompt newer than the
// launch's latest done or fail (owes), a got receipt on the newest prompt
// emits, and so does no got with no armed receipt_due row. Rule 5: once a
// ready is newer than the newest prompt, only blocked and present→missing
// still emit.
func hintRule(q queryer, launch int64, status string) (bool, string, error) {
	if status == "blocked" {
		return true, "", nil
	}
	var promptID int64
	owed, err := owes(q, launch, &promptID)
	if err != nil {
		return false, "", err
	}
	if promptID == 0 {
		return false, "startup", nil
	}
	if !owed {
		return false, "post_done", nil
	}
	var ready sql.NullInt64
	if err := q.QueryRow(`select max(id) from events where launch_id = ? and kind = 'ready'`, launch).Scan(&ready); err != nil {
		return false, "", err
	}
	var hooked bool
	if status == "idle" || status == "unknown" || status == "missing" || status == "done" {
		if err := q.QueryRow(`select exists (select 1 from launches where id = ? and session_source like 'hook:%'
			and session_ref is not null and session_ref != '')`, launch).Scan(&hooked); err != nil {
			return false, "", err
		}
		if hooked {
			if status == "missing" {
				return true, "", nil
			}
			return false, "hooked", nil
		}
	}
	postReady := ready.Valid && ready.Int64 > promptID
	if postReady && status != "missing" {
		return false, "post_ready", nil
	}
	var got, armed bool
	err = q.QueryRow(`select exists (select 1 from events where kind = 'got' and related_event_id = ?),
		exists (select 1 from meta where key = ?)`, promptID, fmt.Sprintf("receipt_due:%d", promptID)).Scan(&got, &armed)
	if err != nil {
		return false, "", err
	}
	if !got && armed && !(postReady && status == "missing") {
		return false, "armed", nil
	}
	return true, "", nil
}

// bumpSuppressed increments hint_suppressed:<day>:<status>:<phase> in meta
// for a hint the emission rules suppressed: launches keeps only the latest
// observation, so the phase F extractor reads the suppressed hints here.
func bumpSuppressed(tx *sql.Tx, status, phase string) error {
	key := fmt.Sprintf("hint_suppressed:%s:%s:%s", now()[:10], status, phase)
	_, err := tx.Exec(`insert into meta (key, value) values (?, '1')
		on conflict(key) do update set value = cast(cast(value as integer) + 1 as text)`, key)
	return err
}

// observeChildren runs one liveness pass for the children of parent.
func observeChildren(db *sql.DB, sock string, parent int64, deadline time.Time) error {
	_, err := observe(db, sock, &parent, deadline)
	return err
}

// observe runs one liveness pass over watchedTasks(parent): one herdr agent
// list, then one CAS write per changed observation. It returns the number of
// lane observations written; with a nil parent the same listing also records
// the server host's roots (lead.go), which count for nothing here.
func observe(db *sql.DB, sock string, parent *int64, deadline time.Time) (int, error) {
	ws, err := watchedTasks(db, parent)
	if err != nil {
		return 0, dbErr(err)
	}
	var leads []lead // the pass over every task also observes the server host's roots
	if parent == nil {
		if leads, err = leadsOn(db, serverHost); err != nil {
			return 0, dbErr(err)
		}
	}
	if len(ws) == 0 && len(leads) == 0 {
		return 0, nil
	}
	agents, err := herdrAgentList(sock, deadline)
	if err != nil {
		return 0, err
	}
	if err := observeLeads(db, serverHost, leads, agents); err != nil {
		return 0, dbErr(err)
	}
	n := 0
	for _, ch := range planChanges(ws, agents) {
		wrote, err := applyChange(db, ch)
		if err != nil {
			return n, err
		}
		if wrote {
			n++
		}
	}
	if err := scanCodexRolloutFallback(db, parent, deadline); err != nil {
		return n, err
	}
	return n, nil
}

// maybeObserve runs a liveness pass, and with scanQuota a quota scan, when
// this consumer's last poll is older than livenessInterval. The poll time is
// claimed before herdr runs, so back-to-back wait processes share one interval.
// While the daemon's heartbeat is fresh the daemon owns the liveness pass:
// last_poll_at is not claimed, so the first wait after the heartbeat goes
// stale observes at once, and a quota scan is throttled by its own claim.
// With no Herdr server on sock it does nothing: the inbox still works, and
// no herdr call may start a server.
func maybeObserve(db *sql.DB, sock string, as int64, deadline time.Time, scanQuota bool) error {
	ws, err := watchedChildren(db, as)
	if err != nil || len(ws) == 0 {
		return dbErr(err)
	}
	if !serverUp(sock) {
		return nil
	}
	fresh, err := daemonFresh(db)
	if err != nil {
		return err
	}
	if fresh {
		if !scanQuota {
			return nil
		}
		due, err := claimPoll(db, `select value from meta where key = ?`,
			`insert into meta (key, value) values (?2, ?1) on conflict(key) do update set value = excluded.value`,
			fmt.Sprintf("quota_poll:%d", as))
		if err != nil || !due {
			return err
		}
		return scanChildrenQuota(db, sock, as, deadline)
	}
	due, err := claimPoll(db, `select last_poll_at from tasks where id = ?`,
		`update tasks set last_poll_at = ?1 where id = ?2`, as)
	if err != nil || !due {
		return err
	}
	if err := observeChildren(db, sock, as, deadline); err != nil || !scanQuota {
		return err
	}
	return scanChildrenQuota(db, sock, as, deadline)
}

// claimPoll reads a poll time with get and, when it is older than
// livenessInterval or absent, writes now with set in the same transaction.
// set takes ?1 = now and ?2 = key.
func claimPoll(db *sql.DB, get, set string, key any) (bool, error) {
	due := false
	err := withTx(db, func(tx *sql.Tx) error {
		var last sql.NullString
		err := tx.QueryRow(get, key).Scan(&last)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return err
		}
		if last.Valid && time.Since(parseTime(last.String)) < livenessInterval {
			return nil
		}
		due = true
		_, err = tx.Exec(set, now(), key)
		return err
	})
	return due, err
}

var (
	quotaLimitRe = regexp.MustCompile(`(?i)hit your (weekly|usage|session) limit`)
	quotaLeftRe  = regexp.MustCompile(`(?i)weekly limit:\s*(\d+)% left`)
)

// quotaOf reads a quota hit from pane text: "limit" with percent 0, or
// "low" for the last "weekly limit: N% left" line with N <= 10.
func quotaOf(text []byte) (kind string, percent int, ok bool) {
	if quotaLimitRe.Match(text) {
		return "limit", 0, true
	}
	ms := quotaLeftRe.FindAllSubmatch(text, -1)
	if len(ms) == 0 {
		return "", 0, false
	}
	n, err := strconv.Atoi(string(ms[len(ms)-1][1]))
	if err != nil || n > 10 {
		return "", 0, false
	}
	return "low", n, true
}

// herdrPaneText runs `herdr agent read <pane> --source visible`; the raw
// stdout is scanned, so JSON and plain output both match.
func herdrPaneText(sock, pane string, deadline time.Time) ([]byte, error) {
	cx, cancel := context.WithTimeout(context.Background(), min(time.Until(deadline), herdrListDeadline))
	defer cancel()
	var stdout bytes.Buffer
	cmd, err := herdrCommand(cx, sock, "agent", "read", pane, "--source", "visible")
	if err != nil {
		return nil, err
	}
	cmd.Stdout = &stdout
	cmd.WaitDelay = herdrWaitDelay
	err = cmd.Run()
	return stdout.Bytes(), err
}

// scanChildrenQuota reads each watched child's visible pane and emits one
// herdr event per distinct quota key of its launch. The event's event_key,
// quota:<launch>:<key>, records the emission durably, so a key seen again on
// the same launch is silent. Pane text is never stored; a failed read is
// skipped silently.
func scanChildrenQuota(db *sql.DB, sock string, parent int64, deadline time.Time) error {
	ws, err := watchedChildren(db, parent)
	if err != nil {
		return dbErr(err)
	}
	for _, w := range ws {
		if time.Until(deadline) <= 0 {
			return nil
		}
		text, err := herdrPaneText(sock, w.Pane, deadline)
		if err != nil {
			continue
		}
		kind, pct, ok := quotaOf(text)
		if !ok {
			continue
		}
		key, summary := "limit", "quota limit hit"
		if kind == "low" {
			key, summary = fmt.Sprintf("low:%d", pct), fmt.Sprintf("quota %d%% left", pct)
		}
		eventKey := fmt.Sprintf("quota:%d:%s", w.LaunchID, key)
		err = withTx(db, func(tx *sql.Tx) error {
			var due bool
			err := tx.QueryRow(`select exists (select 1 from launches where id = ?`+current+`)
				and not exists (select 1 from events where event_key = ?)`, w.LaunchID, eventKey).Scan(&due)
			if err != nil || !due {
				return err
			}
			_, err = insertEvent(tx, event{TaskID: w.TaskID, RecipientTaskID: ptr(parent), LaunchID: ptr(w.LaunchID),
				Kind: "herdr", Summary: summary, Data: map[string]any{"quota": kind, "percent": pct, "pane_id": w.Pane},
				EventKey: eventKey})
			return err
		})
		if err != nil {
			return err
		}
	}
	return nil
}
