package main

import (
	"bufio"
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

const rolloutTailBytes = 1 << 20

type codexFallback struct {
	Task, Parent, Launch int64
	Session, Path        string
}

// scanCodexRolloutFallback reads only the server host's bound Codex rollout tails.
func scanCodexRolloutFallback(db *sql.DB, parent *int64, deadline time.Time) error {
	rows, err := db.Query(`select t.id, t.parent_id, l.id, l.session_ref, l.transcript_path
		from tasks t join launches l on l.id = t.current_launch_id
		where t.parent_id is not null and (? is null or t.parent_id = ?) and t.status = 'open' and t.role != 'gate'
		and (t.waiting_until is null or t.waiting_until <= ?) and l.machine is ? and l.provider = 'codex'
		and l.session_source like 'hook:%' and l.session_ref is not null and l.transcript_path is not null
		order by l.id`, nullInt(parent), nullInt(parent), now(), serverHost)
	if err != nil {
		return dbErr(err)
	}
	var candidates []codexFallback
	for rows.Next() {
		var c codexFallback
		if err := rows.Scan(&c.Task, &c.Parent, &c.Launch, &c.Session, &c.Path); err != nil {
			rows.Close()
			return dbErr(err)
		}
		candidates = append(candidates, c)
	}
	err = rows.Err()
	rows.Close()
	if err != nil {
		return dbErr(err)
	}
	for _, c := range candidates {
		if time.Until(deadline) <= 0 {
			break
		}
		var prompt int64
		owed, err := owes(db, c.Launch, &prompt)
		if err != nil {
			return dbErr(err)
		}
		if !owed || !validCodexRollout(c.Session, c.Path) {
			continue
		}
		var stalled bool
		var promptAt string
		if err := db.QueryRow(`select exists (select 1 from events where event_key = ?), (select created_at from events where id = ?)`,
			"stall:"+strconv.FormatInt(prompt, 10), prompt).Scan(&stalled, &promptAt); err != nil {
			return dbErr(err)
		}
		if stalled {
			continue
		}
		code, found, err := codexRolloutError(c.Path, parseTime(promptAt))
		if err != nil || !found {
			continue
		}
		if time.Until(deadline) <= 0 {
			break
		}
		cx, cancel := context.WithDeadline(context.Background(), deadline)
		tx, err := db.BeginTx(cx, nil)
		if err != nil {
			cancel()
			return dbErr(err)
		}
		var current bool
		err = tx.QueryRow(`select exists (select 1 from tasks t join launches l on l.id = t.current_launch_id
			where t.id = ? and t.status = 'open' and l.id = ? and l.session_source like 'hook:%'
			and l.session_ref = ? and l.transcript_path = ? and l.machine is ? and l.provider = 'codex')`,
			c.Task, c.Launch, c.Session, c.Path, serverHost).Scan(&current)
		if err == nil && current {
			w := &worker{task: &task{ID: c.Task, ParentID: sql.NullInt64{Int64: c.Parent, Valid: true}}, launchID: ptr(c.Launch)}
			err = writeHookStall(tx, w, c.Launch, code)
		}
		if err != nil {
			tx.Rollback()
		} else {
			err = tx.Commit()
		}
		cancel()
		if err != nil {
			return dbErr(err)
		}
	}
	return nil
}

// validCodexRollout checks the hook's transcript path alone: an absolute
// .jsonl under a sessions directory named rollout-*-<session>.jsonl.
func validCodexRollout(session, path string) bool {
	if !filepath.IsAbs(path) || session == "" || !strings.HasSuffix(path, ".jsonl") {
		return false
	}
	path = filepath.Clean(path)
	sep := string(filepath.Separator)
	if !strings.Contains(filepath.Dir(path)+sep, sep+"sessions"+sep) {
		return false
	}
	base := filepath.Base(path)
	return strings.HasPrefix(base, "rollout-") && strings.HasSuffix(base, "-"+session+".jsonl")
}

// codexRolloutError reports the last turn's error when no user message
// follows it and it completed at or after since, the newest prompt's time.
func codexRolloutError(path string, since time.Time) (code string, found bool, err error) {
	f, err := os.Open(path)
	if err != nil {
		return "", false, err
	}
	defer f.Close()
	st, err := f.Stat()
	if err != nil {
		return "", false, err
	}
	offset := st.Size() - rolloutTailBytes
	truncated := offset > 0
	if offset < 0 {
		offset = 0
	}
	if _, err := f.Seek(offset, io.SeekStart); err != nil {
		return "", false, err
	}
	tail, err := io.ReadAll(io.LimitReader(f, rolloutTailBytes))
	if err != nil {
		return "", false, err
	}
	if truncated {
		i := bytes.IndexByte(tail, '\n')
		if i < 0 {
			return "", false, nil
		}
		tail = tail[i+1:]
	}
	var lastComplete struct {
		at   int
		code string
		bad  bool
	}
	lastUser := -1
	scanner := bufio.NewScanner(bytes.NewReader(tail))
	scanner.Buffer(make([]byte, 4096), rolloutTailBytes)
	lineNo := 0
	for scanner.Scan() {
		var row map[string]any
		if json.Unmarshal(scanner.Bytes(), &row) == nil {
			switch hookText(row, "type") {
			case "event_msg":
				payload := hookObject(row["payload"])
				if hookText(payload, "type") == "task_complete" && rolloutTurnTime(row, payload, since) {
					raw := payload["error"]
					code := hookErrorCode(hookObject(raw))
					if raw != nil && code == "" {
						code = "unknown"
					}
					lastComplete = struct {
						at   int
						code string
						bad  bool
					}{lineNo, code, raw != nil}
				}
			case "response_item":
				role := hookText(row, "role")
				if role == "" {
					role = hookText(hookObject(row["payload"]), "role")
				}
				if role == "user" {
					lastUser = lineNo
				}
			}
		}
		lineNo++
	}
	if scanner.Err() != nil || !lastComplete.bad || lastUser > lastComplete.at {
		return "", false, nil
	}
	return lastComplete.code, true, nil
}

// rolloutTurnTime reports whether a task_complete row is not older than
// since. It reads the row's RFC 3339 timestamp, then payload.completed_at
// (epoch seconds). A row with neither counts as older: a false stall would
// spend the newest prompt's only stall key.
func rolloutTurnTime(row, payload map[string]any, since time.Time) bool {
	if s := hookText(row, "timestamp"); s != "" {
		t, err := time.Parse(time.RFC3339Nano, s)
		return err == nil && !t.Before(since)
	}
	if f, ok := payload["completed_at"].(float64); ok {
		return !time.Unix(int64(f), 0).Before(since)
	}
	return false
}
