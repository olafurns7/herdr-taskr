package main

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"syscall"
	"time"
)

// Another host's lanes: the host that owns a pane is the only one that can
// reach it. A client host's daemon sends its agent listing to the server
// (`_host observe`), and its CLI delivers a prompt to its own lane itself
// (`_prompt begin`, then `_prompt outcome`). Both run only over RPC, for
// the caller's own host as whois names it.

// hostHeartbeatKey is the meta key of a client host's last `_host observe`.
func hostHeartbeatKey(machine string) string { return heartbeatKey + ":" + machine }

// hostFresh reports whether machine's daemon called within heartbeatFresh.
func hostFresh(q queryer, machine string) (bool, error) {
	at, ok, err := getMeta(q, hostHeartbeatKey(machine))
	return ok && time.Since(parseTime(at)) < heartbeatFresh, err
}

// freshHosts lists the client hosts whose daemon heartbeat is fresh.
func freshHosts(q queryer) ([]string, error) {
	rows, err := q.Query(`select substr(key, ?), value from meta where key like ? order by key`,
		len(heartbeatKey)+2, heartbeatKey+":%")
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var hosts []string
	for rows.Next() {
		var m, at string
		if err := rows.Scan(&m, &at); err != nil {
			return nil, err
		}
		if time.Since(parseTime(at)) < heartbeatFresh {
			hosts = append(hosts, m)
		}
	}
	return hosts, rows.Err()
}

func hiddenOnly(c *ctx, name string) error {
	if !c.rpc || c.machine == "" {
		return usageErr("%s is only for a client host over RPC", name)
	}
	return nil
}

