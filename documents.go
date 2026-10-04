package main

import (
	"bytes"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"unicode/utf8"
)

const documentCap = 1 << 20

// Tests instrument reads to verify the byte cap and absence of a writer lock.
var readDocumentBody = io.ReadAll

type documentInput struct {
	Path, Host, Hash, Body, Format, Reason string
	Bytes                                  *int64
}

type document struct {
	ID, RootID, TaskID, Version      int64
	Kind, Name, Created              string
	Hash, Format, Reason, Path, Host sql.NullString
	Bytes, EventID                   sql.NullInt64
	Captured                         bool
	Backfill                         int
}

const documentCols = `id, root_id, task_id, kind, name, version, sha256, bytes, format,
	captured, reason, source_path, source_host, event_id, backfill, created_at`

func scanDocument(s interface{ Scan(...any) error }) (document, error) {
	var d document
	err := s.Scan(&d.ID, &d.RootID, &d.TaskID, &d.Kind, &d.Name, &d.Version, &d.Hash, &d.Bytes,
		&d.Format, &d.Captured, &d.Reason, &d.Path, &d.Host, &d.EventID, &d.Backfill, &d.Created)
	return d, err
}

func latestDocument(q queryer, taskID int64, kind, name string) (document, error) {
	return scanDocument(q.QueryRow(`select `+documentCols+` from documents
		where task_id = ? and kind = ? and name = ? order by version desc limit 1`, taskID, kind, name))
}

func bodyDocument(body []byte, path string) documentInput {
	n := int64(len(body))
	sum := sha256.Sum256(body)
	d := documentInput{Path: path, Hash: hex.EncodeToString(sum[:]), Bytes: &n}
	switch {
	case len(body) > documentCap:
		d.Reason = "too_large"
	case !utf8.Valid(body) || bytes.IndexByte(body, 0) >= 0:
		d.Reason = "binary"
	default:
		d.Body, d.Format = string(body), "text"
		switch strings.ToLower(filepath.Ext(path)) {
		case ".md", ".markdown":
			d.Format = "md"
		}
	}
	return d
}

func fileDocument(path, host string) documentInput {
	d := documentInput{Path: path, Host: host, Reason: "missing"}
	if host != "" {
		d.Reason = "client"
		return d
	}
	info, err := os.Stat(path)
	if err != nil || !info.Mode().IsRegular() {
		return d
	}
	n := info.Size()
	d.Bytes = &n
	if n > documentCap {
		d.Reason = "too_large"
		return d
	}
	f, err := os.Open(path)
	if err != nil {
		return d
	}
	defer f.Close()
	body, err := readDocumentBody(io.LimitReader(f, documentCap+1))
	if err != nil {
		return d
	}
	if len(body) > documentCap {
		d.Reason = "too_large"
		if info, err := f.Stat(); err == nil {
			n = max(info.Size(), int64(len(body)))
		} else {
			n = int64(len(body))
		}
		return d // The limited read cannot supply a hash of the entire file.
	}
	return bodyDocument(body, path)
}

func sameDocument(old document, in documentInput) bool {
	if in.Reason == "" {
		return old.Captured && old.Hash.String == in.Hash
	}
	return !old.Captured && old.Reason.String == in.Reason && old.Path.String == in.Path && old.Host.String == in.Host
}

