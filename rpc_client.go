package main

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"io/fs"
	"net"
	"net/http"
	"net/http/httptrace"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// Client mode: with <stateDir>/server.url and no TASKR_DB, this CLI sends
// each command line to the server's RPC route and prints its answer. It
// never opens a local ledger. Help and version still run here, offline.

const (
	serverURLFile = "server.url"
	rpcReplyMax   = 64 << 20
)

// readServerURL returns the one line of <stateDir>/server.url, and whether
// the file exists (an unreadable file exists: it is refused, not skipped).
func readServerURL(dir string) (string, bool, error) {
	b, err := os.ReadFile(filepath.Join(dir, serverURLFile))
	if errors.Is(err, fs.ErrNotExist) {
		return "", false, nil
	} else if err != nil {
		return "", true, err
	}
	v, _, _ := strings.Cut(string(b), "\n")
	return strings.TrimSpace(v), true, nil
}

// clientFail prints an error the way the CLI does and returns its code.
func clientFail(c *ctx, code int, kind, msg string) int {
	c.code = code
	if c.json {
		fmt.Fprintf(c.errw, "taskr %s: %s\n", c.cmd, msg)
	}
	c.emit(map[string]any{"error": msg, "kind": kind})
	return code
}

func clientMain(raw string, readErr error, args []string, getenv func(string) string, stdout, stderr io.Writer) int {
	c := &ctx{getenv: getenv, out: stdout, errw: stderr, client: true, server: raw}
	c.json = c.env("TASKR_FORMAT") == "json"
	var lead []string
	key, rest := "", args
lead:
	for len(rest) > 0 {
		switch a := rest[0]; {
		case a == "--json":
			lead, rest, c.json = append(lead, a), rest[1:], true
		case a == requestKeyFlag && len(rest) > 1:
			key, rest = rest[1], rest[2:]
		case a == requestKeyFlag:
			return clientFail(c, exitUsage, "usage", requestKeyFlag+" needs a value")
		case strings.HasPrefix(a, requestKeyFlag+"="):
			key, rest = strings.TrimPrefix(a, requestKeyFlag+"="), rest[1:]
		default:
			break lead
		}
	}
	name, cargs := rpcCommand(rest)
	c.cmd = name
	if _, known := commands[name]; !known || name == "help" || name == "version" || wantsHelp(cargs) {
		return runCtx(c, append(lead, rest...))
	}
	if name == "hook" {
		if c.env("TASKR_LAUNCH") == "" {
			return exitOK
		}
		_, _, _ = cmdHook(c, cargs)
		return exitOK
	}
	if name == "spool" {
		return runCtx(c, append(lead, rest...))
	}
	if flag := repeatedRPCFlag(cargs); flag != "" {
		return clientFail(c, exitUsage, "usage", "repeated RPC flag --"+flag)
	}
	if readErr != nil {
		return clientFail(c, exitUsage, "usage", serverURLFile+": "+readErr.Error())
	}
	if name == "daemon" {
		return clientDaemon(c, raw, cargs)
	}
	if key == "" {
		key = newRequestKey()
	} else if !requestKeyRe.MatchString(key) {
		return clientFail(c, exitUsage, "usage", requestKeyFlag+" must match [A-Za-z0-9_-]{8,128}")
	}
	cwd, err := os.Getwd()
	if err != nil {
		return clientFail(c, exitUsage, "usage", "current directory: "+err.Error())
	}
	cargs, out, err := clientPaths(name, slices.Clone(cargs), cwd)
	if err != nil {
		return clientFail(c, exitUsage, "usage", err.(*exitErr).msg)
	}
	argv := append(append(slices.Clone(lead), name), cargs...)
	retry := "taskr " + requestKeyFlag + " " + key + " " + shellJoin(slices.Concat(lead, rest))
	fail := func(e *exitErr) int {
		if e.kind == "transport" {
			fmt.Fprintf(stderr, "taskr: server unreachable; retry with: %s\n", retry)
			return clientFail(c, exitHerdr, "transport", "server unreachable ("+e.msg+"); retry with: "+retry)
		}
		return clientFail(c, e.code, e.kind, e.msg)
	}
	env := map[string]string{}
	for _, k := range rpcClientEnv {
		if v := c.env(k); v != "" {
			env[k] = v
		}
	}
	var remoteDoc *rpcDocPayload
	if name == "doc" && len(cargs) > 0 && cargs[0] == "set" {
		remoteDoc, err = clientDocSetPayload(c, cargs)
		if err != nil {
			var e *exitErr
			if errors.As(err, &e) {
				return clientFail(c, e.code, e.kind, e.msg)
			}
			return clientFail(c, exitUsage, "usage", err.Error())
		}
	}
	spoolable := spoolRecordCommand(argv)
	var savedReady *rpcDocPayload
	if name == "ready" {
		savedReady = clientReadySpoolDocument(cargs, env)
	}
	request := rpcClientRequest(argv, cwd, key, env, remoteDoc)
	var spoolDir string
	if spoolable {
		spoolDir, err = stateDir(c)
		if err != nil {
			return clientFail(c, exitUsage, "usage", err.(*exitErr).msg)
		}
		waiting, queued, queueErr := queueSpoolIfWaiting(spoolDir, request, savedReady)
		if queueErr != nil {
			if errors.Is(queueErr, errSpoolFull) {
				fmt.Fprintln(stderr, "taskr: spool full")
				return fail(&exitErr{exitHerdr, "transport", "server unreachable; spool full"})
			}
			return clientFail(c, exitDB, "database", queueErr.Error())
		}
		if queued {
			return clientQueuedResult(c, cargs, key, waiting, stderr)
		}
	}
	cl, e := newRPCClient(raw)
	if e != nil {
		if spoolable && e.kind == "transport" {
			waiting, queueErr := queueSpoolRecord(spoolDir, request, savedReady)
			if queueErr == nil {
				return clientQueuedResult(c, cargs, key, waiting, stderr)
			}
			if errors.Is(queueErr, errSpoolFull) {
				fmt.Fprintln(stderr, "taskr: spool full")
				return fail(&exitErr{exitHerdr, "transport", "server unreachable; spool full"})
			}
			return fail(&exitErr{exitHerdr, "transport", "server unreachable; spool write failed: " + queueErr.Error()})
		}
		return fail(e)
	}
	if name == "new" {
		if err := clientNewChecks(cargs, cl.self.Short); err != nil {
			return clientFail(c, exitUsage, "usage", err.(*exitErr).msg)
		}
	}
	if name == "prompt" {
		if code, done := clientPrompt(c, cl, lead, cargs, cwd, env); done {
			return code
		}
	}
	if name == "doc" && len(cargs) > 0 && cargs[0] == "backfill" {
		return clientDocBackfill(c, cl, cargs[1:], cwd, env)
	}
	rep, e := cl.callRetry(c, argv, cwd, key, retry, env, remoteDoc)
	if e != nil {
		if spoolable && cl.noReply {
			waiting, queueErr := queueSpoolRecord(spoolDir, request, savedReady)
			if queueErr == nil {
				return clientQueuedResult(c, cargs, key, waiting, stderr)
			}
			if errors.Is(queueErr, errSpoolFull) {
				fmt.Fprintln(stderr, "taskr: spool full")
				return fail(&exitErr{exitHerdr, "transport", "server unreachable; spool full"})
			}
			return fail(&exitErr{exitHerdr, "transport", "server unreachable; spool write failed: " + queueErr.Error()})
		}
		return fail(e)
	}
	var uploads []rpcDocWant
	if rep.Upload != nil {
		uploads = *rep.Upload
	}
	rep.Upload = nil
	io.WriteString(stdout, rep.Stdout)
	io.WriteString(stderr, rep.Stderr)
	if out != "" && rep.Exit == exitOK {
		if err := os.WriteFile(out, []byte(rep.Stdout), 0o644); err != nil {
			fmt.Fprintf(stderr, "taskr handover: the handover is recorded, but --out failed: %v\n", err)
			return exitUsage
		}
		fmt.Fprintf(stderr, "taskr handover: wrote %s\n", out)
	}
	if rep.Exit == exitOK {
		clientUploadDocs(cl, uploads, cwd, env)
	}
	return rep.Exit
}

