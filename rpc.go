package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"
)

// The RPC route: a remote host's taskr CLI sends its command line here and
// the daemon runs it in-process against its resident ledger. Only admitted
// tailnet peers reach it (same login, no tags, never this node itself); the
// caller's host is whois's answer, never a body field. A command that may
// write is stored under the caller's request key, so a retry after a lost
// response gets the stored result instead of a second effect.

const (
	rpcPath        = "/api/rpc"
	rpcBodyMax     = 2 << 20
	rpcHeader      = "X-Taskr-RPC"
	requestKeyFlag = "--request-key"
	requestsKeep   = 7 * 24 * time.Hour
)

// rpcClientEnv is what a caller may send: identity strings that are stored,
// never opened. rpcServerEnv comes from the daemon.
var (
	rpcClientEnv = []string{"TASKR_TASK", "TASKR_LAUNCH", "TASKR_FORMAT", "HERDR_PANE_ID", "HERDR_WORKSPACE_ID",
		"HERDR_TAB_ID", "CODEX_HOME", "CODEX_THREAD_ID", "CLAUDE_CONFIG_DIR"}
	rpcServerEnv = []string{"TASKR_DB", "HOME", "HERDR_SOCKET_PATH"}
	requestKeyRe = regexp.MustCompile(`^[A-Za-z0-9_-]{8,128}$`)
)

type rpcRequest struct {
	Argv         []string          `json:"argv"`
	Cwd          string            `json:"cwd"`
	Env          map[string]string `json:"env"`
	RequestKey   string            `json:"request_key"`
	Capabilities []string          `json:"capabilities,omitempty"`
	Document     *rpcDocPayload    `json:"document,omitempty"`
	QueuedAt     string            `json:"queued_at,omitempty"`
}

type rpcReply struct {
	Exit   int           `json:"exit"`
	Stdout string        `json:"stdout"`
	Stderr string        `json:"stderr"`
	Upload *[]rpcDocWant `json:"upload,omitempty"`
}

const docUploadCapability = "doc-upload"

type rpcDocPayload struct {
	Task     int64   `json:"task"`
	Kind     string  `json:"kind"`
	Name     string  `json:"name"`
	Path     string  `json:"path"`
	EventID  *int64  `json:"event_id"`
	Body     *string `json:"body,omitempty"` // base64 text; a pointer allows an empty document
	SHA256   string  `json:"sha256,omitempty"`
	Bytes    *int64  `json:"bytes,omitempty"`
	Reason   string  `json:"reason,omitempty"`
	Backfill bool    `json:"backfill,omitempty"`
	DryRun   bool    `json:"dry_run,omitempty"`
}

type rpcDocWant struct {
	Task    int64  `json:"task"`
	Kind    string `json:"kind"`
	Name    string `json:"name"`
	Path    string `json:"path"`
	EventID *int64 `json:"event_id"`
}

// hiddenCommands are the client's own calls, run only over RPC: `_prompt`
// splits a prompt for a lane, `_host` is the client daemon, and `_hook` records
// one fire-and-forget hook call.
var hiddenCommands = map[string]command{"_prompt": cmdPromptPhase, "_host": cmdHost, "_hook": cmdHookRPC, "_doc": cmdDocRPC}

// freshCommands always run anew and are never stored; a retry is a new read.
var freshCommands = map[string]bool{"wait": true, "status": true, "asks": true, "log": true, "version": true, "help": true}

// rpcCommand is the command name in argv after a leading --json, or "".
func rpcCommand(argv []string) (string, []string) {
	if len(argv) > 0 && argv[0] == "--json" {
		argv = argv[1:]
	}
	if len(argv) == 0 {
		return "", nil
	}
	return argv[0], argv[1:]
}

// wantsHelp reports a -h/--help flag before any `--`.
func wantsHelp(args []string) bool {
	for _, a := range args {
		if a == "--" {
			return false
		}
		name, value, hasValue := strings.Cut(strings.TrimLeft(a, "-"), "=")
		if strings.HasPrefix(a, "-") && (name == "h" || name == "help") && (!hasValue || value == "true") {
			return true
		}
	}
	return false
}

// storedCommand reports whether the handler keeps this command's result
// under its request key: every known command but the fresh ones, daemon,
// and help requests.
func storedCommand(argv []string) bool {
	name, args := rpcCommand(argv)
	if name == "_hook" {
		return true
	}
	if _, ok := commands[name]; !ok || name == "daemon" || freshCommands[name] {
		return false
	}
	if name == "doc" && len(args) > 0 && (args[0] == "get" || args[0] == "ls" || args[0] == "backfill") {
		return false
	}
	return !wantsHelp(args)
}

