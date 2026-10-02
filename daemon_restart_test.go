package main

import (
	"bytes"
	"encoding/json"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"
)

const builtVersion = "v0.6.0-restart-test"

var (
	buildOnce sync.Once
	builtBin  string
	buildErr  error
	buildDir  string
)

// buildTaskr builds this package once per test run, so a restart starts a
// real taskr daemon rather than the test binary.
func buildTaskr(t *testing.T) string {
	t.Helper()
	buildOnce.Do(func() {
		buildDir, buildErr = os.MkdirTemp("", "taskr-bin")
		if buildErr != nil {
			return
		}
		builtBin = filepath.Join(buildDir, "taskr")
		out, err := exec.Command("go", "build", "-o", builtBin, "-ldflags", "-X main.version="+builtVersion, ".").CombinedOutput()
		if err != nil {
			buildErr = err
			builtBin = string(out)
		}
	})
	if buildErr != nil {
		t.Fatalf("go build: %v %s", buildErr, builtBin)
	}
	return builtBin
}

// envHerdr records the environment of whoever runs it, per parent pid, and
// otherwise answers like the fake herdr.
const envHerdr = `#!/bin/sh
d="$(dirname "$0")"
env > "$d/env.$PPID.tmp" && mv "$d/env.$PPID.tmp" "$d/env.$PPID"
case "$1 $2" in
"agent list") cat "$d/list.json" ;;
esac
exit 0
`

// restartRig is a HOME with no TASKR_DB override (a restarted daemon gets
// only HOME, PATH and HERDR_SOCKET_PATH, so it finds the ledger by HOME).
type restartRig struct {
	h    *harness
	s    *fakeSocket
	bin  string
	env  map[string]string
	lock string

	lockFile *os.File // the lock as held by this test, once holdLock took it
}

func newRestartRig(t *testing.T) *restartRig {
	h := newHarness(t)
	h.write("herdr", envHerdr, 0o755)
	r := &restartRig{h: h, s: newFakeSocket(t), bin: buildTaskr(t),
		lock: filepath.Join(h.dir, ".local", "state", "taskr", "daemon.lock")}
	r.env = map[string]string{"HOME": h.dir, "PATH": os.Getenv("PATH"), "HERDR_SOCKET_PATH": r.s.path}
	prev := daemonExecutable
	daemonExecutable = func() (string, error) { return r.bin, nil }
	t.Cleanup(func() { daemonExecutable = prev })
	return r
}

func (r *restartRig) getenv(over map[string]string) func(string) string {
	return func(k string) string {
		if v, ok := over[k]; ok {
			return v
		}
		return r.env[k]
	}
}

func (r *restartRig) run(over map[string]string, args ...string) (int, map[string]any) {
	r.h.t.Helper()
	var out bytes.Buffer
	code := run(append([]string{"--json"}, args...), r.getenv(over), &out, io.Discard)
	var m map[string]any
	lines := strings.Split(strings.TrimSpace(out.String()), "\n")
	if err := json.Unmarshal([]byte(lines[len(lines)-1]), &m); err != nil {
		r.h.t.Fatalf("taskr %v: %q", args, out.String())
	}
	return code, m
}

// startOld runs a taskr daemon subprocess with a polluted environment.
func (r *restartRig) startOld(t *testing.T) (*exec.Cmd, chan struct{}) {
	cmd := exec.Command(r.bin, "daemon")
	cmd.Env = []string{"HOME=" + r.h.dir, "PATH=" + os.Getenv("PATH"), "HERDR_SOCKET_PATH=" + r.s.path, "CLAUDECODE=1"}
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	done := make(chan struct{})
	go func() { cmd.Wait(); close(done) }()
	t.Cleanup(func() {
		cmd.Process.Signal(syscall.SIGTERM)
		select {
		case <-done:
		case <-time.After(10 * time.Second):
			cmd.Process.Kill()
		}
	})
	c, _, _ := r.s.next()
	c.Write([]byte(ack))
	eventually(t, "old daemon's pass", func() bool { return exists(filepath.Join(r.h.bin, "env."+strconv.Itoa(cmd.Process.Pid))) })
	return cmd, done
}

func stopPID(t *testing.T, pid int, lock string) {
	t.Cleanup(func() {
		syscall.Kill(pid, syscall.SIGTERM)
		for end := time.Now().Add(10 * time.Second); time.Now().Before(end); time.Sleep(20 * time.Millisecond) {
			if held, _ := lockHeld(lock); !held {
				return
			}
		}
		syscall.Kill(pid, syscall.SIGKILL)
	})
}

