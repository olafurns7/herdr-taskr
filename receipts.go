package main

import (
	"database/sql"
	"encoding/json"
	"errors"
	"strconv"
	"time"
)

// Asynchronous receipts (v0.11): a prompt sent without --confirm arms a
// receipt deadline in meta, `receipt_due:<attempt>`, inside the attempt's
// transaction. The worker's got deletes it. expireReceipts turns a deadline
// that passed with no got into one prompt_outcome no_receipt event in the
// target's parent inbox; a got after any no_receipt writes late_receipt.

// defaultReceiptTimeout is the receipt window of a prompt without --confirm.
const defaultReceiptTimeout = 300000 * time.Millisecond

// minReceiptTimeout is the shortest window --receipt-timeout accepts; 0 disarms.
const minReceiptTimeout = 60000

const receiptDuePrefix = "receipt_due:"

func receiptDueKey(attempt int64) string { return receiptDuePrefix + strconv.FormatInt(attempt, 10) }

// receiptDue is a pending deadline's meta value.
type receiptDue struct {
	DueAt     string `json:"due_at"`
	Recipient int64  `json:"recipient"`
	Task      int64  `json:"task"`
	Launch    *int64 `json:"launch"`
}

// checkReceiptTimeout validates --receipt-timeout: given with --confirm, or
// 1-59999, is a usage error.
func checkReceiptTimeout(ms int64, given, confirm bool) error {
	switch {
	case !given:
		return nil
	case confirm:
		return usageErr("--receipt-timeout does not apply with --confirm")
	case ms < 0:
		return usageErr("--receipt-timeout must not be negative")
	case ms > 0 && ms < minReceiptTimeout:
		return usageErr("--receipt-timeout must be 0 (no deadline) or at least %d ms", minReceiptTimeout)
	}
	return nil
}

// armReceipt records the receipt deadline of attempt, due window after the
// attempt's own timestamp. It arms nothing when a got already exists.
func armReceipt(tx *sql.Tx, attempt, task int64, launch *int64, recipient int64, window time.Duration) error {
	var created string
	var got bool
	if err := tx.QueryRow(`select created_at, exists (select 1 from events where event_key = ?) from events where id = ?`,
		gotKeyPrefix+strconv.FormatInt(attempt, 10), attempt).Scan(&created, &got); err != nil {
		return err
	}
	if got {
		return nil
	}
	v, err := json.Marshal(receiptDue{DueAt: stamp(parseTime(created).Add(window)), Recipient: recipient, Task: task, Launch: launch})
	if err != nil {
		return err
	}
	_, err = tx.Exec(`insert into meta (key, value) values (?, ?) on conflict(key) do update set value = excluded.value`,
		receiptDueKey(attempt), string(v))
	return err
}

// receiptWindow is the armed window of attempt in milliseconds: its due_at
// less the attempt's timestamp. armed is false without a pending deadline.
func receiptWindow(q queryer, attempt int64) (ms int64, armed bool, err error) {
	var v, created string
	err = q.QueryRow(`select m.value, e.created_at from meta m join events e on e.id = ? where m.key = ?`,
		attempt, receiptDueKey(attempt)).Scan(&v, &created)
	if errors.Is(err, sql.ErrNoRows) {
		return 0, false, nil
	} else if err != nil {
		return 0, false, err
	}
	var d receiptDue
	if err := json.Unmarshal([]byte(v), &d); err != nil {
		return 0, false, nil
	}
	return parseTime(d.DueAt).Sub(parseTime(created)).Milliseconds(), true, nil
}

func disarmReceipt(tx *sql.Tx, attempt int64) error {
	_, err := tx.Exec(`delete from meta where key = ?`, receiptDueKey(attempt))
	return err
}

// deleteTaskReceipts drops task's pending deadlines: a replacement launch or
// a close makes them unmeetable.
func deleteTaskReceipts(tx *sql.Tx, task int64) error {
	rows, err := tx.Query(`select key, value from meta where key >= 'receipt_due:' and key < 'receipt_due;'`)
	if err != nil {
		return err
	}
	var keys []string
	for rows.Next() {
		var k, v string
		if err := rows.Scan(&k, &v); err != nil {
			rows.Close()
			return err
		}
		var d receiptDue
		if json.Unmarshal([]byte(v), &d) != nil || d.Task == task {
			keys = append(keys, k)
		}
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return err
	}
	for _, k := range keys {
		if _, err := tx.Exec(`delete from meta where key = ?`, k); err != nil {
			return err
		}
	}
	return nil
}

