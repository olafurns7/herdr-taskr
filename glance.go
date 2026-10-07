package main

import (
	"context"
	"database/sql"
	"flag"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"
)

const (
	glanceResultsAfter = 30 * time.Minute
	glanceQuietAfter   = 12 * time.Hour
)

type glanceView struct {
	Now       string            `json:"now"`
	Verdict   string            `json:"verdict"`
	NeedsYou  []glanceNeed      `json:"needs_you"`
	Attention []glanceAttention `json:"attention"`
	Campaigns []glanceCampaign  `json:"campaigns"`
	Quiet     glanceQuiet       `json:"quiet"`
}

type glanceNeed struct {
	Kind         string   `json:"kind"`
	Campaign     string   `json:"campaign"`
	RootID       int64    `json:"root_id"`
	Host         string   `json:"host,omitempty"`
	PaneID       string   `json:"pane_id,omitempty"`
	AgeMS        int64    `json:"age_ms"`
	Since        string   `json:"since"`
	AskID        int64    `json:"ask_id,omitempty"`
	Text         string   `json:"text,omitempty"`
	Blocking     *bool    `json:"blocking,omitempty"`
	Asker        string   `json:"asker,omitempty"`
	AskerWaiting *bool    `json:"asker_waiting,omitempty"`
	Also         []string `json:"also,omitempty"`
	NoteID       int64    `json:"note_id,omitempty"`
	Items        []string `json:"items,omitempty"`
}

type glanceAttention struct {
	Kind        string `json:"kind"`
	Campaign    string `json:"campaign,omitempty"`
	RootID      int64  `json:"root_id,omitempty"`
	Lane        string `json:"lane,omitempty"`
	LaneID      int64  `json:"lane_id,omitempty"`
	Text        string `json:"text"`
	AgeMS       int64  `json:"age_ms"`
	Since       string `json:"since,omitempty"`
	Host        string `json:"host,omitempty"`
	PaneID      string `json:"pane_id,omitempty"`
	Recipient   string `json:"recipient,omitempty"`
	RecipientID int64  `json:"recipient_id,omitempty"`
	Count       int    `json:"count,omitempty"`
	Waiting     *bool  `json:"waiting,omitempty"`
}

type glanceCampaign struct {
	ID            int64       `json:"id"`
	Name          string      `json:"name"`
	Host          string      `json:"host,omitempty"`
	PaneID        string      `json:"pane_id,omitempty"`
	Lanes         glanceLanes `json:"lanes"`
	Lead          string      `json:"lead"`
	Last          *glanceLast `json:"last,omitempty"`
	ActivityAgeMS int64       `json:"activity_age_ms"`
}

type glanceLanes struct {
	Working int `json:"working"`
	Ready   int `json:"ready"`
	Open    int `json:"open"`
}

type glanceLast struct {
	Kind  string `json:"kind"`
	Text  string `json:"text"`
	AgeMS int64  `json:"age_ms"`
}

type glanceQuiet struct {
	Count       int      `json:"count"`
	WithBacklog int      `json:"with_backlog"`
	Names       []string `json:"names"`
}

