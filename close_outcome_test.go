package main

import (
	"bytes"
	"encoding/json"
	"strings"
	"testing"
	"time"
)

func TestCloseOutcomeStoredAndLogged(t *testing.T) {
	contractGuard(t)
	for _, outcome := range []string{"accepted", "reworked", "rejected", "abandoned", ""} {
		t.Run(outcome, func(t *testing.T) {
			h := newHarness(t)
			task := h.newTask("lane", "orchestrator", 0)
			args := []string{"close", id(task)}
			if outcome != "" {
				args = append(args, "--outcome", outcome)
			}
			result := h.ok(nil, args...)
			var raw string
			if err := h.openDB().QueryRow(`select data from events where id = ? and kind = 'closed'`, num(result, "event_id")).Scan(&raw); err != nil {
				t.Fatal(err)
			}
			var stored map[string]any
			if err := json.Unmarshal([]byte(raw), &stored); err != nil {
				t.Fatal(err)
			}
			code, events := h.run(nil, "log", id(task))
			if code != exitOK || len(events) != 1 || events[0]["kind"] != "closed" {
				t.Fatalf("log = %d %v", code, events)
			}
			for _, data := range []map[string]any{stored, events[0]["data"].(map[string]any)} {
				got, present := data["outcome"]
				if outcome == "" && present || outcome != "" && got != outcome || data["from_status"] != "open" {
					t.Fatalf("closed data = %v, want outcome %q", data, outcome)
				}
			}
			for _, repeat := range [][]string{{"close", id(task)}, {"close", id(task), "--outcome", "rejected"}} {
				if result := h.ok(nil, repeat...); result["already"] != true {
					t.Fatalf("repeat close = %v", result)
				}
			}
			if n := docCount(t, h.openDB(), `select count(*) from events where task_id = ? and kind = 'closed'`, task); n != 1 {
				t.Fatalf("repeat close wrote %d events", n)
			}
		})
	}
}

func TestCloseOutcomeInvalid(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	task := h.newTask("lane", "orchestrator", 0)
	for _, value := range []string{"unknown", ""} {
		result := h.one(exitUsage, nil, "close", id(task), "--outcome", value)
		for _, want := range []string{"accepted", "reworked", "rejected", "abandoned"} {
			if !strings.Contains(errOf(result), want) {
				t.Fatalf("usage error omits %s: %v", want, result)
			}
		}
	}
	if n := docCount(t, h.openDB(), `select count(*) from events where task_id = ? and kind = 'closed'`, task); n != 0 {
		t.Fatalf("invalid outcome wrote %d events", n)
	}
}

func TestCloseOutcomeQueuedDelivery(t *testing.T) {
	contractGuard(t)
	r := newTwoHost(t)
	host := spoolClientHost(r)
	_, task, _ := spoolMakeWorker(t, r, host, 1)
	home := r.clientHome(spoolDeadURL(t, r))
	setVar(t, &rpcRetryWindow, func([]string) time.Duration { return 30 * time.Millisecond })
	code, out, stderr := spoolRunCLI(r, host, home, nil, "close", id(task), "--outcome", "accepted")
	if code != exitOK || !strings.HasPrefix(out, "qd1 ") {
		t.Fatalf("queued close = %d %q %q", code, out, stderr)
	}
	if n := docCount(t, r.openDB(), `select count(*) from events where task_id = ? and kind = 'closed'`, task); n != 0 {
		t.Fatalf("offline close wrote %d events", n)
	}
	r.caller.Store(host)
	if sent, err := sendSpool(spoolStateDir(home), r.url, nil); err != nil || sent != 1 {
		t.Fatalf("send = %d, %v", sent, err)
	}
	if files, err := readSpoolFiles(spoolQueuePath(home)); err != nil || len(files) != 0 {
		t.Fatalf("delivered queue = %v, %v", files, err)
	}
	code, events := r.run(nil, "log", id(task))
	if code != exitOK || len(events) != 2 || events[1]["kind"] != "closed" || events[1]["data"].(map[string]any)["outcome"] != "accepted" {
		t.Fatalf("delivered log = %d %v", code, events)
	}
}

func TestCloseOutcomeUsage(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	for _, args := range [][]string{{"help"}, {"close", "--help"}} {
		var out, stderr bytes.Buffer
		if code := contractRun(t, args, h.getenv(nil), &out, &stderr); code != exitOK {
			t.Fatalf("help = %d %q %q", code, out.String(), stderr.String())
		}
		for _, want := range []string{"--outcome", "accepted", "reworked", "rejected", "abandoned"} {
			if !strings.Contains(out.String(), want) {
				t.Fatalf("help omits %s: %q", want, out.String())
			}
		}
	}
}
