package main

import (
	"context"
	"database/sql"
	"errors"
	"flag"
	"strconv"
	"strings"
	"time"
)

func cmdCampaign(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("campaign", flag.ContinueOnError)
	page := fs.Int("page", 1, "log page, newest first (100 events per page)")
	all := fs.Bool("all", false, "include closed lanes and their unanswered asks")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	root, err := parseID(pos[0], "campaign id")
	if err != nil {
		return nil, 0, err
	}
	if *page < 1 {
		return nil, 0, usageErr("--page must be >= 1")
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	tx, err := db.BeginTx(context.Background(), &sql.TxOptions{ReadOnly: true})
	if err != nil {
		return nil, 0, dbErr(err)
	}
	defer tx.Rollback()
	out, err := readTUICampaign(tx, root, *page, *all, time.Now())
	if errors.Is(err, sql.ErrNoRows) {
		return nil, 0, rejectErr("campaign %d does not exist or is not a root", root)
	}
	return out, exitOK, dbErr(err)
}

const campaignTree = `with recursive tree(id) as (
 select ? union all select t.id from tasks t join tree on t.parent_id = tree.id
) `

// Reuse the dashboard's goal/plan/decision/document selection, with uncapped
// lanes. Only the TUI projection changes their shapes; the web API keeps its
// existing pagination and captured-document metadata.
func readTUICampaign(q queryer, root int64, page int, all bool, at time.Time) (map[string]any, error) {
	detail, err := readCampaignDetail(q, root, 1, true, all)
	if err != nil {
		return nil, err
	}
	r := detail["root"].(map[string]any)
	t, err := loadTask(q, root)
	if err != nil {
		return nil, err
	}
	leads, err := leadObservations(q)
	if err != nil {
		return nil, err
	}
	lead := firstNonEmpty(leads[root].Status, "unknown")
	if t.PaneID.String == "" {
		lead = "unregistered"
	} else if t.WaitingUntil.Valid && parseTime(t.WaitingUntil.String).After(at) && (lead == "working" || lead == "idle" || lead == "done") {
		lead = "waiting"
	}
	if t.Status == "closed" {
		lead = "closed"
	}
	r["pane_id"], r["lead"], r["age_ms"] = t.PaneID.String, lead, glanceAge(at, r["created_at"].(string))
	goal := []string{}
	if doc, ok := detail["goal"].(map[string]any); ok && doc["captured"] == true {
		body, err := readDashboardDocument(q, doc["doc_id"].(int64))
		if err != nil {
			return nil, err
		}
		goal = strings.Split(body["body"].(string), "\n")
	}
	plan := map[string]any{"version": int64(0), "age_ms": int64(0), "decisions_since": int64(0), "closed_since": int64(0)}
	if doc, ok := detail["plan"].(map[string]any); ok && doc != nil {
		plan = doc
		plan["id"] = doc["doc_id"]
		plan["age_ms"] = glanceAge(at, doc["created_at"].(string))
	}
	lanes := []map[string]any{}
	for _, lane := range detail["lanes"].([]map[string]any) {
		for _, kind := range []string{"brief", "report"} {
			doc, _ := lane[kind].(map[string]any)
			lane[kind] = doc != nil && doc["captured"] == true
		}
		lanes = append(lanes, lane)
	}
	decisions := []map[string]any{}
	ds, err := decisionsInForce(q, root)
	if err != nil {
		return nil, err
	}
	for _, d := range ds {
		text := d.Text
		if d.Kind == "owner_ask" {
			text += "\nAnswer: " + d.Answer
		}
		decisions = append(decisions, map[string]any{"id": d.ID, "kind": d.Kind, "at": d.At, "lane": d.From, "text": text, "owner": d.Kind == "owner_ask"})
	}
	docs, err := campaignDocRows(q, root)
	if err != nil {
		return nil, err
	}
	asks, err := campaignAskRows(q, root, all)
	if err != nil {
		return nil, err
	}
	var total int
	if err := q.QueryRow(campaignTree+`select count(*) from events where task_id in (select id from tree)`, root).Scan(&total); err != nil {
		return nil, err
	}
	metadata := pageMetadata(total, 100, page)
	log, err := campaignLogRows(q, root, metadata["page"].(int))
	if err != nil {
		return nil, err
	}
	prs, err := campaignPRRows(q, root)
	if err != nil {
		return nil, err
	}
	sparks, err := campaignSparks(q, at, root)
	if err != nil {
		return nil, err
	}
	return map[string]any{"root": r, "goal": goal, "plan": plan, "lanes": lanes, "asks": asks, "decisions": decisions, "docs": docs, "log": log, "prs": prs, "spark": sparks[root], "log_page": metadata}, nil
}

func campaignDocRows(q queryer, root int64) ([]map[string]any, error) {
	rows, err := q.Query(campaignTree+`select d.id,d.kind,d.name,t.name,d.version,d.captured
 from documents d join tasks t on t.id = d.task_id where d.task_id in (select id from tree)
 and d.id = (select x.id from documents x where x.task_id = d.task_id and x.kind = d.kind and x.name = d.name order by version desc limit 1)
 order by d.id desc`, root)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	docs := []map[string]any{}
	for rows.Next() {
		var id, version int64
		var kind, name, lane string
		var captured bool
		if err := rows.Scan(&id, &kind, &name, &lane, &version, &captured); err != nil {
			return nil, err
		}
		docs = append(docs, map[string]any{"id": id, "kind": kind, "name": name, "lane": lane, "version": version, "captured": captured})
	}
	return docs, rows.Err()
}

func campaignAskRows(q queryer, root int64, all bool) ([]map[string]any, error) {
	rows, err := q.Query(campaignTree+`, asks as (
 select e.*, row_number() over (partition by (e.answered_by is null) order by e.id desc) as n
 from events e join tasks t on t.id = e.task_id where e.task_id in (select id from tree) and e.kind = 'ask'
 and (e.answered_by is not null or ? or (t.status != 'closed' and (select status from tasks where id = ?) != 'closed'))
 ) select e.id,e.created_at,t.name,coalesce(e.summary,''),e.answered_by is null,
 coalesce(json_extract(e.data,'$.owner'),0),coalesce(json_extract(e.data,'$.blocking'),0),a.id,a.created_at,a.summary,a.kind
 from asks e join tasks t on t.id = e.task_id left join events a on a.id = e.answered_by
 where e.answered_by is null or e.n <= 20 order by (e.answered_by is null) desc,e.id desc`, root, all, root)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	asks := []map[string]any{}
	for rows.Next() {
		var id int64
		var at, lane, text string
		var open, owner, blocking bool
		var answerID sql.NullInt64
		var answerAt, answerText, answerKind sql.NullString
		if err := rows.Scan(&id, &at, &lane, &text, &open, &owner, &blocking, &answerID, &answerAt, &answerText, &answerKind); err != nil {
			return nil, err
		}
		asks = append(asks, map[string]any{"id": id, "kind": "ask", "at": at, "lane": lane, "text": text, "open": open, "owner": owner, "blocking": blocking})
		if answerID.Valid {
			asks = append(asks, map[string]any{"id": answerID.Int64, "kind": answerKind.String, "at": answerAt.String, "lane": lane, "text": answerText.String, "owner": owner})
		}
	}
	return asks, rows.Err()
}

func campaignLogRows(q queryer, root int64, page int) ([]map[string]any, error) {
	rows, err := q.Query(campaignTree+`select e.id,e.kind,e.created_at,t.name,coalesce(e.summary,''),
 e.kind = 'ask' and e.answered_by is null,coalesce(json_extract(e.data,'$.owner'),0),coalesce(json_extract(e.data,'$.blocking'),0)
 from events e join tasks t on t.id = e.task_id where e.task_id in (select id from tree)
 order by e.id desc limit 100 offset ?`, root, (page-1)*100)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	log := []map[string]any{}
	for rows.Next() {
		var id int64
		var kind, at, lane, text string
		var open, owner, blocking bool
		if err := rows.Scan(&id, &kind, &at, &lane, &text, &open, &owner, &blocking); err != nil {
			return nil, err
		}
		log = append(log, map[string]any{"id": id, "kind": kind, "at": at, "lane": lane, "text": text, "open": open, "owner": owner, "blocking": blocking})
	}
	return log, rows.Err()
}

// Keep references verbatim. A number is supplied only for an exact numeric
// PR-valued ref; metadata belongs only to the bare pr row.
func campaignPRRows(q queryer, root int64) ([]map[string]any, error) {
	rows, err := q.Query(campaignTree+`select e.task_id,t.name,json_extract(e.data,'$.key'),json_extract(e.data,'$.value')
 from events e join tasks t on t.id = e.task_id where e.task_id in (select id from tree) and e.kind = 'ref'
 and (json_extract(e.data,'$.key') = 'pr' or json_extract(e.data,'$.key') like 'pr.%')
 and e.id = (select max(x.id) from events x where x.task_id = e.task_id and x.kind = 'ref' and json_extract(x.data,'$.key') = json_extract(e.data,'$.key'))
 and coalesce(json_extract(e.data,'$.value'),'') != '' order by e.task_id,e.id`, root)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	prs := []map[string]any{}
	byTask := map[int64]map[string]any{}
	for rows.Next() {
		var task int64
		var lane, key, value string
		if err := rows.Scan(&task, &lane, &key, &value); err != nil {
			return nil, err
		}
		pr := byTask[task]
		if pr == nil {
			pr = map[string]any{"task_id": task, "lane": lane, "refs": []planRef{}}
			byTask[task] = pr
		}
		pr["refs"] = append(pr["refs"].([]planRef), planRef{Key: key, Value: value})
		if field := strings.TrimPrefix(key, "pr."); key != "pr" && (field == "title" || field == "state" || field == "ci" || field == "review") {
			pr[field] = value
			continue
		}
		row := map[string]any{"task_id": task, "lane": lane, "key": key, "value": value}
		if strings.Trim(value, "0123456789") == "" {
			if n, err := strconv.ParseUint(value, 10, 32); err == nil {
				row["number"] = n
			}
		}
		prs = append(prs, row)
	}
	for _, row := range prs {
		lane := byTask[row["task_id"].(int64)]
		row["refs"] = lane["refs"]
		if row["key"] == "pr" {
			for _, field := range []string{"title", "state", "ci", "review"} {
				if value, ok := lane[field]; ok {
					row[field] = value
				}
			}
		}
	}
	return prs, rows.Err()
}
