package main

import (
	"context"
	"database/sql"
	"flag"
	"sort"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"
)

const (
	glanceResultsAfter = 30 * time.Minute
	glanceQuietAfter   = 12 * time.Hour
)

type glanceView struct {
	Now               string            `json:"now"`
	Verdict           string            `json:"verdict"`
	NeedsYou          []glanceNeed      `json:"needs_you"`
	Attention         []glanceAttention `json:"attention"`
	Campaigns         []glanceCampaign  `json:"campaigns"`
	Quiet             glanceQuiet       `json:"quiet"`
	OwnerNotesPending int               `json:"owner_notes_pending"`
	OwnerNoteRootIDs  []int64           `json:"owner_note_root_ids"`
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
}

type glanceAttention struct {
	Kind        string `json:"kind"`
	Campaign    string `json:"campaign,omitempty"`
	RootID      int64  `json:"root_id,omitempty"`
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
	LeadWaiting   bool        `json:"lead_waiting,omitempty"`
	Last          *glanceLast `json:"last,omitempty"`
	OwnerNote     *glanceLast `json:"owner_note,omitempty"`
	Parked        bool        `json:"parked,omitempty"`
	ParkedActive  bool        `json:"parked_active,omitempty"`
	ParkAgeMS     int64       `json:"park_age_ms,omitempty"`
	ActivityAgeMS int64       `json:"activity_age_ms"`
}

type glanceLanes struct {
	Working int `json:"working"`
	Ready   int `json:"ready"`
	Open    int `json:"open"`
}

type glanceLast struct {
	EventID int64  `json:"event_id"`
	Kind    string `json:"kind"`
	Text    string `json:"text"`
	AgeMS   int64  `json:"age_ms"`
}

type glanceQuiet struct {
	Count int      `json:"count"`
	Names []string `json:"names"`
}

func cmdGlance(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("glance", flag.ContinueOnError)
	watch := fs.Bool("watch", false, "live terminal view")
	every := fs.Duration("every", 5*time.Second, "refresh interval (1s to 5m)")
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		return nil, 0, err
	}
	givenEvery := false
	fs.Visit(func(f *flag.Flag) { givenEvery = givenEvery || f.Name == "every" })
	if *every < time.Second || *every > 5*time.Minute || givenEvery && !*watch {
		return nil, exitUsage, usageErr("--every requires --watch and must be between 1s and 5m")
	}
	if *watch {
		if c.rpc {
			return nil, exitUsage, usageErr("glance --watch runs on the invoking host")
		}
		return watchGlance(c, *every)
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
	active     bool
	activity   string
	lastID     int64
	activityID int64
	parkID     int64
	leadAt     string
}

