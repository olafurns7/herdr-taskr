package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"
	"unicode"

	"github.com/charmbracelet/x/ansi"
)

var watchTestNow = time.Date(2026, 10, 7, 17, 30, 0, 0, time.Local)

func busyGlance() *glanceView {
	blocking := true
	return &glanceView{
		Verdict: "needs_you",
		NeedsYou: []glanceNeed{
			{Kind: "owner_ask", Campaign: "copilot-modular", AgeMS: 12 * time.Minute.Milliseconds(), Blocking: &blocking, Also: []string{"lane failed"}, PaneID: "wN4:p1", Text: "Merge #4840 now or wait for M3?"},
			{Kind: "owner_todo", Campaign: "booked-vs-resolved", AgeMS: time.Hour.Milliseconds(), Items: []string{"1. approve prod deploy of #4833", "2. approve next rollout"}},
		},
		Attention: []glanceAttention{
			{Kind: "lane_failed", Campaign: "mobile-screens", Lane: "impl-tabs", Since: stamp(watchTestNow.Add(-4 * time.Minute)), AgeMS: 4 * time.Minute.Milliseconds(), Text: "lint gate exit 1"},
			{Kind: "results_waiting", Recipient: "orch-hns2", Count: 3, Since: stamp(watchTestNow.Add(-31 * time.Hour)), AgeMS: 31 * time.Hour.Milliseconds(), Text: "reports ready to review"},
		},
		Campaigns: []glanceCampaign{
			{Name: "planner-ui", Lanes: glanceLanes{Working: 3, Open: 5}, Last: &glanceLast{AgeMS: 2 * time.Minute.Milliseconds(), Text: "S4 merged"}},
			{Name: "copilot-modular", Lanes: glanceLanes{Working: 2, Open: 2}, Last: &glanceLast{AgeMS: 14 * time.Minute.Milliseconds(), Text: "M1 review ok"}},
			{Name: "booked-vs-resolved", Lanes: glanceLanes{Open: 1}, ActivityAgeMS: 28 * time.Minute.Milliseconds()},
		},
		Quiet: glanceQuiet{Count: 13, WithBacklog: 4},
	}
}

func TestRenderGlanceGoldens(t *testing.T) {
	rolling := *busyGlance()
	rolling.Verdict = "rolling"
	rolling.NeedsYou = nil
	rolling.Attention = nil
	rolling.Quiet.WithBacklog = 0
	unclear := &glanceView{Verdict: "attention", Attention: []glanceAttention{
		{Kind: "owner_unclear", Campaign: "copilot-modular", Since: stamp(watchTestNow.Add(-time.Hour)), AgeMS: time.Hour.Milliseconds(), Text: "no decision needed now."},
		{Kind: "lead_unknown", Campaign: "never-observed", Text: "lead has never been observed"},
		{Kind: "host_stale", Host: "mac", Text: "no heartbeat recorded"},
	}}
	for _, tc := range []struct {
		name          string
		v             *glanceView
		width, height int
		err           string
	}{
		{"rolling-46", &rolling, 46, 24, ""}, {"busy-46", busyGlance(), 46, 24, ""},
		{"no-data-46", nil, 46, 24, ""}, {"stale-46", busyGlance(), 46, 24, "server unreachable"},
		{"busy-80", busyGlance(), 80, 24, ""}, {"overflow-8", busyGlance(), 46, 8, ""},
		{"narrow-20", busyGlance(), 20, 24, ""},
		{"unclear-46", unclear, 46, 24, ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if tc.name == "busy-46" || tc.name == "busy-80" {
				tc.v.Attention = append(tc.v.Attention, glanceAttention{Kind: "lead_gone", Campaign: "planner-ui",
					Since: stamp(watchTestNow.Add(-5 * time.Minute)), AgeMS: 5 * time.Minute.Milliseconds(), Text: "lead pane wN5:p1 is not in its host's agent list"})
			}
			lines := renderGlance(tc.v, tc.width, tc.height, 2*time.Second, tc.err, false, watchTestNow)
			checkWatchWidths(t, lines, tc.width, tc.height)
			got := strings.Join(lines, "\n") + "\n"
			path := filepath.Join("testdata", "glance", tc.name+".golden")
			if *updateGolden {
				if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(path, []byte(got), 0644); err != nil {
					t.Fatal(err)
				}
			}
			want, err := os.ReadFile(path)
			if err != nil {
				t.Fatal(err)
			}
			if got != string(want) {
				t.Fatalf("frame mismatch\ngot:\n%s\nwant:\n%s", got, want)
			}
			if tc.height == 8 {
				for _, s := range []string{"2 need you", "+2 more need you", "+2 more to check", "+3 more campaigns", "13 quiet", "4 with backlog"} {
					if !strings.Contains(got, s) {
						t.Errorf("missing count %q: %s", s, got)
					}
				}
			}
		})
	}
}