// flagValue returns the value of --name (or -name, --name=V) before any `--`,
// the index of the argument that holds it, and whether it is inline (=V).
func flagValue(args []string, name string) (value string, at int, inline, found bool) {
	for i := 0; i < len(args); i++ {
		a := args[i]
		if a == "--" {
			break
		}
		if !strings.HasPrefix(a, "-") {
			continue
		}
		n, v, hasValue := strings.Cut(strings.TrimLeft(a, "-"), "=")
		if n != name {
			continue
		}
		if hasValue {
			return v, i, true, true
		}
		if i+1 < len(args) {
			return args[i+1], i + 1, false, true
		}
		return "", -1, false, true
	}
	return "", -1, false, false
}

func flagMS(args []string, name string, dflt int64) time.Duration {
	if v, _, _, ok := flagValue(args, name); ok {
		if n, err := strconv.ParseInt(v, 10, 64); err == nil && n >= 0 {
			return time.Duration(n) * time.Millisecond
		}
	}
	return time.Duration(dflt) * time.Millisecond
}

func flagTrue(args []string, name string) bool {
	for _, a := range args {
		if a == "--" {
			return false
		}
		if n, v, hasValue := strings.Cut(strings.TrimLeft(a, "-"), "="); strings.HasPrefix(a, "-") && n == name {
			return !hasValue || v == "true" || v == "1"
		}
	}
	return false
}

// rpcBudget is how long a command may run: wait its --timeout; prompt and
// answer --prompt the Herdr send plus the confirm window; others 30 s. The
// client's HTTP deadline and the server's write deadline are both this plus
// rpcSlack, from this one function, so they agree.
func rpcBudget(argv []string) time.Duration {
	name, args := rpcCommand(argv)
	base := 30 * time.Second
	if wantsHelp(args) {
		return base
	}
	confirm := func() time.Duration {
		if flagTrue(args, "confirm") {
			return flagMS(args, "confirm-timeout", 60000)
		}
		return 0
	}
	switch name {
	case "wait":
		return flagMS(args, "timeout", 540000)
	case "prompt", "_prompt":
		return herdrPromptDeadline + confirm()
	case "answer":
		if flagTrue(args, "prompt") {
			return herdrPromptDeadline + confirm()
		}
	}
	return base
}

const rpcSlack = 10 * time.Second

// rpcPathFlags are the path flags a client absolutizes before sending; the
// server refuses a relative one.
var rpcPathFlags = map[string][]string{"new": {"cwd", "brief", "report"}, "ready": {"report"}, "prompt": {"file"}}

var rpcScalarFlags = map[string]bool{
	"cwd": true, "brief": true, "report": true, "file": true, "out": true,
	"machine": true, "role": true, "planned": true, "timeout": true, "name": true,
	"confirm": true, "confirm-timeout": true, "prompt": true, "receipt-timeout": true,
}

var rpcBoolFlags = map[string]bool{"planned": true, "confirm": true, "prompt": true}

// repeatedRPCFlag finds a repeated RPC-interpreted flag without mistaking a
// consumed value or a positional argument after -- for another flag.
func repeatedRPCFlag(args []string) string {
	seen := map[string]bool{}
	for i := 0; i < len(args); i++ {
		a := args[i]
		if a == "--" {
			break
		}
		if !strings.HasPrefix(a, "-") || a == "-" {
			continue
		}
		name, _, inline := strings.Cut(strings.TrimLeft(a, "-"), "=")
		if !rpcScalarFlags[name] {
			continue
		}
		if seen[name] {
			return name
		}
		seen[name] = true
		if !inline && !rpcBoolFlags[name] && i+1 < len(args) {
			i++
		}
	}
	return ""
}

// rpcCheckArgs refuses what a remote caller cannot run here: the daemon, a
// relative path (it would resolve on this host), and a file write
// (handover --out; the client writes that file itself).
func rpcCheckArgs(argv []string) error {
	name, args := rpcCommand(argv)
	if flag := repeatedRPCFlag(args); flag != "" {
		return usageErr("repeated RPC flag --%s", flag)
	}
	if name == "daemon" {
		return usageErr("daemon runs on its own host; it is not available over RPC")
	}
	if name == "spool" {
		return usageErr("spool commands run on their own host; they are not available over RPC")
	}
	for _, f := range rpcPathFlags[name] {
		if v, _, _, ok := flagValue(args, f); ok && v != "" && !filepath.IsAbs(v) {
			return usageErr("--%s must be an absolute path over RPC, got %q", f, v)
		}
	}
	if _, _, _, ok := flagValue(args, "out"); ok && name == "handover" {
		return usageErr("handover --out writes on the caller's host; the client writes it")
	}
	return nil
}