func readEnvFile(t *testing.T, path string) map[string]string {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	env := map[string]string{}
	for _, l := range strings.Split(strings.TrimSpace(string(b)), "\n") {
		k, v, _ := strings.Cut(l, "=")
		env[k] = v
	}
	return env
}

func TestDaemonRestartReplacesWithMinimalEnv(t *testing.T) {
	r := newRestartRig(t)
	root := num(func() map[string]any {
		_, m := r.run(nil, "new", "top", "--role", "orchestrator", "--cwd", r.h.dir)
		return m
	}(), "task_id")
	_, m := r.run(nil, "new", "w", "--role", "implementer", "--parent", id(root), "--pane", "w9:p1", "--cwd", r.h.dir)
	r.run(nil, "launch", id(num(m, "task_id")), "--provider", "claude", "--model", "m", "--effort", "e")
	r.h.setAgents("w9:p1/working/1")
	old, oldDone := r.startOld(t)
	eventually(t, "old daemon's lock", func() bool { return lockPID(r.lock) == old.Process.Pid })
	before, err := os.Stat(r.lock)
	if err != nil {
		t.Fatal(err)
	}

	// The restarting shell is an agent's: none of this may reach the daemon.
	for k, v := range map[string]string{"CLAUDECODE": "1", "CLAUDE_CODE_CHILD_SESSION": "1", "CLAUDE_CONFIG_DIR": "/c",
		"CODEX_HOME": "/x", "CODEX_THREAD_ID": "t", "TASKR_TASK": "42", "TASKR_LAUNCH": "7"} {
		t.Setenv(k, v)
	}
	code, out := r.run(map[string]string{"HERDR_SOCKET_PATH": "/nonexistent/other.sock", "TASKR_TASK": "42"}, "daemon", "--restart")
	if code != 0 {
		t.Fatalf("restart = %d %v", code, out)
	}
	newPID := int(num(out, "new_pid"))
	stopPID(t, newPID, r.lock)
	if int(num(out, "old_pid")) != old.Process.Pid || newPID == old.Process.Pid || out["version"] != builtVersion ||
		out["old_version"] != builtVersion || out["socket"] != r.s.path || out["restarted"] != true {
		t.Fatalf("restart output = %v", out)
	}
	select {
	case <-oldDone:
	case <-time.After(10 * time.Second):
		t.Fatal("the old daemon did not exit on SIGTERM")
	}

	c, _, _ := r.s.next()
	c.Write([]byte(ack))
	envPath := filepath.Join(r.h.bin, "env."+strconv.Itoa(newPID))
	eventually(t, "new daemon's herdr call", func() bool { return exists(envPath) })
	env := readEnvFile(t, envPath)
	t.Logf("new daemon %d environment as its herdr saw it: %v", newPID, env)
	allowed := map[string]bool{"HOME": true, "PATH": true, "HERDR_SOCKET_PATH": true,
		"PWD": true, "OLDPWD": true, "SHLVL": true, "_": true, "__CF_USER_TEXT_ENCODING": true} // the last five: sh and macOS
	for k := range env {
		if !allowed[k] || strings.HasPrefix(k, "CLAUDE") || strings.HasPrefix(k, "CODEX") || strings.HasPrefix(k, "TASKR_") {
			t.Errorf("new daemon environment carries %s", k)
		}
	}
	if env["HOME"] != r.h.dir || env["PATH"] != os.Getenv("PATH") || env["HERDR_SOCKET_PATH"] != r.s.path {
		t.Fatalf("new daemon environment = %v", env)
	}

	after, err := os.Stat(r.lock)
	if err != nil || !os.SameFile(before, after) || lockPID(r.lock) != newPID {
		t.Fatalf("lock file replaced or not taken: err %v same %v pid %d", err, err == nil && os.SameFile(before, after), lockPID(r.lock))
	}
	// This binary is "dev": the running one is stale.
	_, st := r.run(nil, "daemon", "--status")
	if st["running_version"] != builtVersion || st["stale"] != true || st["started_at"] == nil || int(num(st, "pid")) != newPID {
		t.Fatalf("status = %v", st)
	}
}

