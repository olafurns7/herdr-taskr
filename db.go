package main

import (
	"context"
	"database/sql"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"time"

	_ "modernc.org/sqlite"
)

//go:embed schema.sql
var schemaSQL string

func dbPath(c *ctx) (string, error) {
	if p := c.env("TASKR_DB"); p != "" {
		return p, nil
	}
	home := c.env("HOME")
	if home == "" {
		return "", usageErr("HOME is not set and TASKR_DB is empty")
	}
	return filepath.Join(home, ".local", "state", "taskr", "taskr.db"), nil
}

// openDB opens the ledger with WAL, busy_timeout and foreign keys on every
// pooled connection, and immediate write transactions.
func openDB(c *ctx) (*sql.DB, error) {
	if c.db != nil {
		return c.db, nil // the daemon's resident ledger, already migrated
	}
	if c.client {
		return nil, dbErr(errors.New("client mode never opens a local ledger"))
	}
	p, err := dbPath(c)
	if err != nil {
		return nil, err
	}
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return nil, dbErr(err)
	}
	q := url.Values{}
	q.Add("_pragma", "busy_timeout(5000)")
	q.Add("_pragma", "journal_mode(WAL)")
	q.Add("_pragma", "foreign_keys(1)")
	q.Add("_txlock", "immediate")
	db, err := sql.Open("sqlite", "file:"+p+"?"+q.Encode())
	if err != nil {
		return nil, dbErr(err)
	}
	if err := withTx(db, func(tx *sql.Tx) error {
		if _, err := tx.Exec(schemaSQL); err != nil {
			return err
		}
		return migrate(tx)
	}); err != nil {
		db.Close()
		return nil, dbErr(fmt.Errorf("schema: %w", err))
	}
	return db, nil
}

// closeDB closes a ledger openDB opened; the resident one stays open.
func closeDB(c *ctx, db *sql.DB) {
	if db != c.db {
		db.Close()
	}
}

// migrate adds columns that `create table if not exists` cannot add to an
// existing ledger. It runs in the schema transaction and is idempotent.
func migrate(tx *sql.Tx) error {
	for _, c := range []struct{ table, column string }{
		{"tasks", "waiting_until"},
		{"launches", "workspace_id"}, // v0.6: launch --workspace/--tab
		{"launches", "tab_id"},
		{"tasks", "machine"}, // v0.10: NULL is the server host
		{"launches", "machine"},
		{"launches", "transcript_path"}, // v0.11: harness hook session transcript
		{"requests", "upload"},          // v0.13: captured documents requested from RPC clients
	} {
		var n int
		if err := tx.QueryRow(`select count(*) from pragma_table_info(?) where name = ?`, c.table, c.column).Scan(&n); err != nil {
			return err
		}
		if n == 0 {
			if _, err := tx.Exec(`alter table ` + c.table + ` add column ` + c.column + ` text`); err != nil {
				return err
			}
		}
	}
	return nil
}

// plannedAncestor returns the nearest planned task among id's ancestors, or 0.
func plannedAncestor(q queryer, id int64) (int64, error) {
	for i := 0; i < 1000; i++ {
		var parent sql.NullInt64
		if err := q.QueryRow(`select parent_id from tasks where id = ?`, id).Scan(&parent); err != nil {
			return 0, err
		}
		if !parent.Valid {
			return 0, nil
		}
		id = parent.Int64
		var status string
		if err := q.QueryRow(`select status from tasks where id = ?`, id).Scan(&status); err != nil {
			return 0, err
		}
		if status == "planned" {
			return id, nil
		}
	}
	return 0, fmt.Errorf("parent chain of task %d is too deep", id)
}

const timeFormat = "2006-01-02T15:04:05.000Z"

func now() string { return stamp(time.Now()) }

func stamp(t time.Time) string { return t.UTC().Format(timeFormat) }

func parseTime(s string) time.Time {
	t, _ := time.Parse(timeFormat, s)
	return t
}

// withTx runs fn in one immediate transaction and commits only if fn succeeds.
// The deferred rollback also releases the writer lock when fn panics (the
// dashboard's HTTP server recovers handler panics and keeps running); after a
// commit it is a no-op.
func withTx(db *sql.DB, fn func(tx *sql.Tx) error) error {
	tx, err := db.BeginTx(context.Background(), nil)
	if err != nil {
		return dbErr(err)
	}
	defer tx.Rollback()
	if err := fn(tx); err != nil {
		return dbErr(err)
	}
	return dbErr(tx.Commit())
}

type queryer interface {
	QueryRow(query string, args ...any) *sql.Row
	Query(query string, args ...any) (*sql.Rows, error)
}

type task struct {
	ID, AckedEventID                          int64
	ParentID, CurrentLaunchID, PendingEventID sql.NullInt64
	Name, Role, Status                        string
	AgentName, PaneID, Cwd, LastPollAt        sql.NullString
	WaitingUntil                              sql.NullString
}