// rpcClient is a checked route to the server that server.url names.
type rpcClient struct {
	target  string
	self    *tsSelf
	noReply bool
}

// newRPCClient checks server.url against this node's tailnet and learns this
// node's identity. A refused URL is a usage error; anything that keeps the
// check from running is kind transport.
func newRPCClient(raw string) (*rpcClient, *exitErr) {
	var self *tsSelf
	identity := func() (*tsSelf, error) {
		var err error
		if self == nil {
			self, err = tailscaleIdentity()
		}
		return self, err
	}
	target, err := checkHubURL(raw, func() (string, error) {
		s, err := identity()
		if err != nil {
			return "", err
		}
		return s.Suffix, nil
	})
	if err != nil && !strings.HasPrefix(err.Error(), "cannot check") {
		return nil, &exitErr{exitUsage, "usage", serverURLFile + " refused: " + err.Error()}
	}
	if err == nil {
		_, err = identity() // whois of the server needs this node's user
	}
	if err != nil {
		return nil, &exitErr{exitHerdr, "transport", err.Error()}
	}
	return &rpcClient{target: target, self: self}, nil
}

// rpcRetryWindow is injectable so outage tests need not wait a minute.
var rpcRetryWindow = func(argv []string) time.Duration {
	if spoolRecordCommand(argv) {
		return 3 * time.Second
	}
	return max(60*time.Second, rpcBudget(argv))
}

