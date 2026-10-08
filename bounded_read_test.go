package main

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"
)

// readFrames decodes one compact read: j1 rows in order plus the m1
// trailer, if any. Every line must decode whole: the budget never cuts
// text inside a record.
func readFrames(t *testing.T, out string) (rows []map[string]any, trailer map[string]any) {
	t.Helper()
	if out != "" && !strings.HasSuffix(out, "\n") {
		t.Fatalf("output ends mid-record: %q", out[len(out)-40:])
	}
	for _, l := range strings.Split(strings.TrimSpace(out), "\n") {
		if l == "" {
			continue
		}
		tag, body, ok := strings.Cut(l, " ")
		if !ok {
			t.Fatalf("frame %q", l)
		}
		var m map[string]any
		if err := json.Unmarshal([]byte(body), &m); err != nil {
			t.Fatalf("frame %q: %v", l, err)
		}
		if tag == "m1" {
			trailer = m
			continue
		}
		if tag != "j1" {
			t.Fatalf("unexpected frame %q", l)
		}
		rows = append(rows, m)
	}
	return rows, trailer
}

func frameIDs(rows []map[string]any, rec string) []int64 {
	var ids []int64
	for _, r := range rows {
		if rec == "" || r["rec"] == rec {
			ids = append(ids, int64(r["i"].(float64)))
		}
	}
	return ids
}

// taskIDs identifies status task rows: they carry a role and no record tag.
func taskIDs(rows []map[string]any) []int64 {
	var ids []int64
	for _, r := range rows {
		if _, ok := r["role"]; ok {
			ids = append(ids, int64(r["i"].(float64)))
		}
	}
	return ids
}

func hasInt(ids []int64, want int64) bool {
	for _, id := range ids {
		if id == want {
			return true
		}
	}
	return false
}

func seedEvents(h *harness, task, launch int64, n int) {
	h.t.Helper()
	for i := 0; i < n; i++ {
		h.ok(as(task, launch), "note", fmt.Sprintf("event %03d", i))
	}
}

func eventLogIDs(h *harness, args ...string) []int64 {
	h.t.Helper()
	_, rows := h.run(nil, args...)
	var ids []int64
	for _, m := range rows {
		if m["record"] == "event" {
			ids = append(ids, num(m, "id"))
		}
	}
	return ids
}

