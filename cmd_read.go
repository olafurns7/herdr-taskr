package main

import (
	"database/sql"
	"flag"
	"fmt"
	"strings"
)

// readCapBytes is the hard budget on one compact read (I2). It is lifted by
// --all (status) and --limit 0 (log, asks).
const readCapBytes = 32 << 10
const trailerReserve = 96

type statusTrailer struct {
	Closed int64  `json:"closed"`
	All    string `json:"all"`
}

type logTrailer struct {
	Older    int64  `json:"older,omitempty"`
	Before   int64  `json:"before,omitempty"`
	More     int64  `json:"more,omitempty"`
	Next     int64  `json:"next,omitempty"`
	Launches int64  `json:"launches,omitempty"`
	All      string `json:"all,omitempty"`
}

type answeredTrailer struct {
	Answered int64  `json:"answered"`
	All      string `json:"all"`
}

// trailer prints the m1 continuation line after a compact read that left
// records out; --json never gets one.
func (c *ctx) trailer(v any) {
	if !c.json && v != nil {
		fmt.Fprintf(c.out, "m1 %s\n", jsonText(v))
	}
}

func cmdStatus(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("status", flag.ContinueOnError)
	tree := fs.Int64("tree", 0, "only this task and its descendants")
	all := fs.Bool("all", false, "include closed tasks")
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		return nil, 0, err
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	var ids []int64
	if *tree != 0 {
		ids, err = subtree(db, *tree)
	} else {
		ids, err = listIDs(db, `select id from tasks where ? or status != 'closed' order by id`, *all)
	}
	if err != nil {
		return nil, 0, dbErr(err)
	}
	var rows []map[string]any
	for _, id := range ids {
		m, err := taskStatus(db, id)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		rows = append(rows, m)
	}
	d, err := daemonRecord(db)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	c.lines = true
	var trailer any
	if !c.json {
		rows, trailer = budgetStatus(rows, *tree, *all)
	}
	for _, m := range rows {
		c.emit(m)
	}
	c.emit(d)
	c.trailer(trailer)
	return nil, exitOK, nil
}

// budgetStatus leaves out closed lanes in a tree read (the root is always
// kept) and counts them in the trailer. Open rows are never dropped, so
// open rows may exceed the cap. --all includes the closed lanes.
func budgetStatus(rows []map[string]any, root int64, all bool) ([]map[string]any, any) {
	dropped := int64(0)
	if root != 0 && !all {
		kept := rows[:0]
		for _, m := range rows {
			if m["id"] != root && m["status"] == "closed" {
				dropped++
				continue
			}
			kept = append(kept, m)
		}
		rows = kept
	}
	if root != 0 && !all && dropped > 0 {
		return rows, statusTrailer{Closed: dropped, All: "--all"}
	}
	return rows, nil
}

func taskStatus(q queryer, id int64) (map[string]any, error) {
	var name, role, status, updated string
	var acked, openAsks, blockingAsks, round int64
	var waiting bool
	var parent, launch, pending, oseq, lastReceipt sql.NullInt64
	var agent, ws, tab, pane, report, ostatus, oat, waitingUntil, host sql.NullString
	var present sql.NullBool
	err := q.QueryRow(`select t.name, t.role, t.status, t.updated_at, t.acked_event_id,
		(select count(*) from events a where a.task_id = t.id and a.kind = 'ask' and a.answered_by is null),
		(select count(*) from events a where a.task_id = t.id and a.kind = 'ask' and a.answered_by is null
			and json_extract(a.data, '$.blocking') = 1),
		(select count(*) from events p where p.task_id = t.id and p.kind = 'prompt' and p.launch_id is t.current_launch_id),
		(select g.related_event_id from events g where g.task_id = t.id and g.kind = 'got'
			and g.launch_id is t.current_launch_id order by g.id desc limit 1),
		coalesce(t.waiting_until > ?, 0), t.waiting_until,
		t.parent_id, t.current_launch_id, t.pending_event_id, l.observed_seq,
		t.agent_name, t.workspace_id, t.tab_id, t.pane_id, t.report_path, l.observed_status, l.observed_at, l.present, l.machine
		from tasks t left join launches l on l.id = t.current_launch_id where t.id = ?`, now(), id).
		Scan(&name, &role, &status, &updated, &acked, &openAsks, &blockingAsks, &round, &lastReceipt, &waiting, &waitingUntil,
			&parent, &launch, &pending, &oseq, &agent, &ws, &tab, &pane, &report, &ostatus, &oat, &present, &host)
	if err != nil {
		return nil, err
	}
	m := map[string]any{"id": id, "name": name, "role": role, "status": status, "updated_at": updated,
		"open_asks": openAsks, "blocking_asks": blockingAsks, "acked_event_id": acked, "waiting": waiting, "round": round}
	putInt(m, "last_receipt", lastReceipt)
	if waiting {
		putStr(m, "waiting_until", waitingUntil)
	}
	putInt(m, "parent_id", parent)
	putInt(m, "current_launch_id", launch)
	putInt(m, "pending_event_id", pending)
	putStr(m, "agent_name", agent)
	putStr(m, "workspace_id", ws)
	putStr(m, "tab_id", tab)
	putStr(m, "pane_id", pane)
	putStr(m, "report_path", report)
	if oat.Valid {
		obs := map[string]any{"at": oat.String, "present": present.Bool}
		putStr(obs, "agent_status", ostatus)
		putInt(obs, "state_change_seq", oseq)
		m["observed"] = obs
	}
	// Only the host's own daemon observes its lanes; without a fresh
	// heartbeat their state is unknown.
	if host.Valid && (status == "open" || status == "ready") {
		fresh, err := hostFresh(q, host.String)
		if err != nil {
			return nil, err
		}
		if !fresh {
			obs := map[string]any{"agent_status": "unknown", "host": host.String}
			if oat.Valid {
				obs["at"] = oat.String
			}
			m["observed"] = obs
		}
	}
	n, err := taskNext(q, id)
	if err != nil {
		return nil, err
	}
	if n != nil {
		m["next"] = n.Text
	}
	return m, nil
}