func checkWatchWidths(t *testing.T, lines []string, width, height int) {
	t.Helper()
	if len(lines) > height {
		t.Fatalf("%d lines exceeds %d", len(lines), height)
	}
	for _, line := range lines {
		if n := ansi.StringWidth(ansi.Strip(line)); n > width {
			t.Errorf("width %d exceeds %d: %q", n, width, line)
		}
	}
}

func TestRenderGlanceSanitizingAndColor(t *testing.T) {
	hostile := "\x1b[31mRED\x1b[0m\x1b]8;;https://evil.test\x1b\\link\x1b]8;;\x1b\\\t\n\r\x07 世界 👩‍💻 👋🏽"
	if got := watchText(hostile); got != "REDlink     世界 👩‍💻 👋🏽" {
		t.Fatalf("sanitize = %q", got)
	}
	v := busyGlance()
	v.NeedsYou[0].Campaign = hostile
	v.NeedsYou[0].Text = hostile
	v.NeedsYou[0].PaneID = hostile
	v.NeedsYou[0].Also[0] = hostile
	v.NeedsYou[1].Items[0] = hostile
	v.Attention[0].Campaign = hostile
	v.Attention[0].Lane = hostile
	v.Attention[0].Text = hostile
	v.Campaigns[0].Name = hostile
	v.Campaigns[0].Last.Text = hostile
	for _, color := range []bool{false, true} {
		for _, width := range []int{1, 2, 12, 20, 46, 80} {
			rows := renderGlance(v, width, 24, 0, hostile, color, watchTestNow)
			checkWatchWidths(t, rows, width, 24)
			got := strings.Join(rows, "\n")
			if !color && strings.Contains(got, "\x1b") {
				t.Errorf("uncoloured output has ESC: %q", got)
			}
			if strings.Contains(got, "evil.test") {
				t.Error("OSC URL survived")
			}
			for _, row := range rows {
				for _, r := range ansi.Strip(row) {
					if unicode.IsControl(r) {
						t.Errorf("control rune %U survived", r)
					}
				}
			}
		}
	}
	rows := renderGlance(busyGlance(), 80, 24, 0, "", true, watchTestNow)
	if !strings.HasPrefix(rows[2], "\x1b[31m» ") || !strings.HasSuffix(rows[2], "\x1b[0m") || !strings.HasPrefix(rows[7], "\x1b[33m✗ ") {
		t.Fatalf("first-row colours: %q", rows)
	}
	if rows[3] != "  Merge #4840 now or wait for M3?" {
		t.Fatalf("indent = %q", rows[3])
	}
	for _, height := range []int{0, 1, 2, 4, 8} {
		checkWatchWidths(t, renderGlance(v, 20, height, 0, "", true, watchTestNow), 20, height)
	}
}