func argvSHA(argv []string) string {
	b, _ := json.Marshal(argv)
	sum := sha256.Sum256(b)
	return hex.EncodeToString(sum[:])
}

func rpcRequestSHA(req rpcRequest) string {
	if req.Document == nil {
		return argvSHA(req.Argv)
	}
	b, _ := json.Marshal(struct {
		Argv     []string       `json:"argv"`
		Document *rpcDocPayload `json:"document"`
	}{req.Argv, req.Document})
	sum := sha256.Sum256(b)
	return hex.EncodeToString(sum[:])
}

// rpc admits the caller, then runs its command line. Every refusal before the
// run is an HTTP error; every result of a run, failures included, is 200
// with the exit code and both streams.
func (d *dashboard) rpc(w http.ResponseWriter, r *http.Request) {
	hub := d.hub.Load()
	id, ok := r.Context().Value(identKey{}).(peerIdent)
	switch {
	case hub == nil || !ok:
		httpError(w, http.StatusForbidden, "rpc needs a tailnet identity")
		return
	case id.NodeID == hub.self.NodeID:
		// A process on this host reaching its own tailnet address is not a
		// remote host: it uses the ledger directly.
		httpError(w, http.StatusForbidden, "this node is the server")
		return
	case id.Machine == "" || id.Machine == localMachine():
		httpError(w, http.StatusForbidden, "the node's name is the server's name")
		return
	case r.Header.Get("Origin") != "" || r.Header.Get("Sec-Fetch-Site") != "":
		httpError(w, http.StatusForbidden, "browser requests are refused")
		return
	case !isJSON(r):
		httpError(w, http.StatusUnsupportedMediaType, "content type must be application/json")
		return
	case r.Header.Get(rpcHeader) != "1":
		httpError(w, http.StatusBadRequest, rpcHeader+": 1 is required")
		return
	}
	var req rpcRequest
	if err := decodeStrict(w, r, rpcBodyMax, &req); err != nil {
		if tooLarge(err) {
			httpError(w, http.StatusRequestEntityTooLarge, "request too large")
		} else {
			httpError(w, http.StatusBadRequest, "bad request: "+err.Error())
		}
		return
	}
	switch {
	case len(req.Argv) == 0:
		httpError(w, http.StatusBadRequest, "argv is empty")
		return
	case !filepath.IsAbs(req.Cwd):
		httpError(w, http.StatusBadRequest, "cwd must be an absolute path")
		return
	case !requestKeyRe.MatchString(req.RequestKey):
		httpError(w, http.StatusBadRequest, "request_key must match [A-Za-z0-9_-]{8,128}")
		return
	}
	http.NewResponseController(w).SetWriteDeadline(time.Now().Add(rpcBudget(req.Argv) + rpcSlack))
	name, _ := rpcCommand(req.Argv)
	if _, ok := commands[name]; !ok && name != "help" && hiddenCommands[name] == nil {
		name = "unknown" // the log never carries argv text
	}
	var rep rpcReply
	if storedCommand(req.Argv) {
		rep = d.rpcStored(id.Machine, req)
	} else {
		rep = d.rpcRun(r.Context(), id.Machine, req)
	}
	d.log.logf("rpc: machine=%s cmd=%s key=%s exit=%d", id.Machine, name, req.RequestKey, rep.Exit)
	httpJSON(w, http.StatusOK, rep)
}

