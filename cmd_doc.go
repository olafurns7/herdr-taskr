package main

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/base64"
	"encoding/hex"
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
	file := fs.String("file", "", "document file, read on any host")
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
	var in documentInput
	if c.rpc && c.remoteDoc != nil {
		p := c.remoteDoc
		if !c.docUpload || p.Task != id || p.Kind != kind || p.Name != *name || p.Path != path ||
			p.EventID != nil || p.Backfill || p.DryRun {
			return nil, 0, rejectErr("doc set body does not match its request")
		}
		in, err = docPayloadInput(*p)
		if err != nil {
			return nil, 0, err
		}
		in.Host = c.machine
	} else {
		in = fileDocument(path, "")
	}
	if in.Reason == "missing" {
		if c.rpc && c.remoteDoc == nil {
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
		if _, err := tx.Exec(`delete from search_fts where src = 'doc' and task_id = ? and kind = ? and name = ?`, d.TaskID, d.Kind, d.Name); err != nil {
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

type backfillCandidate struct{ path, kind, name string }

func backfillSourceHost(q queryer, taskID int64, candidate backfillCandidate, taskHost string) (string, error) {
	if candidate.kind != "brief" && candidate.kind != "prompt" {
		return taskHost, nil
	}
	var host sql.NullString
	err := q.QueryRow(`select source_host from documents where task_id = ? and kind = ? and name = ? and captured = 0
		order by version desc limit 1`, taskID, candidate.kind, candidate.name).Scan(&host)
	if errors.Is(err, sql.ErrNoRows) {
		return taskHost, nil
	}
	return host.String, err
}

func backfillCandidates(q queryer, taskID int64) ([]backfillCandidate, error) {
	t, err := loadTask(q, taskID)
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
	var candidates []backfillCandidate
	if brief.String != "" {
		kind := "goal"
		if t.ParentID.Valid {
			kind = "brief"
		}
		candidates = append(candidates, backfillCandidate{brief.String, kind, ""})
	}
	paths, err := listStrings(q, `select distinct json_extract(data, '$.file') from events where task_id = ? and kind = 'prompt' and json_extract(data, '$.file') is not null order by id`, taskID)
	if err != nil {
		return nil, err
	}
	for _, path := range paths {
		if path != "" && path != brief.String {
			candidates = append(candidates, backfillCandidate{path, "prompt", filepath.Base(path)})
		}
	}
	if report != "" {
		candidates = append(candidates, backfillCandidate{report, "report", ""})
	}
	return candidates, nil
}

func backfillMetadata(q queryer, taskID int64, kind string, in documentInput) (int, *int64, error) {
	if kind == "report" || in.Hash == "" {
		return 2, nil, nil
	}
	var eventID int64
	err := q.QueryRow(`select id from events where task_id = ? and kind = 'prompt' and json_extract(data, '$.file') = ? and json_extract(data, '$.sha256') = ? order by id desc limit 1`,
		taskID, in.Path, in.Hash).Scan(&eventID)
	if err == nil {
		return 1, ptr(eventID), nil
	}
	if errors.Is(err, sql.ErrNoRows) {
		return 2, nil, nil
	}
	return 0, nil, err
}

func prepareBackfill(q queryer, taskID int64) ([]backfillFile, error) {
	host, err := documentHost(q, taskID)
	if err != nil {
		return nil, err
	}
	candidates, err := backfillCandidates(q, taskID)
	if err != nil {
		return nil, err
	}
	var files []backfillFile
	for _, candidate := range candidates {
		candidateHost, err := backfillSourceHost(q, taskID, candidate, host)
		if err != nil {
			return nil, err
		}
		f := backfillFile{kind: candidate.kind, name: candidate.name, input: fileDocument(candidate.path, candidateHost)}
		f.backfill, f.eventID, err = backfillMetadata(q, taskID, f.kind, f.input)
		if err != nil {
			return nil, err
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

func docPayloadInput(p rpcDocPayload) (documentInput, error) {
	if p.Task <= 0 || !documentKinds[p.Kind] || p.Path == "" || !filepath.IsAbs(p.Path) {
		return documentInput{}, rejectErr("document task, kind and absolute path are required")
	}
	if p.Kind == "goal" && p.Name != "" || p.Name != "" && !documentNameRe.MatchString(p.Name) {
		return documentInput{}, rejectErr("invalid document name")
	}
	if p.EventID != nil && *p.EventID <= 0 {
		return documentInput{}, rejectErr("event_id must be positive")
	}
	if p.SHA256 != "" {
		if len(p.SHA256) != sha256.Size*2 {
			return documentInput{}, rejectErr("invalid document sha256")
		}
		if _, err := hex.DecodeString(p.SHA256); err != nil {
			return documentInput{}, rejectErr("invalid document sha256")
		}
	}
	if p.Bytes != nil && *p.Bytes < 0 {
		return documentInput{}, rejectErr("invalid document byte count")
	}
	if p.Body != nil {
		if p.Reason != "" || p.SHA256 == "" {
			return documentInput{}, rejectErr("document body needs a sha256 and no reason")
		}
		body, err := base64.StdEncoding.DecodeString(*p.Body)
		if err != nil {
			return documentInput{}, rejectErr("document body is not base64")
		}
		in := bodyDocument(body, p.Path)
		if in.Reason != "" || in.Hash != p.SHA256 || p.Bytes != nil && *p.Bytes != int64(len(body)) {
			return documentInput{}, rejectErr("document body failed size, text or sha256 validation")
		}
		return in, nil
	}
	if p.Reason != "missing" && p.Reason != "too_large" && p.Reason != "binary" {
		return documentInput{}, rejectErr("document body or a supported miss reason is required")
	}
	if p.Reason == "too_large" && (p.Bytes == nil || *p.Bytes <= documentCap) {
		return documentInput{}, rejectErr("too_large document is below the size cap")
	}
	if p.Reason == "binary" && (p.Bytes == nil || *p.Bytes > documentCap || p.SHA256 == "") {
		return documentInput{}, rejectErr("binary document needs its size and sha256")
	}
	return documentInput{Path: p.Path, Hash: p.SHA256, Bytes: p.Bytes, Reason: p.Reason}, nil
}

func hasCapability(caps []string, want string) bool {
	for _, cap := range caps {
		if cap == want {
			return true
		}
	}
	return false
}

func clientBackfillSkipped(q queryer, d rpcDocWant) (bool, error) {
	var skip bool
	err := q.QueryRow(`select exists(select 1 from documents where task_id = ? and kind = ? and name = ?
		and captured = 1 and (backfill = 0 or source_path = ?))`, d.Task, d.Kind, d.Name, d.Path).Scan(&skip)
	return skip, err
}

func cmdDocRPC(c *ctx, args []string) (any, int, error) {
	if err := hiddenOnly(c, "_doc"); err != nil {
		return nil, 0, err
	}
	if !c.docUpload {
		return nil, 0, rejectErr("_doc requires the doc-upload capability")
	}
	if len(args) == 0 {
		return nil, 0, usageErr("_doc: expected put or wanted")
	}
	switch args[0] {
	case "put":
		return cmdDocPut(c, args[1:])
	case "wanted":
		return cmdDocWanted(c, args[1:])
	default:
		return nil, 0, usageErr("_doc: expected put or wanted")
	}
}

func cmdDocPut(c *ctx, args []string) (any, int, error) {
	if _, err := parseArgs(c, flag.NewFlagSet("_doc put", flag.ContinueOnError), args, 0, 0); err != nil {
		return nil, 0, err
	}
	p := c.remoteDoc
	if p == nil {
		return nil, 0, usageErr("_doc put needs a document payload")
	}
	in, err := docPayloadInput(*p)
	if err != nil {
		return nil, 0, err
	}
	in.Host = c.machine
	if p.Reason == "" && p.Body == nil || p.Body != nil && p.Reason != "" {
		return nil, 0, rejectErr("document upload body and reason conflict")
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	var result map[string]any
	err = withTx(db, func(tx *sql.Tx) error {
		if _, err := loadTask(tx, p.Task); errors.Is(err, sql.ErrNoRows) {
			return rejectErr("task %d does not exist", p.Task)
		} else if err != nil {
			return err
		}
		if p.EventID != nil {
			var belongs bool
			if err := tx.QueryRow(`select exists(select 1 from events where id = ? and task_id = ?)`, *p.EventID, p.Task).Scan(&belongs); err != nil {
				return err
			}
			if !belongs {
				return rejectErr("event %d does not belong to task %d", *p.EventID, p.Task)
			}
		}
		backfill, eventID := 0, p.EventID
		if p.Backfill {
			host, err := documentHost(tx, p.Task)
			if err != nil {
				return err
			}
			candidates, err := backfillCandidates(tx, p.Task)
			if err != nil {
				return err
			}
			var candidate *backfillCandidate
			for i := range candidates {
				item := &candidates[i]
				if item.kind == p.Kind && item.name == p.Name && item.path == p.Path {
					candidate = item
					break
				}
			}
			if candidate == nil {
				return rejectErr("document is not a backfill candidate")
			}
			candidateHost, err := backfillSourceHost(tx, p.Task, *candidate, host)
			if err != nil {
				return err
			}
			if candidateHost != c.machine {
				return rejectErr("backfill task host changed")
			}
			backfill, eventID, err = backfillMetadata(tx, p.Task, p.Kind, in)
			if err != nil {
				return err
			}
			if p.EventID != nil && (eventID == nil || *eventID != *p.EventID) {
				return rejectErr("event %d does not match the backfill document", *p.EventID)
			}
			want := rpcDocWant{Task: p.Task, Kind: p.Kind, Name: p.Name, Path: p.Path}
			skip, err := clientBackfillSkipped(tx, want)
			if err != nil {
				return err
			}
			if skip {
				result = map[string]any{"ok": true, "same": true, "count": "unchanged"}
				return nil
			}
		} else {
			latest, err := latestDocument(tx, p.Task, p.Kind, p.Name)
			if errors.Is(err, sql.ErrNoRows) {
				return rejectErr("document upload has no matching capture")
			}
			if err != nil {
				return err
			}
			if latest.Host.String != c.machine || latest.Path.String != p.Path {
				return rejectErr("document upload does not match the latest capture")
			}
		}
		if p.DryRun {
			if _, err := tx.Exec(`savepoint doc_upload_dry_run`); err != nil {
				return err
			}
		}
		d, same, err := storeDocument(tx, p.Task, p.Kind, p.Name, in, eventID, backfill)
		if p.DryRun {
			if _, rollbackErr := tx.Exec(`rollback to doc_upload_dry_run`); err == nil {
				err = rollbackErr
			}
			_, _ = tx.Exec(`release doc_upload_dry_run`)
		}
		if err != nil {
			return err
		}
		count := in.Reason
		if count == "" {
			count = "captured"
		}
		if same {
			count = "unchanged"
		}
		result = map[string]any{"ok": true, "action": "put", "doc_id": d.ID, "version": d.Version,
			"same": same, "count": count}
		return nil
	})
	return result, exitOK, err
}

func cmdDocWanted(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("_doc wanted", flag.ContinueOnError)
	tree := fs.Int64("tree", 0, "only this task and descendants")
	offset := fs.Int("offset", 0, "candidate offset")
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		return nil, 0, err
	}
	if *tree < 0 || *offset < 0 {
		return nil, 0, usageErr("invalid _doc wanted page")
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
	var candidates []rpcDocWant
	for _, taskID := range ids {
		host, err := documentHost(db, taskID)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		files, err := backfillCandidates(db, taskID)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		for _, f := range files {
			candidateHost, err := backfillSourceHost(db, taskID, f, host)
			if err != nil {
				return nil, 0, dbErr(err)
			}
			if candidateHost != c.machine {
				continue
			}
			candidates = append(candidates, rpcDocWant{Task: taskID, Kind: f.kind, Name: f.name, Path: f.path})
		}
	}
	start := min(*offset, len(candidates))
	end := min(start+200, len(candidates))
	wanted := make([]rpcDocWant, 0, end-start)
	unchanged := 0
	for _, candidate := range candidates[start:end] {
		skip, err := clientBackfillSkipped(db, candidate)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		if skip {
			unchanged++
		} else {
			wanted = append(wanted, candidate)
		}
	}
	return map[string]any{"documents": wanted, "offset": end, "more": end < len(candidates), "unchanged": unchanged}, exitOK, nil
}