func TestBoundedLogPaging(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top)
	l := h.launch(w)
	seedEvents(h, w, l, 250)
	all := eventLogIDs(h, "log", id(w))
	if len(all) != 250 {
		t.Fatalf("matched %d events", len(all))
	}
	code, raw, diag := h.compact(nil, "log", id(w), "--since", "0")
	rows, tr := readFrames(t, raw)
	got := frameIDs(rows, "event")
	if code != exitOK || diag != "" || len(got) != 100 || tr == nil ||
		tr["more"] != float64(150) || tr["next"] != float64(all[99]) {
		t.Fatalf("--since 0 = %d %d events, trailer %v, %q", code, len(got), tr, diag)
	}
	for i, e := range got {
		if e != all[i] {
			t.Fatalf("--since 0 event %d = %d, want oldest event %d", i, e, all[i])
		}
	}

	code, raw, diag = h.compact(nil, "--json", "log", id(w), "--since", "0")
	var jsonRows []map[string]any
	for _, line := range strings.Split(strings.TrimSpace(raw), "\n") {
		var m map[string]any
		if err := json.Unmarshal([]byte(line), &m); err != nil {
			t.Fatalf("--json --since 0 row %q: %v", line, err)
		}
		jsonRows = append(jsonRows, m)
	}
	var jsonEvents []int64
	for _, row := range jsonRows {
		if row["record"] == "event" {
			jsonEvents = append(jsonEvents, num(row, "id"))
		}
	}
	if code != exitOK || diag != "" || len(jsonRows) != 101 || len(jsonEvents) != 100 {
		t.Fatalf("--json --since 0 = %d %d rows, %d events, %q", code, len(jsonRows), len(jsonEvents), diag)
	}
	for i, e := range jsonEvents {
		if e != all[i] {
			t.Fatalf("--json --since 0 event %d = %d, want oldest event %d", i, e, all[i])
		}
	}

	code, raw, diag = h.compact(nil, "log", id(w))
	rows, tr = readFrames(t, raw)
	if code != exitOK || diag != "" || len(rows) != 101 || rows[0]["rec"] != "launch" {
		t.Fatalf("default log = %d %d rows %q", code, len(rows), diag)
	}
	if tr == nil || tr["older"] != float64(150) || tr["before"] != rows[1]["i"] {
		t.Fatalf("backward trailer %v", tr)
	}
	for i, r := range rows[1:] {
		if int64(r["i"].(float64)) != all[150+i] {
			t.Fatalf("window row %d is not the newest 100", i)
		}
	}

	// --limit 0 shows everything and lifts the cap: no trailer.
	code, raw, _ = h.compact(nil, "log", id(w), "--limit", "0")
	rows, tr = readFrames(t, raw)
	if code != exitOK || len(rows) != 251 || tr != nil {
		t.Fatalf("--limit 0 = %d %d rows, trailer %v", code, len(rows), tr)
	}

	// --before pages back; together the pages return every event exactly once.
	seen := map[int64]bool{}
	before := int64(0)
	for pages := 0; ; pages++ {
		args := []string{"log", id(w)}
		if before > 0 {
			args = append(args, "--before", id(before))
		}
		_, raw, _ = h.compact(nil, args...)
		rows, tr = readFrames(t, raw)
		for _, e := range frameIDs(rows, "event") {
			if seen[e] {
				t.Fatalf("event %d returned twice", e)
			}
			seen[e] = true
		}
		if tr == nil {
			break
		}
		next := int64(tr["before"].(float64))
		if next >= before && before > 0 {
			t.Fatalf("backward cursor stuck at %d", next)
		}
		before = next
		if pages > 10 {
			t.Fatal("backward paging does not converge")
		}
	}
	if len(seen) != 250 {
		t.Fatalf("backward pages saw %d events", len(seen))
	}

	// --since pages forward, oldest first. Started at a mid cursor, the
	// forward pages return exactly the events after it, once each; with the
	// backward pages above they cover the whole log.
	seen = map[int64]bool{}
	since := all[99]
	first := true
	for pages := 0; ; pages++ {
		_, raw, _ = h.compact(nil, "log", id(w), "--limit", "37", "--since", id(since))
		rows, tr = readFrames(t, raw)
		got := frameIDs(rows, "event")
		for _, e := range got {
			if seen[e] {
				t.Fatalf("event %d returned twice", e)
			}
			seen[e] = true
			if e <= since {
				t.Fatalf("forward page returned event %d at or before the cursor %d", e, since)
			}
		}
		if first {
			if len(got) != 37 || got[0] != all[100] {
				t.Fatalf("first forward page is not the oldest 37 after the cursor: %v", got[:3])
			}
			first = false
		}
		if tr == nil {
			break
		}
		if tr["more"] == nil || tr["next"] == nil {
			t.Fatalf("forward trailer %v", tr)
		}
		since = int64(tr["next"].(float64))
		if pages > 10 {
			t.Fatal("forward paging does not converge")
		}
	}
	if len(seen) != 150 {
		t.Fatalf("forward pages saw %d events, want the 150 after the cursor", len(seen))
	}
}

