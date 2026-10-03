package main

import (
	"database/sql"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// The daemon's own record in meta, written when it takes the lock.
const (
	daemonPIDKey       = "daemon_pid"
	daemonVersionKey   = "daemon_version"
	daemonStartedKey   = "daemon_started_at"
	daemonExeKey       = "daemon_exe"        // its resolved executable path
	daemonProcStartKey = "daemon_proc_start" // its start time as the OS reports it
)

// procIdent is what the OS says about a process: the executable it runs,
// its exact argv, its start time and its effective uid.
type procIdent struct {
	Exe   string
	Argv  []string
	Start string
	UID   int
}

const clientDaemonRecordFile = "client-daemon.json"

const localDaemonRecordFile = "daemon.json"

type clientDaemonRecord struct {
	PID          int      `json:"pid"`
	Executable   string   `json:"executable"`
	Argv         []string `json:"argv"`
	StartTime    string   `json:"start_time"`
	UID          *int     `json:"uid"`
	Version      string   `json:"version"`
	StartedAt    string   `json:"started_at"`
	Stay         bool     `json:"stay,omitempty"`
	Supervised   bool     `json:"supervised,omitempty"`
	HerdrMissing bool     `json:"herdr_missing,omitempty"`
}

type daemonIdentityRecord struct {
	PID   int
	Exe   string
	Argv  []string
	Start string
	UID   *int
}

// resolveExe makes an executable path comparable: absolute, symlinks resolved.
func resolveExe(p string) (string, error) {
	abs, err := filepath.Abs(p)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(abs)
}

// replacedExe undoes Linux's " (deleted)" suffix on /proc/<pid>/exe, which
// the kernel adds once the file at that path was replaced, as install.sh's
// mv does on every upgrade. The path is still the one the process was started
// from; the start time and argv checks still decide.
func replacedExe(link string) string { return strings.TrimSuffix(link, " (deleted)") }

// selfRecord is the identity a starting daemon writes to meta. A part the
// OS cannot tell is recorded empty, which later makes --restart refuse.
func selfRecord() map[string]string {
	rec := map[string]string{daemonPIDKey: strconv.Itoa(os.Getpid()), daemonExeKey: "", daemonProcStartKey: ""}
	if exe, err := os.Executable(); err == nil {
		if r, err := resolveExe(exe); err == nil {
			rec[daemonExeKey] = r
		}
	}
	if id, err := procIdentity(os.Getpid()); err == nil {
		rec[daemonProcStartKey] = id.Start
	}
	return rec
}

func selfClientDaemonRecord() (clientDaemonRecord, error) {
	id, err := procIdentity(os.Getpid())
	if err != nil {
		return clientDaemonRecord{}, err
	}
	exe, err := resolveExe(id.Exe)
	if err != nil {
		return clientDaemonRecord{}, err
	}
	if id.Start == "" || len(id.Argv) < 2 || id.Argv[1] != "daemon" {
		return clientDaemonRecord{}, errors.New("process is not taskr daemon")
	}
	return clientDaemonRecord{PID: os.Getpid(), Executable: exe, Argv: id.Argv, StartTime: id.Start,
		UID: &id.UID, Version: version, StartedAt: now()}, nil
}

func readClientDaemonRecord(path string) (clientDaemonRecord, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return clientDaemonRecord{}, err
	}
	var rec clientDaemonRecord
	if err := json.Unmarshal(b, &rec); err != nil {
		return clientDaemonRecord{}, err
	}
	return rec, nil
}

func writeClientDaemonRecord(path string, rec clientDaemonRecord) error {
	b, err := json.Marshal(rec)
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(path), "."+filepath.Base(path)+"-*")
	if err != nil {
		return err
	}
	name := f.Name()
	ok := false
	defer func() {
		f.Close()
		if !ok {
			os.Remove(name)
		}
	}()
	if err := f.Chmod(0o600); err != nil {
		return err
	}
	if _, err := f.Write(append(b, '\n')); err != nil {
		return err
	}
	if err := f.Sync(); err != nil {
		return err
	}
	if err := f.Close(); err != nil {
		return err
	}
	if err := os.Rename(name, path); err != nil {
		return err
	}
	ok = true
	return nil
}