func TestDaemonRestartWithoutDaemonStartsOne(t *testing.T) {
	r := newRestartRig(t)
	code, out := r.run(nil, "daemon", "--restart")
	if code != 0 {
		t.Fatalf("restart without a daemon = %d %v", code, out)
	}
	newPID := int(num(out, "new_pid"))
	stopPID(t, newPID, r.lock)
	if out["restarted"] != false || out["old_pid"] != nil || out["version"] != builtVersion || lockPID(r.lock) != newPID {
		t.Fatalf("restart output = %v", out)
	}
}

// holdLock takes the daemon lock in this process and records pid in it, as
// a daemon would.
func (r *restartRig) holdLock(t *testing.T, pid int) *os.File {
	t.Helper()
	f := r.lockFile
	if f == nil {
		os.MkdirAll(filepath.Dir(r.lock), 0o755)
		var err error
		if f, err = os.OpenFile(r.lock, os.O_RDWR|os.O_CREATE, 0o644); err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { f.Close() })
		if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
			t.Fatal(err)
		}
		r.lockFile = f
	}
	f.Truncate(0)
	f.WriteAt([]byte(strconv.Itoa(pid)+"\n"), 0)
	return f
}

// refused runs --restart, which must refuse fast (no 10 s wait for a lock
// after a signal) and never echo the other process's command line.
func (r *restartRig) refused(t *testing.T, want string) map[string]any {
	t.Helper()
	began := time.Now()
	code, out := r.run(nil, "daemon", "--restart")
	msg, _ := out["error"].(string)
	if code != exitReject || !strings.Contains(msg, want) || out["new_pid"] != nil || out["old_pid"] != nil {
		t.Fatalf("restart = %d %v, want a refusal containing %q", code, out, want)
	}
	if d := time.Since(began); d > 5*time.Second {
		t.Fatalf("refusal took %v: something was signalled and waited on", d)
	}
	b, _ := json.Marshal(out)
	for _, leak := range []string{"argv", "sleep", "unrelated", "decoy"} {
		if strings.Contains(string(b), leak) {
			t.Fatalf("refusal output carries the process's command line (%s): %s", leak, b)
		}
	}
	return out
}

func TestDaemonRestartRefusesNonTaskrPID(t *testing.T) {
	r := newRestartRig(t)
	sleeper := exec.Command("sleep", "30")
	if err := sleeper.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { sleeper.Process.Kill(); sleeper.Wait() })
	f := r.holdLock(t, sleeper.Process.Pid)
	// No daemon record in the ledger names it (as with a v0.5 daemon).
	r.refused(t, "not signalled")
	if syscall.Kill(sleeper.Process.Pid, 0) != nil || !exists(r.lock) {
		t.Fatal("the non-taskr process was signalled or the lock file removed")
	}
	// A lock held by this very process is never signalled either.
	f.Truncate(0)
	f.WriteAt([]byte(strconv.Itoa(os.Getpid())+"\n"), 0)
	r.refused(t, "this process")
	if code, _ := r.run(nil, "daemon", "--restart", "--status"); code != exitUsage {
		t.Fatalf("--restart with --status = %d", code)
	}
}

// lookalikeC ignores SIGTERM but records it, so a signal is observable.
const lookalikeC = `#include <signal.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdlib.h>
static void term(int s){int f=open("term-seen",O_WRONLY|O_CREAT,0600);if(f>=0)close(f);}
int main(){chdir(getenv("HOME"));signal(SIGTERM,term);int f=open("ready",O_WRONLY|O_CREAT,0600);close(f);for(;;)pause();}`

// startLookalike compiles and runs an inert program named taskr-unrelated.
func (r *restartRig) startLookalike(t *testing.T, args ...string) (*exec.Cmd, string) {
	t.Helper()
	if _, err := exec.LookPath("cc"); err != nil {
		t.Skip("no C compiler for the look-alike process")
	}
	fake := filepath.Join(r.h.bin, "taskr-unrelated")
	if !exists(fake) {
		cc := exec.Command("cc", "-x", "c", "-o", fake, "-")
		cc.Stdin = strings.NewReader(lookalikeC)
		if out, err := cc.CombinedOutput(); err != nil {
			t.Fatalf("compile: %v %s", err, out)
		}
	}
	os.Remove(filepath.Join(r.h.dir, "ready"))
	cmd := exec.Command(fake, args...)
	cmd.Env = []string{"HOME=" + r.h.dir, "PATH=" + os.Getenv("PATH")}
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { cmd.Process.Kill(); cmd.Wait() })
	eventually(t, "look-alike ready", func() bool { return exists(filepath.Join(r.h.dir, "ready")) })
	return cmd, fake
}