func TestRenderGlanceOwnerUnclearAndMissingSince(t *testing.T) {
	v := &glanceView{Verdict: "attention", Attention: []glanceAttention{
		{Kind: "owner_unclear", Campaign: "campaign", Text: "nothing urgent.\x1b[31m\n" + strings.Repeat("x", 100)},
		{Kind: "lead_unknown", Campaign: "campaign"},
		{Kind: "lead_unknown", Campaign: "observed", Since: stamp(watchTestNow.Add(-time.Minute)), AgeMS: time.Minute.Milliseconds()},
		{Kind: "results_waiting", Recipient: "lead", Count: 2},
	}}
	for _, color := range []bool{false, true} {
		rows := renderGlance(v, 46, 24, time.Second, "", color, watchTestNow)
		checkWatchWidths(t, rows, 46, 24)
		if ansi.Strip(rows[2]) != "? campaign  owner unclear" || ansi.Strip(rows[4]) != "? campaign  lead unknown never" || ansi.Strip(rows[6]) != "? observed  lead unknown 1m" || ansi.Strip(rows[8]) != "⌛ lead: 2 waiting" {
			t.Fatalf("attention ages: %q", rows)
		}
		if !strings.HasSuffix(rows[3], "…") || strings.Contains(ansi.Strip(rows[3]), "\x1b") || strings.ContainsAny(rows[3], "\n\r") {
			t.Fatalf("unclear text not sanitised and truncated: %q", rows[3])
		}
		if color && (!strings.HasPrefix(rows[2], "\x1b[33m? ") || strings.Contains(rows[0], "\x1b[32m")) {
			t.Fatalf("unclear colour: %q", rows)
		}
	}
	rows := renderGlance(busyGlance(), 80, 24, 0, "", false, watchTestNow)
	if rows[2] != "» copilot-modular  12m  BLOCKING  lane failed  → wN4:p1" {
		t.Fatalf("folded cue: %q", rows[2])
	}
}

func TestWatchAgesAndAttentionKinds(t *testing.T) {
	backlog := &glanceView{Verdict: "attention", Quiet: glanceQuiet{Count: 3, WithBacklog: 2}}
	if header := renderGlance(backlog, 46, 24, 0, "", false, watchTestNow)[0]; !strings.HasSuffix(header, "2 to check") {
		t.Fatalf("quiet backlog header: %q", header)
	}
	for _, tc := range []struct {
		d    time.Duration
		want string
	}{
		{-time.Second, "0s"}, {59 * time.Second, "59s"}, {time.Minute, "1m"}, {59 * time.Minute, "59m"}, {time.Hour, "1h"}, {47 * time.Hour, "47h"}, {48 * time.Hour, "2d"},
	} {
		if got := watchAge(tc.d); got != tc.want {
			t.Errorf("age %s: %s != %s", tc.d, got, tc.want)
		}
	}
	v := &glanceView{Verdict: "unknown"}
	for _, kind := range []string{"lane_failed", "lane_blocked", "lane_missing", "lane_unknown", "results_waiting", "host_stale", "daemon_unhealthy", "lead_gone", "lead_blocked", "lead_unknown"} {
		a := glanceAttention{Kind: kind, Host: "mac", Recipient: "lead", Count: 2}
		if strings.HasPrefix(kind, "lead_") {
			a.Campaign = "campaign"
		}
		v.Attention = append(v.Attention, a)
	}
	got := strings.Join(renderGlance(v, 80, 24, 0, "", false, watchTestNow), "\n")
	for _, want := range []string{"? unknown", "✗ mac  failed", "! mac  blocked", "? mac  missing", "? mac  unknown", "⌛ lead: 2 waiting", "⚠ mac  stale", "⚠ mac  daemon", "✗ campaign  lead gone", "! campaign  lead blocked", "? campaign  lead unknown"} {
		if !strings.Contains(got, want) {
			t.Errorf("missing %q: %s", want, got)
		}
	}
	header := renderGlance(busyGlance(), 25, 24, time.Hour, "", false, watchTestNow)[0]
	if strings.Contains(header, "· 1h") || !strings.HasSuffix(header, "2 need you") {
		t.Fatalf("header age fallback: %q", header)
	}
}

