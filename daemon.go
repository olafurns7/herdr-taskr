package main

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"maps"
	"net"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

// Daemon timing. Package variables so tests can shrink them.
var (
	daemonSettle    = 100 * time.Millisecond // a wake waits this long for the rest of its burst
	daemonMinGap    = 500 * time.Millisecond // at most one pass per gap while dirty
	daemonHeartbeat = 15 * time.Second
	daemonFallback  = 60 * time.Second
	daemonRetryBase = 500 * time.Millisecond
	daemonRetryCap  = 30 * time.Second
	daemonLogMax    = int64(1 << 20)
)

// heartbeatFresh is how long a daemon heartbeat lets wait skip its own poll.
const heartbeatFresh = 30 * time.Second

const heartbeatKey = "daemon_heartbeat"

// globalKinds are the Herdr events the daemon subscribes to by type alone.
// Status changes need a pane_id per subscription (Herdr 0.9.1 answers
// "invalid_request: missing field pane_id" otherwise), so the request adds one
// statusKind entry per watched pane. Payloads are never parsed: every line is
// only a hint to take a fresh snapshot.
var globalKinds = []string{"pane.exited", "pane.closed", "pane.agent_detected"}

const statusKind = "pane.agent_status_changed"

// watchedPanes is the status subscription set of host's launches: the panes
// of the liveness watch list, waiting tasks included, less launches last
// observed missing (a pane Herdr no longer knows must not keep the request
// failing).
func watchedPanes(q queryer, host sql.NullString) ([]string, error) {
	rows, err := q.Query(`select distinct coalesce(l.pane_id, t.pane_id) as p
		from tasks t join launches l on l.id = t.current_launch_id
		where t.parent_id is not null and t.status != 'closed' and t.role != 'gate'
		and coalesce(l.pane_id, t.pane_id) is not null and l.present = 1 and l.machine is ? order by p`, host)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var panes []string
	for rows.Next() {
		var p string
		if err := rows.Scan(&p); err != nil {
			return nil, err
		}
		panes = append(panes, p)
	}
	return panes, rows.Err()
}

// subscribeRequest is the one line the daemon ever writes on a stream.
func subscribeRequest(panes []string) []byte {
	var subs []map[string]string
	for _, k := range globalKinds {
		subs = append(subs, map[string]string{"type": k})
	}
	for _, p := range panes {
		subs = append(subs, map[string]string{"type": statusKind, "pane_id": p})
	}
	req, _ := json.Marshal(map[string]any{"id": "taskr-daemon", "method": "events.subscribe",
		"params": map[string]any{"subscriptions": subs}})
	return append(req, '\n')
}

func getMeta(q queryer, key string) (string, bool, error) {
	var v sql.NullString
	err := q.QueryRow(`select value from meta where key = ?`, key).Scan(&v)
	if errors.Is(err, sql.ErrNoRows) {
		return "", false, nil
	}
	return v.String, err == nil, err
}

func setMeta(db *sql.DB, key, value string) error {
	_, err := db.Exec(`insert into meta (key, value) values (?, ?)
		on conflict(key) do update set value = excluded.value`, key, value)
	return err
}

// daemonState reads the heartbeat: "none" without one, "fresh" under
// heartbeatFresh, otherwise "stale" (an unparsable value is stale).
func daemonState(q queryer) (state, at string, age time.Duration, err error) {
	at, ok, err := getMeta(q, heartbeatKey)
	if err != nil || !ok {
		return "none", "", 0, err
	}
	age = time.Since(parseTime(at))
	if age < heartbeatFresh {
		return "fresh", at, age, nil
	}
	return "stale", at, age, nil
}

func daemonFresh(q queryer) (bool, error) {
	state, _, _, err := daemonState(q)
	return state == "fresh", dbErr(err)
}

func daemonRecord(q queryer) (map[string]any, error) {
	state, at, age, err := daemonState(q)
	if err != nil {
		return nil, err
	}
	m := map[string]any{"record": "daemon", "daemon": state}
	if at != "" {
		m["heartbeat_at"], m["heartbeat_age_ms"] = at, age.Milliseconds()
	}
	return m, nil
}

func stateDir(c *ctx) (string, error) {
	home := c.env("HOME")
	if home == "" {
		return "", usageErr("HOME is not set")
	}
	return filepath.Join(home, ".local", "state", "taskr"), nil
}

func socketPath(c *ctx) string {
	if p := c.env("HERDR_SOCKET_PATH"); p != "" {
		return p
	}
	return filepath.Join(c.env("HOME"), ".config", "herdr", "herdr.sock")
}

func exists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