func loadTask(q queryer, id int64) (*task, error) {
	t := &task{}
	err := q.QueryRow(`select id, parent_id, name, agent_name, role, status, pane_id, cwd,
		current_launch_id, acked_event_id, pending_event_id, last_poll_at, waiting_until from tasks where id = ?`, id).
		Scan(&t.ID, &t.ParentID, &t.Name, &t.AgentName, &t.Role, &t.Status, &t.PaneID, &t.Cwd,
			&t.CurrentLaunchID, &t.AckedEventID, &t.PendingEventID, &t.LastPollAt, &t.WaitingUntil)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, rejectErr("task %d does not exist", id)
	}
	return t, err
}

type event struct {
	TaskID          int64
	RecipientTaskID *int64
	LaunchID        *int64
	Kind, Summary   string
	Data            map[string]any
	RelatedEventID  *int64
	EventKey        string
}

func nullInt(p *int64) any {
	if p == nil {
		return nil
	}
	return *p
}

func nullStr(s string) any {
	if s == "" {
		return nil
	}
	return s
}

func ptr(n int64) *int64 { return &n }

func insertEvent(tx *sql.Tx, e event) (int64, error) {
	var data any
	if e.Data != nil {
		b, err := json.Marshal(e.Data)
		if err != nil {
			return 0, err
		}
		data = string(b)
	}
	res, err := tx.Exec(`insert into events (task_id, recipient_task_id, launch_id, kind, summary, data,
		related_event_id, event_key, created_at) values (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		e.TaskID, nullInt(e.RecipientTaskID), nullInt(e.LaunchID), e.Kind, nullStr(e.Summary), data,
		nullInt(e.RelatedEventID), nullStr(e.EventKey), now())
	if err != nil {
		return 0, err
	}
	return res.LastInsertId()
}

func (c *ctx) insertEvent(tx *sql.Tx, e event) (int64, error) {
	if c != nil && c.queuedAt != "" {
		data := make(map[string]any, len(e.Data)+1)
		for key, value := range e.Data {
			data[key] = value
		}
		data["queued_at"] = c.queuedAt
		e.Data = data
	}
	return insertEvent(tx, e)
}

const eventCols = `e.id, e.task_id, t.name, e.recipient_task_id, e.launch_id, e.kind, e.summary, e.data,
	e.related_event_id, e.answered_by, e.event_key, e.created_at`

func scanEvent(s interface{ Scan(...any) error }) (map[string]any, error) {
	var id, taskID int64
	var name, kind, created string
	var recip, launch, related, answered sql.NullInt64
	var summary, data, key sql.NullString
	if err := s.Scan(&id, &taskID, &name, &recip, &launch, &kind, &summary, &data, &related, &answered, &key, &created); err != nil {
		return nil, err
	}
	m := map[string]any{"record": "event", "id": id, "task_id": taskID, "task_name": name, "kind": kind, "created_at": created}
	putInt(m, "recipient_task_id", recip)
	putInt(m, "launch_id", launch)
	putInt(m, "related_event_id", related)
	putInt(m, "answered_by", answered)
	putStr(m, "summary", summary)
	putStr(m, "event_key", key)
	if data.Valid {
		var d any
		if json.Unmarshal([]byte(data.String), &d) == nil {
			m["data"] = d
		}
	}
	return m, nil
}

func loadEvent(q queryer, id int64) (map[string]any, error) {
	m, err := scanEvent(q.QueryRow(`select `+eventCols+` from events e join tasks t on t.id = e.task_id where e.id = ?`, id))
	if errors.Is(err, sql.ErrNoRows) {
		return nil, rejectErr("event %d does not exist", id)
	}
	return m, err
}

func putInt(m map[string]any, k string, v sql.NullInt64) {
	if v.Valid {
		m[k] = v.Int64
	}
}

func putStr(m map[string]any, k string, v sql.NullString) {
	if v.Valid {
		m[k] = v.String
	}
}

// parentRecipient returns the parent task id, or nil for a root.
func parentRecipient(t *task) *int64 {
	if t.ParentID.Valid {
		return ptr(t.ParentID.Int64)
	}
	return nil
}

// rootOf walks parent links to the task with no parent.
func rootOf(q queryer, id int64) (int64, error) {
	for i := 0; i < 1000; i++ {
		var parent sql.NullInt64
		if err := q.QueryRow(`select parent_id from tasks where id = ?`, id).Scan(&parent); err != nil {
			return 0, err
		}
		if !parent.Valid {
			return id, nil
		}
		id = parent.Int64
	}
	return 0, fmt.Errorf("parent chain of task %d is too deep", id)
}

// subtree returns id and all its descendants, parents before children.
func subtree(q queryer, id int64) ([]int64, error) {
	rows, err := q.Query(`with recursive sub(id, depth) as (select id, 0 from tasks where id = ?
		union all select t.id, sub.depth + 1 from tasks t join sub on t.parent_id = sub.id)
		select id from sub order by depth, id`, id)
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
	if len(ids) == 0 {
		return nil, rejectErr("task %d does not exist", id)
	}
	return ids, rows.Err()
}