func cmdAsks(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("asks", flag.ContinueOnError)
	open := fs.Bool("open", false, "only unanswered asks")
	tree := fs.Int64("tree", 0, "only asks from this task and its descendants")
	owner := fs.Bool("owner", false, "only owner asks")
	limit := fs.Int64("limit", 0, "latest N answered asks, 0 for all (default 20)")
	limitGiven := false
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		return nil, 0, err
	}
	fs.Visit(func(f *flag.Flag) {
		if f.Name == "limit" {
			limitGiven = true
		}
	})
	if *limit < 0 {
		return nil, 0, usageErr("--limit must be >= 0, got %d", *limit)
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	where := []string{"e.kind = 'ask'"}
	var qargs []any
	if *open {
		where = append(where, "e.answered_by is null")
	}
	if *owner {
		where = append(where, "json_extract(e.data, '$.owner') = 1")
	}
	if *tree != 0 {
		ids, err := subtree(db, *tree)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		where = append(where, "e.task_id in ("+placeholders(len(ids))+")")
		for _, id := range ids {
			qargs = append(qargs, id)
		}
	}
	rows, err := db.Query(`select `+eventCols+`, ans.summary from events e join tasks t on t.id = e.task_id
		left join events ans on ans.id = e.answered_by where `+strings.Join(where, " and ")+` order by e.id`, qargs...)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	defer rows.Close()
	var lines []map[string]any
	for rows.Next() {
		var answer sql.NullString
		var m map[string]any
		m, err = scanEvent(scanExtra{rows, &answer})
		if err != nil {
			return nil, 0, dbErr(err)
		}
		putStr(m, "answer", answer)
		lines = append(lines, m)
	}
	if err := rows.Err(); err != nil {
		return nil, 0, dbErr(err)
	}
	c.lines = true
	var trailer any
	if !c.json || limitGiven {
		lines, trailer = budgetAsks(c.json, limitGiven, *limit, lines)
	}
	for _, m := range lines {
		c.emit(m)
	}
	c.trailer(trailer)
	return nil, exitOK, nil
}

// scanExtra appends extra destinations after the event columns.
type scanExtra struct {
	rows  *sql.Rows
	extra *sql.NullString
}

func (s scanExtra) Scan(dest ...any) error { return s.rows.Scan(append(dest, s.extra)...) }

