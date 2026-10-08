package main

import (
	"flag"
	"strconv"
	"time"
)

func cmdNotes(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("notes", flag.ContinueOnError)
	owner := fs.Bool("owner", false, "only notes for the owner")
	root := fs.Int64("root", 0, "campaign root id")
	since := fs.String("since", "48h", "events after this event id or within this Go duration")
	limit := fs.Int64("limit", 50, "newest N notes; 0 lifts the limit and byte cap")
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		return nil, 0, err
	}
	if *root < 0 || flagWasSet(fs, "root") && *root == 0 {
		return nil, 0, usageErr("--root must be a positive campaign id")
	}
	if *limit < 0 {
		return nil, 0, usageErr("--limit must be >= 0")
	}
	where := `t.parent_id is null and e.kind = 'note'`
	var qargs []any
	if n, err := strconv.ParseInt(*since, 10, 64); err == nil && n >= 0 {
		where += ` and e.id > ?`
		qargs = append(qargs, n)
	} else if duration, err := time.ParseDuration(*since); err == nil && duration > 0 {
		where += ` and e.created_at > ?`
		qargs = append(qargs, stamp(clockNow().Add(-duration)))
	} else {
		return nil, 0, usageErr("--since must be a nonnegative event id or positive Go duration")
	}
	if *owner {
		where += ` and json_extract(e.data, '$.owner') = 1`
	}
	if *root != 0 {
		where += ` and t.id = ?`
		qargs = append(qargs, *root)
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	rows, err := db.Query(`select `+eventCols+` from events e join tasks t on t.id = e.task_id where `+where+` order by e.id desc`, qargs...)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	defer rows.Close()
	var notes []map[string]any
	for rows.Next() {
		note, err := scanEvent(rows)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		notes = append(notes, note)
	}
	if err := rows.Err(); err != nil {
		return nil, 0, dbErr(err)
	}
	matched := len(notes)
	if *limit > 0 {
		if int64(len(notes)) > *limit {
			notes = notes[:*limit]
		}
		if !c.json {
			_, notes, _ = capLog(nil, notes, nil, true)
		}
	}
	c.lines = true
	for _, note := range notes {
		c.emit(note)
	}
	if left := matched - len(notes); left > 0 {
		c.trailer(logTrailer{Older: int64(left), All: "--limit 0"})
	}
	return nil, exitOK, nil
}
