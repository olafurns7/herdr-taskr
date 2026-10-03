package main

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestMain(m *testing.M) {
	dbPollInterval = 50 * time.Millisecond
	dashboardDefaultAddr = "127.0.0.1:0" // never the owner's 7788
	tailscaleFallbacks = nil             // never the real tailscale: every harness puts a fake first on PATH
	code := m.Run()
	if buildDir != "" {
		os.RemoveAll(buildDir)
	}
	os.Exit(code)
}

// harness is one temp ledger plus a fake herdr on PATH. The real herdr is
// never reachable from tests: the fake directory is first on PATH.
type harness struct {
	t      *testing.T
	dir    string
	db     string
	bin    string
	mu     sync.Mutex
	stderr string // taskr's stderr from the last run, under mu

	herdrSock string // a live stand-in Herdr server: accepts and closes
}

const fakeHerdr = `#!/bin/sh
d="$(dirname "$0")"
printf '%s|' "$@" >> "$d/calls.log"
echo >> "$d/calls.log"
case "$1 $2" in
"workspace list")
  echo '{"result":{"workspaces":[]}}'
  exit 0 ;;
"agent list")
  [ -f "$d/list.sleep" ] && sleep "$(cat "$d/list.sleep")"
  if [ -f "$d/list.hold" ]; then sleep 10 & wait; fi
  cat "$d/list.json"
  exit "$(cat "$d/list.exit" 2>/dev/null || echo 0)" ;;
"agent prompt")
  [ -f "$d/prompt.stderr" ] && cat "$d/prompt.stderr" >&2
  [ -f "$d/prompt.stdout" ] && cat "$d/prompt.stdout"
  exit "$(cat "$d/prompt.exit" 2>/dev/null || echo 0)" ;;
"agent read")
  if [ -f "$d/read.hold" ]; then sleep 10 & wait; fi
  if [ -f "$d/read.$3" ]; then cat "$d/read.$3"; exit 0; fi
  err='{"error":{"code":"agent_not_idle"}}'
  printf 'stderr|%s|%s\n' "$3" "$err" >> "$d/calls.log"
  echo "$err" >&2
  exit 1 ;;
"notification show")
  exit "$(cat "$d/notify.exit" 2>/dev/null || echo 0)" ;;
"pane report-metadata")
  exit "$(cat "$d/meta.exit" 2>/dev/null || echo 0)" ;;
*)
  echo '{"error":{"code":"unexpected_call"}}' >&2
  exit 2 ;;
esac
`

func newHarness(t *testing.T) *harness {
	dir := t.TempDir()
	h := &harness{t: t, dir: dir, db: filepath.Join(dir, "state", "taskr.db"), bin: filepath.Join(dir, "bin")}
	if err := os.MkdirAll(h.bin, 0o755); err != nil {
		t.Fatal(err)
	}
	h.write("herdr", fakeHerdr, 0o755)
	h.write("tailscale", fakeTailscale, 0o755)
	h.setAgents()
	h.herdrSock = liveHerdrSocket(t)
	t.Setenv("PATH", h.bin+string(os.PathListSeparator)+os.Getenv("PATH"))
	return h
}

// liveHerdrSocket listens on a short socket path (macOS caps them near 104
// bytes) and closes every connection: enough for taskr's server check, so
// the fake herdr CLI runs. Tests of a missing server use their own path.
func liveHerdrSocket(t *testing.T) string {
	dir, err := os.MkdirTemp("", "hs")
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "h.sock")
	ln, err := net.Listen("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			c.Close()
		}
	}()
	t.Cleanup(func() { ln.Close(); os.RemoveAll(dir) })
	return path
}

func (h *harness) write(name, content string, mode os.FileMode) {
	h.t.Helper()
	if err := os.WriteFile(filepath.Join(h.bin, name), []byte(content), mode); err != nil {
		h.t.Fatal(err)
	}
}

// setAgents writes the fake `herdr agent list` output: pane:status:seq triples.
func (h *harness) setAgents(specs ...string) {
	var agents []map[string]any
	for _, s := range specs {
		var pane, status string
		var seq int
		parts := strings.Split(s, "/")
		pane, status = parts[0], parts[1]
		fmt.Sscan(parts[2], &seq)
		agents = append(agents, map[string]any{"agent": "claude", "name": "a-" + strings.ReplaceAll(pane, ":", "-"),
			"agent_status": status, "state_change_seq": seq, "pane_id": pane, "workspace_id": strings.Split(pane, ":")[0]})
	}
	if agents == nil {
		agents = []map[string]any{}
	}
	b, _ := json.Marshal(map[string]any{"id": "cli:agent:list", "result": map[string]any{"agents": agents}})
	h.write("list.json", string(b), 0o644)
}

