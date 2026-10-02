package main

import (
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"regexp"
	"strconv"
	"strings"
	"time"
)

type capacitySignal int

const (
	capacityUnknown capacitySignal = iota
	capacityWarning
	capacityActivity
	capacitySentence = "Selected model is at capacity. Please try a different model."
)

var (
	capacityANSI      = regexp.MustCompile("\x1b(?:\\[[0-?]*[ -/]*[@-~]|\\][^\x07\x1b]*(?:\x07|\x1b\\\\))")
	capacityFooterRE  = regexp.MustCompile(`^\s*\? for shortcuts(\s{2,}.*)?$`)
	capacityStatusRE  = regexp.MustCompile(`^  \S.* · .+$`)
	capacityWorkingRE = regexp.MustCompile(`^• Working \([0-9]+[smh](?: [0-9]+[smh])* • esc to interrupt\)$`)
	capacityPromptRE  = regexp.MustCompile(`^First taskr got ([1-9][0-9]*)(?:[.;])(?: |$)`)
)

type capacityTail struct {
	signal  capacitySignal
	attempt int64 // current submitted user cell's marker, never its text
}

// ponytail: terminal presentation heuristic, including forgeable native cells;
// replace with a provider-supported structured capacity event when available.
func capacityOf(text string) capacityTail {
	rows := strings.Split(strings.ReplaceAll(capacityANSI.ReplaceAllString(text, ""), "\r\n", "\n"), "\n")
	end := len(rows)
	for end > 0 && strings.TrimSpace(rows[end-1]) == "" {
		end--
	}
	// A composer suggestion is chrome only in the bottom footer position.
	// Arbitrary draft text there is ambiguous, not a submitted turn.
	if end > 0 && capacityFooterRE.MatchString(rows[end-1]) {
		end--
		for end > 0 && strings.TrimSpace(rows[end-1]) == "" {
			end--
		}
		if end > 0 && capacityStatusRE.MatchString(rows[end-1]) {
			statusEnd := end
			end--
			for end > 0 && strings.TrimSpace(rows[end-1]) == "" {
				end--
			}
			if end > 0 && capacityStatusRE.MatchString(rows[end-1]) {
				return capacityTail{}
			}
			if end == 0 || !strings.HasPrefix(strings.TrimSpace(rows[end-1]), "›") {
				end = statusEnd
			}
		}
		if end > 0 && strings.HasPrefix(strings.TrimSpace(rows[end-1]), "›") {
			composer := strings.TrimSpace(rows[end-1])
			if composer != "›" && composer != "› Summarize recent commits" && composer != "› Write tests for @filename" && composer != "› Explain this codebase" && composer != "› Implement {feature}" && composer != "› Find and fix a bug in @filename" && composer != "› Improve documentation in @filename" && composer != "› Ask Codex to do anything" {
				return capacityTail{}
			}
			end--
		}
	} else if end > 0 && strings.TrimSpace(rows[end-1]) == "›" {
		end--
	}
	var tail capacityTail
	fence := ""
	for i := 0; i < end; i++ {
		row, trimmed := rows[i], strings.TrimSpace(rows[i])
		if trimmed == "" {
			continue
		}
		if strings.HasPrefix(trimmed, "```") || strings.HasPrefix(trimmed, "~~~") {
			prefix := trimmed[:3]
			if fence == prefix {
				fence = ""
			} else if fence == "" {
				fence = prefix
			}
			if tail.signal == capacityWarning {
				tail.signal = capacityUnknown
			}
			continue
		}
		if fence != "" {
			continue
		}
		if strings.HasPrefix(row, "› ") {
			cell := strings.TrimPrefix(row, "› ")
			for i+1 < end && strings.HasPrefix(rows[i+1], "  ") && strings.TrimSpace(rows[i+1]) != "" {
				i++
				cell += " " + strings.TrimSpace(rows[i])
			}
			tail.attempt = 0
			if m := capacityPromptRE.FindStringSubmatch(strings.Join(strings.Fields(cell), " ")); m != nil {
				tail.attempt, _ = strconv.ParseInt(m[1], 10, 64)
			}
			if tail.signal == capacityWarning {
				tail.signal = capacityActivity
			}
			continue
		}
		if capacityWorkingRE.MatchString(trimmed) {
			continue
		} // metadata/chrome alone is not recovery
		if strings.HasPrefix(row, "■ ") {
			cell := strings.TrimPrefix(row, "■ ")
			for i+1 < end {
				next := strings.TrimSpace(rows[i+1])
				if next == "" || strings.HasPrefix(next, "›") || strings.HasPrefix(next, "•") || strings.HasPrefix(next, "■") || strings.HasPrefix(next, "└") {
					break
				}
				i++
				cell += " " + strings.TrimSpace(rows[i])
			}
			if strings.Join(strings.Fields(cell), " ") == capacitySentence {
				tail.signal = capacityWarning
			} else {
				tail.signal = capacityUnknown
			}
			continue
		}
		if strings.HasPrefix(row, "• ") {
			if tail.signal == capacityWarning {
				tail.signal = capacityActivity
			}
			continue
		}
		if tail.signal == capacityWarning {
			tail.signal = capacityUnknown
		}
	}
	return tail
}