func TestBoundedLogForwardCapDoesNotSkip(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top)
	l := h.launch(w)
	cursor := num(h.ok(as(w, l), "note", "cursor"), "event_id")
	want := map[int64]bool{}
	for i := 0; i < 40; i++ {
		e := num(h.ok(as(w, l), "note", strings.Repeat("x", 2000)), "event_id")
		want[e] = true
	}

	seen := map[int64]bool{}
	since := cursor
	for pages := 0; ; pages++ {
		code, raw, diag := h.compact(nil, "log", id(w), "--since", fmt.Sprint(since))
		rows, tr := readFrames(t, raw)
		got := frameIDs(rows, "event")
		if code != exitOK || diag != "" || len(got) == 0 {
			t.Fatalf("forward cap page = %d %d events %q", code, len(got), diag)
		}
		for _, e := range got {
			if _, ok := want[e]; !ok || seen[e] {
				t.Fatalf("forward cap returned unexpected or duplicate event %d", e)
			}
			seen[e] = true
		}
		if tr == nil {
			break
		}
		if tr["more"] == nil || tr["next"] == nil {
			t.Fatalf("forward cap trailer %v", tr)
		}
		next := int64(tr["next"].(float64))
		if next <= since {
			t.Fatalf("forward cursor did not advance: %d -> %d", since, next)
		}
		since = next
		if pages > 10 {
			t.Fatal("forward cap paging does not converge")
		}
	}
	if len(seen) != len(want) {
		t.Fatalf("forward cap saw %d of %d events", len(seen), len(want))
	}
}

func TestBoundedLogLaunchRows(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top)
	l1 := h.launch(w)
	h.ok(as(w, l1), "note", "one")
	l2 := h.launch(w) // replacement: l1 is history
	closed := h.newTask("closed", "implementer", top)
	h.launch(closed)
	h.ok(nil, "close", id(closed))
	withEvent := h.newTask("closed-event", "implementer", top)
	wl := h.launch(withEvent)
	h.ok(as(withEvent, wl), "note", "before close")
	h.ok(nil, "close", id(withEvent))

	// A one-event window: only the current launch of the open lane and of
	// the closed lane whose event is in the window.
	code, raw, diag := h.compact(nil, "log", id(top), "--tree", "--limit", "1")
	rows, tr := readFrames(t, raw)
	launches := frameIDs(rows, "launch")
	all := eventLogIDs(h, "log", id(top), "--tree")
	if code != exitOK || diag != "" || len(rows) != 3 ||
		tr == nil || tr["older"] != float64(len(all)-1) || tr["before"] != float64(all[len(all)-1]) ||
		!hasInt(launches, l2) || !hasInt(launches, wl) || len(launches) != 2 {
		t.Fatalf("bounded launch rows = %d %v %v", code, launches, tr)
	}
	if frameIDs(rows, "event")[0] != all[len(all)-1] { // the newest event: the last close
		t.Fatalf("window event %v", rows)
	}

	// --json without the new flags keeps the legacy contract: every launch.
	_, legacy := h.run(nil, "log", id(top), "--tree")
	if len(legacy) != 8 { // 4 launches + 4 events
		t.Fatalf("legacy log rows %d", len(legacy))
	}
	var legacyLaunches []int64
	for _, m := range legacy {
		if m["record"] == "launch" {
			legacyLaunches = append(legacyLaunches, num(m, "id"))
		}
	}
	if len(legacyLaunches) != 4 || !hasInt(legacyLaunches, l1) || !hasInt(legacyLaunches, l2) || !hasInt(legacyLaunches, wl) {
		t.Fatalf("legacy json launch rows %v", legacyLaunches)
	}

	// --json with --limit applies the same selection, still no trailer.
	_, boundedJSON := h.run(nil, "log", id(top), "--tree", "--limit", "1")
	var jsonLaunches []int64
	jsonEvents := 0
	for _, m := range boundedJSON {
		if m["record"] == "launch" {
			jsonLaunches = append(jsonLaunches, num(m, "id"))
		} else {
			jsonEvents++
		}
	}
	if len(boundedJSON) != 3 || jsonEvents != 1 || len(jsonLaunches) != 2 ||
		!hasInt(jsonLaunches, l2) || !hasInt(jsonLaunches, wl) {
		t.Fatalf("json bounded launch rows %v", jsonLaunches)
	}
}