type watchOutput struct {
	mu      sync.Mutex
	b       bytes.Buffer
	changed chan struct{}
}

func (w *watchOutput) Write(p []byte) (int, error) {
	w.mu.Lock()
	n, err := w.b.Write(p)
	w.mu.Unlock()
	select {
	case w.changed <- struct{}{}:
	default:
	}
	return n, err
}
func (w *watchOutput) text() string { w.mu.Lock(); defer w.mu.Unlock(); return w.b.String() }
func (w *watchOutput) wait(t *testing.T, match func(string) bool) string {
	t.Helper()
	deadline := time.NewTimer(3 * time.Second)
	defer deadline.Stop()
	for {
		got := w.text()
		if match(got) {
			return got
		}
		select {
		case <-w.changed:
		case <-deadline.C:
			t.Fatalf("output wait timed out: %q", got)
		}
	}
}
func lastWatchFrame(s string) string {
	at := strings.LastIndex(s, "\x1b[1;1H")
	if at < 0 {
		return ""
	}
	return s[at:]
}

type watchHarness struct {
	out    *watchOutput
	input  *io.PipeWriter
	resize chan os.Signal
	cancel context.CancelFunc
	done   chan error
}

func newWatchHarness(t *testing.T, fetch func(context.Context) (*glanceView, error), every time.Duration, size func() (int, int), now func() time.Time) *watchHarness {
	t.Helper()
	t.Setenv("NO_COLOR", "1")
	cx, cancel := context.WithCancel(context.Background())
	in, input := io.Pipe()
	h := &watchHarness{out: &watchOutput{changed: make(chan struct{}, 1)}, input: input, resize: make(chan os.Signal, 1), cancel: cancel, done: make(chan error, 1)}
	cx = context.WithValue(cx, watchResizeKey{}, (<-chan os.Signal)(h.resize))
	go func() { h.done <- runWatch(cx, fetch, in, h.out, size, every, now) }()
	t.Cleanup(func() { cancel(); in.Close(); input.Close() })
	return h
}
func (h *watchHarness) stop(t *testing.T) {
	t.Helper()
	h.cancel()
	select {
	case err := <-h.done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("watch did not quit promptly")
	}
	if !strings.HasSuffix(h.out.text(), "\x1b[?25h\x1b[?1049l") {
		t.Fatal("missing restore sequence")
	}
}
func watchSize() (int, int)    { return 46, 24 }
func fixedWatchNow() time.Time { return watchTestNow }

func TestRunWatchQuitAndEOF(t *testing.T) {
	for _, key := range []string{"q", "Q", "\x03", "EOF", "cancel"} {
		t.Run(key, func(t *testing.T) {
			started, canceled := make(chan struct{}), make(chan struct{})
			h := newWatchHarness(t, func(cx context.Context) (*glanceView, error) {
				close(started)
				<-cx.Done()
				close(canceled)
				return nil, cx.Err()
			}, time.Hour, watchSize, fixedWatchNow)
			select {
			case <-started:
			case <-time.After(time.Second):
				t.Fatal("fetch not started")
			}
			before := time.Now()
			if key == "EOF" {
				h.input.Close()
			} else if key == "cancel" {
				h.cancel()
			} else {
				if _, err := io.WriteString(h.input, key); err != nil {
					t.Fatal(err)
				}
			}
			select {
			case err := <-h.done:
				if err != nil {
					t.Fatal(err)
				}
			case <-time.After(time.Second):
				t.Fatal("quit blocked on fetch")
			}
			if time.Since(before) > time.Second {
				t.Fatal("slow quit")
			}
			select {
			case <-canceled:
			case <-time.After(time.Second):
				t.Fatal("fetch not canceled")
			}
			got := h.out.text()
			if !strings.HasPrefix(got, "\x1b[?1049h\x1b[?25l") || !strings.HasSuffix(got, "\x1b[?25h\x1b[?1049l") {
				t.Fatalf("lifecycle = %q", got)
			}
		})
	}
}