type capacityAgent struct {
	Provider string `json:"agent"`
	Name     string `json:"name"`
	Pane     string `json:"pane_id"`
	Terminal string `json:"terminal_id"`
	Revision int64  `json:"revision"`
	Session  struct {
		Provider string `json:"agent"`
		Kind     string `json:"kind"`
		Value    string `json:"value"`
	} `json:"agent_session"`
}

type capacityBinding struct {
	Terminal    string `json:"terminal"`
	SessionHash string `json:"session_hash"`
	SocketHash  string `json:"socket_hash"`
}

type capacityState struct {
	Binding  capacityBinding `json:"binding"`
	Episode  int64           `json:"episode"`
	Active   bool            `json:"active"`
	Revision int64           `json:"revision"`
}

func capacityHash(value string) string {
	h := sha256.Sum256([]byte(value))
	return hex.EncodeToString(h[:])
}

// Verified Codex adapter boundary: taskr's CODEX_THREAD_ID is thread_id;
// current Herdr reports the same native value as id. No other kinds/providers.
func codexSessionKind(kind string) bool { return kind == "id" || kind == "thread_id" }

func capacityGet(sock, pane string, deadline time.Time) (capacityAgent, error) {
	var result struct {
		Agent capacityAgent `json:"agent"`
	}
	err := herdrCapacityRequest(sock, "agent.get", map[string]any{"target": pane}, deadline, &result)
	return result.Agent, err
}

type capacityChild struct {
	Task, Parent, Launch                                 int64
	Pane, TaskPane, Name, Model, SessionKind, SessionRef string
	Attempt                                              int64
}

const capacityChildrenSQL = `select t.id, t.parent_id, l.id, coalesce(l.pane_id, t.pane_id),
	coalesce(t.pane_id, ''), t.agent_name, coalesce(l.model, ''), coalesce(l.session_kind, ''), coalesce(l.session_ref, ''),
	coalesce((select max(e.id) from events e where e.task_id = t.id and e.launch_id = l.id and e.kind = 'prompt'), 0)
	from tasks t join launches l on l.id = t.current_launch_id
	where t.parent_id = ? and t.status = 'open' and t.role != 'gate' and l.provider = 'codex'
	and coalesce(l.pane_id, t.pane_id, '') != '' and coalesce(t.agent_name, '') != ''
	and (t.waiting_until is null or t.waiting_until <= ?) and l.machine is null`

func scanCapacityChild(row interface{ Scan(...any) error }) (capacityChild, error) {
	var w capacityChild
	err := row.Scan(&w.Task, &w.Parent, &w.Launch, &w.Pane, &w.TaskPane, &w.Name, &w.Model, &w.SessionKind, &w.SessionRef, &w.Attempt)
	return w, err
}

func capacityChildren(q queryer, parent int64) ([]capacityChild, error) {
	rows, err := q.Query(capacityChildrenSQL+` order by t.id`, parent, now())
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var ws []capacityChild
	for rows.Next() {
		w, err := scanCapacityChild(rows)
		if err != nil {
			return nil, err
		}
		ws = append(ws, w)
	}
	return ws, rows.Err()
}

