package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"
)

var (
	dbPollInterval   = time.Second
	livenessInterval = 15 * time.Second
)

func cmdWait(c *ctx, args []string) (res any, code int, err error) {
	fs := flag.NewFlagSet("wait", flag.ContinueOnError)
	as := fs.Int64("as", 0, "task id whose inbox to read (an orchestrator, or a worker waiting on its own ask)")
	timeout := fs.Int64("timeout", 540000, "milliseconds")
	scanQuota := fs.Bool("scan-quota", false, "also scan children's visible panes for quota lines")
	ack := fs.Int64("ack", 0, "ack this pending handled event, then wait")
	var kinds, from stringFlags
	fs.Var(&kinds, "for", "only return these event kinds (comma-separated; repeatable)")
	fs.Var(&from, "from", "only return events from this task name or id (repeatable)")
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		return nil, 0, err
	}
	asGiven := flagWasSet(fs, "as")
	callerTask := c.env("TASKR_TASK")
	if !asGiven && callerTask == "" {
		return nil, 0, usageErr("wait needs --as TASK_ID")
	}
	if asGiven && *as <= 0 {
		return nil, 0, usageErr("wait needs --as TASK_ID")
	}
	if asGiven && callerTask != "" {
		id, err := parseID(callerTask, "TASKR_TASK")
		if err != nil {
			return nil, 0, err
		}
		if id != *as {
			return nil, 0, rejectErr("--as %d names a different task than TASKR_TASK=%s", *as, callerTask)
		}
	}
	if *timeout < 0 {
		return nil, 0, usageErr("--timeout must not be negative")
	}
	ackGiven := false
	fs.Visit(func(f *flag.Flag) {
		if f.Name == "ack" {
			ackGiven = true
		}
	})
	if ackGiven && *ack <= 0 {
		return nil, 0, usageErr("--ack must be a positive event id")
	}
	wantKind := map[string]bool{}
	for _, value := range kinds {
		for _, kind := range strings.Split(value, ",") {
			if kindCodes[kind] == "" {
				return nil, 0, usageErr("unknown event kind %q", kind)
			}
			wantKind[kind] = true
		}
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	resolveCaller := !asGiven || (callerTask != "" && c.env("TASKR_LAUNCH") != "")
	if resolveCaller {
		if err := withTx(db, func(tx *sql.Tx) error {
			w, err := resolveWorker(c, tx)
			if err == nil && !asGiven {
				*as = w.task.ID
			}
			return err
		}); err != nil {
			return nil, 0, err
		}
	}
	if t, err := loadTask(db, *as); err != nil {
		return nil, 0, dbErr(err)
	} else if t.Status == "planned" {
		return nil, 0, rejectErr("task %d is planned and has no inbox; launch it first", *as)
	}
	if err := checkTaskHost(c, db, *as); err != nil {
		return nil, 0, err
	}
	wantTask := map[int64]bool{}
	for _, value := range from {
		id, err := strconv.ParseInt(value, 10, 64)
		if err == nil {
			if id <= 0 {
				return nil, 0, usageErr("--from task id must be positive")
			}
			if _, err := loadTask(db, id); err != nil {
				return nil, 0, dbErr(err)
			}
		} else {
			ids, err := listIDs(db, `select id from tasks where name = ?`, value)
			if err != nil {
				return nil, 0, dbErr(err)
			}
			if len(ids) != 1 {
				return nil, 0, rejectErr("--from name %q matches %d tasks; use a task id", value, len(ids))
			}
			id = ids[0]
		}
		wantTask[id] = true
	}
	filtered := len(wantKind) > 0 || len(wantTask) > 0
	if *ack != 0 {
		// Over RPC a dropped response makes the caller retry its wait --ack:
		// an event this task already acked counts as done there. The local
		// CLI stays strict.
		if _, err := ackInbox(c, db, *as, *ack, c.rpc); err != nil {
			return nil, 0, err
		}
	}
	skipped := 0
	defer func() {
		if skipped > 0 {
			if c.json {
				c.emit(map[string]any{"as": *as, "skipped": skipped})
			} else {
				fmt.Fprintf(c.out, "sk1 %d\n", skipped)
			}
		}
		if err != nil && *ack != 0 {
			m, ok := res.(map[string]any)
			if !ok {
				m = map[string]any{}
			}
			m["acked_event_id"] = *ack
			res = m
		}
	}()
	deadline := clockNow().Add(time.Duration(*timeout) * time.Millisecond)
	// The server passes the request's context; the CLI stops on a signal.
	sig := c.cx
	if sig == nil {
		var stop context.CancelFunc
		sig, stop = signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM, syscall.SIGHUP)
		defer stop()
	}
	var marker string // this wait's waiting_until, once it blocks
	// Clearing the marker is part of the result: if it fails, the wait exits 4
	// instead of printing its event, and an offered event stays pending, so the
	// next wait replays it. The stale marker then expires at its deadline.
	defer func() {
		if marker == "" {
			return
		}
		cerr := withTx(db, func(tx *sql.Tx) error {
			if err := checkTaskHost(c, tx, *as); err != nil {
				return err
			}
			_, err := tx.Exec(`update tasks set waiting_until = null where id = ? and waiting_until = ?`, *as, marker)
			return err
		})
		if cerr == nil || err != nil {
			return
		}
		out := map[string]any{"as": *as, "stale_waiting_until": marker}
		if m, ok := res.(map[string]any); ok && m["event"] != nil {
			out["pending_event_id"] = m["event"].(map[string]any)["id"]
		}
		res, code, err = out, 0, dbErr(fmt.Errorf("clear waiting_until: %w", cerr))
	}()
	found := func(ev map[string]any, replay bool) (any, int, error) {
		return map[string]any{"as": *as, "event": ev, "replay": replay}, exitOK, nil
	}
	accept := func(ev map[string]any) (bool, error) {
		kind, _ := ev["kind"].(string)
		data, _ := ev["data"].(map[string]any)
		if waitBypass(kind, data) {
			return true, nil
		}
		task, _ := ev["task_id"].(int64)
		if (len(wantKind) == 0 || wantKind[kind]) && (len(wantTask) == 0 || wantTask[task]) {
			return true, nil
		}
		if _, err := ackInbox(c, db, *as, ev["id"].(int64), false); err != nil {
			return false, err
		}
		skipped++
		return false, nil
	}
	timeoutResult := func() (any, int, error) {
		owed, due, err := waitCounts(db, *as)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		return map[string]any{"timeout": true, "as": *as, "owed": owed, "due": due}, exitTimeout, nil
	}
	for {
		select {
		case <-sig.Done():
			return map[string]any{"timeout": true, "interrupted": true, "as": *as}, exitTimeout, nil
		default:
		}
		// Both offer paths return here after a skip. timeout0 still polls the
		// queued backlog immediately, without a positive deadline.
		if skipped > 0 && *timeout > 0 && time.Until(deadline) <= 0 {
			return timeoutResult()
		}
		if *timeout > 0 {
			if err := maybeCapacity(db, socketPath(c), *as, deadline); err != nil {
				return nil, 0, err
			}
		}
		expireReceiptsNow(db) // an expiry error never fails a wait
		ev, replay, coalesced, err := offer(c, db, *as, filtered)
		if err != nil {
			return nil, 0, err
		}
		if coalesced {
			skipped++
			continue
		}
		if ev != nil {
			if match, err := accept(ev); err != nil {
				return nil, 0, err
			} else if match {
				return found(ev, replay)
			}
			continue
		}
		if time.Until(deadline) <= 0 {
			return timeoutResult()
		}
		if marker == "" {
			marker = stamp(deadline)
			if err := withTx(db, func(tx *sql.Tx) error {
				if err := checkTaskHost(c, tx, *as); err != nil {
					return err
				}
				_, err := tx.Exec(`update tasks set waiting_until = ? where id = ?`, marker, *as)
				return err
			}); err != nil {
				marker = ""
				return nil, 0, err
			}
		}
		if err := maybeObserve(db, socketPath(c), *as, deadline, *scanQuota); err != nil {
			return nil, 0, err
		}
		if ev, replay, coalesced, err = offer(c, db, *as, filtered); err != nil {
			return nil, 0, err
		} else if coalesced {
			skipped++
			continue
		} else if ev != nil {
			if match, err := accept(ev); err != nil {
				return nil, 0, err
			} else if match {
				return found(ev, replay)
			}
			continue
		}
		left := time.Until(deadline)
		if left <= 0 {
			return timeoutResult()
		}
		select {
		case <-sig.Done():
			return map[string]any{"timeout": true, "interrupted": true, "as": *as}, exitTimeout, nil
		case <-time.After(min(left, dbPollInterval)):
		}
	}
}