func (r *restartRig) recordDaemon(t *testing.T, pid int, exe, start string) {
	t.Helper()
	db, err := openDB(&ctx{getenv: r.getenv(nil), out: io.Discard, errw: io.Discard})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	for k, v := range map[string]string{daemonPIDKey: strconv.Itoa(pid), daemonExeKey: exe, daemonProcStartKey: start} {
		if err := setMeta(db, k, v); err != nil {
			t.Fatal(err)
		}
	}
}

// The reviewer's probe: an unrelated program whose name contains taskr and
// whose argv holds daemon, under a held lock naming it. Before v0.6's fix it
// received SIGTERM; now nothing is signalled whatever the ledger claims,
// unless every part of the recorded identity matches.
func TestDaemonRestartRefusesLookalike(t *testing.T) {
	r := newRestartRig(t)
	cmd, fake := r.startLookalike(t, "daemon")
	pid := cmd.Process.Pid
	r.holdLock(t, pid)
	id, err := procIdentity(pid)
	if err != nil {
		t.Fatal(err)
	}
	realExe, _ := resolveExe(r.bin)
	fakeExe, _ := resolveExe(fake)
	cases := []struct{ name, exe, start, want string }{
		{"no record", "", "", "records daemon pid 0"},
		{"taskr's executable recorded", realExe, id.Start, "recorded daemon executable"},
		{"reused pid", fakeExe, "1.000000", "reused pid"},
		{"empty start", fakeExe, "", "no executable or start time"},
	}
	for _, c := range cases {
		if c.name != "no record" {
			r.recordDaemon(t, pid, c.exe, c.start)
		}
		r.refused(t, c.want)
		if exists(filepath.Join(r.h.dir, "term-seen")) {
			t.Fatalf("%s: the look-alike received SIGTERM", c.name)
		}
	}
	// argv[1] must be exactly daemon: `taskr-unrelated x daemon` with its own
	// executable and start recorded is still refused.
	cmd2, _ := r.startLookalike(t, "x", "daemon")
	r.holdLock(t, cmd2.Process.Pid)
	id2, _ := procIdentity(cmd2.Process.Pid)
	r.recordDaemon(t, cmd2.Process.Pid, fakeExe, id2.Start)
	r.refused(t, "not running `taskr daemon`")
	if exists(filepath.Join(r.h.dir, "term-seen")) {
		t.Fatal("argv look-alike received SIGTERM")
	}
}

// A real taskr daemon whose recorded start time differs (its pid reused by
// a later process, as far as the ledger can tell) is not signalled.
func TestDaemonRestartRefusesReusedPID(t *testing.T) {
	r := newRestartRig(t)
	root := num(func() map[string]any {
		_, m := r.run(nil, "new", "top", "--role", "orchestrator", "--cwd", r.h.dir)
		return m
	}(), "task_id")
	_, m := r.run(nil, "new", "w", "--role", "implementer", "--parent", id(root), "--pane", "w9:p1", "--cwd", r.h.dir)
	r.run(nil, "launch", id(num(m, "task_id")), "--provider", "claude", "--model", "m", "--effort", "e")
	old, oldDone := r.startOld(t)
	eventually(t, "old daemon's lock", func() bool { return lockPID(r.lock) == old.Process.Pid })
	db, _ := openDB(&ctx{getenv: r.getenv(nil), out: io.Discard, errw: io.Discard})
	defer db.Close()
	exe, _, _ := getMeta(db, daemonExeKey)
	start, _, _ := getMeta(db, daemonProcStartKey)
	realExe, _ := resolveExe(r.bin)
	if exe != realExe || start == "" {
		t.Fatalf("daemon record: exe %q (want %q) start %q", exe, realExe, start)
	}
	setMeta(db, daemonProcStartKey, "1.000000")
	r.refused(t, "reused pid")
	select {
	case <-oldDone:
		t.Fatal("the daemon was stopped")
	case <-time.After(300 * time.Millisecond):
	}
	if held, _ := lockHeld(r.lock); !held || lockPID(r.lock) != old.Process.Pid {
		t.Fatal("the daemon lost its lock")
	}
}