func TestRunWatchStaleRecoveryAndSerialization(t *testing.T) {
	var calls, active atomic.Int32
	release := make(chan struct{}, 3)
	h := newWatchHarness(t, func(cx context.Context) (*glanceView, error) {
		if active.Add(1) != 1 {
			t.Error("overlapping fetches")
		}
		defer active.Add(-1)
		n := calls.Add(1)
		select {
		case <-release:
		case <-cx.Done():
			return nil, cx.Err()
		}
		if n == 2 {
			return nil, errors.New("server unreachable")
		}
		v := busyGlance()
		v.Campaigns[0].Last.Text = fmt.Sprintf("snapshot %d", n)
		return v, nil
	}, 20*time.Millisecond, watchSize, fixedWatchNow)
	release <- struct{}{}
	h.out.wait(t, func(s string) bool { return strings.Contains(lastWatchFrame(s), "snapshot 1") })
	release <- struct{}{}
	h.out.wait(t, func(s string) bool {
		f := lastWatchFrame(s)
		return strings.Contains(f, "stale") && strings.Contains(f, "snapshot 1")
	})
	release <- struct{}{}
	h.out.wait(t, func(s string) bool {
		f := lastWatchFrame(s)
		return !strings.Contains(f, "stale") && strings.Contains(f, "snapshot 3")
	})
	h.stop(t)
	if calls.Load() < 3 {
		t.Fatal("missing fetch sequence")
	}
}

func TestRunWatchResizeAndAge(t *testing.T) {
	var calls atomic.Int32
	var width, seconds atomic.Int64
	width.Store(46)
	h := newWatchHarness(t, func(context.Context) (*glanceView, error) { calls.Add(1); return busyGlance(), nil }, time.Hour, func() (int, int) { return int(width.Load()), 24 }, func() time.Time { return watchTestNow.Add(time.Duration(seconds.Load()) * time.Second) })
	h.out.wait(t, func(s string) bool { return strings.Contains(lastWatchFrame(s), "planner-ui") })
	width.Store(20)
	h.resize <- syscall.SIGWINCH
	h.out.wait(t, func(s string) bool {
		return strings.Contains(lastWatchFrame(s), "\x1b[2;1H\x1b[2K"+strings.Repeat("─", 20)+"\x1b[3;1H") && !strings.Contains(lastWatchFrame(s), strings.Repeat("─", 21))
	})
	if calls.Load() != 1 {
		t.Fatalf("resize fetched: %d", calls.Load())
	}
	width.Store(80)
	seconds.Store(12)
	h.out.wait(t, func(s string) bool { return strings.Contains(lastWatchFrame(s), "· 12s") })
	if calls.Load() != 1 {
		t.Fatal("redraw fetched")
	}
	h.stop(t)
}

type failWatchWriter struct {
	writes  int
	restore bool
}

func (w *failWatchWriter) Write(p []byte) (int, error) {
	w.writes++
	if strings.Contains(string(p), "\x1b[?1049l") {
		w.restore = true
		return len(p), nil
	}
	return 0, errors.New("write failed")
}
func TestRunWatchWriteErrorRestores(t *testing.T) {
	out := &failWatchWriter{}
	err := runWatch(context.Background(), func(context.Context) (*glanceView, error) { return nil, nil }, strings.NewReader(""), out, watchSize, time.Second, fixedWatchNow)
	if err == nil || !out.restore {
		t.Fatalf("err %v restore %v", err, out.restore)
	}
}