func TestBoundedLogCap150(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	const lanes = 150
	text := strings.Repeat("x", 400)
	open := map[int64]int64{}
	var closedLaunches []int64
	for i := 0; i < lanes; i++ {
		w := h.newTask(fmt.Sprintf("lane%03d", i), "implementer", top)
		// Fat location fields push the 150 launch rows over the budget
		// while the 100 open ones plus the newest event still fit.
		l := h.launch(w, "--workspace", strings.Repeat("w", 40), "--tab", strings.Repeat("t", 30),
			"--pane", strings.Repeat("p", 30))
		h.ok(as(w, l), "note", text)
		h.ok(as(w, l), "note", text+"2")
		if i%3 == 0 {
			h.ok(nil, "close", id(w))
			closedLaunches = append(closedLaunches, l)
			continue
		}
		open[w] = l
	}
	matched := eventLogIDs(h, "log", id(top), "--tree")
	newest := matched[len(matched)-1]

	// The default read is over budget and capped: the oldest events go
	// first, then closed tasks' launch rows; the newest event and every
	// open lane's current launch stay.
	code, raw, diag := h.compact(nil, "log", id(top), "--tree")
	rows, tr := readFrames(t, raw)
	if code != exitOK || diag != "" {
		t.Fatalf("capped log = %d %q", code, diag)
	}
	if len(raw) > readCapBytes {
		t.Fatalf("capped log is %d bytes", len(raw))
	}
	uncapped := uncappedLog(t, h, top)
	if urows, utr := readFrames(t, uncapped); utr != nil || len(urows) != len(matched)+lanes || len(uncapped) <= readCapBytes {
		t.Fatalf("--limit 0 reference: %d bytes, %d rows, trailer %v", len(uncapped), len(urows), utr)
	}
	events := frameIDs(rows, "event")
	if len(events) != 1 || events[0] != newest {
		t.Fatalf("cap kept events %v, want the newest %d", events, newest)
	}
	launches := frameIDs(rows, "launch")
	if len(launches) < len(open) || len(launches) >= lanes {
		t.Fatalf("cap kept %d launch rows", len(launches))
	}
	for w, l := range open {
		if !hasInt(launches, l) {
			t.Fatalf("open lane %d lost its launch row", w)
		}
	}
	// Any closed rows that survive the cap are the newest ones.
	var keptClosed []int64
	for _, l := range launches {
		if hasInt(closedLaunches, l) {
			keptClosed = append(keptClosed, l)
		}
	}
	for i, l := range keptClosed {
		if l != closedLaunches[len(closedLaunches)-len(keptClosed)+i] {
			t.Fatalf("cap kept a non-newest closed launch row: %v", launches)
		}
	}
	if tr == nil || tr["older"] != float64(len(matched)-1) || tr["before"] != float64(newest) {
		t.Fatalf("capped backward trailer %v", tr)
	}

	// The forward trailer on one lane: the window after the lane's first
	// event, cut to one, pages forward with more/next.
	lane := top + 1
	laneEvents := eventLogIDs(h, "log", id(lane))
	code, raw, _ = h.compact(nil, "log", id(lane), "--since", id(laneEvents[0]), "--limit", "1")
	rows, tr = readFrames(t, raw)
	if code != exitOK || len(rows) != 2 || tr == nil ||
		tr["more"] != float64(len(laneEvents)-2) || tr["next"] != float64(laneEvents[1]) {
		t.Fatalf("forward trailer rows=%d %v", len(rows), tr)
	}
}