func cmdDaemon(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("daemon", flag.ContinueOnError)
	once := fs.Bool("once", false, "run one observation pass and exit")
	status := fs.Bool("status", false, "print heartbeat age, pid and socket path")
	restart := fs.Bool("restart", false, "stop the running daemon and start this binary's, detached")
	stay := fs.Bool("stay", false, "keep the local ledger serving without Herdr; wait for the daemon lock")
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		return nil, 0, err
	}
	if n := btoi(*once) + btoi(*status) + btoi(*restart); n > 1 {
		return nil, 0, usageErr("daemon: --once, --status and --restart are exclusive")
	}
	if *stay && (*once || *status || *restart) {
		return nil, 0, usageErr("daemon: --stay cannot be combined with --once, --status or --restart")
	}
	dir, err := stateDir(c)
	if err != nil {
		return nil, 0, err
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, 0, dbErr(err)
	}
	lockPath, logPath := filepath.Join(dir, "daemon.lock"), filepath.Join(dir, "daemon.log")
	if *status {
		return daemonStatus(c, dir, lockPath)
	}
	if *restart {
		return daemonRestart(c, lockPath)
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer db.Close()
	lg := openDaemonLog(logPath)
	defer lg.close()
	d := &daemon{db: db, log: lg, sock: socketPath(c), stay: *stay}
	if *once {
		r := d.pass()
		out := map[string]any{"ok": r.err == nil, "once": true, "observed": r.observed, "notified": r.notified}
		return out, exitOK, r.err
	}

	// Stay can wait behind a plugin daemon; signals must also cancel that wait.
	// Do not undo nohup's inherited SIGHUP ignore from the plugin hook.
	sigs := []os.Signal{os.Interrupt, syscall.SIGTERM}
	if !signal.Ignored(syscall.SIGHUP) {
		sigs = append(sigs, syscall.SIGHUP)
	}
	parent := c.cx
	if parent == nil {
		parent = context.Background()
	}
	sig, stop := signal.NotifyContext(parent, sigs...)
	defer stop()
	lock, pid, err := acquireLock(lockPath)
	for *stay && err == nil && lock == nil {
		if !sleepCx(sig, daemonRetryBase) {
			return nil, exitOK, nil
		}
		lock, pid, err = acquireLock(lockPath)
	}
	if err != nil {
		return nil, 0, dbErr(fmt.Errorf("daemon lock: %w", err))
	}
	if lock == nil {
		return map[string]any{"ok": true, "already_running": true, "pid": pid}, exitOK, nil
	}
	defer func() {
		lock.Truncate(0) // a clean exit leaves no pid for --status to probe
		lock.Close()
	}()
	sock := socketPath(c)
	lg.logf("start pid %d version %s socket %s", os.Getpid(), version, sock)
	if *stay {
		if _, err := exec.LookPath("herdr"); err != nil {
			d.herdrMissing = true
			lg.logf("herdr missing; PATH=%s", os.Getenv("PATH"))
		}
	}
	rec := selfRecord()
	rec["daemon_socket"], rec[daemonVersionKey], rec[daemonStartedKey] = sock, version, now()
	uid := os.Getuid()
	identity := clientDaemonRecord{PID: os.Getpid(), Executable: rec[daemonExeKey], StartTime: rec[daemonProcStartKey],
		Argv: append([]string{rec[daemonExeKey], "daemon"}, args...), UID: &uid, Version: version, StartedAt: rec[daemonStartedKey],
		Stay: *stay, Supervised: c.env("INVOCATION_ID") != "" && c.env("SYSTEMD_EXEC_PID") == strconv.Itoa(os.Getpid()),
		HerdrMissing: d.herdrMissing}
	if id, err := procIdentity(os.Getpid()); err == nil && len(id.Argv) >= 2 && id.Argv[1] == "daemon" {
		identity.Argv = id.Argv
	}
	identityPath := filepath.Join(dir, localDaemonRecordFile)
	if err := writeClientDaemonRecord(identityPath, identity); err != nil {
		return nil, 0, dbErr(err)
	}
	defer os.Remove(identityPath)
	if err := withTx(db, func(tx *sql.Tx) error {
		for k, v := range rec {
			if _, err := tx.Exec(`insert into meta (key, value) values (?, ?)
				on conflict(key) do update set value = excluded.value`, k, v); err != nil {
				return err
			}
		}
		return nil
	}); err != nil {
		lg.logf("meta write failed: %v", err)
	}
	// The dashboard is a passenger: a crashed daemon's URL is cleared first,
	// and a dashboard that cannot start leaves the event bridge running.
	if _, err := db.Exec(`delete from meta where key in (?, ?)`, dashboardURLKey, hubURLKey); err != nil {
		lg.logf("meta write failed: %v", err)
	}
	first := map[string]any{"ok": true, "pid": os.Getpid(), "socket": sock, "log": logPath}
	// The dashboard records each listener's URL in meta itself.
	dash := startDashboard(db, lg, dir, *stay)
	if dash != nil {
		d.usage = dash.usage
		if dash.url != "" {
			first["dashboard_url"] = dash.url
		}
		if tu, ok, _ := getMeta(db, hubURLKey); ok {
			first["tailnet_url"] = tu
		}
	}
	// A peer pushes to the hub named in hub.url; the hub itself never pushes.
	var push *pusher
	if cfg, _ := dashboardConfig(dir); cfg.Tailnet {
		if raw, _ := readHubURL(dir); raw != "" {
			lg.logf("peer: this machine is the hub (dashboard.addr tailnet); ignoring hub.url")
		}
	} else {
		push = newPusher(db, lg, dir)
	}
	c.emit(first)
	c.lines = true
	pushDone := make(chan struct{})
	pushCx, pushStop := context.WithCancel(sig)
	if push != nil {
		d.afterPass = push.poke
		go func() {
			defer close(pushDone)
			push.run(pushCx)
		}()
	} else {
		close(pushDone)
	}
	reason := d.run(sig, sock)
	pushStop()
	<-pushDone
	if dash != nil {
		dash.stop()
		if err := dash.usage.flush(db, time.Now()); err != nil {
			lg.logf("dashboard usage flush failed: %v", err)
		}
	}
	lg.logf("exit: %s", reason)
	// A stopped daemon clears its heartbeat so wait resumes its own poll now,
	// not 30 s later, and its dashboard URL. A crashed one leaves the
	// heartbeat to go stale; --status shows its dashboard down by the pid.
	if _, err := db.Exec(`delete from meta where key in (?, ?, ?)`, heartbeatKey, dashboardURLKey, hubURLKey); err != nil {
		lg.logf("heartbeat clear failed: %v", err)
	}
	return nil, exitOK, nil
}

// acquireLock takes the exclusive daemon lock and writes this pid into it.
// A held lock returns a nil file and the holder's recorded pid.
func acquireLock(path string) (*os.File, int, error) {
	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE, 0o644)
	if err != nil {
		return nil, 0, err
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		f.Close()
		if errors.Is(err, syscall.EWOULDBLOCK) {
			return nil, lockPID(path), nil
		}
		return nil, 0, err
	}
	if err := f.Truncate(0); err != nil {
		f.Close()
		return nil, 0, err
	}
	if _, err := f.WriteAt([]byte(strconv.Itoa(os.Getpid())+"\n"), 0); err != nil {
		f.Close()
		return nil, 0, err
	}
	return f, os.Getpid(), nil
}