// offer returns the oldest unacked event addressed to task as and records it
// as pending. replay is true when that event was already pending. Filtered
// waits may acknowledge superseded inbox heads here.
func offer(c *ctx, db *sql.DB, as int64, filtered bool) (map[string]any, bool, bool, error) {
	var ev map[string]any
	var replay, coalesced bool
	err := withTx(db, func(tx *sql.Tx) error {
		t, err := loadTask(tx, as)
		if err != nil {
			return err
		}
		if err := checkTaskHost(c, tx, as); err != nil {
			return err
		}
		var id int64
		err = tx.QueryRow(`select id from events where recipient_task_id = ? and id > ? order by id limit 1`,
			as, t.AckedEventID).Scan(&id)
		if errors.Is(err, sql.ErrNoRows) {
			return nil
		} else if err != nil {
			return err
		}
		ev, err = loadEvent(tx, id)
		if err != nil {
			return err
		}
		coalesced, err = coalesceInboxHead(tx, as, ev, t.PendingEventID, filtered)
		if err != nil {
			return err
		}
		if coalesced {
			_, err = tx.Exec(`update tasks set acked_event_id = ?, pending_event_id = null where id = ?`, id, as)
			return err
		}
		replay = t.PendingEventID.Valid && t.PendingEventID.Int64 == id
		if !replay {
			if _, err := tx.Exec(`update tasks set pending_event_id = ? where id = ?`, id, as); err != nil {
				return err
			}
		}
		return nil
	})
	return ev, replay, coalesced, err
}