// verifyDaemon establishes that pid is the daemon that wrote meta's record:
// the lock names it, meta names it, and the OS reports the recorded
// executable, argv[1] exactly "daemon", the recorded start time (a reused
// pid differs here) and this uid. It returns why not, never the other
// process's command line.
func verifyDaemon(q queryer, lockPID int) (int, error) {
	metaPID := 0
	if v, ok, err := getMeta(q, daemonPIDKey); err != nil {
		return 0, err
	} else if ok {
		metaPID, _ = strconv.Atoi(v)
	}
	switch {
	case lockPID <= 0:
		return 0, rejectErr("the daemon lock is held but records no pid; nothing signalled")
	case lockPID == os.Getpid():
		return 0, rejectErr("the daemon lock is held by this process %d; nothing signalled", lockPID)
	case metaPID != lockPID:
		return 0, rejectErr("the daemon lock names pid %d but the ledger records daemon pid %d (a daemon before v0.6 records none); "+
			"not signalled: stop it by hand, then run daemon --restart", lockPID, metaPID)
	}
	exe, _, _ := getMeta(q, daemonExeKey)
	start, _, _ := getMeta(q, daemonProcStartKey)
	if exe == "" || start == "" {
		return 0, rejectErr("the ledger holds no executable or start time for daemon %d; not signalled", lockPID)
	}
	return verifyDaemonIdentity(daemonIdentityRecord{PID: metaPID, Exe: exe, Start: start}, lockPID)
}

func verifyClientDaemon(rec clientDaemonRecord, lockPID int) (int, error) {
	switch {
	case lockPID <= 0:
		return 0, rejectErr("the daemon lock is held but records no pid; nothing signalled")
	case lockPID == os.Getpid():
		return 0, rejectErr("the daemon lock is held by this process %d; nothing signalled", lockPID)
	case rec.PID != lockPID:
		return 0, rejectErr("the daemon lock names pid %d but the identity record names daemon pid %d; not signalled", lockPID, rec.PID)
	case rec.Executable == "" || rec.StartTime == "" || len(rec.Argv) < 2 || rec.UID == nil:
		return 0, rejectErr("the client daemon identity record is incomplete for daemon %d; not signalled", lockPID)
	}
	return verifyDaemonIdentity(daemonIdentityRecord{PID: rec.PID, Exe: rec.Executable, Argv: rec.Argv,
		Start: rec.StartTime, UID: rec.UID}, lockPID)
}

func verifyDaemonIdentity(rec daemonIdentityRecord, lockPID int) (int, error) {
	id, err := procIdentity(lockPID)
	if err != nil {
		return 0, rejectErr("cannot read the identity of pid %d (%v); not signalled", lockPID, err)
	}
	cur, err := resolveExe(id.Exe)
	switch {
	case err != nil || cur != rec.Exe:
		return 0, rejectErr("pid %d does not run the recorded daemon executable; not signalled", lockPID)
	case len(id.Argv) < 2 || id.Argv[1] != "daemon":
		return 0, rejectErr("pid %d is not running `taskr daemon`; not signalled", lockPID)
	case rec.Argv != nil && !slices.Equal(id.Argv, rec.Argv):
		return 0, rejectErr("pid %d has a different argv than the recorded client daemon; not signalled", lockPID)
	case id.Start != rec.Start:
		return 0, rejectErr("pid %d started at a different time than the recorded daemon (a reused pid); not signalled", lockPID)
	case rec.UID != nil && id.UID != *rec.UID:
		return 0, rejectErr("pid %d belongs to a different user than the recorded daemon; not signalled", lockPID)
	case id.UID != os.Getuid():
		return 0, rejectErr("pid %d belongs to another user; not signalled", lockPID)
	}
	return lockPID, nil
}

// Restart timing and the binary to start; package variables so tests can
// shrink them and start a built taskr instead of the test binary.
var (
	daemonRestartWait = 10 * time.Second
	daemonStartWait   = 10 * time.Second
	daemonExecutable  = os.Executable
)

