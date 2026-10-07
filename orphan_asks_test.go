package main

import (
	"fmt"
	"slices"
	"testing"
)

func TestAsksHideOrphans(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("open-root", "orchestrator", 0)
	lane := h.newTask("closed-asker", "implementer", root)
	launch := h.launch(lane)
	closedAsk := num(h.ok(as(lane, launch), "ask", "closed asker", "--owner"), "ask_id")
	plainAsk := num(h.ok(as(lane, launch), "ask", "closed plain asker"), "ask_id")
	answered := num(h.ok(as(lane, launch), "ask", "answered closed asker", "--owner"), "ask_id")
	h.ok(nil, "answer", id(answered), "recorded answer")
	h.ok(nil, "close", id(lane))
	live := h.newTask("live", "implementer", root)
	liveAsk := num(h.ok(as(live, h.launch(live)), "ask", "normal owner ask", "--owner"), "ask_id")
	intermediate := h.newTask("closed-intermediate", "sub-orchestrator", root)
	child := h.newTask("open-child", "implementer", intermediate)
	childAsk := num(h.ok(as(child, h.launch(child)), "ask", "open root through closed intermediate", "--owner"), "ask_id")
	h.ok(nil, "close", id(intermediate))

	closedRoot := h.newTask("closed-root", "orchestrator", 0)
	sub := h.newTask("open-sub", "sub-orchestrator", closedRoot)
	deep := h.newTask("open-deep-lane", "implementer", sub)
	deepLaunch := h.launch(deep)
	rootOrphan := num(h.ok(as(deep, deepLaunch), "ask", "closed root", "--owner"), "ask_id")
	rootAnswered := num(h.ok(as(deep, deepLaunch), "ask", "answered under closed root", "--owner"), "ask_id")
	h.ok(nil, "answer", id(rootAnswered), "another recorded answer")
	rootAsk := num(h.ok(nil, "ask", "root's own ask", "--as", id(closedRoot), "--owner"), "ask_id")
	h.ok(nil, "close", id(closedRoot))

	for _, tc := range []struct {
		name string
		args []string
		want []int64
	}{
		{"default", nil, []int64{answered, liveAsk, childAsk, rootAnswered}},
		{"open", []string{"--open"}, []int64{liveAsk, childAsk}},
		{"owner", []string{"--owner"}, []int64{answered, liveAsk, childAsk, rootAnswered}},
		{"open owner", []string{"--open", "--owner"}, []int64{liveAsk, childAsk}},
		{"all", []string{"--all"}, []int64{closedAsk, plainAsk, answered, liveAsk, childAsk, rootOrphan, rootAnswered, rootAsk}},
		{"all open", []string{"--all", "--open"}, []int64{closedAsk, plainAsk, liveAsk, childAsk, rootOrphan, rootAsk}},
		{"all owner open limit", []string{"--all", "--owner", "--open", "--limit", "1"}, []int64{closedAsk, liveAsk, childAsk, rootOrphan, rootAsk}},
		{"tree", []string{"--tree", id(root)}, []int64{answered, liveAsk, childAsk}},
		{"all tree owner", []string{"--all", "--tree", id(root), "--owner"}, []int64{closedAsk, answered, liveAsk, childAsk}},
		{"closed root tree", []string{"--tree", id(closedRoot)}, []int64{rootAnswered}},
		{"closed root all open", []string{"--all", "--tree", id(closedRoot), "--owner", "--open", "--limit", "0"}, []int64{rootOrphan, rootAsk}},
		{"answered limit", []string{"--limit", "1"}, []int64{liveAsk, childAsk, rootAnswered}},
		{"all answered limit", []string{"--all", "--limit", "1"}, []int64{closedAsk, plainAsk, liveAsk, childAsk, rootOrphan, rootAnswered, rootAsk}},
	} {
		for _, jsonMode := range []bool{false, true} {
			t.Run(fmt.Sprintf("%s/json=%v", tc.name, jsonMode), func(t *testing.T) {
				args := append([]string{"asks"}, tc.args...)
				var got []int64
				if jsonMode {
					code, rows := h.run(nil, args...)
					if code != exitOK || h.lastStderr() != "" {
						t.Fatalf("asks = %d %v %q", code, rows, h.lastStderr())
					}
					for _, row := range rows {
						got = append(got, num(row, "id"))
						if num(row, "id") == answered && row["answer"] != "recorded answer" ||
							num(row, "id") == rootAnswered && row["answer"] != "another recorded answer" {
							t.Fatalf("answer changed: %v", row)
						}
					}
				} else {
					code, out, stderr := h.compact(nil, args...)
					if code != exitOK || stderr != "" {
						t.Fatalf("asks = %d %q %q", code, out, stderr)
					}
					rows, _ := readFrames(t, out)
					got = frameIDs(rows, "event")
				}
				if !slices.Equal(got, tc.want) {
					t.Fatalf("asks %v = %v, want %v", tc.args, got, tc.want)
				}
			})
		}
	}
}

