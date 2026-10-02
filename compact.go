package main

import (
	"encoding/json"
	"fmt"
)

var kindCodes = map[string]string{
	"got": "g", "ready": "r", "ask": "q", "answer": "a", "owner_answer": "oa",
	"done": "d", "fail": "f", "herdr": "h", "note": "n", "start": "s",
	"prompt": "p", "prompt_outcome": "po", "decision": "dc", "revoke": "rv",
	"ref": "rf", "next": "nx", "handover": "ho", "adopt": "ad", "launch": "l", "closed": "c",
}

// Aliases apply only to the CLI envelope. Arbitrary data/kv/refs, hashes and
// native identity remain verbatim; no shared ledger or dashboard shape changes.
var readAliases = map[string]string{
	"id": "i", "task_id": "t", "task_name": "n", "name": "n", "kind": "k",
	"created_at": "at", "updated_at": "at", "recipient_task_id": "to", "launch_id": "l",
	"current_launch_id": "l", "related_event_id": "rel", "answered_by": "ans",
	"summary": "s", "data": "d", "event_key": "key", "record": "rec", "status": "s",
	"workspace_id": "w", "tab_id": "tab", "pane_id": "pane", "report_path": "rp",
	"parent_id": "par", "acked_event_id": "ack", "pending_event_id": "pend",
	"last_receipt": "got", "open_asks": "a", "blocking_asks": "b", "round": "r",
	"waiting": "wait", "observed": "h", "next": "nx",
}

var resultAliases = map[string]string{
	"attempt_id": "p", "outcome": "o", "outcome_event_id": "oe", "receipt_event_id": "g",
	"receipt": "gok", "round": "r", "event_id": "e", "decision_id": "dc", "kind": "k",
	"answer_id": "a", "delivered": "sent", "asker_waiting": "w", "error": "err", "receipt_due_ms": "due",
}

func jsonText(v any) string { b, _ := json.Marshal(v); return string(b) }

func aliases(m map[string]any, names map[string]string) map[string]any {
	out := make(map[string]any, len(m))
	for k, v := range m {
		if a, ok := names[k]; ok {
			k = a
		}
		out[k] = v
	}
	return out
}

func only(m map[string]any, keys ...string) map[string]any {
	out := map[string]any{}
	for _, k := range keys {
		if v, ok := m[k]; ok {
			out[k] = v
		}
	}
	return out
}

func except(m map[string]any, keys ...string) map[string]any {
	out := aliases(m, nil)
	for _, k := range keys {
		delete(out, k)
	}
	return out
}

func (c *ctx) frame(tag string, v any) { fmt.Fprintf(c.out, "%s %s\n", tag, jsonText(v)) }

// readLine renders one read record (status, asks, log) as its whole compact
// line; cmd_read.go budgets against exactly these bytes.
func readLine(m map[string]any) string {
	out := aliases(m, readAliases)
	if obs, ok := m["observed"].(map[string]any); ok {
		out["h"] = aliases(obs, map[string]string{"agent_status": "s", "state_change_seq": "seq"})
	}
	return "j1 " + jsonText(out) + "\n"
}

func (c *ctx) emitCompact(v any) {
	m, ok := v.(map[string]any)
	if !ok {
		c.frame("j1", v)
		return
	}
	if m["error"] != nil {
		fmt.Fprintf(c.out, "x1 %d %s\n", c.code, jsonText(aliases(m, resultAliases)))
		return
	}
	if m["timeout"] == true {
		owed, hasOwed := m["owed"]
		due, hasDue := m["due"]
		if m["interrupted"] != true && hasOwed && hasDue {
			c.frame("w1", struct {
				Owed any `json:"owed"`
				Due  any `json:"due"`
			}{owed, due})
		}
		fmt.Fprint(c.out, "x1 3 timeout")
		if m["interrupted"] == true {
			fmt.Fprint(c.out, " interrupted")
		}
		fmt.Fprintln(c.out)
		return
	}
	if ev, ok := m["event"].(map[string]any); ok && c.cmd == "wait" {
		kind, _ := ev["kind"].(string)
		code := kindCodes[kind]
		if code == "" {
			code = kind
		}
		summary, _ := ev["summary"].(string)
		data, _ := ev["data"].(map[string]any)
		data = aliases(data, nil)
		if kind == "got" {
			summary = ""
			delete(data, "identity") // available in full log, retained in the ledger
		}
		nullID := func(k string) any {
			if ev[k] == nil {
				return "-"
			}
			return ev[k]
		}
		replay := 0
		if m["replay"] == true {
			replay = 1
		}
		fmt.Fprintf(c.out, "e1\t%v\t%v\t%v\t%s\t%d\t%v\t%s\t%s\n",
			ev["id"], ev["task_id"], nullID("launch_id"), code, replay, nullID("related_event_id"), jsonText(summary), jsonText(data))
		return
	}
	suffix := ""
	if m["duplicate"] == true {
		suffix = " dup"
	}
	switch c.cmd {
	case "new":
		if m["status"] == "planned" {
			suffix = " planned"
		}
		fmt.Fprintf(c.out, "n1 %v%s\n", m["task_id"], suffix)
	case "got":
		fmt.Fprintf(c.out, "g1 %v %v%s\n", m["event_id"], m["round"], suffix)
	case "start", "note", "ready", "ask", "done", "fail":
		fmt.Fprintf(c.out, "%s1 %v%s\n", kindCodes[c.cmd], m["event_id"], suffix)
	case "ack":
		if m["already"] == true {
			suffix = " already"
		}
		fmt.Fprintf(c.out, "k1 %v%s\n", m["acked_event_id"], suffix)
	case "close":
		if m["already"] == true {
			suffix = " already"
		}
		fmt.Fprintf(c.out, "c1 %v%s\n", m["task_id"], suffix)
	case "launch":
		c.frame("l1", only(m, "launch_id", "replaced_launch_id", "status", "was_planned"))
	case "prompt", "answer":
		c.frame(kindCodes[c.cmd]+"1", aliases(except(m, "task_id", "ask_id", "target", "ok", "agent_status"), resultAliases))
	case "next":
		if m["next"] == nil {
			suffix = " clear"
		}
		fmt.Fprintf(c.out, "nx1 %v%s\n", m["event_id"], suffix)
	case "decide":
		c.frame("dc1", aliases(except(m, "ok", "task_id"), resultAliases))
	case "set":
		c.frame("rf1", m["event_ids"])
	case "status", "asks", "log":
		fmt.Fprint(c.out, readLine(m))
	default:
		c.frame("j1", m)
	}
}
