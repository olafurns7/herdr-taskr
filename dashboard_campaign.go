package main

import (
	"database/sql"
	"errors"
	"net/http"
	"strconv"
	"strings"
)

func campaignPage(r *http.Request) int {
	n, err := strconv.Atoi(r.URL.Query().Get("page"))
	if err != nil || n < 1 {
		return 1
	}
	return n
}

func pageMetadata(total, size, page int) map[string]any {
	pages := max(1, (total+size-1)/size)
	return map[string]any{"page": min(page, pages), "pages": pages, "total": total}
}

// All reads share the dashboard admission gate and one ledger snapshot.
func (d *dashboard) campaignRead(w http.ResponseWriter, r *http.Request, read func(*sql.Tx) (any, error)) {
	tx, err := d.db.BeginTx(r.Context(), &sql.TxOptions{ReadOnly: true})
	if err != nil {
		httpError(w, 500, "database error")
		return
	}
	defer tx.Rollback()
	value, err := read(tx)
	if errors.Is(err, sql.ErrNoRows) {
		httpError(w, 404, "not found")
		return
	}
	if err != nil {
		httpError(w, 500, "database error")
		return
	}
	httpJSON(w, 200, value)
}
func (d *dashboard) campaigns(w http.ResponseWriter, r *http.Request) {
	d.campaignRead(w, r, func(tx *sql.Tx) (any, error) { return readCampaigns(tx, campaignPage(r)) })
}
func campaignRouteID(r *http.Request) (int64, error) {
	value := r.PathValue("id")
	if value == "" || value[0] == '0' || strings.Trim(value, "0123456789") != "" {
		return 0, sql.ErrNoRows
	}
	n, err := strconv.ParseInt(value, 10, 64)
	if err != nil || n < 1 {
		return 0, sql.ErrNoRows
	}
	return n, nil
}
func (d *dashboard) campaign(w http.ResponseWriter, r *http.Request) {
	d.campaignRead(w, r, func(tx *sql.Tx) (any, error) {
		id, err := campaignRouteID(r)
		if err != nil {
			return nil, err
		}
		return readCampaign(tx, id, campaignPage(r))
	})
}
func (d *dashboard) doc(w http.ResponseWriter, r *http.Request) {
	d.campaignRead(w, r, func(tx *sql.Tx) (any, error) {
		id, err := campaignRouteID(r)
		if err != nil {
			return nil, err
		}
		return readDashboardDocument(tx, id)
	})
}

func dashboardDocument(q queryer, task int64, kind string) (map[string]any, error) {
	d, err := latestDocument(q, task, kind, "")
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return d.record(), nil
}

// Keep goal selection identical to the handover: latest captured, otherwise latest miss.
func dashboardGoal(q queryer, root int64) (document, error) {
	return scanDocument(q.QueryRow(`select `+documentCols+` from documents where task_id = ? and kind = 'goal' and name = ''
		order by captured desc, version desc limit 1`, root))
}

func readCampaigns(q queryer, page int) (map[string]any, error) {
	var total int
	if err := q.QueryRow(`select count(*) from tasks where parent_id is null`).Scan(&total); err != nil {
		return nil, err
	}
	out := pageMetadata(total, 50, page)
	rows, err := q.Query(`select id, name, status, created_at, coalesce(closed_at, '') from tasks
  where parent_id is null order by id desc limit 50 offset ?`, (out["page"].(int)-1)*50)
	if err != nil {
		return nil, err
	}
	items := []map[string]any{}
	for rows.Next() {
		var id int64
		var name, status, created, closed string
		if err := rows.Scan(&id, &name, &status, &created, &closed); err != nil {
			rows.Close()
			return nil, err
		}
		items = append(items, map[string]any{"id": id, "name": name, "status": status, "created_at": created, "closed_at": closed})
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, err
	}
	for _, item := range items {
		root := item["id"].(int64)
		counts := map[string]int{}
		rows, err := q.Query(`with recursive tree(id) as (select ? union all select t.id from tasks t join tree on t.parent_id = tree.id)
   select status, count(*) from tasks where id in (select id from tree) and id != ? group by status`, root, root)
		if err != nil {
			return nil, err
		}
		for rows.Next() {
			var status string
			var count int
			if err := rows.Scan(&status, &count); err != nil {
				rows.Close()
				return nil, err
			}
			counts[status] = count
		}
		rows.Close()
		if err := rows.Err(); err != nil {
			return nil, err
		}
		item["lane_counts"] = counts
		item["goal"] = ""
		doc, err := dashboardGoal(q, root)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return nil, err
		}
		if err == nil && doc.Captured {
			var body string
			if err := q.QueryRow(`select body from doc_blobs where sha256 = ?`, doc.Hash.String).Scan(&body); err != nil {
				return nil, err
			}
			for _, line := range strings.Split(body, "\n") {
				if strings.TrimSpace(line) != "" {
					item["goal"] = clip(line, 160)
					break
				}
			}
		}
	}
	out["campaigns"] = items
	return out, nil
}