func TestGlanceWatchFlagsAndRPCRefusal(t *testing.T) {
	h := newHarness(t)
	for _, args := range [][]string{{"--watch=false"}, {"-watch=0"}, {"--watch=invalid"}, {"--", "--watch"}, {"watch"}} {
		if glanceWatchRequested(args) {
			t.Errorf("unexpected watch for %q", args)
		}
	}
	for _, args := range [][]string{{"--every", "5s"}, {"--watch", "--every", "0s"}, {"--watch", "--every", "6m"}, {"--watch=false", "--every", "1s"}} {
		h.one(exitUsage, nil, append([]string{"glance"}, args...)...)
	}
	for _, every := range []string{"1s", "5m"} {
		var out, errb bytes.Buffer
		if code := run([]string{"glance", "--watch", "--every", every}, h.getenv(nil), &out, &errb); code != 0 || strings.Contains(out.String(), "\x1b") || !strings.HasPrefix(out.String(), "taskr · ") {
			t.Fatalf("fallback %s: code %d %q %s", every, code, out.String(), errb.String())
		}
	}
	r := newTwoHost(t)
	status, reply, raw := r.post("host-a", rpcBody(t.TempDir(), nil, newRequestKey(), "glance", "--watch"))
	if status != 200 || reply.Exit != exitUsage || !strings.Contains(reply.Stdout, "glance --watch runs on the invoking host") {
		t.Fatalf("RPC watch: HTTP %d %+v %s", status, reply, raw)
	}
	r.caller.Store("host-a")
	for _, watch := range []string{"--watch", "-watch", "--watch=t", "-watch=T", "--watch=TRUE", "-watch=True", "--watch=1"} {
		var out, errb bytes.Buffer
		if code := cliMain([]string{"glance", watch}, clientEnv(r.homes["host-a"], nil), &out, &errb); code != 0 || !strings.HasPrefix(out.String(), "taskr · ") || strings.Contains(out.String(), "\x1b") {
			t.Fatalf("client fallback %s: %d %q %s", watch, code, out.String(), errb.String())
		}
	}
}

func TestRunWatchWaitsAfterFetchCompletion(t *testing.T) {
	started := make(chan time.Time, 8)
	release := make(chan struct{})
	every := 50 * time.Millisecond
	h := newWatchHarness(t, func(cx context.Context) (*glanceView, error) {
		started <- time.Now()
		select {
		case <-release:
		case <-cx.Done():
			return nil, cx.Err()
		}
		return busyGlance(), nil
	}, every, watchSize, fixedWatchNow)
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("fetch not started")
	}
	select {
	case <-started:
		t.Fatal("fetches overlap while pending")
	case <-time.After(2 * every):
	}
	completed := time.Now()
	close(release)
	select {
	case next := <-started:
		if next.Sub(completed) < every {
			t.Fatal("next fetch scheduled before completion + every")
		}
	case <-time.After(time.Second):
		t.Fatal("next fetch not scheduled")
	}
	h.stop(t)
}

func TestRunWatchFrameProtocol(t *testing.T) {
	h := newWatchHarness(t, func(context.Context) (*glanceView, error) { return nil, errors.New("offline") }, time.Hour, watchSize, fixedWatchNow)
	h.out.wait(t, func(s string) bool { return strings.Contains(lastWatchFrame(s), "no data: offline") })
	frame := lastWatchFrame(h.out.text())
	rows := renderGlance(nil, 46, 24, 0, "offline", false, watchTestNow)
	expected := ""
	for i := 1; i <= 24; i++ {
		expected += fmt.Sprintf("\x1b[%d;1H\x1b[2K", i)
		if i <= len(rows) {
			expected += rows[i-1]
		}
	}
	if frame != expected {
		t.Fatalf("frame protocol: %q != %q", frame, expected)
	}
	h.stop(t)
}