// cmdHost is a client daemon's pass: `_host observe --agents JSON` records
// the host's heartbeat, applies its agent listing to its own launches, and
// returns the panes to watch and wanted workspace tokens.
func cmdHost(c *ctx, args []string) (any, int, error) {
	if err := hiddenOnly(c, "_host"); err != nil {
		return nil, 0, err
	}
	fs := flag.NewFlagSet("_host", flag.ContinueOnError)
	list := fs.String("agents", "", "the host's herdr agent list, as a JSON array")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil {
		return nil, 0, err
	}
	if pos[0] != "observe" {
		return nil, 0, usageErr("_host: unknown call %q", pos[0])
	}
	var agents []agentObs
	if err := json.Unmarshal([]byte(*list), &agents); err != nil || agents == nil {
		return nil, 0, usageErr("_host observe: --agents must be a JSON array")
	}
	byPane := map[string]agentObs{}
	for _, a := range agents {
		if a.PaneID == "" || a.Status == "" {
			return nil, 0, usageErr("_host observe: an agent without pane_id or agent_status")
		}
		byPane[a.PaneID] = a
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	host := callerMachine(c)
	if err := setMeta(db, hostHeartbeatKey(c.machine), now()); err != nil {
		return nil, 0, dbErr(err)
	}
	ws, err := watchedOn(db, nil, host)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	n := 0
	for _, ch := range planChanges(ws, byPane) {
		wrote, err := applyChange(db, ch)
		if err != nil {
			return nil, 0, dbErr(err)
		}
		if wrote {
			n++
		}
	}
	panes, err := watchedPanes(db, host)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	if panes == nil {
		panes = []string{}
	}
	want, err := wantedWorkspaceTokens(db, host)
	reply := map[string]any{"ok": true, "observed": n, "watch": panes}
	if err != nil {
		if c.log != nil {
			c.log.logf("campaign token query failed: %v", err)
		}
	} else {
		tokens := map[string]workspaceTokenValues{}
		for workspace, ts := range want {
			if ts.hasCampaign {
				tokens[workspace] = workspaceTokenValues{Campaign: ts.campaign, Parent: ts.parent}
			}
		}
		reply["workspace_tokens"] = tokens
	}
	asks, err := claimOwnerAsks(db, host, nil)
	if err != nil {
		return nil, 0, dbErr(err)
	}
	reply["owner_asks"] = asks
	return reply, exitOK, nil
}

// cmdPromptPhase is a prompt split for a lane on the caller's host.
//
//	_prompt begin ID (--text T | --file ABS --sha256 H --bytes N) [--local-herdr] [--receipt-timeout MS]
//	_prompt outcome ATTEMPT --outcome O [--detail JSON] [--confirm [--confirm-timeout MS]]
//
// begin records the attempt as prompt does and returns what to send, or the
// route "server" with no write when the server host owns the lane.
// --local-herdr says the caller's Herdr server accepts; without it there is
// no attempt. outcome records what the caller's herdr observed.
func cmdPromptPhase(c *ctx, args []string) (any, int, error) {
	if err := hiddenOnly(c, "_prompt"); err != nil {
		return nil, 0, err
	}
	fs := flag.NewFlagSet("_prompt", flag.ContinueOnError)
	text := fs.String("text", "", "literal prompt text")
	file := fs.String("file", "", "the prompt file's absolute path on the caller's host")
	sum := fs.String("sha256", "", "the prompt file's sha256")
	size := fs.Int64("bytes", -1, "the prompt file's size")
	localHerdr := fs.Bool("local-herdr", false, "the caller's Herdr server accepts")
	outcome := fs.String("outcome", "", "activity_observed, rejected or delivery_unknown")
	detail := fs.String("detail", "{}", "herdr's detail, as a JSON object")
	confirm := fs.Bool("confirm", false, "wait for the worker's taskr got receipt")
	confirmTimeout := fs.Int64("confirm-timeout", 60000, "milliseconds to wait for the receipt")
	receiptTimeout := fs.Int64("receipt-timeout", defaultReceiptTimeout.Milliseconds(), "the receipt window to arm; 0 disarms")
	pos, err := parseArgs(c, fs, args, 2, 2)
	if err != nil {
		return nil, 0, err
	}
	if err := checkReceiptTimeout(*receiptTimeout, flagWasSet(fs, "receipt-timeout"), false); err != nil {
		return nil, 0, err
	}
	n, err := parseID(pos[1], "id")
	if err != nil {
		return nil, 0, err
	}
	if *confirmTimeout < 0 {
		return nil, 0, usageErr("--confirm-timeout must not be negative")
	}
	db, err := openDB(c)
	if err != nil {
		return nil, 0, err
	}
	defer closeDB(c, db)
	switch pos[0] {
	case "begin":
		if (*file == "") == (*text == "") {
			return nil, 0, usageErr("prompt needs exactly one of --file and --text")
		}
		data := map[string]any{}
		if *file != "" {
			data = map[string]any{"file": *file, "sha256": *sum, "bytes": *size}
		}
		d := promptDelivery(*text, *file, data, false, 0)
		if *file != "" && d.document != nil {
			d.document.Host = c.machine
		}
		d.receiptTimeout = time.Duration(*receiptTimeout) * time.Millisecond
		a, err := beginAttempt(c, db, n, d, true, func() error {
			if !*localHerdr {
				return herdrErr("Herdr server not reachable on %s; not delivering", c.machine)
			}
			if *file != "" && (*sum == "" || *size < 0) {
				return usageErr("read prompt file: %s is not readable on %s", *file, c.machine)
			}
			return nil
		})
		if err == errServerLane {
			return map[string]any{"ok": true, "route": "server"}, exitOK, nil
		}
		if err != nil {
			return nil, 0, err
		}
		return map[string]any{"ok": true, "route": "here", "task_id": n, "attempt_id": a.id, "target": a.target,
			"agent": a.agent, "text": a.text}, exitOK, nil
	case "outcome":
		return promptOutcome(c, db, n, *outcome, *detail, *confirm, time.Duration(*confirmTimeout)*time.Millisecond)
	}
	return nil, 0, usageErr("_prompt: unknown call %q", pos[0])
}

// promptOutcome records the outcome of attempt id, which must be a prompt to
// a launch on the caller's host with no outcome yet.
func promptOutcome(c *ctx, db *sql.DB, id int64, outcome, detail string, confirm bool, timeout time.Duration) (any, int, error) {
	switch outcome {
	case "activity_observed", "rejected", "delivery_unknown":
	default:
		return nil, 0, usageErr("--outcome must be activity_observed, rejected or delivery_unknown, got %q", outcome)
	}
	var raw map[string]any
	if err := json.Unmarshal([]byte(detail), &raw); err != nil {
		return nil, 0, usageErr("--detail must be a JSON object")
	}
	det := map[string]any{}
	for _, k := range []string{"agent_status", "herdr_error", "herdr_exit", "herdr_message"} {
		if v, ok := raw[k]; ok {
			det[k] = v
		}
	}
	a := attempt{id: id}
	err := withTx(db, func(tx *sql.Tx) error {
		var launch sql.NullInt64
		var host, data sql.NullString
		err := tx.QueryRow(`select e.task_id, e.launch_id, l.machine, e.data from events e
			left join launches l on l.id = e.launch_id where e.id = ? and e.kind = 'prompt'`, id).
			Scan(&a.taskID, &launch, &host, &data)
		if err == sql.ErrNoRows {
			return rejectErr("event %d is not a prompt attempt", id)
		} else if err != nil {
			return err
		}
		if !launch.Valid || host.String != c.machine {
			return rejectErr("prompt attempt %d is not for a lane on %s", id, c.machine)
		}
		a.launchID = ptr(launch.Int64)
		var d struct {
			Target string `json:"target"`
		}
		json.Unmarshal([]byte(data.String), &d)
		a.target = d.Target
		var done bool
		if err := tx.QueryRow(`select exists (select 1 from events where kind = 'prompt_outcome' and related_event_id = ?)`, id).
			Scan(&done); err != nil {
			return err
		}
		if done {
			return rejectErr("prompt attempt %d already has an outcome", id)
		}
		return nil
	})
	if err != nil {
		return nil, 0, err
	}
	return finishDelivery(db, a, outcome, det, confirm, timeout)
}

// clientPrompt delivers a prompt to a lane on this host: the server records
// the attempt, this host's Herdr sends it, and the server records the
// outcome. done is false when the server host owns the lane; the caller
// then sends the prompt as usual.
func clientPrompt(c *ctx, cl *rpcClient, lead, args []string, cwd string, env map[string]string) (code int, done bool) {
	fail := func(e *exitErr) (int, bool) {
		if e.kind == "transport" {
			e.msg = "server unreachable (" + e.msg + "); no attempt is known; inspect `taskr log` before any resend"
		}
		return clientFail(c, e.code, e.kind, e.msg), true
	}
	fs := flag.NewFlagSet("prompt", flag.ContinueOnError)
	file := fs.String("file", "", "")
	text := fs.String("text", "", "")
	confirm := fs.Bool("confirm", false, "")
	confirmTimeout := fs.Int64("confirm-timeout", 60000, "")
	receiptTimeout := fs.Int64("receipt-timeout", defaultReceiptTimeout.Milliseconds(), "")
	pos, err := parseArgs(c, fs, args, 1, 1)
	if err != nil || (*file == "") == (*text == "") || *confirmTimeout < 0 ||
		checkReceiptTimeout(*receiptTimeout, flagWasSet(fs, "receipt-timeout"), *confirm) != nil {
		return 0, false // the server answers the usage error
	}
	if *confirm {
		*receiptTimeout = 0 // --confirm waits for the receipt itself
	}
	begin := []string{"--json", "_prompt", "begin", pos[0], "--receipt-timeout", fmt.Sprint(*receiptTimeout)}
	if *file != "" {
		begin = append(begin, "--file", *file)
		if b, err := os.ReadFile(*file); err == nil {
			s := sha256.Sum256(b)
			begin = append(begin, "--sha256", hex.EncodeToString(s[:]), "--bytes", fmt.Sprint(len(b)))
		}
	}
	if *text != "" {
		begin = append(begin, "--text", *text)
	}
	sock := socketPath(c)
	if serverUp(sock) {
		begin = append(begin, "--local-herdr")
	}
	rep, e := cl.call(begin, cwd, newRequestKey(), env)
	if e != nil {
		return fail(e)
	}
	var uploads []rpcDocWant
	if rep.Upload != nil {
		uploads = *rep.Upload
	}
	rep.Upload = nil
	var b struct {
		Route   string `json:"route"`
		Attempt int64  `json:"attempt_id"`
		Target  string `json:"target"`
		Text    string `json:"text"`
		Error   string `json:"error"`
		Kind    string `json:"kind"`
	}
	json.Unmarshal([]byte(lastLine(rep.Stdout)), &b)
	if rep.Exit != exitOK {
		return clientFail(c, rep.Exit, b.Kind, b.Error), true
	}
	if b.Route == "server" {
		return 0, false
	}
	outcome, detail := runHerdrPrompt(c, sock, b.Target, b.Text)
	dj, _ := json.Marshal(detail)
	argv := append(slices.Clone(lead), "_prompt", "outcome", fmt.Sprint(b.Attempt), "--outcome", outcome, "--detail", string(dj))
	if *confirm {
		argv = append(argv, "--confirm", "--confirm-timeout", fmt.Sprint(*confirmTimeout))
	}
	rep, e = cl.call(argv, cwd, newRequestKey(), env)
	if e != nil {
		return clientFail(c, exitHerdr, e.kind, fmt.Sprintf(
			"prompt attempt %d: outcome %s observed here, but the server did not record it (%s); inspect the agent before any resend",
			b.Attempt, outcome, e.msg)), true
	}
	io.WriteString(c.out, rep.Stdout)
	io.WriteString(c.errw, rep.Stderr)
	if rep.Exit == exitOK {
		clientUploadDocs(cl, uploads, cwd, env)
	}
	return rep.Exit, true
}

func newRequestKey() string {
	b := make([]byte, 16)
	rand.Read(b)
	return hex.EncodeToString(b)
}

func lastLine(s string) string {
	lines := strings.Split(strings.TrimSpace(s), "\n")
	return lines[len(lines)-1]
}

// clientObserveEvery is the client daemon's pass interval.
var clientObserveEvery = 5 * time.Second

const clientStateFile = "client-state.json"

// clientState is what the client daemon last saw of the server.
type clientState struct {
	LastCallAt string `json:"last_call_at,omitempty"`
	LastError  string `json:"last_error,omitempty"`
}

// clientDaemon is `taskr daemon` on a client host: no ledger, dashboard,
// or peer push. Every clientObserveEvery, and after each Herdr pane
// change, it sends this host's agent listing to the server, which returns
// the panes to watch and workspace tokens to publish locally.
func clientDaemon(c *ctx, raw string, args []string) int {
	fs := flag.NewFlagSet("daemon", flag.ContinueOnError)
	once := fs.Bool("once", false, "run one observation pass and exit")
	status := fs.Bool("status", false, "print the mode, the server and the last call")
	restart := fs.Bool("restart", false, "restart the client daemon")
	stay := fs.Bool("stay", false, "local ledger host only")
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		return clientFail(c, exitUsage, "usage", err.(*exitErr).msg)
	}
	if *stay {
		return clientFail(c, exitUsage, "usage", "daemon --stay is only available in local mode")
	}
	if btoi(*once)+btoi(*status)+btoi(*restart) > 1 {
		return clientFail(c, exitUsage, "usage", "daemon in client mode: use --once, --status, --restart, or neither")
	}
	dir, err := stateDir(c)
	if err == nil {
		err = os.MkdirAll(dir, 0o755)
	}
	if err != nil {
		return clientFail(c, exitUsage, "usage", err.Error())
	}
	lockPath, statePath := filepath.Join(dir, "daemon.lock"), filepath.Join(dir, clientStateFile)
	if *status {
		out := map[string]any{"ok": true, "mode": "client", "server": raw}
		queued, refused, bad := spoolCounts(dir)
		out["spool"] = map[string]int{"queued": queued, "refused": refused, "bad": bad}
		var st clientState
		if b, err := os.ReadFile(statePath); err == nil {
			json.Unmarshal(b, &st)
		} else if b, err := os.ReadFile(filepath.Join(dir, clientDaemonRecordFile)); err == nil {
			json.Unmarshal(b, &st) // v0.10 stored relay state in this file.
		}
		if st.LastCallAt != "" {
			out["last_call_at"] = st.LastCallAt
		}
		if st.LastError != "" {
			out["last_error"] = st.LastError
		}
		pid := lockPID(lockPath)
		out["running"] = pid > 0 && syscall.Kill(pid, 0) == nil
		if out["running"] == true {
			out["pid"] = pid
			rec, err := readClientDaemonRecord(filepath.Join(dir, clientDaemonRecordFile))
			if err != nil || rec.PID != pid || rec.Version == "" {
				out["running_version"], out["stale"] = "unknown", true
			} else {
				out["running_version"], out["stale"] = rec.Version, rec.Version != version
				if rec.StartedAt != "" {
					out["started_at"] = rec.StartedAt
				}
			}
		}
		c.emit(out)
		return exitOK
	}
	if *restart {
		out, code, err := clientDaemonRestart(c, dir, lockPath)
		if err != nil {
			if e, ok := err.(*exitErr); ok {
				return clientFail(c, e.code, e.kind, e.msg)
			}
			return clientFail(c, exitDB, "database", err.Error())
		}
		c.emit(out)
		return code
	}
	lg := openDaemonLog(filepath.Join(dir, "daemon.log"))
	defer lg.close()
	sock := socketPath(c)
	h := &hostRelay{raw: raw, sock: sock, statePath: statePath, log: lg}
	if *once {
		err := h.pass()
		out := map[string]any{"ok": err == nil, "once": true, "mode": "client", "watch": h.watch}
		if err != nil {
			out["error"] = err.Error()
		}
		c.emit(out)
		if err != nil {
			return exitHerdr
		}
		return exitOK
	}
	lock, pid, err := acquireLock(lockPath)
	if err != nil {
		return clientFail(c, exitDB, "database", "daemon lock: "+err.Error())
	}
	if lock == nil {
		c.emit(map[string]any{"ok": true, "already_running": true, "pid": pid})
		return exitOK
	}
	recordPath := filepath.Join(dir, clientDaemonRecordFile)
	defer func() {
		if err := os.Remove(recordPath); err != nil {
			lg.logf("client daemon identity removal failed: %v", err)
		}
		lock.Truncate(0)
		lock.Close()
	}()
	rec, identityErr := selfClientDaemonRecord()
	if identityErr != nil {
		lg.logf("client daemon identity unavailable: %v", identityErr)
	} else if identityErr = writeClientDaemonRecord(recordPath, rec); identityErr != nil {
		lg.logf("client daemon identity write failed: %v", identityErr)
	}
	if identityErr != nil {
		if err := os.Remove(recordPath); err != nil && !os.IsNotExist(err) {
			lg.logf("client daemon identity removal failed: %v", err)
		}
	}
	lg.logf("start client pid %d version %s socket %s server %s", os.Getpid(), version, sock, raw)
	c.emit(map[string]any{"ok": true, "pid": os.Getpid(), "mode": "client", "server": raw, "socket": sock})
	sigs := []os.Signal{os.Interrupt, syscall.SIGTERM}
	if !signal.Ignored(syscall.SIGHUP) {
		sigs = append(sigs, syscall.SIGHUP)
	}
	sig, stop := signal.NotifyContext(context.Background(), sigs...)
	defer stop()
	lg.logf("exit: %s", h.run(sig))
	return exitOK
}