func readGlance(db *sql.DB, at time.Time) (*glanceView, error) {
	tx, err := db.BeginTx(context.Background(), &sql.TxOptions{ReadOnly: true})
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()
	v := &glanceView{Now: stamp(at), Verdict: "rolling", NeedsYou: []glanceNeed{},
		Attention: []glanceAttention{}, Campaigns: []glanceCampaign{}, Quiet: glanceQuiet{Names: []string{}}, OwnerNoteRootIDs: []int64{}}
	roots := map[int64]*glanceRoot{}
	tasks := map[int64]*glanceTask{}
	hosts := map[string]bool{}
	asked := map[int64]bool{}
	if err := glanceTasks(tx, at, roots, tasks, hosts); err != nil {
		return nil, err
	}
	leads, err := leadObservations(tx)
	if err != nil {
		return nil, err
	}
	for id, r := range roots {
		r.Lead, r.leadAt = firstNonEmpty(leads[id].Status, "unknown"), leads[id].ObservedAt
	}
	if err := glanceOwnerAsks(tx, at, v, roots, tasks, asked); err != nil {
		return nil, err
	}
	if err := glanceOwnerNotes(tx, at, v, roots); err != nil {
		return nil, err
	}
	if err := glanceBacklog(tx, at, v, roots, tasks, hosts); err != nil {
		return nil, err
	}
	rows, err := tx.Query(`select t.id, e.id, e.created_at from tasks t
 join events e on e.id = (select max(id) from events where task_id = t.id
 and kind = 'ref' and json_extract(data, '$.key') = 'glance.state')
 where t.parent_id is null and t.status != 'closed' and json_extract(e.data, '$.value') = 'parked'`)
	if err != nil {
		return nil, err
	}
	for rows.Next() {
		var rootID, eventID int64
		var since string
		if err := rows.Scan(&rootID, &eventID, &since); err != nil {
			rows.Close()
			return nil, err
		}
		if r := roots[rootID]; r != nil {
			r.Parked, r.parkID, r.ParkAgeMS = true, eventID, glanceAge(at, since)
		}
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, err
	}
	finishGlance(at, v, roots)
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
		t.current_launch_id is not null, coalesce(t.waiting_until > ?, 0), coalesce(l.observed_status, ''), coalesce(l.observed_at, ''), l.present,
		le.kind, le.summary, le.created_at, coalesce(ae.created_at, (select created_at from tasks where id = tree.root), ''),
		h.activity_id, ms.id, ms.kind, ms.summary, ms.created_at, coalesce(json_extract(ms.data, '$.key'), ''),
		coalesce(json_extract(ms.data, '$.value'), ''), coalesce(json_extract(ms.data, '$.owner'), 0)
		from tree join tasks t on t.id = tree.id join heads h on h.root = tree.root
		left join launches l on l.id = t.current_launch_id
		left join events le on le.id = (select max(x.id) from events x where x.task_id = t.id and x.kind not in `+laneReportSkip+`)
		left join events ae on ae.id = h.activity_id
		left join events ms on ms.id = h.milestone_id
		where t.status != 'closed'
		order by tree.root, (t.id = tree.root) desc, t.id`, stamp(at))
	if err != nil {
		return err
	}
	for rows.Next() {
		t := &glanceTask{}
		var activity, key, value string
		var present sql.NullBool
		var kind, summary, since, msKind, msText, msAt sql.NullString
		var msID, activityID sql.NullInt64
		var owner, launched bool
		if err := rows.Scan(&t.rootID, &t.ID, &t.ParentID, &t.Name, &t.Role, &t.Status, &t.PaneID, &t.host, &t.launchHost,
			&launched, &t.Waiting, &t.AgentStatus, &t.ObservedAt, &present, &kind, &summary, &since, &activity,
			&activityID, &msID, &msKind, &msText, &msAt, &key, &value, &owner); err != nil {
			rows.Close()
			return err
		}
		if t.ParentID == 0 {
			roots[t.ID] = &glanceRoot{glanceCampaign: glanceCampaign{ID: t.ID, Name: t.Name, Host: t.host, PaneID: t.PaneID, LeadWaiting: t.Waiting}}
		}
		r := roots[t.rootID]
		r.activityID = max(r.activityID, activityID.Int64)
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
			r.Last = &glanceLast{EventID: msID.Int64, Kind: msKind.String, Text: clip(oneLine(text), 120), AgeMS: glanceAge(at, msAt.String)}
		}
		if present.Valid {
			t.Present = &present.Bool
		}
		if kind.Valid {
			t.LastEvent = &lastEvent{Kind: kind.String, Summary: clip(summary.String, stateSummaryMax), At: since.String, AgeMS: glanceAge(at, since.String)}
		}
		t.Mark = laneMark(&t.stateTask)
		if t.Role == "gate" && !launched && t.Mark == markWorking {
			t.Mark = markPlanned
		}
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

// Newest owner note is context indefinitely; legacy OWNER items only count
// toward migration when this root has no open owner ask.
func glanceOwnerNotes(tx *sql.Tx, at time.Time, v *glanceView, roots map[int64]*glanceRoot) error {
	rows, err := tx.Query(`select t.id, e.id, coalesce(e.summary, ''), e.created_at,
 coalesce((select summary from events where task_id = t.id and kind = 'note'
 and json_extract(data, '$.owner') = 1 and instr(summary, 'OWNER:') > 0 order by id desc limit 1), '')
 from tasks t join events e on e.id = (select max(id) from events where task_id = t.id and kind = 'note' and json_extract(data, '$.owner') = 1)
 where t.parent_id is null and t.status != 'closed' order by t.id`)
	if err != nil {
		return err
	}
	defer rows.Close()
	asked := map[int64]bool{}
	for _, n := range v.NeedsYou {
		asked[n.RootID] = true
	}
	for rows.Next() {
		var rootID, noteID int64
		var text, since, ownerText string
		if err := rows.Scan(&rootID, &noteID, &text, &since, &ownerText); err != nil {
			return err
		}
		r := roots[rootID]
		r.OwnerNote = &glanceLast{EventID: noteID, Kind: "note", Text: clip(oneLine(text), 120), AgeMS: glanceAge(at, since)}
		if ownerNoteHasItems(ownerText) && !asked[rootID] {
			v.OwnerNotesPending++
			v.OwnerNoteRootIDs = append(v.OwnerNoteRootIDs, rootID)
		}
	}
	return rows.Err()
}

func glanceBacklog(tx *sql.Tx, at time.Time, v *glanceView, roots map[int64]*glanceRoot, tasks map[int64]*glanceTask, hosts map[string]bool) error {
	add := func(a glanceAttention) {
		a.Text, a.AgeMS = clip(oneLine(a.Text), attentionTextMax), glanceAge(at, a.Since)
		v.Attention = append(v.Attention, a)
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
		where r.parent_id is null and r.status != 'closed' and e.id > r.acked_event_id
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
		if t.Waiting || (r.Lead != "idle" && r.Lead != "done") {
			continue
		}
		a.Kind, a.Campaign, a.RootID = "lead_idle_results", r.Name, r.ID
		a.Recipient, a.Waiting, a.Host, a.PaneID = t.Name, &t.Waiting, t.host, t.PaneID
		r.active = true
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

func finishGlance(at time.Time, v *glanceView, roots map[int64]*glanceRoot) {
	ordered := make([]*glanceRoot, 0, len(roots))
	for _, r := range roots {
		r.ActivityAgeMS = glanceAge(at, r.activity)
		r.active = r.active || (r.activity != "" && r.ActivityAgeMS < glanceQuietAfter.Milliseconds()) || (r.PaneID == "" && r.Lanes.Open > 0)
		ordered = append(ordered, r)
	}
	sort.Slice(ordered, func(i, j int) bool {
		if ordered[i].activity != ordered[j].activity {
			return ordered[i].activity > ordered[j].activity
		}
		return ordered[i].ID < ordered[j].ID
	})
	kept := v.Attention[:0]
	for _, a := range v.Attention {
		if a.RootID != 0 && (roots[a.RootID].Parked || !roots[a.RootID].active) {
			continue
		}
		kept = append(kept, a)
	}
	v.Attention = kept
	for _, r := range ordered {
		if r.Parked {
			r.ParkedActive = r.activityID > r.parkID
			if r.ParkedActive {
				v.Attention = append(v.Attention, glanceAttention{Kind: "parked_active", Campaign: r.Name, RootID: r.ID, Host: r.Host, PaneID: r.PaneID, Text: "parked but active", AgeMS: r.ActivityAgeMS, Since: r.activity})
			}
			v.Campaigns = append(v.Campaigns, r.glanceCampaign)
			continue
		}
		if r.PaneID == "" {
			r.Lead = "unregistered"
		}
		if r.active {
			a := glanceAttention{Campaign: r.Name, RootID: r.ID, PaneID: r.PaneID, Host: r.Host,
				Since: r.leadAt, AgeMS: glanceAge(at, r.leadAt)}
			switch r.Lead {
			case "gone":
				a.Kind, a.Text = "lead_gone", "lead pane "+r.PaneID+" is not in its host's agent list"
			case "blocked":
				a.Kind, a.Text = "lead_blocked", "Herdr sees an approval or question dialog in the lead's pane"
			case "unregistered":
				if r.Lanes.Open > 0 && r.ActivityAgeMS >= (2*time.Hour).Milliseconds() {
					a.Kind, a.Text, a.Since, a.AgeMS = "lead_unregistered_silent", "lead unregistered and silent", r.activity, r.ActivityAgeMS
				}
			case "unknown":
				a.Kind, a.Text = "lead_unknown", "lead liveness unknown: never observed or its host is not reporting"
			}
			if a.Kind != "" {
				v.Attention = append(v.Attention, a)
			}
			if r.LeadWaiting && r.PaneID != "" && (r.Lead == "idle" || r.Lead == "done" || r.Lead == "working") {
				r.Lead = "waiting"
			}
			v.Campaigns = append(v.Campaigns, r.glanceCampaign)
		} else {
			v.Quiet.Count++
			v.Quiet.Names = append(v.Quiet.Names, r.Name)
		}
	}
	sort.SliceStable(v.NeedsYou, func(i, j int) bool {
		a, b := v.NeedsYou[i], v.NeedsYou[j]
		ab := a.Kind == "owner_ask" && a.Blocking != nil && *a.Blocking
		bb := b.Kind == "owner_ask" && b.Blocking != nil && *b.Blocking
		if ab != bb {
			return ab
		}
		return a.AgeMS > b.AgeMS
	})
	sort.SliceStable(v.Attention, func(i, j int) bool {
		a, b := v.Attention[i], v.Attention[j]
		if glanceRank(a.Kind) != glanceRank(b.Kind) {
			return glanceRank(a.Kind) < glanceRank(b.Kind)
		}
		if a.AgeMS != b.AgeMS {
			return a.AgeMS > b.AgeMS
		}
		if a.RootID != b.RootID {
			return a.RootID < b.RootID
		}
		return a.Host < b.Host
	})
	for _, a := range v.Attention {
		switch a.Kind {
		case "lead_blocked", "lead_gone", "lead_idle_results", "parked_active", "lead_unregistered_silent":
			v.Verdict = "attention"
		default:
			if v.Verdict == "rolling" {
				v.Verdict = "unknown"
			}
		}
	}
	if len(v.NeedsYou) > 0 {
		v.Verdict = "needs_you"
	}
}

func glanceRank(kind string) int {
	switch kind {
	case "host_stale", "lead_unknown":
		kind = attentionStale
	case "lead_blocked":
		kind = attentionBlocked
	case "lead_gone":
		kind = attentionMissing
	case "lead_idle_results", "parked_active", "lead_unregistered_silent":
		kind = attentionWorkWaits
	}
	return attentionRank[kind]
}

// Labels are explicit segments, not an inferred prose action queue.
func glanceOwnerValue(text string) (string, bool) {
	for offset := 0; offset < len(text); {
		i := strings.Index(text[offset:], "OWNER:")
		if i < 0 {
			return "", false
		}
		i += offset
		previous, _ := utf8.DecodeLastRuneInString(text[:i])
		if i == 0 || unicode.IsSpace(previous) {
			value := text[i+len("OWNER:"):]
			for _, label := range []string{"DONE:", "HAPPENED:", "NOW:"} {
				for j := 0; j < len(value); {
					k := strings.Index(value[j:], label)
					if k < 0 {
						break
					}
					k += j
					previous, _ := utf8.DecodeLastRuneInString(value[:k])
					if k == 0 || unicode.IsSpace(previous) {
						value = value[:k]
						break
					}
					j = k + len(label)
				}
			}
			return strings.TrimSpace(value), true
		}
		offset = i + len("OWNER:")
	}
	return "", false
}

func ownerNoteHasItems(text string) bool {
	value, ok := glanceOwnerValue(text)
	if !ok {
		return false
	}
	value = strings.ToLower(value)
	for _, empty := range []string{"nothing", "nothing yet", "nothing new", "nothing now"} {
		if tail, ok := strings.CutPrefix(value, empty); ok {
			tail = strings.TrimSpace(tail)
			if tail == "" || strings.HasPrefix(tail, ".") || strings.HasPrefix(tail, "(") {
				return false
			}
		}
	}
	return true
}