func TestRunWatchFullWidthRow(t *testing.T) {
	h := newWatchHarness(t, func(context.Context) (*glanceView, error) { return busyGlance(), nil }, time.Hour, watchSize, fixedWatchNow)
	h.out.wait(t, func(s string) bool { return strings.Contains(lastWatchFrame(s), "planner-ui") })
	frame := lastWatchFrame(h.out.text())
	for i, row := range renderGlance(busyGlance(), 46, 24, 0, "", false, watchTestNow) {
		if ansi.StringWidth(row) == 46 && !strings.Contains(frame, fmt.Sprintf("\x1b[%d;1H\x1b[2K%s\x1b[%d;1H", i+1, row, i+2)) {
			t.Fatalf("full-width row erased before cursor move: %q", frame)
		}
	}
	if strings.ContainsAny(frame, "\r\n") || strings.Contains(frame, "\x1b[K") || strings.Contains(frame, "\x1b[J") {
		t.Fatalf("unexpected erase or newline: %q", frame)
	}
	h.stop(t)
}

func TestRenderGlanceNarrowHeaders(t *testing.T) {
	for _, tc := range []struct {
		width     int
		err, want string
	}{
		{16, "", "17:30 2 need you"},
		{20, "", "17:30     2 need you"},
		{16, "offline", "stale 2s: offli…"},
		{20, "offline", "   stale 2s: offline"},
	} {
		header := renderGlance(busyGlance(), tc.width, 24, 2*time.Second, tc.err, false, watchTestNow)[0]
		if header != tc.want {
			t.Errorf("width %d err %q: %q != %q", tc.width, tc.err, header, tc.want)
		}
	}
}

func TestRenderGlanceUncertainHeadersNeverGreen(t *testing.T) {
	for _, tc := range []struct {
		v         *glanceView
		err, want string
	}{
		{&glanceView{Verdict: "rolling"}, "offline", "stale 2s: offline"},
		{nil, "", "no data"},
		{nil, "offline", "no data: offline"},
		{&glanceView{Verdict: "unknown"}, "", "? unknown"},
	} {
		header := renderGlance(tc.v, 46, 24, 2*time.Second, tc.err, true, watchTestNow)[0]
		if strings.Contains(header, "\x1b[32m") || !strings.Contains(header, "\x1b[33m"+tc.want) {
			t.Errorf("uncertain header: %q", header)
		}
	}
}

func TestRenderGlanceOverflowSkipsUnhelpfulDrop(t *testing.T) {
	v := busyGlance()
	v.Campaigns = v.Campaigns[:1]
	v.Quiet = glanceQuiet{}
	rows := renderGlance(v, 46, 12, 0, "", false, watchTestNow)
	got := strings.Join(rows, "\n")
	checkWatchWidths(t, rows, 46, 12)
	for _, want := range []string{"planner-ui", "+1 more to check", "mobile-screens", "Merge #4840"} {
		if !strings.Contains(got, want) {
			t.Errorf("overflow missing %q: %s", want, got)
		}
	}
}

func TestRenderGlanceMoreCampaignsThanHeightKeepsCounts(t *testing.T) {
	v := busyGlance()
	v.Campaigns = nil
	for i := 0; i < 20; i++ {
		v.Campaigns = append(v.Campaigns, glanceCampaign{Name: fmt.Sprintf("campaign-%d", i)})
	}
	rows := renderGlance(v, 46, 8, 0, "", false, watchTestNow)
	checkWatchWidths(t, rows, 46, 8)
	got := strings.Join(rows, "\n")
	for _, want := range []string{"2 need you", "+2 more need you", "+2 more to check", "+20 more campaigns", "13 quiet", "4 with backlog"} {
		if !strings.Contains(got, want) {
			t.Errorf("overflow missing %q: %s", want, got)
		}
	}
}

func TestRenderGlanceTinyHeightKeepsHeaderVerdict(t *testing.T) {
	for height := 1; height <= 4; height++ {
		rows := renderGlance(busyGlance(), 46, height, 0, "", false, watchTestNow)
		checkWatchWidths(t, rows, 46, height)
		if !strings.HasSuffix(rows[0], "2 need you") {
			t.Errorf("height %d lost header verdict: %q", height, rows)
		}
		if height >= 2 && !strings.Contains(rows[1], "+2 more need you") {
			t.Errorf("height %d lost needs-you priority: %q", height, rows)
		}
		if height >= 3 && !strings.Contains(rows[2], "+2 more to check") {
			t.Errorf("height %d lost attention priority: %q", height, rows)
		}
		if height >= 4 && !strings.Contains(rows[3], "+3 more campaigns") {
			t.Errorf("height %d lost campaigns priority: %q", height, rows)
		}
	}
}