// hostRelay is the client daemon's state: the server's last watch list and
// the subscription that wakes it, plus the shared workspace token writer.
type hostRelay struct {
	raw, sock, statePath string
	log                  *daemonLog
	mu                   sync.Mutex
	watch                []string
	workspaceWriter      *daemon
}

type workspaceTokenValues struct {
	Campaign string `json:"campaign"`
	Parent   string `json:"parent,omitempty"`
}

// pass sends one agent listing. Without a listing nothing is sent: an empty
// one would mark every lane missing, while silence lets the heartbeat go
// stale and the lanes show unknown.
func (h *hostRelay) pass() error {
	err := h.observe()
	st := clientState{LastCallAt: now()}
	if err != nil {
		st.LastError = err.Error()
		h.log.limited("relay", time.Minute, "observe failed: %v", err)
	}
	b, _ := json.Marshal(st)
	os.WriteFile(h.statePath, b, 0o644)
	return err
}

func (h *hostRelay) observe() error {
	if !serverUp(h.sock) {
		return herdrErr("Herdr server not reachable at %s; nothing sent", h.sock)
	}
	agents, err := herdrAgentList(h.sock, time.Now().Add(herdrListDeadline))
	if err != nil {
		return err
	}
	list := make([]agentObs, 0, len(agents))
	for _, a := range agents {
		list = append(list, a)
	}
	slices.SortFunc(list, func(a, b agentObs) int { return strings.Compare(a.PaneID, b.PaneID) })
	js, _ := json.Marshal(list)
	cl, e := newRPCClient(h.raw)
	if e != nil {
		return e
	}
	rep, e := cl.call([]string{"--json", "_host", "observe", "--agents", string(js)}, filepath.Dir(h.statePath), newRequestKey(), nil)
	if e != nil {
		return e
	}
	var r struct {
		Watch           []string                        `json:"watch"`
		OwnerAsks       []ownerAskNotification          `json:"owner_asks"`
		WorkspaceTokens map[string]workspaceTokenValues `json:"workspace_tokens"`
		Error           string                          `json:"error"`
	}
	parseErr := json.Unmarshal([]byte(lastLine(rep.Stdout)), &r)
	if rep.Exit != exitOK {
		return &exitErr{rep.Exit, "rejected", "server: " + r.Error}
	}
	if parseErr != nil {
		return fmt.Errorf("server observe reply: %w", parseErr)
	}
	if _, err := sendSpool(filepath.Dir(h.statePath), h.raw, h.log); err != nil && h.log != nil {
		h.log.limited("spool-send", time.Minute, "spool send failed: %v", err)
	}
	h.mu.Lock()
	h.watch = r.Watch
	h.mu.Unlock()
	var notifyErr error
	for _, a := range r.OwnerAsks {
		if err := herdrRun(h.sock, "notification", "show", "taskr: decision needed",
			"--body", truncate(a.Summary, notifyBodyMax), "--sound", "request"); err != nil {
			h.log.logf("notify ask %d failed: %v", a.ID, err)
			if notifyErr == nil {
				notifyErr = err
			}
			continue
		}
		h.log.logf("notified ask %d", a.ID)
	}
	// A missing field is an older server or a failed query: preserve tokens.
	if r.WorkspaceTokens != nil {
		if h.workspaceWriter == nil {
			h.workspaceWriter = &daemon{sock: h.sock, log: h.log}
		}
		want := map[string]workspaceTokenState{}
		for workspace, ts := range r.WorkspaceTokens {
			want[workspace] = workspaceTokenState{campaign: ts.Campaign, parent: ts.Parent,
				hasCampaign: ts.Campaign != "", hasParent: ts.Parent != ""}
		}
		h.workspaceWriter.writeWorkspaceTokens(want)
	}
	notifySpoolRefused(filepath.Dir(h.statePath), h.sock, h.log)
	notifySpoolBad(filepath.Dir(h.statePath), h.sock, h.log)
	return notifyErr
}