func (cl *rpcClient) callRetry(c *ctx, argv []string, cwd, key, retry string, env map[string]string, documents ...*rpcDocPayload) (rpcReply, *exitErr) {
	cl.noReply = false
	name, _ := rpcCommand(argv)
	parent := context.Background()
	if name != "wait" {
		var stop context.CancelFunc
		parent, stop = signal.NotifyContext(parent, os.Interrupt, syscall.SIGTERM, syscall.SIGHUP)
		defer stop()
	}
	window := rpcRetryWindow(argv)
	if name == "wait" {
		window = rpcBudget(argv)
	}
	deadline := time.Now().Add(window)
	backoff := time.Second
	connected, announced := false, false
	var rep rpcReply
	var lastErr, transportErr *exitErr
	for attempt := 0; ; attempt++ {
		if parent.Err() != nil {
			return rpcReply{}, &exitErr{exitHerdr, "transport", parent.Err().Error()}
		}
		if attempt > 0 && time.Until(deadline) <= 0 {
			if name == "wait" && connected {
				// The server is unavailable, so ledger counts are unknown.
				var out bytes.Buffer
				_, args := rpcCommand(argv)
				tc := &ctx{out: &out, json: c.json || flagTrue(args, "json"), cmd: "wait"}
				as := c.env("TASKR_TASK")
				if v, _, _, ok := flagValue(args, "as"); ok {
					as = v
				}
				task, _ := strconv.ParseInt(as, 10, 64)
				tc.emit(map[string]any{"timeout": true, "as": task, "unreachable": true})
				code := exitOK
				if tc.json {
					code = exitTimeout
				}
				return rpcReply{Exit: code, Stdout: out.String()}, nil
			}
			if transportErr != nil {
				cl.noReply = true
				return rpcReply{}, transportErr
			}
			return rep, lastErr
		}
		cx := parent
		cancel := func() {}
		// A zero-timeout wait still makes one inbox read.
		if name != "wait" || window > 0 {
			httpDeadline := deadline
			if name == "wait" {
				// Keep response slack for the server's normal timeout counts.
				httpDeadline = httpDeadline.Add(rpcSlack)
			} else {
				if attemptDeadline := time.Now().Add(rpcBudget(argv) + rpcSlack); !spoolRecordCommand(argv) && attemptDeadline.After(httpDeadline) {
					httpDeadline = attemptDeadline
				}
			}
			cx, cancel = context.WithDeadline(cx, httpDeadline)
		}
		var retryable, reached bool
		if name == "wait" {
			rep, lastErr, retryable, reached = cl.callOnce(cx, argv, cwd, key, env, documents...)
		} else {
			// Verification has its own timeout; a signal must not wait for it.
			done := make(chan struct{})
			go func() {
				rep, lastErr, retryable, reached = cl.callOnce(cx, argv, cwd, key, env, documents...)
				close(done)
			}()
			select {
			case <-parent.Done():
				cancel()
				return rpcReply{}, &exitErr{exitHerdr, "transport", parent.Err().Error()}
			case <-done:
			}
		}
		cancel()
		if parent.Err() != nil {
			return rpcReply{}, &exitErr{exitHerdr, "transport", parent.Err().Error()}
		}
		if retryable {
			transportErr = lastErr
		}
		connected = connected || reached
		if !retryable && !(name != "wait" && lastErr == nil && rep.Exit == exitHerdr &&
			strings.Contains(rep.Stdout, "outcome unknown (still running")) {
			return rep, lastErr
		}
		if time.Until(deadline) <= 0 {
			continue
		}
		if !announced && !spoolRecordCommand(argv) {
			reason, suffix := "server unreachable", ""
			if name != "wait" {
				if !retryable {
					reason = "request still running"
				}
				suffix = "; if interrupted, retry with: " + retry
			}
			fmt.Fprintf(c.errw, "taskr: %s; retrying until %s%s\n", reason, deadline.UTC().Format(time.RFC3339), suffix)
			announced = true
		}
		timer := time.NewTimer(max(0, min(backoff, time.Until(deadline))))
		select {
		case <-parent.Done():
			timer.Stop()
			return rpcReply{}, &exitErr{exitHerdr, "transport", parent.Err().Error()}
		case <-timer.C:
		}
		timer.Stop()
		backoff = min(2*backoff, 10*time.Second)
		if name == "wait" {
			key = newRequestKey()
			// wait is never stored, and --ack is idempotent on a resend.
			argv = slices.Clone(argv)
			_, args := rpcCommand(argv)
			remaining := fmt.Sprint(max(0, time.Until(deadline).Milliseconds()))
			_, at, inline, found := flagValue(args, "timeout")
			offset := len(argv) - len(args)
			if found && at >= 0 {
				if inline {
					argv[offset+at] = "--timeout=" + remaining
				} else {
					argv[offset+at] = remaining
				}
			} else if !found {
				// Insert before -- so the parser sees the flag.
				at := slices.Index(argv, "--")
				if at < 0 {
					at = len(argv)
				}
				argv = slices.Insert(argv, at, "--timeout", remaining)
			}
		}
	}
}