func lockPID(path string) int {
	b, _ := os.ReadFile(path)
	n, _ := strconv.Atoi(strings.TrimSpace(string(b)))
	return n
}

// daemonStatus never takes the lock, so it cannot make a starting daemon
// believe another instance holds it; liveness of the recorded pid is a probe.
func daemonStatus(c *ctx, dir, lockPath string) (any, int, error) {
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer db.Close()
	out, err := daemonRecord(db)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	delete(out, "record")
	out["ok"] = true
	usage, err := dashboardUsageStatus(db, time.Now())
	if err != nil {
		out["dashboard_usage"] = map[string]any{"error": err.Error()}
	} else {
		out["dashboard_usage"] = usage
	}
	sock, ok, err := getMeta(db, "daemon_socket")
	if err != nil {
		return nil, 0, dbErr(err)
	}
	if !ok {
		sock = socketPath(c)
	}
	out["socket"] = sock
	pid := lockPID(lockPath)
	running := pid > 0 && syscall.Kill(pid, 0) == nil
	out["running"] = running
	out["stay"], out["supervised"] = false, false
	if running {
		out["pid"] = pid
		// The running daemon's own record, if it wrote one (v0.6+); another
		// pid's record is a previous daemon's, so the version is unknown.
		rec, err := runningDaemon(db, pid, dir)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		out["running_version"], out["stale"] = rec.version, rec.version != version
		out["stay"], out["supervised"] = rec.stay, rec.supervised
		if rec.herdrMissing {
			out["herdr_missing"] = true
		}
		if rec.started != "" {
			out["started_at"] = rec.started
		}
	}
	// dashboard: up (the running daemon serves it), down, off, or refused
	// (dashboard.addr names a non-loopback or malformed address).
	url, up, err := getMeta(db, dashboardURLKey)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	cfg, aerr := dashboardConfig(dir)
	addr := cfg.Addr
	switch {
	case running && up:
		out["dashboard"], out["dashboard_url"] = "up", url
	case aerr != nil:
		out["dashboard"], out["dashboard_error"] = "refused", aerr.Error()
	case addr == "":
		out["dashboard"] = "off"
	default:
		out["dashboard"], out["dashboard_url"] = "down", "http://"+addr+"/"
	}
	// role: hub (dashboard.addr tailnet), peer (hub.url), or local.
	raw, _ := readHubURL(dir)
	switch {
	case cfg.Tailnet:
		out["role"] = "hub"
		if tu, ok, err := getMeta(db, hubURLKey); err != nil {
			return nil, 0, dbErr(err)
		} else if ok && running {
			out["tailnet_url"] = tu
		}
	case raw != "":
		out["role"], out["hub_url"] = "peer", raw
		for k, key := range map[string]string{"last_push_ok_at": peerOKKey, "last_push_error": peerErrKey} {
			if v, ok, err := getMeta(db, key); err != nil {
				return nil, 0, dbErr(err)
			} else if ok {
				out[k] = v
			}
		}
	default:
		out["role"] = "local"
	}
	return out, exitOK, nil
}

// daemonLog is the daemon's only diagnostic sink. It is truncated when it
// passes daemonLogMax and never receives pane text. The missing-Herdr startup
// warning includes PATH; no other environment values are logged.
type daemonLog struct {
	mu      sync.Mutex
	f       *os.File
	last    map[string]time.Time // limited: last line per key
	skipped map[string]int
}

func openDaemonLog(path string) *daemonLog {
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_APPEND, 0o644)
	if err != nil {
		return &daemonLog{}
	}
	return &daemonLog{f: f}
}

func (l *daemonLog) logf(format string, a ...any) {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.f == nil {
		return
	}
	if st, err := l.f.Stat(); err == nil && st.Size() > daemonLogMax {
		l.f.Truncate(0)
	}
	fmt.Fprintf(l.f, "%s %s\n", now(), fmt.Sprintf(format, a...))
}

// limited logs at most one line per key per interval; the next line after a
// quiet interval says how many were suppressed.
func (l *daemonLog) limited(key string, every time.Duration, format string, a ...any) {
	l.mu.Lock()
	if l.last == nil {
		l.last, l.skipped = map[string]time.Time{}, map[string]int{}
	}
	if t, ok := l.last[key]; ok && time.Since(t) < every {
		l.skipped[key]++
		l.mu.Unlock()
		return
	}
	l.last[key] = time.Now()
	n := l.skipped[key]
	delete(l.skipped, key)
	l.mu.Unlock()
	if n > 0 {
		format += fmt.Sprintf(" (%d similar suppressed)", n)
	}
	l.logf(format, a...)
}

func (l *daemonLog) close() {
	if l.f != nil {
		l.f.Close()
	}
}

// tokenState is what the daemon last wrote to a task's pane.
type tokenState struct {
	pane, state string
	round       int64
}

type workspaceTokenState struct {
	campaign, parent       string
	hasCampaign, hasParent bool
}

type daemon struct {
	stay            bool
	herdrMissing    bool
	attachments     atomic.Uint64
	db              *sql.DB
	log             *daemonLog
	usage           *dashboardUsage
	tokens          map[int64]tokenState           // nil until the first pass sets the baseline
	workspaceTokens map[string]workspaceTokenState // nil until the workspace list succeeds
	connected       atomic.Bool                    // the subscription is acked and open

	failedWorkspaceWant map[string]workspaceTokenState // wanted pairs at the last failed list
	ownerAskTokens      map[string]int64               // pane -> taskr_owner_ask; nil until the pane list succeeds
	failedOwnerAskWant  map[string]int64               // wanted counts at the last failed list

	mu         sync.Mutex
	want       []string      // the pane set the last pass computed
	subscribed []string      // the open stream's pane set
	streaming  bool          // a stream is open
	resub      chan struct{} // the pane set may differ from the open stream's

	lastStreamErr time.Time                // only the subscribe goroutine touches it
	afterPass     func()                   // the peer's push poke; never blocks
	sock          string                   // the Herdr socket every herdr call is gated on
	watch         func() ([]string, error) // the pane set; nil is this ledger's server-host panes
}

