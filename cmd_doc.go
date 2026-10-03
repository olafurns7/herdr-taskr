package main

import (
	"context"
	"database/sql"
	"errors"
	"flag"
	"fmt"
	"path/filepath"
	"regexp"
	"strings"
)

var documentNameRe = regexp.MustCompile(`^[a-z0-9][a-z0-9._-]{0,63}$`)
var documentKinds = map[string]bool{"brief": true, "prompt": true, "report": true, "handover": true, "goal": true, "plan": true}

func cmdDoc(c *ctx, args []string) (any, int, error) {
	if len(args) == 0 || strings.HasPrefix(args[0], "-") {
		fs := flag.NewFlagSet("doc", flag.ContinueOnError)
		_, err := parseArgs(c, fs, args, 1, 1)
		if err != nil {
			return nil, 0, err
		}
		return nil, 0, usageErr("doc: put the subcommand before its flags")
	}
	switch args[0] {
	case "set":
		return cmdDocSet(c, args[1:])
	case "ls":
		return cmdDocLs(c, args[1:])
	case "get":
		return cmdDocGet(c, args[1:])
	case "rm":
		return cmdDocRm(c, args[1:])
	case "backfill":
		return cmdDocBackfill(c, args[1:])
	}
	return nil, 0, usageErr("doc: expected set, ls, get, rm or backfill")
}