func TestBoundedLogCapReportsDroppedLaunchOnly(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	const lanes = 150
	openLaunches := make([]int64, 0, lanes)
	for i := 0; i < lanes; i++ {
		w := h.newTask(fmt.Sprintf("open%03d", i), "implementer", top)
		l := h.launch(w, "--workspace", strings.Repeat("w", 40), "--tab", strings.Repeat("t", 30),
			"--pane", strings.Repeat("p", 30))
		openLaunches = append(openLaunches, l)
	}
	closed := h.newTask("closed", "implementer", top)
	closedLaunch := h.launch(closed, "--workspace", strings.Repeat("w", 40), "--tab", strings.Repeat("t", 30),
		"--pane", strings.Repeat("p", 30))
	cursor := num(h.ok(as(closed, closedLaunch), "note", "last event before close"), "event_id")
	closeEvent := num(h.ok(nil, "close", id(closed)), "event_id")

	code, raw, diag := h.compact(nil, "log", id(top), "--tree", "--since", id(cursor))
	rows, tr := readFrames(t, raw)
	launches := frameIDs(rows, "launch")
	events := frameIDs(rows, "event")
	if code != exitOK || diag != "" || len(rows) != lanes+1 || len(launches) != lanes ||
		len(events) != 1 || events[0] != closeEvent || hasInt(launches, closedLaunch) {
		t.Fatalf("launch-only cap = %d, %d rows, launches %d, events %v, %q", code, len(rows), len(launches), events, diag)
	}
	for _, l := range openLaunches {
		if !hasInt(launches, l) {
			t.Fatalf("open launch %d was dropped", l)
		}
	}
	if tr == nil || len(tr) != 2 || tr["launches"] != float64(1) || tr["all"] != "--limit 0" {
		t.Fatalf("launch-only trailer %v", tr)
	}
	_, uncapped, _ := h.compact(nil, "log", id(top), "--tree", "--since", id(cursor), "--limit", "0")
	uncappedRows, uncappedTrailer := readFrames(t, uncapped)
	if len(uncappedRows) != lanes+2 || uncappedTrailer != nil {
		t.Fatalf("uncapped launch-only reference = %d rows, trailer %v", len(uncappedRows), uncappedTrailer)
	}
}

func uncappedLog(t *testing.T, h *harness, top int64) string {
	t.Helper()
	_, raw, _ := h.compact(nil, "log", id(top), "--tree", "--limit", "0")
	return raw
}

func TestBoundedLogCapKeepsClosedRowsWhenEventsFit(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	text := strings.Repeat("x", 1500)
	for i := 0; i < 10; i++ {
		w := h.newTask(fmt.Sprintf("lane%02d", i), "implementer", top)
		l := h.launch(w)
		h.ok(as(w, l), "note", text)
		h.ok(as(w, l), "note", text+"2")
		if i < 5 {
			h.ok(nil, "close", id(w))
		}
	}
	// The default window fits only after dropping the oldest events; the
	// closed lanes' launch rows must survive: events go first.
	code, raw, diag := h.compact(nil, "log", id(top), "--tree")
	rows, tr := readFrames(t, raw)
	events := frameIDs(rows, "event")
	launches := frameIDs(rows, "launch")
	if code != exitOK || diag != "" || len(raw) > readCapBytes || len(launches) != 10 || len(events) < 2 {
		t.Fatalf("cap order = %d bytes %d events %d launches %q", len(raw), len(events), len(launches), diag)
	}
	all := eventLogIDs(h, "log", id(top), "--tree")
	want := all[len(all)-len(events):]
	for i, e := range events {
		if e != want[i] {
			t.Fatalf("cap kept a non-newest event: %v", events)
		}
	}
	if tr == nil || tr["older"] != float64(len(all)-len(events)) {
		t.Fatalf("trailer %v", tr)
	}
}

