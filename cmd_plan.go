package main

import (
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"
)

// The plan an orchestrator used to keep in handoff files and state journals:
// each lane's next step, the owner's rules, reference ids, and a rendered
// handover a new session adopts. All of it is events on the ledger, so the
// log keeps the history and the latest one wins.

const (
	refKeysMax  = 20
	refValueMax = 200 // runes
)

var refKeyRe = regexp.MustCompile(`^[a-z][a-z0-9_.-]{0,31}$`)

// handoverNow is the render clock; tests pin it for golden output.
var handoverNow = time.Now

// openPlanTask loads a task an orchestrator annotates: any task but a closed one.
func openPlanTask(tx *sql.Tx, id int64) (*task, error) {
	t, err := loadTask(tx, id)
	if err != nil {
		return nil, err
	}
	if t.Status == "closed" {
		return nil, rejectErr("task %d is closed", id)
	}
	return t, nil
}

func cmdNext(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("next", flag.ContinueOnError)
	clr := fs.Bool("clear", false, "clear the lane's next step")
	pos, err := parseArgs(c, fs, args, 1, 2)
	if err != nil {
		return nil, 0, err
	}
	id, err := parseID(pos[0], "task id")
	if err != nil {
		return nil, 0, err
	}
	var text string
	if len(pos) == 2 {
		text = strings.TrimSpace(pos[1])
	}
	switch {
	case *clr && len(pos) == 2:
		return nil, 0, usageErr("next: give TEXT or --clear, not both")
	case !*clr && text == "":
		return nil, 0, usageErr("next needs TEXT or --clear")
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	out := map[string]any{"ok": true, "task_id": id, "next": nil}
	err = withTx(db, func(tx *sql.Tx) error {
		if _, err := openPlanTask(tx, id); err != nil {
			return err
		}
		e := event{TaskID: id, Kind: "next", Summary: text}
		if *clr {
			e.Data = map[string]any{"clear": true}
		} else {
			out["next"] = text
		}
		eid, err := c.insertEvent(tx, e)
		out["event_id"] = eid
		return err
	})
	if err != nil {
		return nil, 0, err
	}
	return out, exitOK, nil
}

// planNext is a task's current next step.
type planNext struct {
	EventID  int64
	Text, At string
}

// taskNext returns the latest next event of a task, or nil when there is
// none or the latest one cleared it.
func taskNext(q queryer, id int64) (*planNext, error) {
	var n planNext
	var clear bool
	err := q.QueryRow(`select id, coalesce(summary, ''), created_at, coalesce(json_extract(data, '$.clear'), 0)
		from events where task_id = ? and kind = 'next' order by id desc limit 1`, id).Scan(&n.EventID, &n.Text, &n.At, &clear)
	if errors.Is(err, sql.ErrNoRows) || (err == nil && clear) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return &n, nil
}

type planRef struct {
	Key   string `json:"key"`
	Value string `json:"value"`
}

// taskRefs returns a task's references, latest value per key, by key; a key
// whose latest value is empty was deleted.
func taskRefs(q queryer, id int64) ([]planRef, error) {
	rows, err := q.Query(`select json_extract(data, '$.key'), coalesce(json_extract(data, '$.value'), '') from events
		where id in (select max(id) from events where task_id = ? and kind = 'ref' group by json_extract(data, '$.key'))
		order by 1`, id)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var refs []planRef
	for rows.Next() {
		var r planRef
		if err := rows.Scan(&r.Key, &r.Value); err != nil {
			return nil, err
		}
		if r.Value != "" {
			refs = append(refs, r)
		}
	}
	return refs, rows.Err()
}

// checkRefValue refuses a value that is too long or holds a control
// character: references are ids and one-line pointers.
func checkRefValue(k, v string) error {
	if utf8.RuneCountInString(v) > refValueMax {
		return usageErr("set: the value of %s is longer than %d characters", k, refValueMax)
	}
	for _, r := range v {
		if unicode.IsControl(r) {
			return usageErr("set: the value of %s holds a control character; references are one line", k)
		}
	}
	return nil
}

// cmdSet records references as ref events, one per changed key: the log
// keeps every value a key had, and the latest wins without a new table.
func cmdSet(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("set", flag.ContinueOnError)
	pos, err := parseArgs(c, fs, args, 2, 1+2*refKeysMax)
	if err != nil {
		return nil, 0, err
	}
	id, err := parseID(pos[0], "task id")
	if err != nil {
		return nil, 0, err
	}
	var keys []string
	vals := map[string]string{}
	for _, kv := range pos[1:] {
		k, v, ok := strings.Cut(kv, "=")
		if !ok {
			return nil, 0, usageErr("set wants KEY=VALUE (KEY= deletes), got %q", kv)
		}
		if !refKeyRe.MatchString(k) {
			return nil, 0, usageErr("set: key %q must match [a-z][a-z0-9_.-]{0,31}", k)
		}
		if _, dup := vals[k]; dup {
			return nil, 0, usageErr("set: key %s given twice", k)
		}
		v = strings.TrimSpace(v)
		if err := checkRefValue(k, v); err != nil {
			return nil, 0, err
		}
		keys, vals[k] = append(keys, k), v
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	out := map[string]any{"ok": true, "task_id": id}
	err = withTx(db, func(tx *sql.Tx) error {
		if _, err := openPlanTask(tx, id); err != nil {
			return err
		}
		cur, err := taskRefs(tx, id)
		if err != nil {
			return err
		}
		have := map[string]string{}
		for _, r := range cur {
			have[r.Key] = r.Value
		}
		var changed []string
		for _, k := range keys {
			if have[k] == vals[k] {
				continue
			}
			changed = append(changed, k)
			if vals[k] == "" {
				delete(have, k)
			} else {
				have[k] = vals[k]
			}
		}
		if len(have) > refKeysMax {
			return rejectErr("task %d would have %d references; at most %d (delete one with KEY=)", id, len(have), refKeysMax)
		}
		ids := []int64{}
		for _, k := range changed {
			summary := k + "=" + vals[k]
			eid, err := insertEvent(tx, event{TaskID: id, Kind: "ref", Summary: summary,
				Data: map[string]any{"key": k, "value": vals[k]}})
			if err != nil {
				return err
			}
			ids = append(ids, eid)
		}
		out["event_ids"], out["refs"] = ids, have
		return nil
	})
	if err != nil {
		return nil, 0, err
	}
	return out, exitOK, nil
}

func cmdDecide(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("decide", flag.ContinueOnError)
	as := fs.Int64("as", 0, "the root orchestrator's own task id")
	revoke := fs.Int64("revoke", 0, "retire this decision (or answered owner ask) event")
	pos, err := parseArgs(c, fs, args, 0, 1)
	if err != nil {
		return nil, 0, err
	}
	if *as <= 0 {
		return nil, 0, usageErr("decide needs --as ROOT_TASK_ID")
	}
	var text string
	if len(pos) == 1 {
		text = strings.TrimSpace(pos[0])
	}
	if (*revoke != 0) == (text != "") {
		return nil, 0, usageErr("decide needs exactly one of TEXT and --revoke EVENT_ID")
	}
	if *revoke < 0 {
		return nil, 0, usageErr("--revoke must be a positive event id")
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	out := map[string]any{"ok": true}
	err = withTx(db, func(tx *sql.Tx) error {
		w, err := resolveWriter(c, tx, *as)
		if err != nil {
			return err
		}
		root := w.task.ID
		out["task_id"] = root
		if *revoke == 0 {
			eid, err := c.insertEvent(tx, event{TaskID: root, Kind: "decision", Summary: text})
			out["event_id"], out["kind"], out["decision_id"] = eid, "decision", eid
			return err
		}
		ds, err := decisionsInForce(tx, root)
		if err != nil {
			return err
		}
		var target *planDecision
		for i := range ds {
			if ds[i].ID == *revoke {
				target = &ds[i]
			}
		}
		if target == nil {
			var by int64
			err := tx.QueryRow(`select id from events where kind = 'revoke' and related_event_id = ? and task_id = ?`, *revoke, root).Scan(&by)
			if err == nil {
				return rejectErr("event %d is already revoked by event %d", *revoke, by)
			} else if !errors.Is(err, sql.ErrNoRows) {
				return err
			}
			return rejectErr("event %d is not a decision in force under task %d", *revoke, root)
		}
		eid, err := c.insertEvent(tx, event{TaskID: root, Kind: "revoke", Summary: clip(target.Text, stateSummaryMax),
			RelatedEventID: ptr(*revoke), Data: map[string]any{"revoked": *revoke, "revoked_kind": target.Kind}})
		out["event_id"], out["kind"], out["revoked"] = eid, "revoke", *revoke
		return err
	})
	if err != nil {
		return nil, 0, err
	}
	return out, exitOK, nil
}

// planDecision is a rule in force under a root: a decide event, or an
// answered owner ask anywhere in its tree (read in place, never copied).
type planDecision struct {
	ID       int64  `json:"id"`
	Kind     string `json:"kind"` // decision | owner_ask
	Text     string `json:"text"`
	Answer   string `json:"answer,omitempty"`
	AnswerID int64  `json:"answer_id,omitempty"`
	From     string `json:"from,omitempty"` // the asker's name, for an owner ask
	At       string `json:"at"`
	AgeMS    int64  `json:"age_ms"`
}

// decisionsInForce lists a root's decisions and answered owner asks that no
// revoke event retired, oldest first.
func decisionsInForce(q queryer, root int64) ([]planDecision, error) {
	rows, err := q.Query(`with recursive sub(id) as (select ? union all select t.id from tasks t join sub on t.parent_id = sub.id)
		select e.id, e.kind, coalesce(e.summary, ''), coalesce(a.created_at, e.created_at), t.name,
		coalesce(a.summary, ''), coalesce(a.id, 0)
		from events e join tasks t on t.id = e.task_id left join events a on a.id = e.answered_by
		where ((e.kind = 'decision' and e.task_id = ?)
			or (e.kind = 'ask' and e.answered_by is not null and json_extract(e.data, '$.owner') = 1
				and e.task_id in (select id from sub)))
		and not exists (select 1 from events r where r.kind = 'revoke' and r.related_event_id = e.id and r.task_id = ?)
		order by e.id`, root, root, root)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var ds []planDecision
	for rows.Next() {
		var d planDecision
		var kind, from string
		if err := rows.Scan(&d.ID, &kind, &d.Text, &d.At, &from, &d.Answer, &d.AnswerID); err != nil {
			return nil, err
		}
		d.Kind = "decision"
		if kind == "ask" {
			d.Kind, d.From = "owner_ask", from
		}
		ds = append(ds, d)
	}
	return ds, rows.Err()
}

// ---- Handover ----------------------------------------------------------

type handoverLane struct {
	ID, ParentID        int64
	Depth               int
	Name, Role, Status  string
	Parent              string
	Agent, Launch, Pane string
	Report              string
	ObsStatus, ObsAt    string
	Present             sql.NullBool
	ClosedAt            string
	ClosedEvent         int64  // the task's `closed` event, 0 for a pre-v0.6 close
	Final               string // the closed lane's last done, fail or ready summary
	Next                *planNext
	Refs                []planRef
}

type handoverAsk struct {
	ID              int64
	Owner, Blocking bool
	From            string
	FromID          int64
	Text            string
}

type handoverMark struct {
	EventID  int64
	At, Note string
}

type handoverData struct {
	Documents             handoverDocuments
	Root                  handoverLane
	Workspace, Tab, Cwd   string
	Acked, Unacked        int64
	Pending               int64
	Decisions             []planDecision
	Live, Planned, Closed []handoverLane
	Asks                  []handoverAsk
	Since                 *handoverMark // the handover before this one, or nil
	Latest                *handoverMark // adopt: the handover being re-rendered
	Note                  string
	Adopted               bool
}

// loadHandover reads everything a handover shows. since is the handover the
// closed-lanes list counts from (nil: every closed lane).
func loadHandover(q queryer, root int64, since *handoverMark) (*handoverData, error) {
	d := &handoverData{Since: since, Documents: loadHandoverDocuments(q, root)}
	var ws, tab, cwd, pane sql.NullString
	var pending sql.NullInt64
	err := q.QueryRow(`select id, name, role, status, workspace_id, tab_id, pane_id, cwd, acked_event_id, pending_event_id,
		(select count(*) from events where recipient_task_id = tasks.id and id > tasks.acked_event_id)
		from tasks where id = ?`, root).Scan(&d.Root.ID, &d.Root.Name, &d.Root.Role, &d.Root.Status, &ws, &tab, &pane, &cwd,
		&d.Acked, &pending, &d.Unacked)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, rejectErr("task %d does not exist", root)
	} else if err != nil {
		return nil, err
	}
	d.Workspace, d.Tab, d.Root.Pane, d.Cwd, d.Pending = ws.String, tab.String, pane.String, cwd.String, pending.Int64
	if d.Root.Next, err = taskNext(q, root); err != nil {
		return nil, err
	}
	if d.Root.Refs, err = taskRefs(q, root); err != nil {
		return nil, err
	}
	if d.Decisions, err = decisionsInForce(q, root); err != nil {
		return nil, err
	}
	rows, err := q.Query(`with recursive tree(id, depth, path) as (
			select id, 0, printf('%012d', id) from tasks where id = ?
			union all select t.id, tree.depth + 1, tree.path || '/' || printf('%012d', t.id)
			from tasks t join tree on t.parent_id = tree.id)
		select t.id, coalesce(t.parent_id, 0), tree.depth, t.name, t.role, t.status, coalesce(p.name, ''),
		coalesce(t.agent_name, ''), coalesce(l.provider || '/' || l.model || '/' || l.effort, ''),
		coalesce(l.pane_id, t.pane_id, ''), coalesce(t.report_path, ''),
		coalesce(l.observed_status, ''), coalesce(l.observed_at, ''), l.present, coalesce(t.closed_at, ''),
		coalesce((select max(c.id) from events c where c.task_id = t.id and c.kind = 'closed'), 0),
		coalesce((select f.summary from events f where f.task_id = t.id and f.kind in ('done', 'fail', 'ready')
			order by f.id desc limit 1), '')
		from tree join tasks t on t.id = tree.id left join tasks p on p.id = t.parent_id
		left join launches l on l.id = t.current_launch_id
		where tree.depth > 0 order by tree.path`, root)
	if err != nil {
		return nil, err
	}
	var all []handoverLane
	for rows.Next() {
		var l handoverLane
		if err := rows.Scan(&l.ID, &l.ParentID, &l.Depth, &l.Name, &l.Role, &l.Status, &l.Parent, &l.Agent, &l.Launch,
			&l.Pane, &l.Report, &l.ObsStatus, &l.ObsAt, &l.Present, &l.ClosedAt, &l.ClosedEvent, &l.Final); err != nil {
			rows.Close()
			return nil, err
		}
		all = append(all, l)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, err
	}
	for _, l := range all {
		switch {
		case l.Status == "closed":
			// Event order, not clocks: a close after the previous handover
			// has a later event id however the timestamps compare. A close
			// with no event predates v0.6 and so every v0.6 handover.
			if since == nil || l.ClosedEvent > since.EventID {
				d.Closed = append(d.Closed, l)
			}
			continue
		case l.Status == "planned":
			d.Planned = append(d.Planned, l)
		default:
			d.Live = append(d.Live, l)
		}
	}
	for _, list := range [][]handoverLane{d.Live, d.Planned} {
		for i := range list {
			l := &list[i]
			if l.Next, err = taskNext(q, l.ID); err != nil {
				return nil, err
			}
			if l.Refs, err = taskRefs(q, l.ID); err != nil {
				return nil, err
			}
		}
	}
	sort.SliceStable(d.Closed, func(i, j int) bool {
		if d.Closed[i].ClosedEvent != d.Closed[j].ClosedEvent {
			return d.Closed[i].ClosedEvent < d.Closed[j].ClosedEvent
		}
		return d.Closed[i].ID < d.Closed[j].ID
	})
	// Open asks of the tree's non-closed tasks: owner asks first, then by id.
	arows, err := q.Query(`with recursive sub(id) as (select ? union all select t.id from tasks t join sub on t.parent_id = sub.id)
		select e.id, coalesce(json_extract(e.data, '$.owner'), 0), coalesce(json_extract(e.data, '$.blocking'), 0),
		t.name, t.id, coalesce(e.summary, '')
		from events e join tasks t on t.id = e.task_id
		where e.kind = 'ask' and e.answered_by is null and t.status != 'closed' and e.task_id in (select id from sub)
		order by coalesce(json_extract(e.data, '$.owner'), 0) desc, e.id`, root)
	if err != nil {
		return nil, err
	}
	defer arows.Close()
	for arows.Next() {
		var a handoverAsk
		if err := arows.Scan(&a.ID, &a.Owner, &a.Blocking, &a.From, &a.FromID, &a.Text); err != nil {
			return nil, err
		}
		d.Asks = append(d.Asks, a)
	}
	return d, arows.Err()
}

// handoverMarks returns the root's handover events, newest first, at most n.
func handoverMarks(q queryer, root int64, n int) ([]handoverMark, error) {
	rows, err := q.Query(`select id, created_at, coalesce(summary, '') from events where task_id = ? and kind = 'handover'
		order by id desc limit ?`, root, n)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var ms []handoverMark
	for rows.Next() {
		var m handoverMark
		if err := rows.Scan(&m.EventID, &m.At, &m.Note); err != nil {
			return nil, err
		}
		ms = append(ms, m)
	}
	return ms, rows.Err()
}

// ageText is a coarse age of ts at now, like the dashboard's.
func ageText(now time.Time, ts string) string {
	t := parseTime(ts)
	if t.IsZero() {
		return ""
	}
	s := int64(max(0, now.Sub(t)).Seconds())
	switch {
	case s < 60:
		return fmt.Sprintf("%ds ago", s)
	case s < 3600:
		return fmt.Sprintf("%dm ago", s/60)
	case s < 48*3600:
		return fmt.Sprintf("%dh ago", s/3600)
	}
	return fmt.Sprintf("%dd ago", s/86400)
}

// mdEscaper makes ledger text literal Markdown in one pass (so a backslash
// already in the text is escaped before, and never by, the escapes it adds):
// Markdown punctuation gets a backslash, HTML metacharacters become entities.
var mdEscaper = strings.NewReplacer(`\`, `\\`, "`", "\\`", `*`, `\*`, `_`, `\_`, `{`, `\{`, `}`, `\}`,
	`[`, `\[`, `]`, `\]`, `(`, `\(`, `)`, `\)`, `#`, `\#`, `+`, `\+`, `-`, `\-`, `.`, `\.`, `!`, `\!`, `|`, `\|`,
	`&`, `&amp;`, `<`, `&lt;`, `>`, `&gt;`)

// md is the one escaper for every ledger-derived string in a handover: it
// folds whitespace, newlines included, to single spaces and escapes the rest.
func md(s string) string {
	return mdEscaper.Replace(strings.Join(strings.Fields(s), " "))
}

// mdOr is md, or none when s is empty.
func mdOr(s, none string) string {
	if strings.TrimSpace(s) == "" {
		return none
	}
	return md(s)
}

// cell is one table cell: already-escaped text, or a dash when empty.
func cell(s string) string {
	if s == "" {
		return "–"
	}
	return s
}

func refsText(refs []planRef, sep string) string {
	var parts []string
	for _, r := range refs {
		parts = append(parts, md(r.Key)+"="+md(r.Value))
	}
	return strings.Join(parts, sep)
}

func laneLabel(l handoverLane) string {
	if l.Depth > 1 {
		return fmt.Sprintf("%s (task %d, under %s)", md(l.Name), l.ID, md(l.Parent))
	}
	return fmt.Sprintf("%s (task %d)", md(l.Name), l.ID)
}

func (l handoverLane) herdrText(now time.Time) string {
	switch {
	case l.ObsAt == "":
		return "not observed"
	case l.Present.Valid && !l.Present.Bool:
		return "pane gone, " + ageText(now, l.ObsAt)
	}
	return md(l.ObsStatus) + ", " + ageText(now, l.ObsAt)
}

func nextText(n *planNext, now time.Time) string {
	if n == nil {
		return ""
	}
	return md(n.Text) + " (" + ageText(now, n.At) + ")"
}

// renderHandover writes the Markdown a successor reads. Every list has a
// fixed order (tree order, ids, keys), so one ledger renders one text, and
// every string from the ledger goes through md.
func renderHandover(d *handoverData, now time.Time) string {
	var b strings.Builder
	p := func(format string, a ...any) { fmt.Fprintf(&b, format, a...) }
	r := d.Root
	p("# taskr handover: %s (task %d)\n\n", md(r.Name), r.ID)
	if d.Adopted && d.Latest != nil {
		p("Handover event %d (%s), re-rendered %s from the taskr ledger for the session that adopted it.\n",
			d.Latest.EventID, md(d.Latest.At), md(stamp(now)))
	} else if d.Adopted {
		p("No handover was recorded; rendered %s from the taskr ledger for the session that adopted it.\n", md(stamp(now)))
	} else {
		p("Rendered %s from the taskr ledger.\n", md(stamp(now)))
	}
	p("A new session takes over with `taskr adopt %d`, then `taskr wait --as %d`.\n\n", r.ID, r.ID)

	renderHandoverDocuments(&b, r.ID, d.Documents)
	p("## Identity\n\n")
	p("- Orchestrator: %s, task %d, %s, status %s\n", md(r.Name), r.ID, md(r.Role), md(r.Status))
	p("- Where: workspace %s, tab %s, pane %s\n", mdOr(d.Workspace, "none"), mdOr(d.Tab, "none"), mdOr(r.Pane, "none"))
	p("- Cwd: %s\n", mdOr(d.Cwd, "none"))
	inbox := fmt.Sprintf("acked through event %d, %d unacked", d.Acked, d.Unacked)
	if d.Pending != 0 {
		inbox += fmt.Sprintf(", event %d offered and not acked (the next wait replays it)", d.Pending)
	}
	p("- Inbox: %s\n", inbox)
	if r.Next != nil {
		p("- Next: %s\n", nextText(r.Next, now))
	} else {
		p("- Next: none\n")
	}
	p("- Refs: %s\n\n", orNone(refsText(r.Refs, ", ")))

	p("## Decisions in force\n\n")
	if len(d.Decisions) == 0 {
		p("None.\n")
	}
	for _, x := range d.Decisions {
		if x.Kind == "owner_ask" {
			p("- Owner answer to ask %d from %s (answer event %d): %s\n  Answer: %s\n", x.ID, md(x.From), x.AnswerID, md(x.Text), md(x.Answer))
		} else {
			p("- Decision %d (%s): %s\n", x.ID, md(x.At), md(x.Text))
		}
	}
	p("\n## Live lanes\n\n")
	if len(d.Live) == 0 {
		p("None.\n")
	} else {
		p("| Lane | Role | Agent | Provider/model/effort | Pane | Status | Herdr | Next | Refs | Report |\n")
		p("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n")
		for _, l := range d.Live {
			p("| %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |\n", cell(laneLabel(l)), cell(md(l.Role)), cell(md(l.Agent)),
				cell(md(l.Launch)), cell(md(l.Pane)), cell(md(l.Status)), cell(l.herdrText(now)), cell(nextText(l.Next, now)),
				cell(refsText(l.Refs, ", ")), cell(md(l.Report)))
		}
	}
	p("\n## Planned lanes\n\n")
	if len(d.Planned) == 0 {
		p("None.\n")
	}
	for _, l := range d.Planned {
		next := "no next step recorded"
		if l.Next != nil {
			next = "next: " + nextText(l.Next, now)
		}
		p("- %s, %s: %s", laneLabel(l), md(l.Role), next)
		if len(l.Refs) > 0 {
			p("; refs %s", refsText(l.Refs, ", "))
		}
		p("\n")
	}
	p("\n## Open asks\n\n")
	if len(d.Asks) == 0 {
		p("None.\n")
	}
	for _, a := range d.Asks {
		var tags []string
		if a.Owner {
			tags = append(tags, "owner")
		}
		if a.Blocking {
			tags = append(tags, "blocking")
		}
		kind := fmt.Sprintf("ask %d", a.ID)
		if len(tags) > 0 {
			kind += " (" + strings.Join(tags, ", ") + ")"
		}
		p("- %s from %s (task %d): %s\n", kind, md(a.From), a.FromID, md(a.Text))
	}
	if d.Since != nil {
		p("\n## Closed since the previous handover (event %d, %s)\n\n", d.Since.EventID, md(d.Since.At))
	} else {
		p("\n## Closed lanes (no earlier handover)\n\n")
	}
	if len(d.Closed) == 0 {
		p("None.\n")
	}
	for _, l := range d.Closed {
		p("- %s, %s, closed %s: %s", laneLabel(l), md(l.Role), md(l.ClosedAt), mdOr(l.Final, "no final summary"))
		if l.Report != "" {
			p(" (report %s)", md(l.Report))
		}
		p("\n")
	}
	if strings.TrimSpace(d.Note) != "" {
		p("\n## Note\n\n%s\n", md(d.Note))
	}
	return b.String()
}

func orNone(s string) string {
	if s == "" {
		return "none"
	}
	return s
}

func cmdHandover(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("handover", flag.ContinueOnError)
	as := fs.Int64("as", 0, "the root orchestrator's own task id")
	note := fs.String("note", "", "a note for the successor")
	outPath := fs.String("out", "", "also write the Markdown to this file")
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		return nil, 0, err
	}
	if *as <= 0 {
		return nil, 0, usageErr("handover needs --as ROOT_TASK_ID")
	}
	out := *outPath
	if strings.HasPrefix(out, "-") {
		return nil, 0, usageErr("--out must be a path that does not start with `-`, got %q", out)
	}
	if out != "" {
		abs, err := filepath.Abs(out)
		if err != nil {
			return nil, 0, usageErr("--out: %v", err)
		}
		out = abs
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	var text string
	var eid int64
	err = withTx(db, func(tx *sql.Tx) error {
		w, err := resolveWriter(c, tx, *as)
		if err != nil {
			return err
		}
		prev, err := handoverMarks(tx, w.task.ID, 1)
		if err != nil {
			return err
		}
		var since *handoverMark
		if len(prev) > 0 {
			since = &prev[0]
		}
		d, err := loadHandover(tx, w.task.ID, since)
		if err != nil {
			return err
		}
		d.Note = strings.TrimSpace(*note)
		text = renderHandover(d, handoverNow())
		if out != "" {
			if err := os.WriteFile(out, []byte(text), 0o644); err != nil {
				return usageErr("--out: %v", err)
			}
		}
		sum := sha256.Sum256([]byte(text))
		data := map[string]any{"sha256": hex.EncodeToString(sum[:]), "bytes": len(text), "counts": map[string]any{
			"live": len(d.Live), "planned": len(d.Planned), "decisions": len(d.Decisions),
			"open_asks": len(d.Asks), "closed_since": len(d.Closed)}}
		if out != "" {
			data["out"] = out
		}
		eid, err = insertEvent(tx, event{TaskID: w.task.ID, Kind: "handover", Summary: d.Note, Data: data})
		if err != nil {
			return err
		}
		return captureDocument(tx, func() error {
			in := bodyDocument([]byte(text), out)
			if in.Reason == "" {
				in.Format = "md"
			}
			_, _, err := storeDocument(tx, w.task.ID, "handover", "", in, ptr(eid), 0)
			return err
		})
	})
	if err != nil {
		return nil, 0, err
	}
	fmt.Fprint(c.out, text)
	if out != "" {
		fmt.Fprintf(c.errw, "taskr handover: recorded event %d; wrote %s\n", eid, out)
	} else {
		fmt.Fprintf(c.errw, "taskr handover: recorded event %d\n", eid)
	}
	c.lines = true
	return nil, exitOK, nil
}

// cmdAdopt rebinds a root orchestrator to the successor's pane and prints the
// latest handover re-rendered from the ledger as it is now. The inbox is left
// as it is, so the successor's wait resumes where the predecessor stopped.
func cmdAdopt(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("adopt", flag.ContinueOnError)
	ws := fs.String("workspace", "", "Herdr workspace id (default: HERDR_WORKSPACE_ID)")
	tab := fs.String("tab", "", "Herdr tab id (default: HERDR_TAB_ID)")
	pane := fs.String("pane", "", "Herdr pane id (default: HERDR_PANE_ID)")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	if err := validateLocationFlags(fs, *ws, *tab, *pane); err != nil {
		return nil, 0, err
	}
	id, err := parseID(pos[0], "task id")
	if err != nil {
		return nil, 0, err
	}
	nw := firstNonEmpty(*ws, c.env("HERDR_WORKSPACE_ID"))
	nt := firstNonEmpty(*tab, c.env("HERDR_TAB_ID"))
	np := firstNonEmpty(*pane, c.env("HERDR_PANE_ID"))
	for _, v := range []struct{ value, field string }{{nw, "--workspace"}, {nt, "--tab"}, {np, "--pane"}} {
		if v.value != "" {
			if err := validateHerdrID(v.value, v.field); err != nil {
				return nil, 0, err
			}
		}
	}
	if np == "" {
		return nil, 0, usageErr("adopt needs --pane or HERDR_PANE_ID matching [0-9A-Za-z:_-]+")
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	var text string
	var eid int64
	var oldPane string
	err = withTx(db, func(tx *sql.Tx) error {
		t, err := loadTask(tx, id)
		if err != nil {
			return err
		}
		switch {
		case t.Status == "closed":
			return rejectErr("task %d is closed", id)
		case t.ParentID.Valid:
			return rejectErr("adopt is for a root orchestrator: task %d has parent %d; its parent relaunches it", id, t.ParentID.Int64)
		case t.CurrentLaunchID.Valid:
			return rejectErr("adopt is for a root orchestrator: task %d has launch %d", id, t.CurrentLaunchID.Int64)
		}
		var ow, ot, op sql.NullString
		if err := tx.QueryRow(`select workspace_id, tab_id, pane_id from tasks where id = ?`, id).Scan(&ow, &ot, &op); err != nil {
			return err
		}
		oldPane = op.String
		// The predecessor's wait is gone with its session: its waiting_until
		// marker no longer means anyone is waiting.
		// The root moves to the adopter's host, so its --as writes pass the host check there.
		if _, err := tx.Exec(`update tasks set workspace_id = ?, tab_id = ?, pane_id = ?, machine = ?, waiting_until = null, updated_at = ?
			where id = ?`, nullStr(nw), nullStr(nt), np, callerMachine(c), now(), id); err != nil {
			return err
		}
		loc := func(w, t, p string) map[string]any {
			m := map[string]any{}
			for k, v := range map[string]string{"workspace_id": w, "tab_id": t, "pane_id": p} {
				if v != "" {
					m[k] = v
				}
			}
			return m
		}
		eid, err = insertEvent(tx, event{TaskID: id, Kind: "adopt", Summary: "adopted at pane " + np,
			Data: map[string]any{"old": loc(ow.String, ot.String, op.String), "new": loc(nw, nt, np)}})
		if err != nil {
			return err
		}
		marks, err := handoverMarks(tx, id, 2)
		if err != nil {
			return err
		}
		var latest, since *handoverMark
		if len(marks) > 0 {
			latest = &marks[0]
		}
		if len(marks) > 1 {
			since = &marks[1]
		}
		d, err := loadHandover(tx, id, since)
		if err != nil {
			return err
		}
		d.Adopted, d.Latest = true, latest
		if latest != nil {
			d.Note = latest.Note
		}
		text = renderHandover(d, handoverNow())
		return nil
	})
	if err != nil {
		return nil, 0, err
	}
	fmt.Fprint(c.out, text)
	fmt.Fprintf(c.errw, "taskr adopt: task %d now at pane %s (was %s); event %d\n", id, np, orNone(oldPane), eid)
	c.lines = true
	return nil, exitOK, nil
}