// budgetAsks keeps every open ask plus the latest keep answered asks. The
// limit and the cap drop only answered asks, oldest first; open asks are
// never dropped and the newest record is never removed, so the output may
// exceed the cap when the open asks alone are larger than the budget.
// --limit 0 shows every answered ask and lifts the cap.
func budgetAsks(jsonMode, limitGiven bool, limit int64, lines []map[string]any) ([]map[string]any, any) {
	var open, answered []map[string]any
	for _, m := range lines {
		if m["answered_by"] == nil {
			open = append(open, m)
		} else {
			answered = append(answered, m)
		}
	}
	keep := int64(20)
	if limitGiven {
		keep = limit
	}
	dropped := int64(0)
	if keep > 0 && int64(len(answered)) > keep {
		dropped = int64(len(answered)) - keep
		answered = answered[len(answered)-int(keep):]
	}
	if !jsonMode && !(limitGiven && limit == 0) {
		total := readTotal(open) + readTotal(answered)
		newest := int64(0)
		if len(open) > 0 {
			newest = open[len(open)-1]["id"].(int64)
		}
		if len(answered) > 0 {
			if last := answered[len(answered)-1]["id"].(int64); last > newest {
				newest = last
			}
		}
		for len(answered) > 0 && total > readCapBytes-trailerReserve {
			if answered[0]["id"].(int64) == newest {
				break // never remove the newest record
			}
			total -= len(readLine(answered[0]))
			answered = answered[1:]
			dropped++
		}
	}
	lines = mergeByID(open, answered)
	if dropped > 0 {
		return lines, answeredTrailer{Answered: dropped, All: "--limit 0"}
	}
	return lines, nil
}