func TestBoundedStatusTreeTrailer(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	a := h.newTask("a", "implementer", top)
	b := h.newTask("b", "implementer", top)
	c := h.newTask("c", "implementer", top)
	h.ok(nil, "close", id(b))
	h.ok(nil, "close", id(c))

	code, raw, diag := h.compact(nil, "status", "--tree", id(top))
	rows, tr := readFrames(t, raw)
	if code != exitOK || diag != "" || len(rows) != 3 { // root, open lane, daemon
		t.Fatalf("tree status = %d %d rows %q", code, len(rows), diag)
	}
	if got := taskIDs(rows); !hasInt(got, top) || !hasInt(got, a) || len(got) != 2 {
		t.Fatalf("tree rows %v", got)
	}
	if tr == nil || tr["closed"] != float64(2) || tr["all"] != "--all" {
		t.Fatalf("tree trailer %v", tr)
	}

	// --all includes the closed lanes and prints no trailer.
	code, raw, _ = h.compact(nil, "status", "--tree", id(top), "--all")
	rows, tr = readFrames(t, raw)
	if code != exitOK || len(rows) != 5 || tr != nil {
		t.Fatalf("--all = %d %d rows, trailer %v", code, len(rows), tr)
	}

	// The host-wide status keeps its set: closed lanes stay out, silently.
	code, raw, _ = h.compact(nil, "status")
	rows, tr = readFrames(t, raw)
	got := taskIDs(rows)
	if code != exitOK || tr != nil || !hasInt(got, top) || hasInt(got, b) || hasInt(got, c) {
		t.Fatalf("host-wide status = %d rows, trailer %v", len(rows), tr)
	}
}

func TestBoundedStatusOpenRowsOverCap(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	text := strings.Repeat("n", 400)
	for i := 0; i < 150; i++ {
		w := h.newTask(fmt.Sprintf("lane%03d", i), "implementer", top)
		h.ok(nil, "next", id(w), text)
	}
	code, raw, diag := h.compact(nil, "status", "--tree", id(top))
	rows, tr := readFrames(t, raw)
	// 151 open task rows plus the daemon record, even though they overflow
	// the cap: open rows are never dropped.
	if code != exitOK || diag != "" || len(rows) != 152 || tr != nil {
		t.Fatalf("open rows over cap = %d %d rows, trailer %v", code, len(rows), tr)
	}
	if len(raw) <= readCapBytes {
		t.Fatalf("fixture does not overflow: %d bytes", len(raw))
	}
}