func campaignDocuments(q queryer, root int64) (map[string]any, map[string]any, []map[string]any, error) {
	rows, err := q.Query(`select `+documentCols+` from documents where task_id = ? and kind in ('goal','plan')
  and id = (select id from documents d where d.task_id = documents.task_id and d.kind = documents.kind and d.name = documents.name
    order by case when d.kind = 'goal' then d.captured else 0 end desc, d.version desc limit 1)
  order by kind,name`, root)
	if err != nil {
		return nil, nil, nil, err
	}
	var goal, plan map[string]any
	named := []map[string]any{}
	var planEvent int64
	for rows.Next() {
		d, err := scanDocument(rows)
		if err != nil {
			rows.Close()
			return nil, nil, nil, err
		}
		if d.Kind == "goal" {
			goal = d.record()
		} else if d.Name == "" {
			plan = d.record()
			planEvent = d.EventID.Int64
		} else {
			named = append(named, d.record())
		}
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, nil, nil, err
	}
	if plan != nil {
		var decisions, closed int64
		// Match handover counters: all decision/close events in the tree after the plan event, including revoked decisions.
		err := q.QueryRow(`with recursive tree(id) as (select ? union all select t.id from tasks t join tree on t.parent_id = tree.id)
   select coalesce(sum(kind = 'decision'),0),coalesce(sum(kind = 'closed'),0) from events where task_id in (select id from tree) and id > ?`, root, planEvent).Scan(&decisions, &closed)
		if err != nil {
			return nil, nil, nil, err
		}
		plan["decisions_since"], plan["closed_since"] = decisions, closed
	}
	return goal, plan, named, nil
}