func cmdLog(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("log", flag.ContinueOnError)
	tree := fs.Bool("tree", false, "include descendants")
	since := fs.Int64("since", 0, "only events after this id")
	before := fs.Int64("before", 0, "only events before this id")
	limit := fs.Int64("limit", 0, "newest N events, 0 for all (default 100)")
	sinceGiven, beforeGiven, limitGiven := false, false, false
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	fs.Visit(func(f *flag.Flag) {
		switch f.Name {
		case "since":
			sinceGiven = true
		case "before":
			beforeGiven = true
		case "limit":
			limitGiven = true
		}
	})
	if *limit < 0 {
		return nil, 0, usageErr("--limit must be >= 0, got %d", *limit)
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
	ids := []int64{id}
	if *tree {
		if ids, err = subtree(db, id); err != nil {
			return nil, 0, dbErr(err)
		}
	} else if _, err := loadTask(db, id); err != nil {
		return nil, 0, dbErr(err)
	}
	in := placeholders(len(ids))
	var qargs []any
	for _, n := range ids {
		qargs = append(qargs, n)
	}
	var launches []map[string]any
	lrows, err := db.Query(`select id, task_id, provider, model, effort, account, native_home, session_ref, session_kind,
		session_source, herdr_scope, pane_id, observed_status, observed_seq, observed_version, present, observed_at, recorded_at,
		workspace_id, tab_id
		from launches where task_id in (`+in+`) order by id`, qargs...)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	for lrows.Next() {
		var lid, tid, version int64
		var present bool
		var recorded string
		var seq sql.NullInt64
		s := make([]sql.NullString, 14)
		if err := lrows.Scan(&lid, &tid, &s[0], &s[1], &s[2], &s[3], &s[4], &s[5], &s[6], &s[7], &s[8], &s[9], &s[10],
			&seq, &version, &present, &s[11], &recorded, &s[12], &s[13]); err != nil {
			lrows.Close()
			return nil, 0, dbErr(err)
		}
		m := map[string]any{"record": "launch", "id": lid, "task_id": tid, "observed_version": version,
			"present": present, "recorded_at": recorded}
		for i, k := range []string{"provider", "model", "effort", "account", "native_home", "session_ref", "session_kind",
			"session_source", "herdr_scope", "pane_id", "observed_status", "observed_at", "workspace_id", "tab_id"} {
			putStr(m, k, s[i])
		}
		putInt(m, "observed_seq", seq)
		launches = append(launches, m)
	}
	if err := lrows.Err(); err != nil {
		lrows.Close()
		return nil, 0, dbErr(err)
	}
	lrows.Close()
	eq := `select ` + eventCols + ` from events e join tasks t on t.id = e.task_id
		where e.task_id in (` + in + `)`
	var eargs []any
	eargs = append(eargs, qargs...)
	if sinceGiven {
		eq += ` and e.id > ?`
		eargs = append(eargs, *since)
	}
	if beforeGiven {
		eq += ` and e.id < ?`
		eargs = append(eargs, *before)
	}
	eq += ` order by e.id`
	rows, err := db.Query(eq, eargs...)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	defer rows.Close()
	var matched []map[string]any
	for rows.Next() {
		m, err := scanEvent(rows)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		matched = append(matched, m)
	}
	if err := rows.Err(); err != nil {
		return nil, 0, dbErr(err)
	}
	events := matched
	var trailer any
	if bounded := !c.json || limitGiven || sinceGiven || beforeGiven; bounded {
		keep := int64(100)
		if limitGiven {
			keep = *limit
		}
		if sinceGiven {
			// The forward cursor never skips events: the oldest ones
			// after the cursor go first.
			if keep > 0 && len(events) > int(keep) {
				events = events[:keep]
			}
		} else if keep > 0 && len(events) > int(keep) {
			events = events[len(events)-int(keep):]
		}
		statuses, current, err := taskStates(db, in, qargs)
		if err != nil {
			return nil, 0, err
		}
		// Launch rows: only the current launch of each task that is not
		// closed or has an event in the window.
		inWindow := map[int64]bool{}
		for _, e := range events {
			inWindow[e["task_id"].(int64)] = true
		}
		kept := launches[:0]
		for _, m := range launches {
			tid := m["task_id"].(int64)
			if m["id"].(int64) != current[tid] {
				continue
			}
			if statuses[tid] == "closed" && !inWindow[tid] {
				continue
			}
			kept = append(kept, m)
		}
		launches = kept
		droppedLaunches := int64(0)
		if !c.json && !(limitGiven && *limit == 0) {
			launches, events, droppedLaunches = capLog(launches, events, statuses, sinceGiven)
		}
		if left := int64(len(matched) - len(events)); left > 0 {
			if sinceGiven {
				trailer = logTrailer{More: left, Next: events[len(events)-1]["id"].(int64), Launches: droppedLaunches}
			} else {
				trailer = logTrailer{Older: left, Before: events[0]["id"].(int64), Launches: droppedLaunches}
			}
		} else if droppedLaunches > 0 {
			trailer = logTrailer{Launches: droppedLaunches, All: "--limit 0"}
		}
	}
	c.lines = true
	for _, m := range launches {
		c.emit(m)
	}
	for _, m := range events {
		c.emit(m)
	}
	c.trailer(trailer)
	return nil, exitOK, nil
}

// taskStates maps each task's status and current launch id.
func taskStates(q queryer, in string, qargs []any) (map[int64]string, map[int64]int64, error) {
	rows, err := q.Query(`select id, status, current_launch_id from tasks where id in (`+in+`)`, qargs...)
	if err != nil {
		return nil, nil, dbErr(err)
	}
	defer rows.Close()
	statuses := map[int64]string{}
	current := map[int64]int64{}
	for rows.Next() {
		var tid int64
		var status string
		var launch sql.NullInt64
		if err := rows.Scan(&tid, &status, &launch); err != nil {
			return nil, nil, dbErr(err)
		}
		statuses[tid] = status
		if launch.Valid {
			current[tid] = launch.Int64
		}
	}
	return statuses, current, rows.Err()
}

// capLog budgets event rows before closed launch rows. Backward pages drop
// oldest events first; forward pages drop newest events first. The newest
// backward event, oldest forward event, and open launch rows stay. Whole
// records are never cut, so those rows may exceed the cap on their own.
func capLog(launches, events []map[string]any, statuses map[int64]string, forward bool) ([]map[string]any, []map[string]any, int64) {
	total := readTotal(launches) + readTotal(events)
	for len(events) > 1 && total > readCapBytes-trailerReserve {
		if forward {
			total -= len(readLine(events[len(events)-1]))
			events = events[:len(events)-1]
		} else {
			total -= len(readLine(events[0]))
			events = events[1:]
		}
	}
	droppedLaunches := int64(0)
	for i := 0; i < len(launches) && total > readCapBytes-trailerReserve; {
		if statuses[launches[i]["task_id"].(int64)] == "closed" {
			total -= len(readLine(launches[i]))
			launches = append(launches[:i], launches[i+1:]...)
			droppedLaunches++
			continue
		}
		i++
	}
	return launches, events, droppedLaunches
}

// readTotal measures the compact bytes of whole records.
func readTotal(lines []map[string]any) int {
	total := 0
	for _, m := range lines {
		total += len(readLine(m))
	}
	return total
}

// mergeByID merges two id-ascending record slices back into one.
func mergeByID(a, b []map[string]any) []map[string]any {
	out := make([]map[string]any, 0, len(a)+len(b))
	i, j := 0, 0
	for i < len(a) && j < len(b) {
		if a[i]["id"].(int64) < b[j]["id"].(int64) {
			out = append(out, a[i])
			i++
			continue
		}
		out = append(out, b[j])
		j++
	}
	out = append(out, a[i:]...)
	out = append(out, b[j:]...)
	return out
}

func placeholders(n int) string {
	return strings.TrimSuffix(strings.Repeat("?,", n), ",")
}

func listIDs(q queryer, query string, args ...any) ([]int64, error) {
	rows, err := q.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var ids []int64
	for rows.Next() {
		var n int64
		if err := rows.Scan(&n); err != nil {
			return nil, err
		}
		ids = append(ids, n)
	}
	return ids, rows.Err()
}