func (cl *rpcClient) call(argv []string, cwd, key string, env map[string]string, documents ...*rpcDocPayload) (rpcReply, *exitErr) {
	rep, e, _, _ := cl.callOnce(context.Background(), argv, cwd, key, env, documents...)
	return rep, e
}

var rpcHTTPTransport = func() http.RoundTripper {
	return &http.Transport{Proxy: nil, DialContext: transportDial, DisableKeepAlives: true}
}

// callOnce reports retryability and whether it connected to the verified server.
func (cl *rpcClient) callOnce(parent context.Context, argv []string, cwd, key string, env map[string]string, documents ...*rpcDocPayload) (rpcReply, *exitErr, bool, bool) {
	var document *rpcDocPayload
	if len(documents) > 0 {
		document = documents[0]
	}
	return cl.callOnceMode(parent, argv, cwd, key, env, document, true, true)
}

func (cl *rpcClient) callOnceNoFallback(parent context.Context, argv []string, cwd, key string, env map[string]string, document *rpcDocPayload) (rpcReply, *exitErr, bool, bool) {
	return cl.callOnceMode(parent, argv, cwd, key, env, document, true, false)
}

func (cl *rpcClient) callOnceMode(parent context.Context, argv []string, cwd, key string, env map[string]string, document *rpcDocPayload, capability, fallback bool) (rpcReply, *exitErr, bool, bool) {
	req := rpcRequest{Argv: argv, Cwd: cwd, Env: env, RequestKey: key, Document: document}
	if capability && rpcCarriesDocument(argv) {
		req.Capabilities = []string{docUploadCapability}
	}
	return cl.callOnceRequest(parent, req, fallback)
}