func readCampaign(q queryer, root int64, page int) (map[string]any, error) {
	var name, role, status, host, created, closed string
	err := q.QueryRow(`select name,role,status,coalesce(machine,''),created_at,coalesce(closed_at,'') from tasks where id = ? and parent_id is null`, root).Scan(&name, &role, &status, &host, &created, &closed)
	if err != nil {
		return nil, err
	}
	next, err := taskNext(q, root)
	if err != nil {
		return nil, err
	}
	nextText := ""
	if next != nil {
		nextText = next.Text
	}
	goal, plan, named, err := campaignDocuments(q, root)
	if err != nil {
		return nil, err
	}
	decisions, err := decisionsInForce(q, root)
	if err != nil {
		return nil, err
	}
	ds := []map[string]any{}
	for _, d := range decisions {
		text := d.Text
		if d.Kind == "owner_ask" {
			text += "\nAnswer: " + d.Answer
		}
		ds = append(ds, map[string]any{"id": d.ID, "time": d.At, "text": text})
	}
	marks, err := handoverMarks(q, root, -1)
	if err != nil {
		return nil, err
	}
	handovers := []map[string]any{}
	for i := len(marks) - 1; i >= 0; i-- {
		m := marks[i]
		var docID sql.NullInt64
		err := q.QueryRow(`select id from documents where task_id = ? and kind = 'handover' and event_id = ? and captured = 1 order by version desc limit 1`, root, m.EventID).Scan(&docID)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return nil, err
		}
		h := map[string]any{"id": m.EventID, "time": m.At, "note": m.Note, "doc_id": nil}
		putInt(h, "doc_id", docID)
		handovers = append(handovers, h)
	}
	const tree = `with recursive tree(id,depth) as (select ?,0 union all select t.id,tree.depth+1 from tasks t join tree on t.parent_id = tree.id) `
	var total int
	if err := q.QueryRow(tree+`select count(*) from tree where depth > 0`, root).Scan(&total); err != nil {
		return nil, err
	}
	out := pageMetadata(total, 100, page)
	rows, err := q.Query(tree+`select t.id,t.parent_id,tree.depth,t.name,t.role,t.status,coalesce(case when t.current_launch_id is null then t.machine else l.machine end,''),t.created_at,coalesce(t.closed_at,''),
  coalesce(l.provider,''),coalesce(l.model,''),coalesce(l.effort,''),
  coalesce((select summary from events e where e.task_id = t.id and kind in ('done','fail','ready') order by id desc limit 1),'')
  from tree join tasks t on t.id = tree.id left join launches l on l.id = t.current_launch_id where depth > 0 order by t.id limit 100 offset ?`, root, (out["page"].(int)-1)*100)
	if err != nil {
		return nil, err
	}
	lanes := []map[string]any{}
	for rows.Next() {
		var id, parent int64
		var depth int
		var name, role, status, host, created, closed, provider, model, effort, summary string
		if err := rows.Scan(&id, &parent, &depth, &name, &role, &status, &host, &created, &closed, &provider, &model, &effort, &summary); err != nil {
			rows.Close()
			return nil, err
		}
		lanes = append(lanes, map[string]any{"id": id, "parent_id": parent, "depth": depth, "name": name, "role": role, "status": status, "host": host, "created_at": created, "closed_at": closed, "provider": provider, "model": model, "effort": effort, "summary": clip(summary, 300)})
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, err
	}
	for _, l := range lanes {
		for _, kind := range []string{"brief", "report"} {
			doc, err := dashboardDocument(q, l["id"].(int64), kind)
			if err != nil {
				return nil, err
			}
			l[kind] = doc
		}
	}
	out["root"] = map[string]any{"id": root, "name": name, "role": role, "status": status, "host": host, "created_at": created, "closed_at": closed, "next": nextText}
	if goal != nil && goal["captured"] == true {
		latest, err := latestDocument(q, root, "goal", "")
		if err != nil {
			return nil, err
		}
		if !latest.Captured && latest.Version > goal["version"].(int64) {
			out["goal_miss"] = latest.record()
		}
	}
	out["goal"], out["plan"], out["documents"], out["decisions"], out["handovers"], out["lanes"] = goal, plan, named, ds, handovers, lanes
	return out, nil
}

func readDashboardDocument(q queryer, id int64) (map[string]any, error) {
	d, err := scanDocument(q.QueryRow(`select `+documentCols+` from documents where id = ?`, id))
	if err != nil {
		return nil, err
	}
	out := d.record()
	out["root_id"], out["sha256"] = d.RootID, nil
	putStr(out, "sha256", d.Hash)
	var lane string
	if err := q.QueryRow(`select name from tasks where id = ?`, d.TaskID).Scan(&lane); err != nil {
		return nil, err
	}
	out["lane"] = lane
	out["captured_at"] = nil
	if d.Captured {
		out["captured_at"] = d.Created
	}
	if d.Captured {
		var body string
		if err := q.QueryRow(`select body from doc_blobs where sha256 = ?`, d.Hash.String).Scan(&body); err != nil {
			return nil, err
		}
		out["body"] = body
	}
	rows, err := q.Query(`select id,version,created_at,event_id from documents where task_id = ? and kind = ? and name = ? order by version`, d.TaskID, d.Kind, d.Name)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	versions := []map[string]any{}
	for rows.Next() {
		var id, version int64
		var created string
		var event sql.NullInt64
		if err := rows.Scan(&id, &version, &created, &event); err != nil {
			return nil, err
		}
		v := map[string]any{"id": id, "version": version, "created_at": created, "event_id": nil}
		putInt(v, "event_id", event)
		versions = append(versions, v)
	}
	out["versions"] = versions
	return out, rows.Err()
}