// panes is the pane set the subscription watches.
func (d *daemon) panes() ([]string, error) {
	if d.watch != nil {
		return d.watch()
	}
	return watchedPanes(d.db, serverHost)
}

// refreshPanes records the current pane set and asks the open stream to
// resubscribe when its set differs.
func (d *daemon) refreshPanes() {
	panes, err := d.panes()
	if err != nil {
		d.log.logf("pane set query failed: %v", err)
		return
	}
	d.mu.Lock()
	d.want = panes
	differs := d.streaming && !slices.Equal(panes, d.subscribed)
	d.mu.Unlock()
	if differs {
		select {
		case d.resub <- struct{}{}:
		default:
		}
	}
}

type passResult struct {
	observed, notified, tokens int
	err                        error
}

// pass keeps the released observation, notification, pane-token and heartbeat
// order, then publishes workspace tokens. Every failure is logged and the later
// steps still run.
func (d *daemon) pass() passResult {
	var r passResult
	// Receipt deadlines need the ledger, not Herdr.
	if d.db != nil {
		if err := expireReceiptsNow(d.db); err != nil {
			d.log.logf("receipt expiry failed: %v", err)
		}
	}
	if d.herdrMissing || !serverUp(d.sock) {
		d.log.limited("no-server", time.Minute, "no Herdr server accepts on %s; skipping this pass's herdr calls", d.sock)
		r.err = herdrErr("Herdr server not reachable at %s; no herdr command run", d.sock)
		return r
	}
	deadline := time.Now().Add(herdrListDeadline)
	r.observed, r.err = observe(d.db, d.sock, nil, deadline)
	if r.err != nil {
		d.log.logf("observe failed: %v", r.err)
	}
	r.notified = d.notifyOwners()
	r.tokens = d.writeTokens()
	if d.connected.Load() {
		d.heartbeat()
	}
	r.tokens += d.writeCampaignTokens()
	r.tokens += d.writeOwnerAskTokens()
	return r
}

func (d *daemon) heartbeat() {
	if err := setMeta(d.db, heartbeatKey, now()); err != nil {
		d.log.logf("heartbeat write failed: %v", err)
	}
}

// herdrRun runs one herdr command from an argument array; its output is
// discarded and never logged.
func herdrRun(sock string, args ...string) error {
	cx, cancel := context.WithTimeout(context.Background(), herdrListDeadline)
	defer cancel()
	cmd, err := herdrCommand(cx, sock, args...)
	if err != nil {
		return err
	}
	cmd.WaitDelay = herdrWaitDelay
	return cmd.Run()
}

const notifyBodyMax = 120

type ownerAskNotification struct {
	ID      int64  `json:"id"`
	Summary string `json:"summary"`
}

// truncate shortens s to at most n runes, marking a cut with an ellipsis.
func truncate(s string, n int) string {
	r := []rune(strings.Join(strings.Fields(s), " "))
	if len(r) <= n {
		return string(r)
	}
	return string(r[:n-1]) + "…"
}

// notifyOwners shows one Herdr notification per blocking owner ask of a task on
// the server host (another host's asks are not this Herdr's). The meta
// marker notified:<event_id> is claimed before the command runs, so an ask
// is notified at most once even when the command fails.
func (d *daemon) notifyOwners() int {
	asks, err := claimOwnerAsks(d.db, sql.NullString{}, d.log)
	if err != nil {
		d.log.logf("owner asks query failed: %v", err)
		return 0
	}
	n := 0
	for _, a := range asks {
		if err := herdrRun(d.sock, "notification", "show", "taskr: decision needed",
			"--body", truncate(a.Summary, notifyBodyMax), "--sound", "request"); err != nil {
			d.log.logf("notify ask %d failed: %v", a.ID, err)
			continue
		}
		d.log.logf("notified ask %d", a.ID)
		n++
	}
	return n
}

func claimOwnerAsks(db *sql.DB, machine sql.NullString, log *daemonLog) ([]ownerAskNotification, error) {
	rows, err := db.Query(`select e.id, coalesce(e.summary, '') from events e join tasks t on t.id = e.task_id
		where e.kind = 'ask' and e.answered_by is null and json_extract(e.data, '$.owner') = 1 and t.machine is ?
		and json_extract(e.data, '$.blocking') = 1
		and not exists (select 1 from meta m where m.key = 'notified:' || e.id) order by e.id`, machine)
	if err != nil {
		return nil, err
	}
	var candidates []ownerAskNotification
	for rows.Next() {
		var a ownerAskNotification
		if err := rows.Scan(&a.ID, &a.Summary); err != nil {
			rows.Close()
			return nil, err
		}
		candidates = append(candidates, a)
	}
	if err := rows.Err(); err != nil {
		rows.Close()
		return nil, err
	}
	rows.Close()
	var claimed []ownerAskNotification
	for _, a := range candidates {
		res, err := db.Exec(`insert into meta (key, value) values (?, ?) on conflict(key) do nothing`,
			fmt.Sprintf("notified:%d", a.ID), now())
		if err != nil {
			if log != nil {
				log.logf("notify ask %d: claim failed: %v", a.ID, err)
			}
			continue
		}
		if k, _ := res.RowsAffected(); k == 1 {
			claimed = append(claimed, a)
		}
	}
	return claimed, nil
}