func (cl *rpcClient) callOnceRequest(parent context.Context, reqBody rpcRequest, fallback bool) (rpcReply, *exitErr, bool, bool) {
	unreachable := func(why string, retryable, reached bool) (rpcReply, *exitErr, bool, bool) {
		return rpcReply{}, &exitErr{exitHerdr, "transport", why}, retryable, reached
	}
	argv := reqBody.Argv
	body, _ := json.Marshal(reqBody)
	u, _ := url.Parse(cl.target)
	cx, cancel := context.WithTimeout(parent, rpcBudget(argv)+rpcSlack)
	defer cancel()
	whois := newWhoisCache(cl.self.Login)
	whois.arg = hubWhoisArg
	conn, err := (&pusher{hubWhois: whois}).dial(cx, "tcp", u.Host)
	if err != nil {
		return unreachable(err.Error(), true, false)
	}
	vc := conn.(*verifiedConn)
	defer vc.Close()
	usedOurs := false
	trace := &httptrace.ClientTrace{GotConn: func(info httptrace.GotConnInfo) { usedOurs = info.Conn == net.Conn(vc) }}
	rcx := context.WithValue(httptrace.WithClientTrace(cx, trace), verifiedConnKey{}, vc)
	req, _ := http.NewRequestWithContext(rcx, http.MethodPost, "http://"+u.Host+rpcPath, bytes.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set(rpcHeader, "1")
	hc := &http.Client{Transport: rpcHTTPTransport(),
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	resp, err := hc.Do(req)
	if err != nil {
		return unreachable(err.Error(), true, true)
	}
	defer resp.Body.Close()
	rb, err := io.ReadAll(io.LimitReader(resp.Body, rpcReplyMax+1))
	switch {
	case err != nil:
		return unreachable("reply cut off: "+err.Error(), usedOurs && resp.StatusCode == http.StatusOK, true)
	case len(rb) > rpcReplyMax:
		return unreachable("reply too large", false, true)
	case !usedOurs:
		return unreachable("the request did not use the verified server connection", false, true)
	}
	if resp.StatusCode == http.StatusBadRequest && fallback {
		var refusal struct {
			Error string `json:"error"`
		}
		if json.Unmarshal(rb, &refusal) == nil {
			changed := false
			switch {
			case len(reqBody.Capabilities) > 0 && strings.Contains(refusal.Error, `unknown field "capabilities"`):
				reqBody.Capabilities, reqBody.Document = nil, nil
				changed = true
			case reqBody.Document != nil && strings.Contains(refusal.Error, `unknown field "document"`):
				reqBody.Document = nil
				changed = true
			case reqBody.QueuedAt != "" && strings.Contains(refusal.Error, `unknown field "queued_at"`):
				reqBody.QueuedAt = ""
				changed = true
			}
			if changed {
				vc.Close()
				return cl.callOnceRequest(parent, reqBody, true)
			}
		}
	}
	if resp.StatusCode != http.StatusOK {
		var e struct {
			Error string `json:"error"`
		}
		json.Unmarshal(rb, &e)
		msg := fmt.Sprintf("server answered %d: %s", resp.StatusCode, truncate(e.Error, 300))
		switch {
		case resp.StatusCode == http.StatusForbidden:
			return rpcReply{}, &exitErr{exitReject, "rejected", msg}, false, true
		case resp.StatusCode >= 400 && resp.StatusCode < 500:
			return rpcReply{}, &exitErr{exitUsage, "usage", msg}, false, true
		}
		return unreachable(msg, false, true)
	}
	var rep rpcReply
	if err := json.Unmarshal(rb, &rep); err != nil {
		return unreachable("reply is not JSON", false, true)
	}
	return rep, nil, false, true
}

func (cl *rpcClient) callStored(cx context.Context, req rpcRequest) (rpcReply, *exitErr, bool) {
	rep, err, noReply, _ := cl.callOnceRequest(cx, req, true)
	return rep, err, noReply
}

func rpcClientRequest(argv []string, cwd, key string, env map[string]string, document *rpcDocPayload) rpcRequest {
	req := rpcRequest{Argv: slices.Clone(argv), Cwd: cwd, Env: env, RequestKey: key, Document: document}
	if rpcCarriesDocument(argv) {
		req.Capabilities = []string{docUploadCapability}
	}
	return req
}

func rpcCarriesDocument(argv []string) bool {
	name, args := rpcCommand(argv)
	switch name {
	case "new", "prompt", "_prompt", "ready", "done", "fail", "close", "_doc":
		return true
	case "doc":
		return len(args) > 0 && (args[0] == "set" || args[0] == "backfill")
	default:
		return false
	}
}

func spoolRecordCommand(argv []string) bool {
	name, args := rpcCommand(argv)
	switch name {
	case "got", "ready", "done", "fail", "decide", "next", "note", "_hook":
		return true
	case "close":
		_, _, _, outcome := flagValue(args, "outcome")
		return !outcome
	default:
		return false
	}
}

func clientReadySpoolDocument(args []string, env map[string]string) *rpcDocPayload {
	path, _, _, ok := flagValue(args, "report")
	if !ok || path == "" {
		return nil
	}
	task, err := strconv.ParseInt(env["TASKR_TASK"], 10, 64)
	if err != nil || task <= 0 {
		return nil
	}
	want := rpcDocWant{Task: task, Kind: "report", Path: path}
	return clientDocPayload(want, fileDocument(path, ""), false, false)
}

func clientQueuedResult(c *ctx, args []string, key string, waiting int, stderr io.Writer) int {
	c.json = c.json || flagTrue(args, "json")
	c.emit(map[string]any{"queued": true, "request_key": key})
	fmt.Fprintf(stderr, "taskr: server unreachable; queued (%d waiting)\n", waiting)
	return exitOK
}

// clientPaths makes the path flags absolute here, before they reach another
// host: --cwd against the current directory, new's --brief and --report
// against its --cwd (as the local CLI does), and handover's --out is taken
// out of argv and returned, since this host writes that file.
func clientPaths(name string, args []string, cwd string) ([]string, string, error) {
	base := cwd
	pathFlags := rpcPathFlags[name]
	if name == "doc" && len(args) > 0 && args[0] == "set" {
		pathFlags = []string{"file"}
	}
	for _, f := range pathFlags {
		v, at, inline, ok := flagValue(args, f)
		if !ok || at < 0 || v == "" {
			continue
		}
		b := cwd
		if name == "new" && f != "cwd" {
			b = base
		}
		abs, err := absolutePath(b, v, "--"+f)
		if err != nil {
			return nil, "", err
		}
		if inline {
			args[at] = args[at][:strings.Index(args[at], "=")+1] + abs
		} else {
			args[at] = abs
		}
		if f == "cwd" {
			base = abs
		}
	}
	if name != "handover" {
		return args, "", nil
	}
	v, at, inline, ok := flagValue(args, "out")
	if !ok {
		return args, "", nil
	}
	if at < 0 {
		return nil, "", usageErr("--out needs a value")
	}
	if strings.HasPrefix(v, "-") {
		return nil, "", usageErr("--out must be a path that does not start with `-`, got %q", v)
	}
	if inline {
		args = slices.Delete(args, at, at+1)
	} else {
		args = slices.Delete(args, at-1, at+1)
	}
	if v == "" {
		return args, "", nil
	}
	out, err := absolutePath(cwd, v, "--out")
	if err != nil {
		return nil, "", err
	}
	if err := requireDirectory(filepath.Dir(out)); err != nil {
		return nil, "", usageErr("--out: directory %s does not exist", filepath.Dir(out))
	}
	return args, out, nil
}

func clientDocSetPayload(c *ctx, args []string) (*rpcDocPayload, error) {
	if len(args) < 3 || args[0] != "set" {
		return nil, nil
	}
	fs := flag.NewFlagSet("doc set", flag.ContinueOnError)
	file := fs.String("file", "", "")
	name := fs.String("name", "", "")
	pos, err := parseArgs(c, fs, args[1:], 2, 2)
	if err != nil {
		return nil, nil // let the server retain the command's normal validation and output
	}
	id, err := parseID(pos[0], "task id")
	if err != nil {
		return nil, nil
	}
	kind := pos[1]
	if kind != "goal" && kind != "plan" || kind == "goal" && flagWasSet(fs, "name") ||
		flagWasSet(fs, "name") && !documentNameRe.MatchString(*name) || *file == "" {
		return nil, nil
	}
	in := fileDocument(*file, "")
	if in.Reason == "missing" {
		return nil, usageErr("document file is missing or unreadable: %s", *file)
	}
	if in.Reason != "" {
		return nil, usageErr("document file: %s (%s)", in.Reason, *file)
	}
	body := base64.StdEncoding.EncodeToString([]byte(in.Body))
	return &rpcDocPayload{Task: id, Kind: kind, Name: *name, Path: in.Path, Body: &body,
		SHA256: in.Hash, Bytes: in.Bytes}, nil
}

func clientDocPayload(want rpcDocWant, in documentInput, backfill, dryRun bool) *rpcDocPayload {
	p := &rpcDocPayload{Task: want.Task, Kind: want.Kind, Name: want.Name, Path: want.Path,
		EventID: want.EventID, Bytes: in.Bytes, Backfill: backfill, DryRun: dryRun}
	if in.Reason != "" {
		p.Reason = in.Reason
		if in.Reason == "binary" {
			p.SHA256 = in.Hash
		}
		return p
	}
	body := base64.StdEncoding.EncodeToString([]byte(in.Body))
	p.Body, p.SHA256 = &body, in.Hash
	return p
}

func clientUploadDocs(cl *rpcClient, wants []rpcDocWant, cwd string, env map[string]string) {
	clientUploadSpoolDocs(cl, wants, cwd, env, nil)
}

func clientUploadSpoolDocs(cl *rpcClient, wants []rpcDocWant, cwd string, env map[string]string, saved *rpcDocPayload) {
	for _, want := range wants {
		var payload *rpcDocPayload
		if saved != nil && saved.Task == want.Task && saved.Kind == want.Kind && saved.Name == want.Name && saved.Path == want.Path {
			copy := *saved
			copy.EventID = want.EventID
			payload = &copy
		} else {
			in := fileDocument(want.Path, "")
			payload = clientDocPayload(want, in, false, false)
		}
		cx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		_, _, _, _ = cl.callOnceNoFallback(cx, []string{"--json", "_doc", "put"}, cwd, newRequestKey(), env, payload)
		cancel()
	}
}

func clientDocBackfill(c *ctx, cl *rpcClient, args []string, cwd string, env map[string]string) int {
	fs := flag.NewFlagSet("doc backfill", flag.ContinueOnError)
	tree := fs.Int64("tree", 0, "only this task and descendants")
	dry := fs.Bool("dry-run", false, "count without writing")
	if _, err := parseArgs(c, fs, args, 0, 0); err != nil {
		var e *exitErr
		if errors.As(err, &e) {
			return clientFail(c, e.code, e.kind, e.msg)
		}
		return clientFail(c, exitUsage, "usage", err.Error())
	}
	if *tree < 0 {
		return clientFail(c, exitUsage, "usage", "--tree must be a positive task id")
	}
	counts := map[string]int{"captured": 0, "too_large": 0, "binary": 0, "missing": 0, "client": 0, "unchanged": 0}
	offset := 0
	for {
		argv := []string{"--json", "_doc", "wanted", "--offset", strconv.Itoa(offset)}
		if *tree != 0 {
			argv = append(argv, "--tree", strconv.FormatInt(*tree, 10))
		}
		rep, err := cl.callRetry(c, argv, cwd, newRequestKey(), "taskr doc backfill", env)
		if err != nil {
			return clientFail(c, err.code, err.kind, err.msg)
		}
		if rep.Exit != exitOK {
			return clientFail(c, rep.Exit, "rejected", lastLine(rep.Stdout))
		}
		var page struct {
			Documents []rpcDocWant `json:"documents"`
			Offset    int          `json:"offset"`
			More      bool         `json:"more"`
			Unchanged int          `json:"unchanged"`
		}
		if err := json.Unmarshal([]byte(lastLine(rep.Stdout)), &page); err != nil {
			return clientFail(c, exitDB, "database", "invalid _doc wanted reply")
		}
		counts["unchanged"] += page.Unchanged
		for _, want := range page.Documents {
			in := fileDocument(want.Path, "")
			payload := clientDocPayload(want, in, true, *dry)
			cx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			put, putErr, _, _ := cl.callOnceNoFallback(cx, []string{"--json", "_doc", "put"}, cwd, newRequestKey(), env, payload)
			cancel()
			if putErr != nil {
				return clientFail(c, putErr.code, putErr.kind, putErr.msg)
			}
			if put.Exit != exitOK {
				var failure struct {
					Error string `json:"error"`
					Kind  string `json:"kind"`
				}
				_ = json.Unmarshal([]byte(lastLine(put.Stdout)), &failure)
				return clientFail(c, put.Exit, failure.Kind, failure.Error)
			}
			var result struct {
				Count string `json:"count"`
			}
			if err := json.Unmarshal([]byte(lastLine(put.Stdout)), &result); err != nil {
				return clientFail(c, exitDB, "database", "invalid _doc put reply")
			}
			if _, ok := counts[result.Count]; !ok {
				return clientFail(c, exitDB, "database", "invalid _doc put count")
			}
			counts[result.Count]++
		}
		if !page.More {
			break
		}
		offset = page.Offset
	}
	c.emit(counts)
	return exitOK
}

// clientNewChecks checks --brief on the caller's host. --cwd belongs to the
// task's host, so the client checks it only when the task will run here.
func clientNewChecks(args []string, self string) error {
	if role, _, _, _ := flagValue(args, "role"); role == "gate" || flagTrue(args, "planned") {
		return nil
	}
	localTask := true
	if m, _, _, given := flagValue(args, "machine"); given && m != machineLabel(self) {
		localTask = false
	}
	if localTask {
		dir, _, _, ok := flagValue(args, "cwd")
		if !ok || dir == "" {
			dir, _ = os.Getwd()
		}
		if err := requireDirectory(dir); err != nil {
			return err
		}
	}
	if brief, _, _, ok := flagValue(args, "brief"); ok && brief != "" {
		return requireFile(brief)
	}
	return nil
}

// shellJoin quotes args for a copy-paste retry line.
func shellJoin(args []string) string {
	q := make([]string, len(args))
	for i, a := range args {
		if a != "" && strings.Trim(a, "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_=.,/:@%+") == "" {
			q[i] = a
		} else {
			q[i] = "'" + strings.ReplaceAll(a, "'", `'\''`) + "'"
		}
	}
	return strings.Join(q, " ")
}