func btoi(b bool) int {
	if b {
		return 1
	}
	return 0
}

type daemonRec struct {
	version, started string
	args             []string
	stay, supervised bool
	herdrMissing     bool
	procStart        string
}

// runningDaemon reads the meta record of the daemon with this pid; version is
// "unknown" when the record is missing or belongs to another pid.
func runningDaemon(q queryer, pid int, dir string) (daemonRec, error) {
	rec := daemonRec{version: "unknown"}
	p, ok, err := getMeta(q, daemonPIDKey)
	if err != nil || !ok || p != strconv.Itoa(pid) {
		return rec, err
	}
	if v, ok, err := getMeta(q, daemonVersionKey); err != nil {
		return rec, err
	} else if ok {
		rec.version = v
	}
	s, _, err := getMeta(q, daemonStartedKey)
	rec.started = s
	if identity, e := readClientDaemonRecord(filepath.Join(dir, localDaemonRecordFile)); e == nil && identity.PID == pid && len(identity.Argv) >= 2 {
		rec.args = slices.Clone(identity.Argv[2:])
		rec.stay, rec.supervised, rec.procStart = identity.Stay, identity.Supervised, identity.StartTime
		rec.herdrMissing = identity.HerdrMissing
	}
	return rec, err
}

// lockHeld reports whether a process holds the daemon lock. It takes the lock
// for an instant when it is free; it never creates or deletes the file.
func lockHeld(path string) (bool, error) {
	f, err := os.OpenFile(path, os.O_RDWR, 0)
	if errors.Is(err, os.ErrNotExist) {
		return false, nil
	} else if err != nil {
		return false, err
	}
	defer f.Close()
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		if errors.Is(err, syscall.EWOULDBLOCK) {
			return true, nil
		}
		return false, err
	}
	syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
	return false, nil
}

// daemonRestart stops the running daemon (only the one verifyDaemon
// establishes), waits for its lock, and starts this binary's daemon
// detached with a minimal environment built here: HOME, PATH and
// HERDR_SOCKET_PATH, nothing inherited. Without a running daemon it only
// starts one. The lock file is never deleted.
func daemonRestart(c *ctx, lockPath string) (any, int, error) {
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer db.Close()
	return daemonRestartWith(c, lockPath,
		func() (string, error) {
			sock, ok, err := getMeta(db, "daemon_socket")
			if err != nil {
				return "", dbErr(err)
			}
			if !ok || sock == "" {
				sock = socketPath(c)
			}
			return sock, nil
		},
		func(pid int) (int, error) {
			verified, err := verifyDaemon(db, pid)
			if err != nil {
				return 0, err
			}
			path := filepath.Join(filepath.Dir(lockPath), localDaemonRecordFile)
			if rec, err := readClientDaemonRecord(path); err == nil {
				verified, err := verifyClientDaemon(rec, pid)
				if err != nil {
					return 0, rejectErr("local daemon identity record %s: %v", path, err)
				}
				return verified, nil
			} else if !errors.Is(err, os.ErrNotExist) {
				return 0, rejectErr("cannot read the local daemon identity record %s; not signalled", path)
			}
			return verified, nil // older local daemons recorded only meta
		},
		func(pid int) (daemonRec, error) { return runningDaemon(db, pid, filepath.Dir(lockPath)) })
}

func clientDaemonRestart(c *ctx, dir, lockPath string) (any, int, error) {
	path := filepath.Join(dir, clientDaemonRecordFile)
	return daemonRestartWith(c, lockPath,
		func() (string, error) { return socketPath(c), nil },
		func(pid int) (int, error) {
			rec, err := readClientDaemonRecord(path)
			if err != nil {
				return 0, rejectErr("cannot read the client daemon identity record (%v); not signalled", err)
			}
			return verifyClientDaemon(rec, pid)
		},
		func(pid int) (daemonRec, error) {
			rec, err := readClientDaemonRecord(path)
			if errors.Is(err, os.ErrNotExist) {
				return daemonRec{version: "unknown"}, nil
			}
			if err != nil {
				return daemonRec{}, err
			}
			if rec.PID != pid || rec.Version == "" || len(rec.Argv) < 2 {
				return daemonRec{version: "unknown"}, nil
			}
			return daemonRec{version: rec.Version, started: rec.StartedAt, args: slices.Clone(rec.Argv[2:])}, nil
		})
}