func waitBypass(kind string, data map[string]any) bool {
	if kind != "herdr" {
		return false
	}
	if data["reason"] == "model_capacity" || data["reason"] == "stall" {
		return true
	}
	_, quota := data["quota"]
	return quota
}

func coalesceInboxHead(tx *sql.Tx, as int64, ev map[string]any, pending sql.NullInt64, filtered bool) (bool, error) {
	kind, _ := ev["kind"].(string)
	if !filtered {
		if kind == "prompt_outcome" && ev["summary"] == "late_receipt" {
			_, err := takeNoReceiptCoalesced(tx, ev)
			return false, err
		}
		return false, nil
	}
	data, _ := ev["data"].(map[string]any)
	if waitBypass(kind, data) {
		return false, nil
	}
	if kind == "herdr" {
		_, reason := data["reason"]
		_, quota := data["quota"]
		if reason || quota {
			return false, nil
		}
		taskID, _ := ev["task_id"].(int64)
		launchID, hasLaunch := ev["launch_id"].(int64)
		var status string
		var current sql.NullInt64
		if err := tx.QueryRow(`select status, current_launch_id from tasks where id = ?`, taskID).Scan(&status, &current); err != nil {
			return false, err
		}
		if status == "closed" || !hasLaunch || !current.Valid || current.Int64 != launchID {
			return true, nil
		}
		var newer bool
		if err := tx.QueryRow(`select exists (select 1 from events where recipient_task_id = ? and id > ?
			and launch_id = ? and kind = 'herdr' and json_type(data, '$.reason') is null
			and json_type(data, '$.quota') is null)`, as, ev["id"], launchID).Scan(&newer); err != nil {
			return false, err
		}
		if newer {
			return true, nil
		}
		if err := tx.QueryRow(`select exists (select 1 from events where recipient_task_id = ? and id > ?
			and launch_id = ? and kind in ('ready', 'done', 'fail', 'ask'))`, as, ev["id"], launchID).Scan(&newer); err != nil {
			return false, err
		}
		return newer, nil
	}
	if kind != "prompt_outcome" {
		return false, nil
	}
	if ev["summary"] == "no_receipt" {
		noReceiptID, ok := ev["id"].(int64)
		if !ok || (pending.Valid && pending.Int64 == noReceiptID) {
			return false, nil
		}
		var got bool
		if err := tx.QueryRow(`select exists (select 1 from events where kind = 'got' and related_event_id = ?)`, ev["related_event_id"]).Scan(&got); err != nil {
			return false, err
		}
		if !got {
			return false, nil
		}
		_, err := tx.Exec(`insert into meta (key, value) values (?, '1') on conflict(key) do nothing`, noReceiptCoalescedKey(noReceiptID))
		return err == nil, err
	}
	if ev["summary"] != "late_receipt" {
		return false, nil
	}
	return takeNoReceiptCoalesced(tx, ev)
}