func cmdDocSet(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("doc set", flag.ContinueOnError)
	file := fs.String("file", "", "document file on the server host")
	name := fs.String("name", "", "plan name, [a-z0-9][a-z0-9._-]{0,63}")
	pos, err := parseArgs(c, fs, args, 2, 2)
	if err != nil {
		return nil, 0, err
	}
	id, err := parseID(pos[0], "task id")
	if err != nil {
		return nil, 0, err
	}
	kind := pos[1]
	if kind != "goal" && kind != "plan" {
		return nil, 0, usageErr("doc set accepts goal or plan")
	}
	if kind == "goal" && flagWasSet(fs, "name") {
		return nil, 0, usageErr("goal takes no --name")
	}
	if flagWasSet(fs, "name") && !documentNameRe.MatchString(*name) {
		return nil, 0, usageErr("--name must match [a-z0-9][a-z0-9._-]{0,63}")
	}
	if *file == "" {
		return nil, 0, usageErr("doc set needs --file PATH")
	}
	path := *file
	if c.rpc && !filepath.IsAbs(path) {
		path = filepath.Join(c.cwd, path)
	}
	path, err = filepath.Abs(path)
	if err != nil {
		return nil, 0, usageErr("document path: %v", err)
	}
	in := fileDocument(path, "")
	if in.Reason == "missing" {
		if c.rpc {
			return nil, 0, usageErr("file not found on the server host; in this release the file must be on that host's disk")
		}
		return nil, 0, usageErr("document file is missing or unreadable: %s", path)
	}
	if in.Reason != "" {
		return nil, 0, usageErr("document file: %s (%s)", in.Reason, path)
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	var d document
	var same bool
	err = withTx(db, func(tx *sql.Tx) error {
		t, err := openPlanTask(tx, id)
		if err != nil {
			return err
		}
		if kind == "goal" && t.ParentID.Valid {
			return rejectErr("goal is only accepted for a root task")
		}
		d, same, err = storeDocument(tx, id, kind, *name, in, nil, 0)
		if err != nil || same {
			return err
		}
		summary := kind
		if *name != "" {
			summary += "/" + *name
		}
		eid, err := insertEvent(tx, event{TaskID: id, Kind: "doc", Summary: fmt.Sprintf("%s v%d", summary, d.Version), Data: map[string]any{
			"doc_id": d.ID, "kind": kind, "name": *name, "version": d.Version, "sha256": in.Hash, "bytes": *in.Bytes}})
		if err != nil {
			return err
		}
		_, err = tx.Exec(`update documents set event_id = ? where id = ?`, eid, d.ID)
		return err
	})
	if err != nil {
		return nil, 0, err
	}
	return map[string]any{"ok": true, "action": "set", "doc_id": d.ID, "version": d.Version, "same": same}, exitOK, nil
}

func (d document) record() map[string]any {
	m := map[string]any{"doc_id": d.ID, "task_id": d.TaskID, "kind": d.Kind, "name": d.Name,
		"version": d.Version, "bytes": nil, "format": nil, "captured": d.Captured, "reason": nil,
		"event_id": nil, "source_path": nil, "source_host": nil, "backfill": d.Backfill, "created_at": d.Created}
	putInt(m, "bytes", d.Bytes)
	putInt(m, "event_id", d.EventID)
	putStr(m, "format", d.Format)
	putStr(m, "reason", d.Reason)
	putStr(m, "source_path", d.Path)
	putStr(m, "source_host", d.Host)
	return m
}

func cmdDocLs(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("doc ls", flag.ContinueOnError)
	tree := fs.Bool("tree", false, "include descendants")
	kind := fs.String("kind", "", "document kind")
	versions := fs.Bool("versions", false, "include every version")
	limit := fs.Int("limit", 100, "newest N documents; 0 for all (default 100)")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	id, err := parseID(pos[0], "task id")
	if err != nil {
		return nil, 0, err
	}
	if *limit < 0 {
		return nil, 0, usageErr("--limit must be >= 0")
	}
	if *kind != "" && !documentKinds[*kind] {
		return nil, 0, usageErr("unknown document kind %q", *kind)
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	ids := []int64{id}
	if _, err := loadTask(db, id); err != nil {
		return nil, 0, dbErr(err)
	}
	if *tree {
		ids, err = subtree(db, id)
		if err != nil {
			return nil, 0, dbErr(err)
		}
	}
	where := `task_id in (` + placeholders(len(ids)) + `)`
	var qargs []any
	for _, id := range ids {
		qargs = append(qargs, id)
	}
	if *kind != "" {
		where += ` and kind = ?`
		qargs = append(qargs, *kind)
	}
	if !*versions {
		where += ` and version = (select max(version) from documents d where d.task_id = documents.task_id and d.kind = documents.kind and d.name = documents.name)`
	}
	rows, err := db.Query(`select `+documentCols+` from documents where `+where+` order by id desc`, qargs...)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	defer rows.Close()
	var lines []map[string]any
	for rows.Next() {
		d, err := scanDocument(rows)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		lines = append(lines, d.record())
	}
	if err := rows.Err(); err != nil {
		return nil, 0, dbErr(err)
	}
	total, kept := len(lines), 0
	budget := 0
	for _, line := range lines {
		if *limit > 0 && (kept >= *limit || (kept > 0 && budget+len(readLine(line)) > readCapBytes-trailerReserve)) {
			break
		}
		budget += len(readLine(line))
		kept++
	}
	c.lines = true
	for _, line := range lines[:kept] {
		c.emit(line)
	}
	if kept < total {
		c.trailer(map[string]any{"older": total - kept, "all": "--limit 0"})
	}
	return nil, exitOK, nil
}

func cmdDocGet(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("doc get", flag.ContinueOnError)
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	id, err := parseID(pos[0], "document id")
	if err != nil {
		return nil, 0, err
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	d, err := scanDocument(db.QueryRow(`select `+documentCols+` from documents where id = ?`, id))
	if errors.Is(err, sql.ErrNoRows) {
		return nil, 0, rejectErr("document %d does not exist", id)
	}
	if err != nil {
		return nil, 0, dbErr(err)
	}
	if !d.Captured {
		return nil, 0, rejectErr("document %d not captured (%s): %s", id, d.Reason.String, d.Path.String)
	}
	var body string
	if err := db.QueryRow(`select body from doc_blobs where sha256 = ?`, d.Hash.String).Scan(&body); err != nil {
		return nil, 0, dbErr(err)
	}
	c.lines = true
	_, err = fmt.Fprint(c.out, body)
	return nil, exitOK, err
}

func cmdDocRm(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("doc rm", flag.ContinueOnError)
	purge := fs.Bool("purge", false, "delete all versions and unshared blobs; the write-ahead log and earlier backups can still hold the text")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	id, err := parseID(pos[0], "document id")
	if err != nil {
		return nil, 0, err
	}
	if !*purge {
		return nil, 0, usageErr("doc rm requires --purge; the write-ahead log and earlier backups can still hold the text")
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	var removed int64
	err = withTx(db, func(tx *sql.Tx) error {
		d, err := scanDocument(tx.QueryRow(`select `+documentCols+` from documents where id = ?`, id))
		if errors.Is(err, sql.ErrNoRows) {
			return rejectErr("document %d does not exist", id)
		}
		if err != nil {
			return err
		}
		hashes, err := listStrings(tx, `select distinct sha256 from documents where task_id = ? and kind = ? and name = ? and sha256 is not null`, d.TaskID, d.Kind, d.Name)
		if err != nil {
			return err
		}
		if _, err := tx.Exec(`pragma secure_delete = on`); err != nil {
			return err
		}
		res, err := tx.Exec(`delete from documents where task_id = ? and kind = ? and name = ?`, d.TaskID, d.Kind, d.Name)
		if err != nil {
			return err
		}
		removed, err = res.RowsAffected()
		if err != nil {
			return err
		}
		for _, hash := range hashes {
			if _, err := tx.Exec(`delete from doc_blobs where sha256 = ? and not exists (select 1 from documents where sha256 = ? and captured = 1)`, hash, hash); err != nil {
				return err
			}
		}
		label := d.Kind
		if d.Name != "" {
			label += "/" + d.Name
		}
		_, err = insertEvent(tx, event{TaskID: d.TaskID, Kind: "doc", Summary: fmt.Sprintf("purged %s (%d versions)", label, removed)})
		return err
	})
	if err != nil {
		return nil, 0, err
	}
	checkpointDocuments(db)
	return map[string]any{"ok": true, "action": "rm", "doc_id": id, "removed": removed}, exitOK, nil
}

// Checkpointing is best effort and must not wait for other ledger readers.
func checkpointDocuments(db *sql.DB) {
	ctx := context.Background()
	conn, err := db.Conn(ctx)
	if err != nil {
		return
	}
	defer conn.Close()
	var timeout int
	if err := conn.QueryRowContext(ctx, `pragma busy_timeout`).Scan(&timeout); err != nil {
		return
	}
	if _, err := conn.ExecContext(ctx, `pragma busy_timeout = 0`); err != nil {
		return
	}
	defer conn.ExecContext(ctx, fmt.Sprintf("pragma busy_timeout = %d", timeout))
	_, _ = conn.ExecContext(ctx, `pragma wal_checkpoint(truncate)`)
}

func cmdDocBackfill(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("doc backfill", flag.ContinueOnError)
	tree := fs.Int64("tree", 0, "only this task and descendants")
	dry := fs.Bool("dry-run", false, "count without writing")
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		return nil, 0, err
	}
	if c.rpc {
		return nil, 0, rejectErr("doc backfill is only available locally on the ledger's host")
	}
	if *tree < 0 {
		return nil, 0, usageErr("--tree must be a positive task id")
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	ids, err := listIDs(db, `select id from tasks order by id`)
	if *tree != 0 {
		ids, err = subtree(db, *tree)
	}
	if err != nil {
		return nil, 0, dbErr(err)
	}
	counts := map[string]any{"captured": 0, "too_large": 0, "binary": 0, "missing": 0, "client": 0, "unchanged": 0}
	for _, taskID := range ids {
		files, err := prepareBackfill(db, taskID)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		err = withTx(db, func(tx *sql.Tx) error {
			if *dry {
				if _, err := tx.Exec(`savepoint doc_dry_run`); err != nil {
					return err
				}
			}
			if err := backfillTask(tx, taskID, files, counts); err != nil {
				return err
			}
			if *dry {
				_, err := tx.Exec(`rollback to doc_dry_run`)
				return err
			}
			return nil
		})
		if err != nil {
			return nil, 0, err
		}
	}
	return counts, exitOK, nil
}

type backfillFile struct {
	kind, name string
	input      documentInput
	eventID    *int64
	backfill   int
}

func prepareBackfill(q queryer, taskID int64) ([]backfillFile, error) {
	t, err := loadTask(q, taskID)
	if err != nil {
		return nil, err
	}
	host, err := documentHost(q, taskID)
	if err != nil {
		return nil, err
	}
	var brief sql.NullString
	if err := q.QueryRow(`select brief_path from tasks where id = ?`, taskID).Scan(&brief); err != nil {
		return nil, err
	}
	report, err := reportDocumentPath(q, taskID)
	if err != nil {
		return nil, err
	}
	type candidate struct{ path, kind, name string }
	var candidates []candidate
	if brief.String != "" {
		kind := "goal"
		if t.ParentID.Valid {
			kind = "brief"
		}
		candidates = append(candidates, candidate{brief.String, kind, ""})
	}
	paths, err := listStrings(q, `select distinct json_extract(data, '$.file') from events where task_id = ? and kind = 'prompt' and json_extract(data, '$.file') is not null order by id`, taskID)
	if err != nil {
		return nil, err
	}
	for _, path := range paths {
		if path != "" && path != brief.String {
			candidates = append(candidates, candidate{path, "prompt", filepath.Base(path)})
		}
	}
	if report != "" {
		candidates = append(candidates, candidate{report, "report", ""})
	}
	var files []backfillFile
	for _, candidate := range candidates {
		f := backfillFile{kind: candidate.kind, name: candidate.name, input: fileDocument(candidate.path, host), backfill: 2}
		if f.kind != "report" && f.input.Hash != "" {
			var eid int64
			err := q.QueryRow(`select id from events where task_id = ? and kind = 'prompt' and json_extract(data, '$.file') = ? and json_extract(data, '$.sha256') = ? order by id desc limit 1`, taskID, f.input.Path, f.input.Hash).Scan(&eid)
			if err == nil {
				f.backfill, f.eventID = 1, ptr(eid)
			} else if !errors.Is(err, sql.ErrNoRows) {
				return nil, err
			}
		}
		files = append(files, f)
	}
	return files, nil
}

func backfillTask(tx *sql.Tx, taskID int64, files []backfillFile, counts map[string]any) error {
	for _, f := range files {
		in := f.input
		var skip bool
		if err := tx.QueryRow(`select exists(select 1 from documents where task_id = ? and kind = ? and name = ?
            and (backfill = 0 or (? != '' and captured = 1)))`, taskID, f.kind, f.name, in.Reason).Scan(&skip); err != nil {
			return err
		}
		if skip {
			counts["unchanged"] = counts["unchanged"].(int) + 1
			continue
		}
		// Backfill is idempotent across paths sharing a basename/version stream.
		match, values := `captured = 1 and sha256 = ?`, []any{taskID, f.kind, f.name, in.Path, in.Hash}
		if in.Reason != "" {
			match, values[4] = `captured = 0 and reason = ?`, in.Reason
		}
		var exists bool
		if err := tx.QueryRow(`select exists(select 1 from documents where task_id = ? and kind = ? and name = ? and source_path = ? and `+match+`)`, values...).Scan(&exists); err != nil {
			return err
		}
		if exists {
			counts["unchanged"] = counts["unchanged"].(int) + 1
			continue
		}
		_, same, err := storeDocument(tx, taskID, f.kind, f.name, in, f.eventID, f.backfill)
		if err != nil {
			return err
		}
		key := in.Reason
		if key == "" {
			key = "captured"
		}
		if same {
			key = "unchanged"
		}
		counts[key] = counts[key].(int) + 1
	}
	return nil
}