// rpcRun runs one command line in-process for a caller on machine. cx ends
// a wait (the request's context); a panic is exit 4.
func (d *dashboard) rpcRun(cx context.Context, machine string, req rpcRequest) (rep rpcReply) {
	serverEnv := d.serverEnv
	if serverEnv == nil {
		serverEnv = os.Getenv
	}
	env := map[string]string{}
	for _, k := range rpcClientEnv {
		if v := req.Env[k]; v != "" {
			env[k] = v
		}
	}
	for _, k := range rpcServerEnv {
		if v := serverEnv(k); v != "" {
			env[k] = v
		}
	}
	var out, errw bytes.Buffer
	c := &ctx{getenv: func(k string) string { return env[k] }, out: &out, errw: &errw,
		db: d.db, log: d.log, cx: cx, rpc: true, machine: machine, cwd: req.Cwd,
		docUpload: hasCapability(req.Capabilities, docUploadCapability), remoteDoc: req.Document, queuedAt: req.QueuedAt}
	defer func() {
		if p := recover(); p != nil {
			d.log.logf("rpc: machine=%s key=%s panicked: %s", machine, req.RequestKey, truncate(fmt.Sprint(p), 200))
			fmt.Fprintln(&errw, "taskr: internal error; inspect `taskr log` before a retry")
			rep = rpcReply{Exit: exitDB, Stdout: out.String(), Stderr: errw.String()}
		}
	}()
	code := runCtx(c, req.Argv)
	rep = rpcReply{Exit: code, Stdout: out.String(), Stderr: errw.String()}
	if code == exitOK && c.docUpload {
		if len(c.docUploads) > 0 {
			upload := c.docUploads
			rep.Upload = &upload
		}
	}
	return rep
}

// rpcStored runs a command that may write at most once per request key. In
// one immediate transaction it claims the key as running (pruning week-old
// keys), or returns the stored result, or refuses a key reused by another
// host or another command line. The run continues when the caller goes
// away, and its result is stored for the retry.
func (d *dashboard) rpcStored(machine string, req rpcRequest) rpcReply {
	sha := rpcRequestSHA(req)
	var prior *rpcReply
	var refusal error
	err := withTx(d.db, func(tx *sql.Tx) error {
		if _, err := tx.Exec(`delete from requests where created_at < ?`, stamp(time.Now().Add(-requestsKeep))); err != nil {
			return err
		}
		var m sql.NullString
		var argv, state string
		var exit sql.NullInt64
		var stdout, stderr, upload sql.NullString
		err := tx.QueryRow(`select machine, argv_sha, state, exit, stdout, stderr, upload from requests where key = ?`, req.RequestKey).
			Scan(&m, &argv, &state, &exit, &stdout, &stderr, &upload)
		switch {
		case errors.Is(err, sql.ErrNoRows):
			_, err = tx.Exec(`insert into requests (key, machine, argv_sha, state, created_at) values (?, ?, ?, 'running', ?)`,
				req.RequestKey, machine, sha, now())
			return err
		case err != nil:
			return err
		case m.String != machine || argv != sha:
			refusal = rejectErr("request key %s belongs to another command", req.RequestKey)
		case state == "done":
			prior = &rpcReply{Exit: int(exit.Int64), Stdout: stdout.String, Stderr: stderr.String}
			if upload.Valid {
				if err := json.Unmarshal([]byte(upload.String), &prior.Upload); err != nil {
					return err
				}
			}
		default:
			refusal = herdrErr("request %s: outcome unknown (still running, or the server stopped during it); inspect `taskr log` before any resend", req.RequestKey)
		}
		return nil
	})
	if err == nil && refusal != nil {
		err = refusal
	}
	if err != nil {
		return d.rpcError(req, err)
	}
	if prior != nil {
		return *prior
	}
	rep := d.rpcRun(context.Background(), machine, req)
	upload, _ := json.Marshal(rep.Upload)
	if _, err := d.db.Exec(`update requests set state = 'done', exit = ?, stdout = ?, stderr = ?, upload = ? where key = ?`,
		rep.Exit, rep.Stdout, rep.Stderr, string(upload), req.RequestKey); err != nil {
		d.log.logf("rpc: storing the result of %s failed: %v", req.RequestKey, err)
	}
	return rep
}

// rpcError formats a refusal the way the CLI prints an error.
func (d *dashboard) rpcError(req rpcRequest, err error) rpcReply {
	var e *exitErr
	if !errors.As(err, &e) {
		e = &exitErr{exitDB, "database", err.Error()}
	}
	var out, errw bytes.Buffer
	name, _ := rpcCommand(req.Argv)
	c := &ctx{getenv: func(string) string { return "" }, out: &out, errw: &errw, cmd: name, code: e.code}
	c.json = req.Env["TASKR_FORMAT"] == "json" || req.Argv[0] == "--json"
	if c.json {
		fmt.Fprintf(&errw, "taskr %s: %s\n", name, e.msg)
	}
	c.emit(map[string]any{"error": e.msg, "kind": e.kind})
	return rpcReply{Exit: e.code, Stdout: out.String(), Stderr: errw.String()}
}