func TestCloseOrphanOwnerAsks(t *testing.T) {
	for _, target := range []string{"lane", "root", "none"} {
		for _, jsonMode := range []bool{false, true} {
			t.Run(fmt.Sprintf("%s/json=%v", target, jsonMode), func(t *testing.T) {
				h := newHarness(t)
				root := h.newTask("root", "orchestrator", 0)
				lane := h.newTask("lane", "implementer", root)
				launch := h.launch(lane)
				rootAsk := num(h.ok(nil, "ask", "root ask", "--as", id(root), "--owner"), "ask_id")
				laneAsk := num(h.ok(as(lane, launch), "ask", "lane ask", "--owner"), "ask_id")
				plain := num(h.ok(as(lane, launch), "ask", "plain ask"), "ask_id")
				answered := num(h.ok(as(lane, launch), "ask", "answered ask", "--owner"), "ask_id")
				h.ok(nil, "answer", id(answered), "yes")
				sub := h.newTask("closed-sub", "sub-orchestrator", root)
				subAsk := num(h.ok(as(sub, h.launch(sub)), "ask", "already orphaned", "--owner"), "ask_id")
				child := h.newTask("open-child", "implementer", sub)
				childAsk := num(h.ok(as(child, h.launch(child)), "ask", "child ask", "--owner"), "ask_id")
				h.ok(nil, "close", id(sub))
				other := h.newTask("other-root", "orchestrator", 0)
				h.ok(nil, "ask", "unrelated", "--as", id(other), "--owner")
				empty := h.newTask("no-owner-asks", "implementer", root)
				h.ok(as(empty, h.launch(empty)), "ask", "only a plain ask")
				closeID := lane
				want := []int64{laneAsk}
				warning := fmt.Sprintf("taskr: closing %d orphans owner ask(s) %d; the owner can no longer answer them\n", lane, laneAsk)
				if target == "root" {
					closeID, want = root, []int64{rootAsk, laneAsk, childAsk}
					warning = fmt.Sprintf("taskr: closing %d orphans owner ask(s) %d, %d, %d; the owner can no longer answer them\n", root, rootAsk, laneAsk, childAsk)
				} else if target == "none" {
					closeID, want, warning = empty, nil, ""
				}
				before := docCount(t, h.openDB(), `select count(*) from events`)
				if jsonMode {
					result := h.ok(nil, "close", id(closeID))
					if len(want) == 0 {
						if _, present := result["orphaned_owner_asks"]; present {
							t.Fatalf("unexpected orphan field: %v", result)
						}
					} else if jsonText(result["orphaned_owner_asks"]) != jsonText(want) {
						t.Fatalf("close result = %v, want orphan ids %v", result, want)
					}
					if h.lastStderr() != warning {
						t.Fatalf("stderr = %q, want %q", h.lastStderr(), warning)
					}
				} else {
					code, out, stderr := h.compact(nil, "close", id(closeID))
					if code != exitOK || out != fmt.Sprintf("c1 %d\n", closeID) || stderr != warning {
						t.Fatalf("compact close = %d %q %q, want warning %q", code, out, stderr, warning)
					}
				}
				if after := docCount(t, h.openDB(), `select count(*) from events`); after != before+1 {
					t.Fatalf("close wrote %d events, want 1", after-before)
				}
				for _, ask := range []int64{rootAsk, laneAsk, plain, subAsk, childAsk} {
					if n := docCount(t, h.openDB(), `select count(*) from events where id = ? and answered_by is null`, ask); n != 1 {
						t.Fatalf("close answered ask %d", ask)
					}
				}
				result := h.ok(nil, "close", id(closeID))
				if result["already"] != true || result["orphaned_owner_asks"] != nil || h.lastStderr() != "" {
					t.Fatalf("repeat close = %v stderr %q", result, h.lastStderr())
				}
				code, out, stderr := h.compact(nil, "close", id(closeID))
				if code != exitOK || out != fmt.Sprintf("c1 %d already\n", closeID) || stderr != "" {
					t.Fatalf("repeat compact close = %d %q %q", code, out, stderr)
				}
				if after := docCount(t, h.openDB(), `select count(*) from events`); after != before+1 {
					t.Fatalf("repeat close wrote events: %d vs %d", after, before+1)
				}
			})
		}
	}
}

func TestCloseOrphanWarningRPC(t *testing.T) {
	r := newTwoHost(t)
	_, lane, launch := spoolMakeWorker(t, r, "host-a", 1)
	ask := num(r.want(exitOK, "host-a", as(lane, launch), "ask", "owner question", "--owner"), "ask_id")
	code, result, stderr := r.cli("host-a", nil, "close", id(lane))
	warning := fmt.Sprintf("taskr: closing %d orphans owner ask(s) %d; the owner can no longer answer them\n", lane, ask)
	if code != exitOK || jsonText(result["orphaned_owner_asks"]) != jsonText([]int64{ask}) || stderr != warning {
		t.Fatalf("RPC close = %d %v %q, want warning %q", code, result, stderr, warning)
	}
	code, result, stderr = r.cli("host-a", nil, "close", id(lane))
	if code != exitOK || result["already"] != true || result["orphaned_owner_asks"] != nil || stderr != "" {
		t.Fatalf("RPC repeat close = %d %v %q", code, result, stderr)
	}
}

func TestCloseMidLevelKeepsDescendantAsks(t *testing.T) {
	h := newHarness(t)
	root := h.newTask("root", "orchestrator", 0)
	sub := h.newTask("sub", "sub-orchestrator", root)
	subAsk := num(h.ok(as(sub, h.launch(sub)), "ask", "sub ask", "--owner"), "ask_id")
	child := h.newTask("child", "implementer", sub)
	h.ok(as(child, h.launch(child)), "ask", "child ask", "--owner")
	result := h.ok(nil, "close", id(sub))
	if jsonText(result["orphaned_owner_asks"]) != jsonText([]int64{subAsk}) {
		t.Fatalf("mid-level close = %v, want only its own ask %d", result, subAsk)
	}
}