func (h *harness) lastStderr() string {
	h.mu.Lock()
	defer h.mu.Unlock()
	return h.stderr
}

func (h *harness) calls(prefix string) []string {
	b, _ := os.ReadFile(filepath.Join(h.bin, "calls.log"))
	var out []string
	for _, l := range strings.Split(string(b), "\n") {
		if strings.HasPrefix(l, prefix) {
			out = append(out, l)
		}
	}
	return out
}

func (h *harness) getenv(env map[string]string) func(string) string {
	return func(k string) string {
		switch k {
		case "TASKR_DB":
			return h.db
		case "HOME":
			return h.dir
		case "HERDR_SOCKET_PATH":
			if v, ok := env[k]; ok {
				return v
			}
			return h.herdrSock
		}
		return env[k]
	}
}

// run invokes taskr in-process and decodes each stdout line.
func (h *harness) run(env map[string]string, args ...string) (int, []map[string]any) {
	h.t.Helper()
	var out, errb bytes.Buffer
	code := run(append([]string{"--json"}, args...), h.getenv(env), &out, &errb)
	h.mu.Lock()
	h.stderr = errb.String()
	h.mu.Unlock()
	var lines []map[string]any
	for _, l := range strings.Split(strings.TrimSpace(out.String()), "\n") {
		if l == "" {
			continue
		}
		var m map[string]any
		if err := json.Unmarshal([]byte(l), &m); err != nil {
			h.t.Fatalf("taskr %v: stdout line is not JSON: %q", args, l)
		}
		lines = append(lines, m)
	}
	return code, lines
}

// one runs a command that must print exactly one object and exit with want.
func (h *harness) one(want int, env map[string]string, args ...string) map[string]any {
	h.t.Helper()
	code, lines := h.run(env, args...)
	if len(lines) != 1 {
		h.t.Fatalf("taskr %v: want one JSON object, got %d", args, len(lines))
	}
	if code != want {
		h.t.Fatalf("taskr %v: exit %d, want %d; output %v", args, code, want, lines[0])
	}
	return lines[0]
}

func (h *harness) ok(env map[string]string, args ...string) map[string]any {
	h.t.Helper()
	return h.one(exitOK, env, args...)
}

func (h *harness) newTask(name, role string, parent int64, extra ...string) int64 {
	h.t.Helper()
	args := []string{"new", name, "--role", role, "--cwd", h.dir}
	if parent != 0 {
		args = append(args, "--parent", fmt.Sprint(parent))
	}
	return num(h.ok(nil, append(args, extra...)...), "task_id")
}

func (h *harness) launch(task int64, extra ...string) int64 {
	h.t.Helper()
	args := []string{"launch", fmt.Sprint(task), "--provider", "claude", "--model", "claude-opus-5-5", "--effort", "high"}
	return num(h.ok(nil, append(args, extra...)...), "launch_id")
}

func as(task, launch int64) map[string]string {
	env := map[string]string{"TASKR_TASK": fmt.Sprint(task)}
	if launch != 0 {
		env["TASKR_LAUNCH"] = fmt.Sprint(launch)
	}
	return env
}

func num(m map[string]any, k string) int64 {
	f, _ := m[k].(float64)
	return int64(f)
}

func eventOf(m map[string]any) map[string]any {
	ev, _ := m["event"].(map[string]any)
	return ev
}

func id(v int64) string { return fmt.Sprint(v) }

func (h *harness) openDB() *sql.DB {
	h.t.Helper()
	db, err := openDB(&ctx{getenv: h.getenv(nil), out: io.Discard, errw: io.Discard})
	if err != nil {
		h.t.Fatal(err)
	}
	h.t.Cleanup(func() { db.Close() })
	return db
}

func (h *harness) countHerdr(recipient int64) int {
	h.t.Helper()
	var n int
	if err := h.openDB().QueryRow(`select count(*) from events where kind = 'herdr' and recipient_task_id = ?`, recipient).Scan(&n); err != nil {
		h.t.Fatal(err)
	}
	return n
}