func TestRunWatchFetchPanic(t *testing.T) {
	h := newWatchHarness(t, func(context.Context) (*glanceView, error) { panic("boom") }, time.Hour, watchSize, fixedWatchNow)
	h.out.wait(t, func(s string) bool { return strings.Contains(lastWatchFrame(s), "no data: fetch panic: boom") })
	h.stop(t)
}

func TestGlanceWatchRPCLogging(t *testing.T) {
	r := newTwoHost(t)
	for _, tc := range []struct {
		key  string
		args []string
		exit int
	}{
		{"watch-log-success", []string{"glance"}, 0},
		{"watch-log-failure", []string{"glance", "--bad-flag"}, exitUsage},
		{"watch-log-other", []string{"status"}, 0},
	} {
		status, reply, raw := r.post("host-a", rpcBody(t.TempDir(), nil, tc.key, tc.args...))
		if status != 200 || reply.Exit != tc.exit {
			t.Fatalf("%s: HTTP %d %+v %s", tc.key, status, reply, raw)
		}
	}
	logText, err := os.ReadFile(filepath.Join(r.stateDir(), "daemon.log"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(logText), "key=watch-log-success") || !strings.Contains(string(logText), "cmd=glance key=watch-log-failure exit=2") || !strings.Contains(string(logText), "cmd=status key=watch-log-other exit=0") {
		t.Fatalf("RPC audit log: %s", logText)
	}
}

func TestGlanceWatchClientFetchDeadline(t *testing.T) {
	r := newTwoHost(t)
	canceled := make(chan struct{}, 1)
	var calls atomic.Int32
	endpoint, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
		calls.Add(1)
		io.Copy(io.Discard, q.Body)
		<-q.Context().Done()
		canceled <- struct{}{}
	})
	go srv.Serve(ln)
	var out bytes.Buffer
	start := time.Now()
	_, code, err := watchGlance(&ctx{client: true, server: endpoint, out: &out}, 100*time.Millisecond)
	if code != 1 || err == nil || calls.Load() != 1 || time.Since(start) > time.Second {
		t.Fatalf("deadline: code %d err %v calls %d elapsed %s", code, err, calls.Load(), time.Since(start))
	}
	select {
	case <-canceled:
	case <-time.After(time.Second):
		t.Fatal("hanging request was not canceled")
	}
}

func TestGlanceWatchClientFetchErrors(t *testing.T) {
	for _, kind := range []string{"transport", "decode"} {
		t.Run(kind, func(t *testing.T) {
			r := newTwoHost(t)
			var calls atomic.Int32
			endpoint, ln, srv := retryEndpoint(t, r, func(w http.ResponseWriter, q *http.Request) {
				calls.Add(1)
				var req rpcRequest
				if err := json.NewDecoder(q.Body).Decode(&req); err != nil {
					t.Error(err)
				}
				if strings.Join(req.Argv, " ") != "--json glance" || req.RequestKey == "" {
					t.Errorf("fetch request: %+v", req)
				}
				if kind == "transport" {
					w.WriteHeader(http.StatusServiceUnavailable)
					return
				}
				json.NewEncoder(w).Encode(rpcReply{Exit: exitOK, Stdout: "invalid snapshot JSON"})
			})
			go srv.Serve(ln)
			var out, errb bytes.Buffer
			if code := cliMain([]string{"glance", "--watch"}, clientEnv(r.clientHome(endpoint), nil), &out, &errb); code != 1 || calls.Load() != 1 || strings.Contains(errb.String(), "retrying") {
				t.Fatalf("fetch failure: code %d calls %d %s %s", code, calls.Load(), out.String(), errb.String())
			}
		})
	}
}
