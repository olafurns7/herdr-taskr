package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"testing"
	"time"
)

func (r *restartRig) clientMode(t *testing.T) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(r.lock), 0o755); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(filepath.Dir(r.lock), serverURLFile)
	if err := os.WriteFile(path, []byte("http://[::1]:1\n"), 0o644); err != nil {
		t.Fatal(err)
	}
}

func (r *restartRig) runClient(args ...string) (int, map[string]any) {
	var out, errb bytes.Buffer
	env := func(k string) string {
		switch k {
		case "TASKR_DB":
			return ""
		case "HOME":
			return r.h.dir
		case "HERDR_SOCKET_PATH":
			return r.s.path
		case "PATH":
			return os.Getenv("PATH")
		default:
			return r.env[k]
		}
	}
	code := contractCLIMain(r.h.t, append([]string{"--json"}, args...), env, &out, &errb)
	lines := bytes.Split(bytes.TrimSpace(out.Bytes()), []byte("\n"))
	var result map[string]any
	if len(lines) == 0 || json.Unmarshal(lines[len(lines)-1], &result) != nil {
		r.h.t.Fatalf("client taskr %v: output %q; stderr %q", args, out.String(), errb.String())
	}
	return code, result
}

func TestClientDaemonRestart(t *testing.T) {
	contractGuard(t)
	fakeTailnetHooks(t)
	r := newRestartRig(t)
	r.clientMode(t)
	old, oldDone := r.startOld(t)
	eventually(t, "client daemon's lock", func() bool { return lockPID(r.lock) == old.Process.Pid })
	path := filepath.Join(filepath.Dir(r.lock), clientDaemonRecordFile)
	rec, err := readClientDaemonRecord(path)
	if err != nil || rec.PID != old.Process.Pid || rec.Version != builtVersion || len(rec.Argv) < 2 || rec.Argv[1] != "daemon" || rec.UID == nil {
		t.Fatalf("client daemon record = %+v (%v)", rec, err)
	}
	if st, err := os.Stat(path); err != nil || st.Mode().Perm() != 0o600 {
		t.Fatalf("client daemon record mode = %v (%v)", st, err)
	}

	code, out := r.runClient("daemon", "--restart")
	if code != exitOK {
		t.Fatalf("client restart = %d %v", code, out)
	}
	newPID := int(num(out, "new_pid"))
	stopPID(t, newPID, r.lock)
	if int(num(out, "old_pid")) != old.Process.Pid || newPID == old.Process.Pid || out["version"] != builtVersion ||
		out["old_version"] != builtVersion || out["socket"] != r.s.path || out["restarted"] != true {
		t.Fatalf("client restart output = %v", out)
	}
	select {
	case <-oldDone:
	case <-time.After(10 * time.Second):
		t.Fatal("the old client daemon did not exit on SIGTERM")
	}
	newRec, err := readClientDaemonRecord(path)
	if err != nil || newRec.PID != newPID || newRec.Version != builtVersion {
		t.Fatalf("restarted client daemon record = %+v (%v)", newRec, err)
	}
	_, st := r.runClient("daemon", "--status")
	if st["running_version"] != builtVersion || st["stale"] != true || st["started_at"] == nil {
		t.Fatalf("client daemon status = %v", st)
	}
	if temps, _ := filepath.Glob(filepath.Join(filepath.Dir(path), "."+filepath.Base(path)+"-*")); len(temps) != 0 {
		t.Fatalf("atomic record write left temp files: %v", temps)
	}

	if err := syscall.Kill(newPID, syscall.SIGTERM); err != nil {
		t.Fatal(err)
	}
	eventually(t, "client daemon clean-exit record removal", func() bool {
		_, err := os.Stat(path)
		return errors.Is(err, os.ErrNotExist)
	})
}

func TestClientDaemonRestartRefusesUnverifiedRecord(t *testing.T) {
	contractGuard(t)
	fakeTailnetHooks(t)
	for _, tc := range []struct {
		name   string
		mutate func(*testing.T, string, clientDaemonRecord)
	}{
		{"missing", func(t *testing.T, path string, _ clientDaemonRecord) {
			if err := os.Remove(path); err != nil {
				t.Fatal(err)
			}
		}},
		{"pid", func(t *testing.T, path string, rec clientDaemonRecord) {
			rec.PID++
			writeClientDaemonRecordOrFail(t, path, rec)
		}},
		{"start time", func(t *testing.T, path string, rec clientDaemonRecord) {
			rec.StartTime = "1.000000"
			writeClientDaemonRecordOrFail(t, path, rec)
		}},
		{"argv", func(t *testing.T, path string, rec clientDaemonRecord) {
			rec.Argv[0] += ".other"
			writeClientDaemonRecordOrFail(t, path, rec)
		}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			r := newRestartRig(t)
			r.clientMode(t)
			old, oldDone := r.startOld(t)
			eventually(t, "client daemon's lock", func() bool { return lockPID(r.lock) == old.Process.Pid })
			path := filepath.Join(filepath.Dir(r.lock), clientDaemonRecordFile)
			rec, err := readClientDaemonRecord(path)
			if err != nil {
				t.Fatal(err)
			}
			tc.mutate(t, path, rec)

			if tc.name == "missing" {
				_, status := r.runClient("daemon", "--status")
				if status["running_version"] != "unknown" || status["stale"] != true {
					t.Fatalf("status without identity record = %v", status)
				}
			}
			code, out := r.runClient("daemon", "--restart")
			if code != exitReject || out["kind"] != "rejected" {
				t.Fatalf("client restart = %d %v, want refusal", code, out)
			}
			time.Sleep(150 * time.Millisecond)
			select {
			case <-oldDone:
				t.Fatal("client restart signalled the daemon with an unverified record")
			default:
			}
			if syscall.Kill(old.Process.Pid, 0) != nil || lockPID(r.lock) != old.Process.Pid {
				t.Fatal("client restart stopped the daemon or changed its lock")
			}
		})
	}
}