func TestBoundedAsksBudget(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top)
	l := h.launch(w)
	var answered []int64
	for i := 0; i < 25; i++ {
		ask := num(h.ok(as(w, l), "ask", fmt.Sprintf("question %02d", i)), "ask_id")
		h.ok(nil, "answer", id(ask), "main")
		answered = append(answered, ask)
	}
	open1 := num(h.ok(as(w, l), "ask", "open one"), "ask_id")
	open2 := num(h.ok(as(w, l), "ask", "open two"), "ask_id")

	code, raw, diag := h.compact(nil, "asks")
	rows, tr := readFrames(t, raw)
	got := frameIDs(rows, "event")
	// 25 answered asks: the latest 20 stay, the trailer counts the rest.
	if code != exitOK || diag != "" || len(rows) != 22 ||
		tr == nil || tr["answered"] != float64(5) || tr["all"] != "--limit 0" {
		t.Fatalf("small asks = %d %d rows, trailer %v", code, len(rows), tr)
	}
	for i, want := range answered {
		if hasInt(got, want) != (i >= 5) {
			t.Fatalf("small asks kept answered ask %d: %v", want, got)
		}
	}
	if !hasInt(got, open1) || !hasInt(got, open2) {
		t.Fatalf("open asks lost: %v", got)
	}

	// 25 answered asks more, with answers that dwarf the budget. The limit
	// keeps the latest 20, then the cap drops the oldest answered until the
	// output fits; the trailer counts everything left out.
	var bigAnswered []int64
	for i := 0; i < 25; i++ {
		ask := num(h.ok(as(w, l), "ask", fmt.Sprintf("big %02d", i)), "ask_id")
		h.ok(nil, "answer", id(ask), strings.Repeat("a", 10000))
		bigAnswered = append(bigAnswered, ask)
	}
	allAnswered := append(append([]int64{}, answered...), bigAnswered...) // ascending ids
	code, raw, _ = h.compact(nil, "asks")
	rows, tr = readFrames(t, raw)
	if tr != nil && len(raw) > readCapBytes {
		t.Fatalf("asks with trailer is %d bytes", len(raw))
	}
	ids := frameIDs(rows, "event")
	opens := 0
	last := int64(0)
	for _, e := range ids {
		if e == open1 || e == open2 {
			opens++
		}
		if e > last {
			last = e
		}
	}
	shown := len(ids) - opens
	if code != exitOK || opens != 2 || shown <= 0 || shown >= 20 || tr == nil ||
		tr["answered"] != float64(50-shown) || tr["all"] != "--limit 0" {
		t.Fatalf("asks budget = %d opens %d shown, trailer %v", code, opens, tr)
	}
	if last != allAnswered[len(allAnswered)-1] { // the newest ask is answered and must survive
		t.Fatalf("newest ask dropped under the budget")
	}
	for i, e := range ids[opens:] {
		if e != allAnswered[len(allAnswered)-shown+i] {
			t.Fatalf("cap kept a non-newest answered ask: %v", ids)
		}
	}

	// --limit narrows the answered window; the fat answers still hit the
	// cap, so only as many as fit stay, the newest ones. --limit 0 lifts
	// everything.
	code, raw, _ = h.compact(nil, "asks", "--limit", "10")
	rows, tr = readFrames(t, raw)
	ids = frameIDs(rows, "event")
	opens, shown = 0, 0
	for _, e := range ids {
		if e == open1 || e == open2 {
			opens++
		}
	}
	shown = len(ids) - opens
	if code != exitOK || opens != 2 || shown <= 0 || shown >= 10 || tr == nil ||
		tr["answered"] != float64(50-shown) || tr["all"] != "--limit 0" {
		t.Fatalf("--limit 10 = %d rows, trailer %v", len(rows), tr)
	}
	for i, e := range ids[opens:] {
		if e != allAnswered[len(allAnswered)-shown+i] {
			t.Fatalf("--limit 10 kept a non-newest answered ask: %v", ids)
		}
	}
	code, raw, _ = h.compact(nil, "asks", "--limit", "0")
	rows, tr = readFrames(t, raw)
	if code != exitOK || len(rows) != 52 || tr != nil {
		t.Fatalf("--limit 0 = %d %d rows, trailer %v", code, len(rows), tr)
	}

	// --json keeps the legacy contract without --limit and applies the
	// selection without a trailer with it.
	_, allJSON := h.run(nil, "asks")
	if len(allJSON) != 52 {
		t.Fatalf("json asks = %d rows", len(allJSON))
	}
	_, limJSON := h.run(nil, "asks", "--limit", "10")
	if len(limJSON) != 12 {
		t.Fatalf("json asks --limit = %d rows", len(limJSON))
	}
}

func TestBoundedBudgetSubset(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	top := h.newTask("top", "orchestrator", 0)
	w := h.newTask("lane", "implementer", top)
	l := h.launch(w)
	seedEvents(h, w, l, 30)

	// Under the default window nothing is left out: every legacy row is
	// present and no m1 appears.
	_, legacy := h.run(nil, "log", id(w))
	code, raw, _ := h.compact(nil, "log", id(w))
	rows, tr := readFrames(t, raw)
	if code != exitOK || tr != nil || len(rows) != len(legacy) {
		t.Fatalf("small log = %d %d rows, legacy %d, trailer %v", code, len(rows), len(legacy), tr)
	}
	for i, r := range rows {
		if r["rec"] != legacy[i]["record"] || r["i"] != legacy[i]["id"] {
			t.Fatalf("row %d is not the legacy row", i)
		}
	}

	// A tight window is a subset of the legacy rows, with the trailer.
	code, raw, _ = h.compact(nil, "log", id(w), "--limit", "5")
	rows, tr = readFrames(t, raw)
	if code != exitOK || len(rows) != 6 || tr == nil || tr["older"] != float64(25) {
		t.Fatalf("budgeted log = %d %d rows, trailer %v", code, len(rows), tr)
	}
	for _, r := range rows {
		found := false
		for _, o := range legacy {
			if o["record"] == r["rec"] && o["id"] == r["i"] {
				found = true
				break
			}
		}
		if !found {
			t.Fatalf("budgeted row %v is not a legacy row", r)
		}
	}
}