// taskToken is the sidebar state of a task: a finished task is done; else a
// task in its own blocking wait is waiting; else one with an open ask is ask;
// else its status (open or ready).
func taskToken(status string, waiting bool, openAsks int64) string {
	switch {
	case status == "done" || status == "failed":
		return "done"
	case waiting:
		return "waiting"
	case openAsks > 0:
		return "ask"
	case status == "ready":
		return "ready"
	}
	return "open"
}

// writeTokens reports taskr_state and taskr_round on each task's pane whose
// token changed since the last successful write. The first call only records
// the baseline. A task whose launch is observed missing is skipped; a failed
// write is logged and retried on a later pass. Only server-host panes get
// tokens: a launchless task's host is its own machine.
func (d *daemon) writeTokens() int {
	rows, err := d.db.Query(`select t.id, coalesce(l.pane_id, t.pane_id), t.status, coalesce(t.waiting_until > ?, 0),
		(select count(*) from events a where a.task_id = t.id and a.kind = 'ask' and a.answered_by is null),
		(select count(*) from events p where p.task_id = t.id and p.kind = 'prompt' and p.launch_id is t.current_launch_id),
		coalesce(l.present, 1)
		from tasks t left join launches l on l.id = t.current_launch_id
		where t.status not in ('closed', 'planned') and coalesce(l.pane_id, t.pane_id) is not null
		and (case when l.id is null then t.machine else l.machine end) is null order by t.id`, now())
	if err != nil {
		d.log.logf("token query failed: %v", err)
		return 0
	}
	cur := map[int64]tokenState{}
	var order []int64
	for rows.Next() {
		var id, asks, round int64
		var pane, status string
		var waiting, present bool
		if err := rows.Scan(&id, &pane, &status, &waiting, &asks, &round, &present); err != nil {
			d.log.logf("token scan failed: %v", err)
			rows.Close()
			return 0
		}
		if !present {
			continue
		}
		cur[id] = tokenState{pane: pane, state: taskToken(status, waiting, asks), round: round}
		order = append(order, id)
	}
	rows.Close()
	if d.tokens == nil {
		d.tokens = cur
		return 0
	}
	n := 0
	for _, id := range order {
		ts := cur[id]
		if prev, ok := d.tokens[id]; ok && prev == ts {
			continue
		}
		if err := herdrRun(d.sock, "pane", "report-metadata", ts.pane, "--source", "taskr",
			"--token", "taskr_state="+ts.state, "--token", "taskr_round="+strconv.FormatInt(ts.round, 10)); err != nil {
			d.log.logf("token write for task %d failed: %v", id, err)
			continue
		}
		d.tokens[id] = ts
		n++
	}
	return n
}

// writeCampaignTokens computes workspace ownership after the pane tokens and
// heartbeat, so no campaign query or Herdr call delays the released path.
func (d *daemon) writeCampaignTokens() int {
	workspaces, err := wantedWorkspaceTokens(d.db, serverHost)
	if err != nil {
		d.log.logf("campaign token query failed: %v", err)
		return 0
	}
	return d.writeWorkspaceTokens(workspaces)
}