func storeDocument(tx *sql.Tx, taskID int64, kind, name string, in documentInput, eventID *int64, backfill int) (document, bool, error) {
	old, err := latestDocument(tx, taskID, kind, name)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return document{}, false, err
	}
	if err == nil && sameDocument(old, in) {
		return old, true, nil
	}
	if in.Reason == "missing" || in.Reason == "client" {
		captured, err := scanDocument(tx.QueryRow(`select `+documentCols+` from documents
            where task_id = ? and kind = ? and name = ? and captured = 1 order by version desc limit 1`, taskID, kind, name))
		if err == nil {
			if in.Reason == "missing" || captured.Host.String == in.Host && captured.Path.String == in.Path {
				return captured, true, nil
			}
		} else if !errors.Is(err, sql.ErrNoRows) {
			return document{}, false, err
		}
	}
	root, err := rootOf(tx, taskID)
	if err != nil {
		return document{}, false, err
	}
	if in.Reason == "" {
		if _, err := tx.Exec(`insert or ignore into doc_blobs (sha256, bytes, body) values (?, ?, ?)`, in.Hash, *in.Bytes, in.Body); err != nil {
			return document{}, false, err
		}
	}
	res, err := tx.Exec(`insert into documents (root_id, task_id, kind, name, version, sha256, bytes, format,
		captured, reason, source_path, source_host, event_id, backfill, created_at) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		root, taskID, kind, name, old.Version+1, nullStr(in.Hash), nullInt(in.Bytes), nullStr(in.Format),
		in.Reason == "", nullStr(in.Reason), nullStr(in.Path), nullStr(in.Host), nullInt(eventID), backfill, now())
	if err != nil {
		return document{}, false, err
	}
	id, err := res.LastInsertId()
	if err != nil {
		return document{}, false, err
	}
	d, err := scanDocument(tx.QueryRow(`select `+documentCols+` from documents where id = ?`, id))
	if err == nil && d.Captured {
		_, err = tx.Exec(`delete from search_fts where src = 'doc' and task_id = ? and kind = ? and name = ?`, taskID, kind, name)
		if err == nil {
			_, err = tx.Exec(`insert into search_fts(body, src, ref, kind, name, root_id, task_id, at)
				values (?, 'doc', ?, ?, ?, ?, ?, ?)`, in.Body, d.ID, kind, name, root, taskID, d.Created)
		}
	}
	return d, false, err
}

// Statement failures and panics are isolated; a lost transaction must fail the command.
func captureDocument(tx *sql.Tx, fn func() error) (err error) {
	if _, err := tx.Exec(`savepoint doc_capture`); err != nil {
		return nil
	}
	rollback := func() error {
		_, err := tx.Exec(`rollback to doc_capture`)
		_, _ = tx.Exec(`release doc_capture`)
		return err
	}
	defer func() {
		if recover() != nil {
			err = rollback()
		}
	}()
	if err := fn(); err != nil {
		return rollback()
	}
	_, _ = tx.Exec(`release doc_capture`)
	return nil
}

func documentHost(q queryer, taskID int64) (string, error) {
	var host sql.NullString
	err := q.QueryRow(`select case when t.current_launch_id is null then t.machine else l.machine end
		from tasks t left join launches l on l.id = t.current_launch_id where t.id = ?`, taskID).Scan(&host)
	return host.String, err
}

func recordClientDocument(c *ctx, taskID int64, kind, name string, in documentInput, eventID *int64) {
	if c != nil && c.docUpload && in.Reason == "client" && in.Host == c.machine {
		c.docUploads = append(c.docUploads, rpcDocWant{
			Task: taskID, Kind: kind, Name: name, Path: in.Path, EventID: eventID,
		})
	}
}

func captureBrief(tx *sql.Tx, c *ctx, taskID int64, in *documentInput) error {
	if in == nil {
		return nil
	}
	kind, stored := "goal", false
	err := captureDocument(tx, func() error {
		t, err := loadTask(tx, taskID)
		if err != nil {
			return err
		}
		kind = "goal"
		if t.ParentID.Valid {
			kind = "brief"
		}
		_, _, err = storeDocument(tx, taskID, kind, "", *in, nil, 0)
		stored = err == nil
		return err
	})
	if err == nil && stored {
		recordClientDocument(c, taskID, kind, "", *in, nil)
	}
	return err
}

func capturePrompt(tx *sql.Tx, c *ctx, taskID, eventID int64, in documentInput) error {
	kind, name, stored := "prompt", "", false
	err := captureDocument(tx, func() error {
		if in.Path != "" {
			var brief sql.NullString
			if err := tx.QueryRow(`select brief_path from tasks where id = ?`, taskID).Scan(&brief); err != nil {
				return err
			}
			if in.Path == brief.String {
				kind = "brief"
			} else {
				name = filepath.Base(in.Path)
			}
			if c != nil && c.machine != "" {
				in.Host, in.Reason, in.Body, in.Format = c.machine, "client", "", ""
			}
		}
		_, _, err := storeDocument(tx, taskID, kind, name, in, ptr(eventID), 0)
		stored = err == nil
		return err
	})
	if err == nil && stored {
		recordClientDocument(c, taskID, kind, name, in, ptr(eventID))
	}
	return err
}

func reportDocumentPath(q queryer, taskID int64) (string, error) {
	var path sql.NullString
	err := q.QueryRow(`select coalesce((select json_extract(data, '$.report') from events
		where task_id = t.id and kind = 'ready' order by id desc limit 1), t.report_path)
		from tasks t where id = ?`, taskID).Scan(&path)
	return path.String, err
}

type preparedReport struct {
	input    documentInput
	latestID int64
}

func newestReportID(q queryer, taskID int64) (int64, error) {
	var id int64
	err := q.QueryRow(`select coalesce(max(id), 0) from documents where task_id = ? and kind = 'report'`, taskID).Scan(&id)
	return id, err
}

// prepareReport reads before the event transaction. Ready uses its explicit path
// or the task default; terminal commands use the latest ready override.
func prepareReport(q queryer, taskID int64, explicit string, ready bool) *preparedReport {
	path := explicit
	var err error
	if path == "" {
		if ready {
			var value sql.NullString
			err = q.QueryRow(`select report_path from tasks where id = ?`, taskID).Scan(&value)
			path = value.String
		} else {
			path, err = reportDocumentPath(q, taskID)
		}
	}
	if err != nil || path == "" {
		return nil
	}
	host, err := documentHost(q, taskID)
	if err != nil {
		return nil
	}
	latestID, err := newestReportID(q, taskID)
	if err != nil {
		return nil
	}
	return &preparedReport{input: fileDocument(path, host), latestID: latestID}
}

func captureReport(tx *sql.Tx, c *ctx, taskID, eventID int64, report *preparedReport) error {
	if report == nil {
		return nil
	}
	stored := false
	err := captureDocument(tx, func() error {
		path, err := reportDocumentPath(tx, taskID)
		if err != nil {
			return err
		}
		host, err := documentHost(tx, taskID)
		if err != nil {
			return err
		}
		latestID, err := newestReportID(tx, taskID)
		if err != nil {
			return err
		}
		if path != report.input.Path || host != report.input.Host || latestID != report.latestID {
			return nil
		}
		_, _, err = storeDocument(tx, taskID, "report", "", report.input, ptr(eventID), 0)
		stored = err == nil
		return err
	})
	if err == nil && stored {
		recordClientDocument(c, taskID, "report", "", report.input, ptr(eventID))
	}
	return err
}

type handoverDocuments struct {
	Goal, Plan        *document
	GoalBody          string
	Named             []document
	Decisions, Closed int64
}

func loadHandoverDocuments(q queryer, root int64) handoverDocuments {
	var result handoverDocuments
	rows, err := q.Query(`select `+documentCols+` from documents where task_id = ? and kind in ('goal', 'plan')
		and id = (select id from documents d where d.task_id = documents.task_id and d.kind = documents.kind and d.name = documents.name
            order by case when d.kind = 'goal' then d.captured else 0 end desc, d.version desc limit 1)
        order by kind, name`, root)
	if err != nil {
		return result
	}
	for rows.Next() {
		d, err := scanDocument(rows)
		if err != nil {
			rows.Close()
			return handoverDocuments{}
		}
		switch {
		case d.Kind == "goal":
			result.Goal = &d
		case d.Name == "":
			result.Plan = &d
		default:
			result.Named = append(result.Named, d)
		}
	}
	err = rows.Err()
	rows.Close()
	if err != nil {
		return handoverDocuments{}
	}
	if result.Goal != nil && result.Goal.Captured {
		_ = q.QueryRow(`select body from doc_blobs where sha256 = ?`, result.Goal.Hash.String).Scan(&result.GoalBody)
	}
	if result.Plan != nil {
		_ = q.QueryRow(`with recursive tree(id) as (select ? union all select t.id from tasks t join tree on t.parent_id = tree.id)
			select coalesce(sum(kind = 'decision'), 0), coalesce(sum(kind = 'closed'), 0) from events
			where task_id in (select id from tree) and id > ?`, root, result.Plan.EventID.Int64).Scan(&result.Decisions, &result.Closed)
	}
	return result
}

func renderHandoverDocuments(b *strings.Builder, root int64, docs handoverDocuments) {
	if docs.Goal == nil {
		fmt.Fprintf(b, "Goal: none recorded; run `taskr doc set %d goal --file PATH`\n\n", root)
	} else if d := docs.Goal; d.Captured {
		fmt.Fprintf(b, "Goal (doc %d, v%d):\n", d.ID, d.Version)
		n := 0
		for _, line := range strings.Split(docs.GoalBody, "\n") {
			if strings.TrimSpace(line) == "" {
				continue
			}
			runes := []rune(line)
			if len(runes) > 160 {
				runes = runes[:160]
			}
			fmt.Fprintln(b, md(string(runes)))
			n++
			if n == 3 {
				break
			}
		}
		fmt.Fprintf(b, "Full text: `taskr doc get %d`\n\n", d.ID)
	} else {
		host := d.Host.String
		if host == "" {
			host = "server host"
		}
		fmt.Fprintf(b, "Goal: recorded at %s on %s, not captured (%s)\n\n", md(d.Path.String), md(host), md(d.Reason.String))
	}
	if d := docs.Plan; d != nil {
		fmt.Fprintf(b, "Plan (doc %d, v%d, event %d): since then %d decisions, %d lanes closed.\n`taskr doc get %d`\n\n", d.ID, d.Version, d.EventID.Int64, docs.Decisions, docs.Closed, d.ID)
	}
	if len(docs.Named) > 0 {
		var labels []string
		for _, d := range docs.Named {
			labels = append(labels, fmt.Sprintf("%s (doc %d, v%d)", md(d.Name), d.ID, d.Version))
		}
		fmt.Fprintf(b, "Documents: %s\n\n", strings.Join(labels, ", "))
	}
}