func capacityStillCurrent(q queryer, w capacityChild) (bool, error) {
	got, err := scanCapacityChild(q.QueryRow(capacityChildrenSQL+` and t.id = ?`, w.Parent, now(), w.Task))
	if errors.Is(err, sql.ErrNoRows) {
		return false, nil
	}
	return got == w, err
}

func (a capacityAgent) binding(w capacityChild, sock string) (capacityBinding, bool) {
	if a.Provider != "codex" || a.Session.Provider != "codex" || a.Name != w.Name || a.Pane != w.Pane ||
		(w.TaskPane != "" && w.TaskPane != w.Pane) ||
		a.Terminal == "" || a.Session.Value == "" || !codexSessionKind(a.Session.Kind) {
		return capacityBinding{}, false
	}
	if (w.SessionRef != "" || w.SessionKind != "") && (!codexSessionKind(w.SessionKind) || w.SessionRef != a.Session.Value) {
		return capacityBinding{}, false
	}
	return capacityBinding{Terminal: a.Terminal, SessionHash: capacityHash(a.Session.Value), SocketHash: capacityHash(sock)}, true
}

func capacityKey(launch int64) string { return fmt.Sprintf("capacity:%d", launch) }

func loadCapacity(q queryer, launch int64) (capacityState, bool, error) {
	var state capacityState
	raw, found, err := getMeta(q, capacityKey(launch))
	if err != nil || !found {
		return state, found, err
	}
	if json.Unmarshal([]byte(raw), &state) != nil {
		return state, true, fmt.Errorf("invalid capacity ledger record")
	}
	return state, true, nil
}

func saveCapacity(tx *sql.Tx, launch int64, state capacityState) error {
	b, err := json.Marshal(state)
	if err != nil {
		return err
	}
	_, err = tx.Exec(`insert into meta (key, value) values (?, ?) on conflict(key) do update set value = excluded.value`, capacityKey(launch), string(b))
	return err
}

// Best-effort metadata before transport. An unavailable read does not change
// delivery/retry/receipt behavior, and an existing binding is never replaced.
func bindCapacityBeforePrompt(db *sql.DB, sock string, task, launch int64) {
	var parent sql.NullInt64
	if db.QueryRow(`select parent_id from tasks where id = ?`, task).Scan(&parent) != nil || !parent.Valid {
		return
	}
	w, err := scanCapacityChild(db.QueryRow(capacityChildrenSQL+` and t.id = ? and l.id = ?`, parent.Int64, now(), task, launch))
	if err != nil {
		return
	}
	a, err := capacityGet(sock, w.Pane, time.Now().Add(500*time.Millisecond))
	if err != nil {
		return
	}
	binding, ok := a.binding(w, sock)
	if !ok {
		return
	}
	_ = withTx(db, func(tx *sql.Tx) error {
		current, err := capacityStillCurrent(tx, w)
		if err != nil || !current {
			return err
		}
		_, found, err := loadCapacity(tx, launch)
		if err != nil || found {
			return err
		}
		return saveCapacity(tx, launch, capacityState{Binding: binding, Revision: a.Revision})
	})
}