// wantedWorkspaceTokens reads this host's active tasks and current launches,
// then only their ancestors. Closed ancestors still name the campaign.
func wantedWorkspaceTokens(q queryer, host sql.NullString) (map[string]workspaceTokenState, error) {
	rows, err := q.Query(`with recursive local_tasks as (
		select t.id, case when l.pane_id is not null then l.workspace_id else t.workspace_id end as workspace,
		coalesce(l.present, 1) as present
		from tasks t left join launches l on l.id = t.current_launch_id
		where t.status not in ('closed', 'planned') and coalesce(l.pane_id, t.pane_id) is not null
		and (case when l.id is null then t.machine else l.machine end) is ?
		), ancestors(id, ancestor) as (
		select id, id from local_tasks
		union select a.id, t.parent_id from ancestors a join tasks t on t.id = a.ancestor where t.parent_id is not null
		)
		select t.id, t.workspace, r.id, r.name, t.present
		from local_tasks t join ancestors a on a.id = t.id join tasks r on r.id = a.ancestor
		where r.parent_id is null order by t.id`, host)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	workspaces := map[string]workspaceTokenState{}
	workspaceRoots := map[string]int64{}
	rootWorkspaces := map[int64]string{}
	for rows.Next() {
		var id, root int64
		var rootName string
		var workspace sql.NullString
		var present bool
		if err := rows.Scan(&id, &workspace, &root, &rootName, &present); err != nil {
			return nil, err
		}
		if !present || workspace.String == "" {
			continue
		}
		if id == root {
			rootWorkspaces[root] = workspace.String
		}
		if prev, ok := workspaceRoots[workspace.String]; !ok {
			workspaceRoots[workspace.String] = root
			workspaces[workspace.String] = workspaceTokenState{campaign: rootName, hasCampaign: true}
		} else if prev != root {
			workspaceRoots[workspace.String] = 0 // multiple roots cannot own one workspace
			workspaces[workspace.String] = workspaceTokenState{}
		}
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	for workspace, root := range workspaceRoots {
		if parent := rootWorkspaces[root]; parent != "" {
			if parent == workspace {
				workspaces[workspace] = workspaceTokenState{}
			} else {
				ts := workspaces[workspace]
				ts.parent, ts.hasParent = parent, true
				workspaces[workspace] = ts
			}
		}
	}
	return workspaces, nil
}

// herdrWorkspaceTokens reads only the workspace ids and the two taskr tokens.
// Other reporters' tokens are never written or cleared.
func herdrWorkspaceTokens(sock string) (map[string]workspaceTokenState, error) {
	cx, cancel := context.WithTimeout(context.Background(), herdrListDeadline)
	defer cancel()
	cmd, err := herdrCommand(cx, sock, "workspace", "list")
	if err != nil {
		return nil, err
	}
	var stdout bytes.Buffer
	cmd.Stdout = &stdout
	cmd.WaitDelay = herdrWaitDelay
	if err := cmd.Run(); err != nil {
		return nil, fmt.Errorf("herdr workspace list failed: %w", err)
	}
	var resp struct {
		Result *struct {
			Workspaces []struct {
				ID     string            `json:"workspace_id"`
				Tokens map[string]string `json:"tokens"`
			} `json:"workspaces"`
		} `json:"result"`
	}
	if err := json.Unmarshal(stdout.Bytes(), &resp); err != nil {
		return nil, fmt.Errorf("herdr workspace list returned malformed JSON: %w", err)
	}
	if resp.Result == nil || resp.Result.Workspaces == nil {
		return nil, errors.New("herdr workspace list returned no result.workspaces")
	}
	workspaces := map[string]workspaceTokenState{}
	for _, w := range resp.Result.Workspaces {
		if w.ID == "" {
			return nil, errors.New("herdr workspace list entry without workspace_id")
		}
		ts := workspaceTokenState{}
		ts.campaign, ts.hasCampaign = w.Tokens["taskr_campaign"]
		ts.parent, ts.hasParent = w.Tokens["taskr_parent"]
		workspaces[w.ID] = ts
	}
	return workspaces, nil
}

// writeWorkspaceTokens reconciles on startup, then lists when a pair changes.
// A failed list waits for a changed wanted pair or the daemon fallback tick.
func (d *daemon) writeWorkspaceTokens(want map[string]workspaceTokenState) int {
	if d.failedWorkspaceWant != nil && maps.Equal(want, d.failedWorkspaceWant) {
		return 0
	}
	changed := d.workspaceTokens == nil
	for workspace, ts := range want {
		changed = changed || d.workspaceTokens[workspace] != ts
	}
	for workspace, ts := range d.workspaceTokens {
		changed = changed || want[workspace] != ts
	}
	if !changed {
		return 0
	}
	listed, err := herdrWorkspaceTokens(d.sock)
	if err != nil {
		d.failedWorkspaceWant = maps.Clone(want)
		d.log.logf("workspace token list failed: %v", err)
		return 0
	}
	d.failedWorkspaceWant = nil
	d.workspaceTokens = listed
	var order []string
	for workspace := range listed {
		order = append(order, workspace)
	}
	for workspace, ts := range want {
		if _, exists := listed[workspace]; !exists {
			// An unlisted workspace is satisfied without a metadata call.
			d.workspaceTokens[workspace] = ts
		}
	}
	slices.Sort(order)
	n := 0
	for _, workspace := range order {
		ts := want[workspace]
		if d.workspaceTokens[workspace] == ts {
			continue
		}
		args := []string{"workspace", "report-metadata", workspace, "--source", "taskr"}
		if ts.hasCampaign {
			args = append(args, "--token", "taskr_campaign="+ts.campaign)
		} else {
			args = append(args, "--clear-token", "taskr_campaign")
		}
		if ts.hasParent {
			args = append(args, "--token", "taskr_parent="+ts.parent)
		} else {
			args = append(args, "--clear-token", "taskr_parent")
		}
		if err := herdrRun(d.sock, args...); err != nil {
			d.log.logf("workspace token write for %s failed: %v", workspace, err)
			continue
		}
		d.workspaceTokens[workspace] = ts
		n++
	}
	return n
}

// fallbackTick lets failed lists retry. The owner ask cache is a pane list
// snapshot, so it is dropped too: panes absent from the last list are revisited.
func (d *daemon) fallbackTick() {
	d.failedWorkspaceWant, d.failedOwnerAskWant, d.ownerAskTokens = nil, nil, nil
}

// wantedOwnerAsks maps each of host's panes to its tasks' own open owner asks
// (n > 0 only). An ask is on the asker's task; --owner only routes it to the root.
func wantedOwnerAsks(q queryer, host sql.NullString) (map[string]int64, error) {
	rows, err := q.Query(`select coalesce(l.pane_id, t.pane_id), count(*)
		from tasks t left join launches l on l.id = t.current_launch_id
		join events o on o.task_id = t.id and o.kind = 'ask' and o.answered_by is null
			and coalesce(json_extract(o.data, '$.owner'), 0)
		where t.status not in ('closed', 'planned') and coalesce(l.pane_id, t.pane_id) is not null
		and (case when l.id is null then t.machine else l.machine end) is ? and coalesce(l.present, 1)
		group by 1`, host)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	want := map[string]int64{}
	for rows.Next() {
		var pane string
		var n int64
		if err := rows.Scan(&pane, &n); err != nil {
			return nil, err
		}
		want[pane] = n
	}
	return want, rows.Err()
}

// writeOwnerAskTokens reconciles taskr_owner_ask with the panes Herdr lists,
// on startup and whenever the wanted counts change: it sets differing counts
// and clears the token on every listed pane that is not wanted (a moved
// launch, or an ask answered or a task closed while the daemon was down).
// A failed list waits for a changed want or the fallback tick; a failed write
// or a wanted pane absent from the list leaves the cache unequal, so the next
// pass lists again. The fallback tick drops the cache, so a stale pane absent
// from the last list is cleared once it appears. taskr owns the taskr_ token
// namespace: no other source writes taskr_owner_ask, so the merged list value
// is taskr's own.
func (d *daemon) writeOwnerAskTokens() int {
	want, err := wantedOwnerAsks(d.db, serverHost)
	if err != nil {
		d.log.logf("owner ask token query failed: %v", err)
		return 0
	}
	return d.reconcileOwnerAskTokens(want)
}

// reconcileOwnerAskTokens applies want to the panes on d.sock under the rules
// above. A client host's daemon calls it with the server's map for its host.
func (d *daemon) reconcileOwnerAskTokens(want map[string]int64) int {
	if d.failedOwnerAskWant != nil && maps.Equal(want, d.failedOwnerAskWant) {
		return 0
	}
	if d.ownerAskTokens != nil && maps.Equal(want, d.ownerAskTokens) {
		return 0
	}
	listed, panes, err := herdrOwnerAskTokens(d.sock)
	if err != nil {
		d.failedOwnerAskWant = want
		d.log.logf("owner ask token list failed: %v", err)
		return 0
	}
	d.failedOwnerAskWant = nil
	d.ownerAskTokens = listed
	order := slices.Sorted(maps.Keys(panes))
	n := 0
	for _, pane := range order {
		w, have := want[pane], listed[pane]
		var args []string
		switch {
		case w > 0 && (!panes[pane] || have != w):
			args = []string{"--token", "taskr_owner_ask=" + strconv.FormatInt(w, 10)}
		case w == 0 && panes[pane]:
			args = []string{"--clear-token", "taskr_owner_ask"}
		default:
			continue
		}
		if err := herdrRun(d.sock, append([]string{"pane", "report-metadata", pane, "--source", "taskr"}, args...)...); err != nil {
			d.log.logf("owner ask token write for %s failed: %v", pane, err)
			continue
		}
		if w > 0 {
			d.ownerAskTokens[pane] = w
		} else {
			delete(d.ownerAskTokens, pane)
		}
		n++
	}
	return n
}

// herdrOwnerAskTokens lists every pane; panes[p] says whether p carries
// taskr_owner_ask, and listed holds its count (-1 when not a number).
func herdrOwnerAskTokens(sock string) (listed map[string]int64, panes map[string]bool, err error) {
	cx, cancel := context.WithTimeout(context.Background(), herdrListDeadline)
	defer cancel()
	cmd, err := herdrCommand(cx, sock, "pane", "list")
	if err != nil {
		return nil, nil, err
	}
	var stdout bytes.Buffer
	cmd.Stdout = &stdout
	cmd.WaitDelay = herdrWaitDelay
	if err := cmd.Run(); err != nil {
		return nil, nil, fmt.Errorf("herdr pane list failed: %w", err)
	}
	var resp struct {
		Result *struct {
			Panes []struct {
				ID     string            `json:"pane_id"`
				Tokens map[string]string `json:"tokens"`
			} `json:"panes"`
		} `json:"result"`
	}
	if err := json.Unmarshal(stdout.Bytes(), &resp); err != nil {
		return nil, nil, fmt.Errorf("herdr pane list returned malformed JSON: %w", err)
	}
	if resp.Result == nil || resp.Result.Panes == nil {
		return nil, nil, errors.New("herdr pane list returned no result.panes")
	}
	listed, panes = map[string]int64{}, map[string]bool{}
	for _, p := range resp.Result.Panes {
		if p.ID == "" {
			return nil, nil, errors.New("herdr pane list entry without pane_id")
		}
		v, ok := p.Tokens["taskr_owner_ask"]
		panes[p.ID] = ok
		if ok {
			n, err := strconv.ParseInt(v, 10, 64)
			if err != nil || n <= 0 {
				n = -1
			}
			listed[p.ID] = n
		}
	}
	return listed, panes, nil
}

// run is the resident loop: a subscription reader marks the daemon dirty and
// one worker runs passes. It returns why it stopped.
func (d *daemon) run(parent context.Context, sock string) string {
	d.sock = sock
	cx, cancel := context.WithCancel(parent)
	defer cancel()
	dirty := make(chan struct{}, 1)
	mark := func() {
		select {
		case dirty <- struct{}{}:
		default:
		}
	}
	d.resub = make(chan struct{}, 1)
	gone := make(chan struct{})
	readerDone := make(chan struct{})
	go func() {
		defer close(readerDone)
		if d.subscribe(cx, sock, mark) {
			close(gone)
		}
	}()
	defer func() { cancel(); <-readerDone }()

	hb, fb := time.NewTicker(daemonHeartbeat), time.NewTicker(daemonFallback)
	defer hb.Stop()
	defer fb.Stop()
	var usageFlush <-chan time.Time
	if d.usage != nil {
		ticker := time.NewTicker(time.Minute)
		defer ticker.Stop()
		usageFlush = ticker.C
	}
	var last time.Time
	var attachment uint64
	for {
		select {
		case <-cx.Done():
			return "signal"
		case <-gone:
			return "socket removed"
		case <-hb.C:
			if !d.stay && !exists(sock) {
				return "socket removed"
			}
			if d.connected.Load() {
				d.heartbeat()
			}
			continue
		case <-usageFlush:
			if err := d.usage.flush(d.db, time.Now()); err != nil {
				d.log.logf("dashboard usage flush failed: %v", err)
			}
			continue
		case <-fb.C:
			d.fallbackTick()
		case <-dirty:
			wait := max(daemonSettle, time.Until(last.Add(daemonMinGap)))
			select {
			case <-cx.Done():
				return "signal"
			case <-gone:
				return "socket removed"
			case <-time.After(wait):
			}
		}
		select {
		case <-dirty: // this pass covers every wake so far
		default:
		}
		last = time.Now()
		if n := d.attachments.Load(); d.stay && n != attachment {
			// A restarted Herdr may have lost all metadata, even if ledger state stayed the same.
			attachment = n
			d.tokens = map[int64]tokenState{}
			d.workspaceTokens, d.failedWorkspaceWant = nil, nil
			d.ownerAskTokens, d.failedOwnerAskWant = nil, nil
		}
		d.pass()
		d.refreshPanes()
		if d.afterPass != nil {
			d.afterPass()
		}
	}
}

// subscribe keeps one events.subscribe stream open, reconnecting with capped
// exponential backoff, or at once when the pane set changed. It returns true
// when the socket file is gone and false when cx is canceled.
func (d *daemon) subscribe(cx context.Context, sock string, mark func()) bool {
	attempts := 0
	attach := true
	for {
		if cx.Err() != nil {
			return false
		}
		if !d.stay && !exists(sock) {
			return true
		}
		var dialer net.Dialer
		conn, err := dialer.DialContext(cx, "unix", sock)
		acked, resubscribe := false, false
		if err == nil {
			acked, resubscribe = d.stream(cx, conn, mark, attach)
			if acked {
				attempts = 0
			}
		} else {
			if d.stay {
				d.log.limited("stay-connect", time.Minute, "connect failed: %v", err)
			} else {
				d.log.logf("connect failed: %v", err)
			}
		}
		if acked || !resubscribe {
			attach = !resubscribe
		}
		if cx.Err() != nil {
			return false
		}
		if !d.stay && !exists(sock) {
			return true
		}
		if resubscribe {
			continue
		}
		attempts++
		delay := min(daemonRetryCap, daemonRetryBase<<min(attempts-1, 6))
		if d.stay {
			delay = min(delay, 2*time.Second)
		}
		if !d.stay {
			d.log.logf("disconnected; reconnect in %v", delay)
		}
		select {
		case <-cx.Done():
			return false
		case <-time.After(delay):
		}
	}
}

// stream subscribes with the current pane set, writing the request once and
// then only reading: the server treats client bytes on a streaming connection
// as a disconnect. Any chunk holding a line end, the ack included, marks the
// daemon dirty. A pane set change closes the stream so the caller reconnects
// with a new request (replay on connect and coalescing make that safe), and so
// does an error reply after the ack (Herdr 0.9.2's events_lost). It returns
// whether the ack arrived and whether it closed to resubscribe.
func (d *daemon) stream(cx context.Context, conn net.Conn, mark func(), attach bool) (acked, resubscribe bool) {
	panes, err := d.panes()
	if err != nil {
		d.log.logf("pane set query failed: %v", err)
		conn.Close()
		return false, false
	}
	d.mu.Lock()
	d.subscribed, d.streaming = panes, true
	d.mu.Unlock()
	var resubbed atomic.Bool
	done := make(chan struct{})
	closerDone := make(chan struct{})
	defer func() {
		close(done)
		<-closerDone
		d.mu.Lock()
		d.streaming = false
		d.mu.Unlock()
		d.connected.Store(false)
		resubscribe = resubbed.Load()
	}()
	go func() {
		defer close(closerDone)
		defer conn.Close()
		for {
			select {
			case <-cx.Done():
				return
			case <-done:
				return
			case <-d.resub:
				d.mu.Lock()
				differs, n := !slices.Equal(d.want, panes), len(d.want)
				d.mu.Unlock()
				if differs {
					d.log.logf("pane set changed; resubscribing with %d panes", n)
					resubbed.Store(true)
					return
				}
			}
		}
	}()
	if _, err := conn.Write(subscribeRequest(panes)); err != nil {
		d.log.logf("subscribe write failed: %v", err)
		return false, false
	}
	// line is the current line while it is short enough to be a reply
	// (an ack or an error); longer lines are event bursts and only wake.
	var line []byte
	long := false
	buf := make([]byte, 32<<10)
	for {
		n, err := conn.Read(buf)
		if n > 0 {
			chunk := buf[:n]
			if bytes.IndexByte(chunk, '\n') >= 0 {
				mark()
			}
			for len(chunk) > 0 {
				i := bytes.IndexByte(chunk, '\n')
				part := chunk
				if i >= 0 {
					part = chunk[:i]
				}
				if !long && len(line)+len(part) <= streamLineMax {
					line = append(line, part...)
				} else {
					long, line = true, line[:0]
				}
				if i < 0 {
					break
				}
				chunk = chunk[i+1:]
				if !acked {
					acked = true
					d.ackLine(line)
					if d.connected.Load() && d.stay && attach {
						d.attachments.Add(1)
						mark()
					}
				} else if !long && d.streamError(line) {
					// Herdr 0.9.2 reports a reader that fell behind (events_lost)
					// on the stream itself: resubscribe at once; the mark above
					// makes the next pass recover whatever was lost. A second
					// error within streamErrorGap takes the backoff path, so a
					// server that errors on every stream cannot spin the loop.
					if time.Since(d.lastStreamErr) >= streamErrorGap {
						resubbed.Store(true)
					}
					d.lastStreamErr = time.Now()
					return acked, true
				}
				line, long = line[:0], false
			}
		}
		if err != nil {
			return acked, false
		}
	}
}

// streamLineMax bounds the bytes kept of one stream line; error replies are
// short, event bursts are not parsed.
const streamLineMax = 64 << 10

// streamErrorGap is the least time between two immediate resubscribes after
// a stream error.
var streamErrorGap = time.Second

// streamError reports whether a post-ack line is an error reply, logging its
// code and message (rate-limited).
func (d *daemon) streamError(line []byte) bool {
	if !bytes.Contains(line, []byte(`"error"`)) {
		return false
	}
	var resp struct {
		Error *struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if json.Unmarshal(line, &resp) != nil || resp.Error == nil {
		return false
	}
	d.log.limited("stream-error", 10*time.Second, "stream error: %s %s; resubscribing",
		truncate(resp.Error.Code, 60), truncate(resp.Error.Message, 200))
	return true
}

// ackLine records the subscription outcome. An error reply is logged by its
// code and message only.
func (d *daemon) ackLine(line []byte) {
	var resp struct {
		Error *struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if json.Unmarshal(line, &resp) == nil && resp.Error != nil {
		d.log.logf("subscribe rejected: %s %s", resp.Error.Code, truncate(resp.Error.Message, 200))
		return
	}
	d.connected.Store(true)
	d.mu.Lock()
	n := len(d.subscribed)
	d.mu.Unlock()
	d.log.logf("subscribed with %d panes", n)
}