func writeClientDaemonRecordOrFail(t *testing.T, path string, rec clientDaemonRecord) {
	t.Helper()
	if err := writeClientDaemonRecord(path, rec); err != nil {
		t.Fatal(err)
	}
}

func TestClientDaemonStatusUsesIdentityRecord(t *testing.T) {
	contractGuard(t)
	r := newRestartRig(t)
	r.clientMode(t)
	path := filepath.Join(filepath.Dir(r.lock), clientDaemonRecordFile)
	uid := os.Getuid()
	if err := writeClientDaemonRecord(path, clientDaemonRecord{PID: 424242, Version: "v0.11.0", UID: &uid}); err != nil {
		t.Fatal(err)
	}
	r.holdLock(t, os.Getpid())
	_, status := r.runClient("daemon", "--status")
	if status["running"] != true || status["running_version"] != "unknown" || status["stale"] != true {
		t.Fatalf("client status with mismatched pid = %v", status)
	}
}

func TestClientDaemonIdentityFailureDoesNotStopDaemon(t *testing.T) {
	contractGuard(t)
	fakeTailnetHooks(t)
	r := newRestartRig(t)
	r.clientMode(t)
	path := filepath.Join(filepath.Dir(r.lock), clientDaemonRecordFile)
	uid := os.Getuid()
	if err := writeClientDaemonRecord(path, clientDaemonRecord{PID: os.Getpid(), Executable: r.bin,
		Argv: []string{r.bin, "daemon"}, StartTime: "stale", UID: &uid, Version: builtVersion}); err != nil {
		t.Fatal(err)
	}

	cmd := exec.Command(r.bin, "--json", "daemon")
	cmd.Env = []string{"HOME=" + r.h.dir, "PATH=" + os.Getenv("PATH"), "HERDR_SOCKET_PATH=" + r.s.path}
	output, err := os.CreateTemp(t.TempDir(), "client-daemon-output")
	if err != nil {
		t.Fatal(err)
	}
	outputPath := output.Name()
	cmd.Stdout = output
	if err := cmd.Start(); err != nil {
		output.Close()
		t.Fatal(err)
	}
	done := make(chan struct{})
	var waitErr error
	go func() { waitErr = cmd.Wait(); close(done) }()
	t.Cleanup(func() {
		select {
		case <-done:
			return
		default:
		}
		_ = cmd.Process.Signal(syscall.SIGTERM)
		select {
		case <-done:
		case <-time.After(10 * time.Second):
			_ = cmd.Process.Kill()
			<-done
			t.Errorf("client daemon did not exit after SIGTERM")
		}
	})
	if err := output.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case <-done:
		b, _ := os.ReadFile(outputPath)
		t.Fatalf("`taskr --json daemon` exited at start (%v): %s", waitErr, b)
	case <-time.After(2 * time.Second):
	}
	if lockPID(r.lock) != cmd.Process.Pid {
		t.Fatalf("client daemon did not hold its lock: pid %d", cmd.Process.Pid)
	}
	b, err := os.ReadFile(outputPath)
	var started map[string]any
	if err != nil || json.Unmarshal(bytes.TrimSpace(b), &started) != nil ||
		started["mode"] != "client" || started["pid"] != float64(cmd.Process.Pid) {
		t.Fatalf("client daemon start output = %s (%v)", b, err)
	}
	if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("stale client daemon identity record remains: %v", err)
	}

	code, out := r.runClient("daemon", "--restart")
	if code != exitReject || out["kind"] != "rejected" {
		t.Fatalf("client restart without identity record = %d %v, want refusal", code, out)
	}
	time.Sleep(150 * time.Millisecond)
	select {
	case <-done:
		t.Fatal("client restart signalled the daemon without an identity record")
	default:
	}
	if lockPID(r.lock) != cmd.Process.Pid {
		t.Fatal("client restart changed the daemon lock without an identity record")
	}
	if err := cmd.Process.Signal(syscall.SIGTERM); err != nil {
		t.Fatal(err)
	}
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatal("client daemon did not exit on SIGTERM")
	}
	if waitErr != nil {
		t.Fatalf("client daemon exit = %v", waitErr)
	}
	if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("client daemon identity record after exit = %v", err)
	}
}