func cmdGlance(c *ctx, args []string) (any, int, error) {
	if _, err := parseArgs(c, flag.NewFlagSet("glance", flag.ContinueOnError), args, 0, 0); err != nil {
		return nil, 0, err
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	v, err := readGlance(db, time.Now())
	return v, exitOK, dbErr(err)
}

// Traverse closed intermediates too: their open descendants still belong
// to the open root, and their events still contribute to tree activity.
const glanceTrees = `with recursive tree(root, id) as (
	select id, id from tasks where parent_id is null and status != 'closed'
	union all select tree.root, t.id from tasks t join tree on t.parent_id = tree.id
) `

type glanceTask struct {
	stateTask
	rootID     int64
	host       string
	launchHost string
}

type glanceRoot struct {
	glanceCampaign
	active   bool
	activity string
	lastID   int64
}

func readGlance(db *sql.DB, at time.Time) (*glanceView, error) {
	tx, err := db.BeginTx(context.Background(), &sql.TxOptions{ReadOnly: true})
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()
	v := &glanceView{Now: stamp(at), Verdict: "rolling", NeedsYou: []glanceNeed{},
		Attention: []glanceAttention{}, Campaigns: []glanceCampaign{}, Quiet: glanceQuiet{Names: []string{}}}
	roots := map[int64]*glanceRoot{}
	tasks := map[int64]*glanceTask{}
	hosts := map[string]bool{}
	asked := map[int64]bool{}
	if err := glanceTasks(tx, at, roots, tasks, hosts); err != nil {
		return nil, err
	}
	if err := glanceOwnerAsks(tx, at, v, roots, tasks, asked); err != nil {
		return nil, err
	}
	if err := glanceOwnerTodos(tx, at, v, roots); err != nil {
		return nil, err
	}
	if err := glanceBacklog(tx, at, v, roots, tasks, hosts, asked); err != nil {
		return nil, err
	}
	finishGlance(tx, at, v, roots)
	return v, nil
}

func glanceAge(at time.Time, ts string) int64 {
	if ts == "" {
		return 0
	}
	return max(0, at.Sub(parseTime(ts)).Milliseconds())
}

func glanceTasks(tx *sql.Tx, at time.Time, roots map[int64]*glanceRoot, tasks map[int64]*glanceTask, hosts map[string]bool) error {
	// Each latest-event lookup uses events_task, without a global milestone cap.
	kinds := `'` + strings.Join(milestoneKinds, `','`) + `'`
	keys := `'` + strings.Join(milestoneRefKeys, `','`) + `'`
	rows, err := tx.Query(glanceTrees+`, heads as (
		select tree.root, max(a.id) as activity_id, max(m.id) as milestone_id
		from tree join tasks t on t.id = tree.id
		left join events a on a.id = (select max(x.id) from events x where x.task_id = t.id)
		left join events m on m.id = (select x.id from events x where x.task_id = t.id and
			(x.kind in (`+kinds+`) or (x.kind = 'note' and t.parent_id is null) or
			(x.kind = 'ref' and json_extract(x.data, '$.key') in (`+keys+`) and coalesce(json_extract(x.data, '$.value'), '') != ''))
			order by x.id desc limit 1)
		group by tree.root)
		select tree.root, t.id, coalesce(t.parent_id, 0), t.name, t.role, t.status,
		coalesce(l.pane_id, t.pane_id, ''), coalesce(l.machine, t.machine, ''), coalesce(l.machine, ''),
		coalesce(t.waiting_until > ?, 0), coalesce(l.observed_status, ''), coalesce(l.observed_at, ''), l.present,
		le.kind, le.summary, le.created_at, coalesce(ae.created_at, ''), coalesce(own.created_at, ''),
		ms.id, ms.kind, ms.summary, ms.created_at, coalesce(json_extract(ms.data, '$.key'), ''),
		coalesce(json_extract(ms.data, '$.value'), ''), coalesce(json_extract(ms.data, '$.owner'), 0)
		from tree join tasks t on t.id = tree.id join heads h on h.root = tree.root
		left join launches l on l.id = t.current_launch_id
		left join events le on le.id = (select max(x.id) from events x where x.task_id = t.id and x.kind not in `+laneReportSkip+`)
		left join events ae on ae.id = h.activity_id
		left join events own on t.parent_id is null and own.id = (select max(x.id) from events x where x.task_id = t.id)
		left join events ms on ms.id = h.milestone_id
		where t.status != 'closed'
		order by tree.root, (t.id = tree.root) desc, t.id`, stamp(at))
	if err != nil {
		return err
	}
	for rows.Next() {
		t := &glanceTask{}
		var activity, ownActivity, key, value string
		var present sql.NullBool
		var kind, summary, since, msKind, msText, msAt sql.NullString
		var msID sql.NullInt64
		var owner bool
		if err := rows.Scan(&t.rootID, &t.ID, &t.ParentID, &t.Name, &t.Role, &t.Status, &t.PaneID, &t.host, &t.launchHost,
			&t.Waiting, &t.AgentStatus, &t.ObservedAt, &present, &kind, &summary, &since, &activity, &ownActivity,
			&msID, &msKind, &msText, &msAt, &key, &value, &owner); err != nil {
			rows.Close()
			return err
		}
		if t.ParentID == 0 {
			lead := "quiet"
			if t.Waiting {
				lead = "waiting"
			} else if ownActivity != "" && glanceAge(at, ownActivity) < (10*time.Minute).Milliseconds() {
				lead = "reported"
			}
			roots[t.ID] = &glanceRoot{glanceCampaign: glanceCampaign{ID: t.ID, Name: t.Name, Host: t.host, PaneID: t.PaneID, Lead: lead}}
		}
		r := roots[t.rootID]
		if activity > r.activity {
			r.activity = activity
		}
		if msID.Valid && msID.Int64 > r.lastID {
			text := msText.String
			if msKind.String == "ref" {
				text = key + " " + value
			} else if msKind.String == "note" && owner {
				if val, ok := glanceOwnerValue(text); ok {
					text = val
				}
			}
			r.lastID = msID.Int64
			r.Last = &glanceLast{Kind: msKind.String, Text: clip(oneLine(text), 120), AgeMS: glanceAge(at, msAt.String)}
		}
		if present.Valid {
			t.Present = &present.Bool
		}
		if kind.Valid {
			t.LastEvent = &lastEvent{Kind: kind.String, Summary: clip(summary.String, stateSummaryMax), At: since.String, AgeMS: glanceAge(at, since.String)}
		}
		t.Mark = laneMark(&t.stateTask)
		// Freshness is read after closing rows, so the transaction needs no
		// concurrent statement while this result is being scanned.
		if t.launchHost != "" {
			hosts[t.launchHost] = false
		}
		if t.ParentID == 0 && t.host != "" {
			hosts[t.host] = false
		}
		tasks[t.ID] = t
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return err
	}
	for host := range hosts {
		fresh, err := hostFresh(tx, host)
		if err != nil {
			return err
		}
		hosts[host] = fresh
	}
	for _, t := range tasks {
		if t.ParentID == 0 {
			continue
		}
		if t.launchHost != "" && !hosts[t.launchHost] && (t.Status == "open" || t.Status == "ready") {
			t.Mark = "unknown"
		}
		r := roots[t.rootID]
		r.Lanes.Open++
		switch t.Mark {
		case markWorking:
			r.Lanes.Working++
		case markReady:
			r.Lanes.Ready++
		}
		// A failed lane alone is historical backlog once its tree goes quiet.
		switch t.Mark {
		case markWorking, markReady, markBlocked, markMissing, "unknown":
			r.active = true
		}
	}
	return nil
}

func glanceOwnerAsks(tx *sql.Tx, at time.Time, v *glanceView, roots map[int64]*glanceRoot, tasks map[int64]*glanceTask, asked map[int64]bool) error {
	rows, err := tx.Query(glanceTrees + `select e.id, e.task_id, coalesce(e.summary, ''), e.created_at,
		coalesce(json_extract(e.data, '$.blocking'), 0)
		from tree join tasks t on t.id = tree.id join events e on e.task_id = t.id
		where t.status != 'closed' and e.kind = 'ask' and e.answered_by is null and json_extract(e.data, '$.owner') = 1
		order by e.id`)
	if err != nil {
		return err
	}
	for rows.Next() {
		var n glanceNeed
		var taskID int64
		var blocking bool
		if err := rows.Scan(&n.AskID, &taskID, &n.Text, &n.Since, &blocking); err != nil {
			rows.Close()
			return err
		}
		t := tasks[taskID]
		r := roots[t.rootID]
		n.Kind, n.Campaign, n.RootID, n.Host, n.PaneID = "owner_ask", r.Name, r.ID, r.Host, r.PaneID
		n.AgeMS, n.Blocking, n.AskerWaiting = glanceAge(at, n.Since), &blocking, &t.Waiting
		if t.ParentID != 0 {
			n.Asker = t.Name
			if !asked[t.ID] {
				switch t.Mark {
				case markFailed, markBlocked, markMissing:
					n.Also = []string{"lane " + t.Mark}
				}
			}
			asked[t.ID] = true
		}
		r.active = true
		v.NeedsYou = append(v.NeedsYou, n)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return err
	}
	return nil
}

func glanceOwnerTodos(tx *sql.Tx, at time.Time, v *glanceView, roots map[int64]*glanceRoot) error {
	rows, err := tx.Query(`select t.id, e.id, coalesce(e.summary, ''), e.created_at from tasks t
		join events e on e.task_id = t.id
		where t.parent_id is null and t.status != 'closed' and e.kind = 'note'
			and json_extract(e.data, '$.owner') = 1 order by t.id, e.id desc`)
	if err != nil {
		return err
	}
	// ponytail: scan owner notes once; index qualifying notes if history dominates.
	seen := map[int64]bool{}
	first := len(v.NeedsYou)
	for rows.Next() {
		var rootID, noteID int64
		var text, since string
		if err := rows.Scan(&rootID, &noteID, &text, &since); err != nil {
			rows.Close()
			return err
		}
		if seen[rootID] || !strings.HasPrefix(strings.TrimSpace(text), "OWNER:") {
			continue
		}
		seen[rootID] = true
		if items := glanceOwnerItems(text); len(items) != 0 {
			r := roots[rootID]
			r.active = true
			v.NeedsYou = append(v.NeedsYou, glanceNeed{Kind: "owner_todo", Campaign: r.Name, RootID: rootID,
				Host: r.Host, PaneID: r.PaneID, NoteID: noteID, Items: items, Since: since, AgeMS: glanceAge(at, since)})
		}
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return err
	}
	todos := v.NeedsYou[first:]
	sort.Slice(todos, func(i, j int) bool { return todos[i].NoteID < todos[j].NoteID })
	return nil
}

func glanceBacklog(tx *sql.Tx, at time.Time, v *glanceView, roots map[int64]*glanceRoot, tasks map[int64]*glanceTask, hosts map[string]bool, asked map[int64]bool) error {
	add := func(a glanceAttention) {
		a.Text, a.AgeMS = clip(oneLine(a.Text), attentionTextMax), glanceAge(at, a.Since)
		v.Attention = append(v.Attention, a)
	}
	for _, t := range tasks {
		if t.ParentID == 0 {
			continue
		}
		r := roots[t.rootID]
		a := glanceAttention{Campaign: r.Name, RootID: r.ID, Lane: t.Name, LaneID: t.ID, Host: t.host, PaneID: t.PaneID}
		last := ""
		if t.LastEvent != nil {
			last = t.LastEvent.Summary
			if t.LastEvent.Kind == "ask" {
				last = "it asked a question"
			}
		}
		switch t.Mark {
		case markFailed:
			a.Kind, a.Text = "lane_failed", firstNonEmpty(last, "the lane reported fail")
			if t.LastEvent != nil {
				a.Since = t.LastEvent.At
			}
		case markBlocked:
			a.Kind, a.Text, a.Since = "lane_blocked", "Herdr sees an approval or question dialog in its pane", t.ObservedAt
			if last != "" {
				a.Text += "; last: " + last
			}
		case markMissing:
			a.Kind, a.Text, a.Since = "lane_missing", "pane "+firstNonEmpty(t.PaneID, "?")+" is gone; last: "+firstNonEmpty(last, "nothing reported"), t.ObservedAt
		case "unknown":
			a.Kind, a.Text, a.Since = "lane_unknown", "the lane's host has not reported", t.ObservedAt
		}
		if a.Kind != "" && (!asked[t.ID] || a.Kind == "lane_unknown") {
			add(a)
		}
	}
	// Filter observations before joining recipients and their scope.
	// ponytail: one history scan; use recipient-index ranges if history dominates.
	rows, err := tx.Query(glanceTrees+`, signals as materialized (
		select e.id, e.recipient_task_id, e.created_at from events e where e.created_at < ? and (
			e.kind in ('ready', 'done', 'fail') or
			(e.kind = 'prompt_outcome' and json_extract(e.data, '$.outcome') = 'no_receipt' and
				e.launch_id is (select current_launch_id from tasks where id = e.task_id)) or
			(e.kind = 'herdr' and (json_extract(e.data, '$.quota') = 'limit' or
				(json_extract(e.data, '$.reason') = 'stall' and
					e.launch_id is (select current_launch_id from tasks where id = e.task_id)))))
	), backlog as (
		select r.id as recipient, count(*) as n, min(e.created_at) as since, max(e.id) as newest
		from signals e join tasks r on r.id = e.recipient_task_id join tree on tree.id = r.id
		where r.status != 'closed' and e.id > r.acked_event_id
		group by r.id)
		select b.recipient, b.n, b.since, sender.name || ' ' || e.kind || ': ' || coalesce(e.summary, '')
		from backlog b join events e on e.id = b.newest join tasks sender on sender.id = e.task_id
		order by b.recipient`, stamp(at.Add(-glanceResultsAfter)))
	if err != nil {
		return err
	}
	for rows.Next() {
		var a glanceAttention
		if err := rows.Scan(&a.RecipientID, &a.Count, &a.Since, &a.Text); err != nil {
			rows.Close()
			return err
		}
		t := tasks[a.RecipientID]
		r := roots[t.rootID]
		a.Kind, a.Campaign, a.RootID = "results_waiting", r.Name, r.ID
		a.Recipient, a.Waiting, a.Host, a.PaneID = t.Name, &t.Waiting, t.host, t.PaneID
		add(a)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return err
	}
	for host, fresh := range hosts {
		if !fresh {
			hb, _, err := getMeta(tx, hostHeartbeatKey(host))
			if err != nil {
				return err
			}
			add(glanceAttention{Kind: "host_stale", Host: host, Since: hb, Text: host + " daemon has not reported; its lanes show unknown"})
		}
	}
	state, hb, _, err := daemonState(tx)
	if err != nil {
		return err
	}
	if state != "fresh" {
		text := "the taskr daemon has no live Herdr connection; no heartbeat"
		if state == "stale" {
			text = "the taskr daemon's heartbeat stopped; Herdr events are not arriving"
		}
		add(glanceAttention{Kind: "daemon_unhealthy", Text: text, Since: hb})
	}
	return nil
}

func finishGlance(_ *sql.Tx, at time.Time, v *glanceView, roots map[int64]*glanceRoot) {
	ordered := make([]*glanceRoot, 0, len(roots))
	for _, r := range roots {
		r.ActivityAgeMS = glanceAge(at, r.activity)
		r.active = r.active || (r.activity != "" && r.ActivityAgeMS < glanceQuietAfter.Milliseconds())
		ordered = append(ordered, r)
	}
	sort.Slice(ordered, func(i, j int) bool {
		if ordered[i].activity != ordered[j].activity {
			return ordered[i].activity > ordered[j].activity
		}
		return ordered[i].ID < ordered[j].ID
	})
	backlog := map[int64]bool{}
	kept := v.Attention[:0]
	for _, a := range v.Attention {
		if a.RootID != 0 && !roots[a.RootID].active {
			backlog[a.RootID] = true
			continue
		}
		kept = append(kept, a)
	}
	v.Attention = kept
	for _, r := range ordered {
		if r.active {
			v.Campaigns = append(v.Campaigns, r.glanceCampaign)
		} else {
			v.Quiet.Count++
			v.Quiet.Names = append(v.Quiet.Names, r.Name)
			if backlog[r.ID] {
				v.Quiet.WithBacklog++
			}
		}
	}
	sort.SliceStable(v.NeedsYou, func(i, j int) bool { return v.NeedsYou[i].AgeMS > v.NeedsYou[j].AgeMS })
	sort.SliceStable(v.Attention, func(i, j int) bool {
		a, b := v.Attention[i], v.Attention[j]
		if glanceRank(a.Kind) != glanceRank(b.Kind) {
			return glanceRank(a.Kind) < glanceRank(b.Kind)
		}
		if a.AgeMS != b.AgeMS {
			return a.AgeMS > b.AgeMS
		}
		if a.LaneID != b.LaneID {
			return a.LaneID < b.LaneID
		}
		return a.Host < b.Host
	})
	for _, a := range v.Attention {
		switch a.Kind {
		case "lane_failed", "lane_blocked", "lane_missing", "results_waiting":
			v.Verdict = "attention"
		default:
			if v.Verdict == "rolling" {
				v.Verdict = "unknown"
			}
		}
	}
	if v.Quiet.WithBacklog > 0 {
		v.Verdict = "attention"
	}
	if len(v.NeedsYou) > 0 {
		v.Verdict = "needs_you"
	}
}

func glanceRank(kind string) int {
	switch kind {
	case "lane_unknown", "host_stale":
		kind = attentionStale
	case "results_waiting":
		kind = attentionWorkWaits
	}
	return attentionRank[kind]
}

var (
	glanceOwnerEnd = regexp.MustCompile(`(?m)(?: |^)(?:DONE|HAPPENED|NOW):`)
	glanceNothing  = regexp.MustCompile(`^nothing(\s+(yet|new|now))?\s*($|[.,;:(]|\s-)`)
)

func glanceOwnerValue(text string) (string, bool) {
	value, ok := strings.CutPrefix(strings.TrimSpace(text), "OWNER:")
	if !ok {
		return "", false
	}
	if loc := glanceOwnerEnd.FindStringIndex(value); loc != nil {
		value = value[:loc[0]]
	}
	return strings.TrimSpace(value), true
}

func glanceOwnerItems(text string) []string {
	value, ok := glanceOwnerValue(text)
	if !ok || glanceNothing.MatchString(strings.ToLower(value)) {
		return nil
	}
	items := []string{}
	for n := 1; ; n++ {
		boundary := `\s`
		if n == 1 {
			boundary = `(^|\s)`
		}
		loc := regexp.MustCompile(boundary + strconv.Itoa(n) + `[.)]`).FindStringIndex(value)
		if loc == nil {
			if n == 1 {
				return []string{value}
			}
			if item := strings.TrimSpace(value); item != "" {
				items = append(items, item)
			}
			return items
		}
		if item := strings.TrimSpace(value[:loc[0]]); item != "" {
			items = append(items, item)
		}
		value = value[loc[1]:]
	}
}