func noReceiptCoalescedKey(eventID int64) string {
	return "no_receipt_coalesced:" + strconv.FormatInt(eventID, 10)
}

func takeNoReceiptCoalesced(tx *sql.Tx, ev map[string]any) (bool, error) {
	eventID, ok := ev["id"].(int64)
	if !ok {
		return false, nil
	}
	var noReceiptID sql.NullInt64
	if err := tx.QueryRow(`select json_extract(data, '$.no_receipt_event_id') from events where id = ?`, eventID).Scan(&noReceiptID); err != nil {
		return false, err
	}
	if !noReceiptID.Valid {
		return false, nil
	}
	res, err := tx.Exec(`delete from meta where key = ?`, noReceiptCoalescedKey(noReceiptID.Int64))
	if err != nil {
		return false, err
	}
	n, err := res.RowsAffected()
	return n > 0, err
}

func waitCounts(q queryer, as int64) (owed, due int, err error) {
	rows, err := q.Query(`select current_launch_id from tasks where parent_id = ? and current_launch_id is not null and status != 'closed'`, as)
	if err != nil {
		return 0, 0, err
	}
	var launches []int64
	for rows.Next() {
		var launch int64
		if err := rows.Scan(&launch); err != nil {
			rows.Close()
			return 0, 0, err
		}
		launches = append(launches, launch)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return 0, 0, err
	}
	for _, launch := range launches {
		isOwed, err := owes(q, launch)
		if err != nil {
			return 0, 0, err
		}
		if isOwed {
			owed++
		}
	}
	rows, err = q.Query(`select value from meta where key >= 'receipt_due:' and key < 'receipt_due;'`)
	if err != nil {
		return 0, 0, err
	}
	defer rows.Close()
	for rows.Next() {
		var value string
		if err := rows.Scan(&value); err != nil {
			return 0, 0, err
		}
		var receipt receiptDue
		if json.Unmarshal([]byte(value), &receipt) == nil && receipt.Recipient == as {
			due++
		}
	}
	return owed, due, rows.Err()
}

func cmdAck(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("ack", flag.ContinueOnError)
	as := fs.Int64("as", 0, "task id whose inbox event to ack")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	evID, err := parseID(pos[0], "event id")
	if err != nil {
		return nil, 0, err
	}
	if *as <= 0 {
		return nil, 0, usageErr("ack needs --as TASK_ID")
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	out, err := ackInbox(c, db, *as, evID, true)
	if err != nil {
		return nil, 0, err
	}
	return out, exitOK, nil
}

// ackInbox shares the pending-event check. wait --ack is strict: only the
// handled pending event may advance its loop; standalone ack stays idempotent.
func ackInbox(c *ctx, db *sql.DB, as, evID int64, allowAlready bool) (map[string]any, error) {
	out := map[string]any{"ok": true, "as": as, "acked_event_id": evID}
	err := withTx(db, func(tx *sql.Tx) error {
		t, err := loadTask(tx, as)
		if err != nil {
			return err
		}
		if err := checkTaskHost(c, tx, as); err != nil {
			return err
		}
		if t.Status == "planned" {
			return rejectErr("task %d is planned and has no inbox; launch it first", as)
		}
		if t.PendingEventID.Valid && t.PendingEventID.Int64 == evID {
			_, err := tx.Exec(`update tasks set acked_event_id = ?, pending_event_id = null where id = ?`, evID, as)
			return err
		}
		var recip sql.NullInt64
		err = tx.QueryRow(`select recipient_task_id from events where id = ?`, evID).Scan(&recip)
		if allowAlready && err == nil && evID <= t.AckedEventID && recip.Valid && recip.Int64 == as {
			out["already"] = true
			out["acked_event_id"] = t.AckedEventID
			return nil
		}
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return err
		}
		if t.PendingEventID.Valid {
			return rejectErr("event %d is not the pending event %d of task %d", evID, t.PendingEventID.Int64, as)
		}
		return rejectErr("task %d has no pending event; run wait first", as)
	})
	if err != nil {
		return nil, err
	}
	return out, nil
}

type stringFlags []string

func (s *stringFlags) String() string     { return strings.Join(*s, ",") }
func (s *stringFlags) Set(v string) error { *s = append(*s, v); return nil }