func observeCapacityChild(db *sql.DB, sock string, w capacityChild, deadline time.Time) error {
	state, found, err := loadCapacity(db, w.Launch)
	if err != nil {
		return dbErr(err)
	}
	before, err := capacityGet(sock, w.Pane, deadline)
	if err != nil {
		return nil
	}
	binding, ok := before.binding(w, sock)
	if !ok || (found && state.Binding != binding) {
		return nil
	}
	var result struct {
		Read struct {
			Pane      string `json:"pane_id"`
			Source    string `json:"source"`
			Format    string `json:"format"`
			Text      string `json:"text"`
			Revision  *int64 `json:"revision"`
			Truncated bool   `json:"truncated"`
		} `json:"read"`
	}
	if herdrCapacityRequest(sock, "agent.read", map[string]any{"target": w.Pane, "source": "detection", "format": "text", "strip_ansi": true}, deadline, &result) != nil {
		return nil
	}
	r := result.Read
	if r.Pane != w.Pane || r.Source != "detection" || r.Format != "text" || r.Revision == nil || *r.Revision < 0 {
		return nil
	}
	revision := *r.Revision
	if revision == 0 {
		revision = before.Revision
	}
	if revision < 0 {
		return nil
	}
	// Detection truncates the older prefix, not the tail. The parser requires
	// the entire native warning cell and allows only known chrome afterwards.
	tail := capacityOf(r.Text)
	if tail.signal == capacityUnknown {
		return nil
	}
	if !found && w.SessionRef == "" && (w.Attempt == 0 || tail.attempt != w.Attempt) {
		return nil
	}
	after, err := capacityGet(sock, w.Pane, deadline)
	if err != nil {
		return nil
	}
	afterBinding, ok := after.binding(w, sock)
	if !ok || afterBinding != binding {
		return nil
	}
	return withTx(db, func(tx *sql.Tx) error {
		current, err := capacityStillCurrent(tx, w)
		if err != nil || !current {
			return err
		}
		latest, exists, err := loadCapacity(tx, w.Launch)
		if err != nil {
			return err
		}
		if exists && latest.Binding != binding {
			return nil
		}
		if exists && revision <= latest.Revision {
			return nil
		}
		if !exists {
			if found {
				return nil
			} // an expected binding disappeared during the read
			latest.Binding = binding
		}
		latest.Revision = revision
		if tail.signal == capacityActivity {
			latest.Active = false
		} else if !latest.Active {
			latest.Episode++
			latest.Active = true
			_, err := insertEvent(tx, event{TaskID: w.Task, RecipientTaskID: ptr(w.Parent), LaunchID: ptr(w.Launch), Kind: "herdr",
				Summary: "Codex capacity warning observed; inspect helper before retry", EventKey: fmt.Sprintf("capacity:%d:%d", w.Launch, latest.Episode),
				Data: map[string]any{"reason": "model_capacity", "provider": "codex", "model": w.Model, "model_source": "launch",
					"pane_id": w.Pane, "agent_name": w.Name, "episode": latest.Episode, "source": "detection", "action": "inspect_before_retry"}})
			if err != nil {
				return err
			}
		}
		return saveCapacity(tx, w.Launch, latest)
	})
}

func scanChildrenCapacity(db *sql.DB, sock string, parent int64, deadline time.Time) error {
	ws, err := capacityChildren(db, parent)
	if err != nil || len(ws) == 0 {
		return dbErr(err)
	}
	if limit := time.Now().Add(2 * time.Second); limit.Before(deadline) {
		deadline = limit
	}
	cursorKey := fmt.Sprintf("capacity_cursor:%d", parent)
	raw, _, err := getMeta(db, cursorKey)
	if err != nil {
		return dbErr(err)
	}
	cursor, _ := strconv.ParseInt(raw, 10, 64)
	start := 0
	for i, w := range ws {
		if w.Task > cursor {
			start = i
			break
		}
	}
	for i := 0; i < len(ws) && time.Until(deadline) > 0; i++ {
		w := ws[(start+i)%len(ws)]
		if err := observeCapacityChild(db, sock, w, deadline); err != nil {
			return err
		}
		if err := setMeta(db, cursorKey, strconv.FormatInt(w.Task, 10)); err != nil {
			return dbErr(err)
		}
	}
	return nil
}

func maybeCapacity(db *sql.DB, sock string, parent int64, deadline time.Time) error {
	if time.Until(deadline) <= 0 {
		return nil
	}
	ws, err := capacityChildren(db, parent)
	if err != nil || len(ws) == 0 {
		return dbErr(err)
	}
	due, err := claimPoll(db, `select value from meta where key = ?`,
		`insert into meta (key, value) values (?2, ?1) on conflict(key) do update set value = excluded.value`, fmt.Sprintf("capacity_poll:%d", parent))
	if err != nil || !due {
		return err
	}
	return scanChildrenCapacity(db, sock, parent, deadline)
}