// run passes every clientObserveEvery and after each Herdr event, until cx
// ends or the Herdr socket goes away.
func (h *hostRelay) run(cx context.Context) string {
	cx, cancel := context.WithCancel(cx)
	defer cancel()
	d := &daemon{log: h.log, sock: h.sock, resub: make(chan struct{}, 1), watch: func() ([]string, error) {
		h.mu.Lock()
		defer h.mu.Unlock()
		return slices.Clone(h.watch), nil
	}}
	dirty := make(chan struct{}, 1)
	mark := func() {
		select {
		case dirty <- struct{}{}:
		default:
		}
	}
	gone, readerDone := make(chan struct{}), make(chan struct{})
	go func() {
		defer close(readerDone)
		if d.subscribe(cx, h.sock, mark) {
			close(gone)
		}
	}()
	defer func() { cancel(); <-readerDone }()
	tick := time.NewTicker(clientObserveEvery)
	defer tick.Stop()
	h.pass()
	d.refreshPanes()
	for {
		select {
		case <-cx.Done():
			return "signal"
		case <-gone:
			return "socket removed"
		case <-tick.C:
			if h.workspaceWriter != nil {
				h.workspaceWriter.failedWorkspaceWant = nil
			}
		case <-dirty:
			select {
			case <-cx.Done():
				return "signal"
			case <-time.After(daemonSettle):
			}
		}
		select {
		case <-dirty:
		default:
		}
		h.pass()
		d.refreshPanes()
	}
}