// expireReceipts materializes every deadline due at now. It is idempotent:
// each row is deleted in the same transaction that writes its one
// no_receipt, keyed no_receipt:<attempt>. A deadline that can no longer be
// met (task closed, launch not current, a got already recorded) is dropped
// without an alarm.
func expireReceipts(tx *sql.Tx, now time.Time) error {
	rows, err := tx.Query(`select key, value from meta where key >= 'receipt_due:' and key < 'receipt_due;'`)
	if err != nil {
		return err
	}
	type due struct {
		key     string
		attempt int64
		d       receiptDue
		bad     bool
	}
	var hits []due
	for rows.Next() {
		var k, v string
		if err := rows.Scan(&k, &v); err != nil {
			rows.Close()
			return err
		}
		x := due{key: k}
		var perr error
		x.attempt, perr = strconv.ParseInt(k[len(receiptDuePrefix):], 10, 64)
		x.bad = perr != nil || json.Unmarshal([]byte(v), &x.d) != nil
		if x.bad || !parseTime(x.d.DueAt).After(now) {
			hits = append(hits, x)
		}
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return err
	}
	for _, x := range hits {
		if _, err := tx.Exec(`delete from meta where key = ?`, x.key); err != nil {
			return err
		}
		if x.bad {
			continue
		}
		var status string
		var current sql.NullInt64
		err := tx.QueryRow(`select status, current_launch_id from tasks where id = ?`, x.d.Task).Scan(&status, &current)
		if errors.Is(err, sql.ErrNoRows) {
			continue
		} else if err != nil {
			return err
		}
		if status == "closed" || x.d.Launch == nil || !current.Valid || current.Int64 != *x.d.Launch {
			continue
		}
		var created string
		var met bool
		err = tx.QueryRow(`select created_at, exists (select 1 from events where event_key in (?, ?)) from events where id = ?`,
			gotKeyPrefix+strconv.FormatInt(x.attempt, 10), noReceiptKeyPrefix+strconv.FormatInt(x.attempt, 10), x.attempt).
			Scan(&created, &met)
		if errors.Is(err, sql.ErrNoRows) {
			continue
		} else if err != nil {
			return err
		}
		if met {
			continue
		}
		window := parseTime(x.d.DueAt).Sub(parseTime(created)).Milliseconds()
		if _, err := insertEvent(tx, event{TaskID: x.d.Task, RecipientTaskID: ptr(x.d.Recipient), LaunchID: x.d.Launch,
			Kind: "prompt_outcome", Summary: "no_receipt", RelatedEventID: ptr(x.attempt),
			EventKey: noReceiptKeyPrefix + strconv.FormatInt(x.attempt, 10),
			Data:     map[string]any{"outcome": "no_receipt", "window_ms": window, "async": true}}); err != nil {
			return err
		}
	}
	return nil
}

// expireReceiptsNow runs expireReceipts for a wait or a daemon pass. It
// opens a write transaction only when a deadline is pending; the caller
// ignores its error, which never fails a wait.
func expireReceiptsNow(db *sql.DB) error {
	var pending bool
	if err := db.QueryRow(`select exists (select 1 from meta where key >= 'receipt_due:' and key < 'receipt_due;')`).
		Scan(&pending); err != nil || !pending {
		return err
	}
	return withTx(db, func(tx *sql.Tx) error { return expireReceipts(tx, time.Now()) })
}

// lateReceipt writes prompt_outcome late_receipt when attempt already has a
// no_receipt (async or --confirm), to that alarm's inbox or else the worker's
// parent. The task_id lookup uses the events_task index.
func lateReceipt(c *ctx, tx *sql.Tx, w *worker, attempt, gotID int64) error {
	var nr int64
	var recip sql.NullInt64
	var at string
	err := tx.QueryRow(`select id, recipient_task_id, created_at from events where task_id = ? and kind = 'prompt_outcome'
		and summary = 'no_receipt' and related_event_id = ? order by id limit 1`, w.task.ID, attempt).Scan(&nr, &recip, &at)
	if errors.Is(err, sql.ErrNoRows) {
		return nil
	} else if err != nil {
		return err
	}
	var gotAt string
	if err := tx.QueryRow(`select created_at from events where id = ?`, gotID).Scan(&gotAt); err != nil {
		return err
	}
	to := parentRecipient(w.task)
	if recip.Valid {
		to = ptr(recip.Int64)
	}
	_, err = c.insertEvent(tx, event{TaskID: w.task.ID, RecipientTaskID: to, LaunchID: w.launchID,
		Kind: "prompt_outcome", Summary: "late_receipt", RelatedEventID: ptr(attempt),
		EventKey: lateReceiptKeyPrefix + strconv.FormatInt(attempt, 10),
		Data: map[string]any{"outcome": "late_receipt", "got_event_id": gotID, "no_receipt_event_id": nr,
			"delay_ms": parseTime(gotAt).Sub(parseTime(at)).Milliseconds()}})
	return err
}