// The upgrade case: install.sh moves a new binary over the old daemon's
// executable (a new inode at the same path) while the old daemon runs. The
// path is still the recorded one, so --restart replaces it.
func TestDaemonRestartAfterBinaryReplaced(t *testing.T) {
	r := newRestartRig(t)
	own := filepath.Join(r.h.dir, "inst", "taskr")
	copyFile := func(dst string) {
		t.Helper()
		b, err := os.ReadFile(r.bin)
		if err != nil {
			t.Fatal(err)
		}
		os.MkdirAll(filepath.Dir(dst), 0o755)
		if err := os.WriteFile(dst, b, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	copyFile(own)
	r.bin = own
	_, m := r.run(nil, "new", "top", "--role", "orchestrator", "--cwd", r.h.dir)
	_, m = r.run(nil, "new", "w", "--role", "implementer", "--parent", id(num(m, "task_id")), "--pane", "w9:p1", "--cwd", r.h.dir)
	r.run(nil, "launch", id(num(m, "task_id")), "--provider", "claude", "--model", "m", "--effort", "e")
	r.h.setAgents("w9:p1/working/1")
	old, oldDone := r.startOld(t)
	eventually(t, "old daemon's lock", func() bool { return lockPID(r.lock) == old.Process.Pid })
	before, _ := os.Stat(own)
	copyFile(own + ".new")
	if err := os.Rename(own+".new", own); err != nil { // install.sh: mv -f
		t.Fatal(err)
	}
	if after, _ := os.Stat(own); os.SameFile(before, after) {
		t.Fatal("the binary was not replaced by a new file")
	}
	code, out := r.run(nil, "daemon", "--restart")
	if code != 0 || int(num(out, "old_pid")) != old.Process.Pid {
		t.Fatalf("restart after an upgrade = %d %v", code, out)
	}
	stopPID(t, int(num(out, "new_pid")), r.lock)
	<-oldDone
}

func TestReplacedExe(t *testing.T) {
	for in, want := range map[string]string{
		"/home/u/.local/bin/taskr (deleted)": "/home/u/.local/bin/taskr",
		"/home/u/.local/bin/taskr":           "/home/u/.local/bin/taskr",
		"/x/taskr (deleted) (deleted)":       "/x/taskr (deleted)",
	} {
		if got := replacedExe(in); got != want {
			t.Errorf("replacedExe(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestDaemonStatusStale(t *testing.T) {
	h := newHarness(t)
	s := newFakeSocket(t)
	h.startDaemon(s)
	s.next()
	st := h.ok(nil, "daemon", "--status")
	if st["running_version"] != version || st["stale"] != false || st["started_at"] == nil {
		t.Fatalf("status of this build's daemon = %v", st)
	}
	db := h.openDB()
	setMeta(db, daemonVersionKey, "v0.5.0")
	if st := h.ok(nil, "daemon", "--status"); st["running_version"] != "v0.5.0" || st["stale"] != true {
		t.Fatalf("status of an older daemon = %v", st)
	}
	// A record left by another pid (a v0.5 daemon writes none): unknown, stale.
	setMeta(db, daemonPIDKey, "1")
	if st := h.ok(nil, "daemon", "--status"); st["running_version"] != "unknown" || st["stale"] != true || st["started_at"] != nil {
		t.Fatalf("status without this pid's record = %v", st)
	}
}

func TestProcIdentitySelf(t *testing.T) {
	id, err := procIdentity(os.Getpid())
	if err != nil {
		t.Fatal(err)
	}
	exe, _ := os.Executable()
	want, _ := resolveExe(exe)
	got, _ := resolveExe(id.Exe)
	if got != want || id.UID != os.Getuid() || id.Start == "" || strings.Join(id.Argv, " ") != strings.Join(os.Args, " ") {
		t.Fatalf("identity of this process = %+v (exe %q, want %q, uid %d)", id, got, want, os.Getuid())
	}
	if again, _ := procIdentity(os.Getpid()); again.Start != id.Start {
		t.Fatalf("start time is not stable: %q then %q", id.Start, again.Start)
	}
	child := exec.Command("sleep", "5")
	child.Start()
	defer func() { child.Process.Kill(); child.Wait() }()
	var cid procIdent
	deadline := time.Now().Add(2 * time.Second)
	for {
		cid, err = procIdentity(child.Process.Pid)
		if (len(cid.Argv) > 0 && cid.Argv[0] != "") || !time.Now().Before(deadline) {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	if len(cid.Argv) == 0 {
		t.Fatalf("identity of a child = %+v %v", cid, err)
	}
	if err != nil || cid.Start == id.Start || cid.Argv[0] != "sleep" {
		t.Fatalf("identity of a child = %+v %v", cid, err)
	}
	if _, err := procIdentity(1 << 30); err == nil {
		t.Fatal("identity of a pid that cannot exist")
	}
}