func daemonRestartWith(c *ctx, lockPath string, socket func() (string, error),
	verify func(int) (int, error), record func(int) (daemonRec, error)) (any, int, error) {
	out := map[string]any{"ok": true, "restarted": false}
	args := []string{"daemon"}
	held, err := lockHeld(lockPath)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	if held {
		pid, err := verify(lockPID(lockPath))
		if err != nil {
			return map[string]any{"pid": lockPID(lockPath)}, 0, err
		}
		old, oldErr := record(pid)
		if oldErr != nil {
			return nil, 0, dbErr(oldErr)
		}
		args = append(args, old.args...)
		if err := syscall.Kill(pid, syscall.SIGTERM); err != nil {
			return nil, 0, rejectErr("stopping daemon %d: %v", pid, err)
		}
		out["old_pid"], out["restarted"] = pid, true
		out["old_version"] = old.version
		if old.supervised {
			// The supervisor owns relaunch. Accept only a verified lock holder with a new OS start time.
			for end := time.Now().Add(daemonRestartWait); time.Now().Before(end); time.Sleep(50 * time.Millisecond) {
				newPID := lockPID(lockPath)
				if held, e := lockHeld(lockPath); e != nil || !held || newPID <= 0 {
					continue
				}
				if _, e := verify(newPID); e != nil {
					continue
				}
				next, e := record(newPID)
				if e == nil && next.procStart != "" && next.procStart != old.procStart {
					out["new_pid"], out["version"], out["started_at"] = newPID, next.version, next.started
					out["supervised"], out["started_detached"] = true, false
					out["socket"], _ = socket()
					return out, exitOK, nil
				}
			}
			out["started_detached"] = true
		}
		freed := false
		for end := time.Now().Add(daemonRestartWait); time.Now().Before(end); time.Sleep(50 * time.Millisecond) {
			if held, err := lockHeld(lockPath); err == nil && !held {
				freed = true
				break
			}
		}
		if !freed {
			return out, 0, rejectErr("daemon %d did not release its lock within %v; nothing started", pid, daemonRestartWait)
		}
	}
	sock, err := socket()
	if err != nil {
		return nil, 0, err
	}
	exe, err := daemonExecutable()
	if err != nil {
		return out, 0, usageErr("cannot find this taskr binary: %v", err)
	}
	home := c.env("HOME")
	env := []string{"HOME=" + home, "HERDR_SOCKET_PATH=" + sock}
	if p := c.env("PATH"); p != "" {
		env = append(env, "PATH="+p)
	}
	cmd := exec.Command(exe, args...)
	cmd.Env, cmd.Dir = env, home
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	if err := cmd.Start(); err != nil {
		return out, 0, herdrErr("starting the daemon: %v", err)
	}
	newPID := cmd.Process.Pid
	exited := make(chan error, 1)
	go func() { exited <- cmd.Wait() }()
	out["new_pid"], out["socket"] = newPID, sock
	for end := time.Now().Add(daemonStartWait); ; time.Sleep(50 * time.Millisecond) {
		select {
		case err := <-exited:
			out["lock_pid"] = lockPID(lockPath)
			return out, 0, rejectErr("the new daemon %d exited at start (%v); `taskr daemon --status` shows who holds the lock", newPID, err)
		default:
		}
		rec, err := record(newPID)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return out, 0, dbErr(err)
		}
		if rec.version != "unknown" {
			out["version"], out["started_at"] = rec.version, rec.started
			return out, exitOK, nil
		}
		if time.Now().After(end) {
			return out, 0, rejectErr("the new daemon %d did not record itself within %v; check daemon.log", newPID, daemonStartWait)
		}
	}
}
