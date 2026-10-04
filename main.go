package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

const (
	exitOK      = 0
	exitUsage   = 2
	exitTimeout = 3
	exitDB      = 4
	exitHerdr   = 5
	exitReject  = 6
)

var version = "dev"

// exitErr carries the exit code and the error kind printed on stdout.
type exitErr struct {
	code int
	kind string
	msg  string
}

func (e *exitErr) Error() string { return e.msg }

func usageErr(format string, a ...any) error {
	return &exitErr{exitUsage, "usage", fmt.Sprintf(format, a...)}
}

func rejectErr(format string, a ...any) error {
	return &exitErr{exitReject, "rejected", fmt.Sprintf(format, a...)}
}

func herdrErr(format string, a ...any) error {
	return &exitErr{exitHerdr, "herdr", fmt.Sprintf(format, a...)}
}

func dbErr(err error) error {
	if err == nil {
		return nil
	}
	var e *exitErr
	if errors.As(err, &e) {
		return err
	}
	return &exitErr{exitDB, "database", err.Error()}
}

// allowedEnv is the complete list of variables taskr reads.
var allowedEnv = map[string]bool{
	"TASKR_DB": true, "TASKR_TASK": true, "TASKR_LAUNCH": true, "HERDR_PANE_ID": true,
	"HERDR_WORKSPACE_ID": true, "HERDR_TAB_ID": true, // adopt's defaults
	"HERDR_ENV":  true,
	"CODEX_HOME": true, "CODEX_THREAD_ID": true, "CLAUDE_CONFIG_DIR": true, "HOME": true,
	"HERDR_SOCKET_PATH": true,
	"TASKR_FORMAT":      true,
	"PATH":              true, // daemon --restart hands it to the new daemon
	"INVOCATION_ID":     true, // with the service's own pid, marks supervision; value is never recorded
	"SYSTEMD_EXEC_PID":  true,
}

// ctx is one invocation: arguments, a restricted environment, and output streams.
type ctx struct {
	getenv func(string) string
	out    io.Writer
	errw   io.Writer
	lines  bool // the command already streamed JSON lines
	json   bool // legacy CLI contract, explicitly selected
	cmd    string
	code   int

	// Set by the RPC handler for a remote caller; zero for the local CLI.
	db         *sql.DB         // the daemon's resident ledger
	log        *daemonLog      // the server daemon's log for remote calls
	cx         context.Context // ends a wait; nil means the CLI's signal context
	rpc        bool            // a remote caller: paths come absolute, no file effects here
	machine    string          // the caller's host label; "" is the server host
	cwd        string          // the caller's working directory
	client     bool            // client mode: never open a local ledger
	server     string          // client mode: the server URL, for version
	docUpload  bool
	docUploads []rpcDocWant
	remoteDoc  *rpcDocPayload
}

func (c *ctx) env(k string) string {
	if !allowedEnv[k] {
		panic("taskr: reading disallowed environment variable " + k)
	}
	return c.getenv(k)
}

func (c *ctx) emit(v any) {
	if !c.json {
		c.emitCompact(v)
		return
	}
	b, _ := json.Marshal(v)
	fmt.Fprintf(c.out, "%s\n", b)
}

type command func(c *ctx, args []string) (any, int, error)

// commands is filled in init: the daemon's RPC route runs commands, so a
// static initializer would refer to itself through cmdDaemon.
var commands map[string]command

func init() { commands = commandTable() }

func commandTable() map[string]command {
	return map[string]command{
		"version": cmdVersion,
		"new":     cmdNew, "launch": cmdLaunch, "close": cmdClose,
		"start": cmdStart, "got": cmdGot, "note": cmdNote, "ready": cmdReady, "ask": cmdAsk, "done": cmdDone, "fail": cmdFail,
		"prompt": cmdPrompt, "answer": cmdAnswer, "hook": cmdHook,
		"wait": cmdWait, "ack": cmdAck,
		"next": cmdNext, "decide": cmdDecide, "set": cmdSet, "handover": cmdHandover, "adopt": cmdAdopt,
		"status": cmdStatus, "asks": cmdAsks, "log": cmdLog,
		"daemon": cmdDaemon, "doc": cmdDoc,
	}
}

const usageText = `usage: taskr <command> [args]
worker:       start | got ATTEMPT_ID | note TEXT | ready TEXT --report PATH [--kv K=V]... | ask TEXT [--blocking] [--owner] | done [TEXT] | fail TEXT
              note TEXT --as ID | ask TEXT --owner --as ID   (a root orchestrator: no parent, no launch)
orchestrator: new NAME --role ROLE [--parent ID] [--planned] ... | launch ID --provider P --model M --effort E | prompt ID (--file PATH | --text TEXT) [--receipt-timeout MS] [--confirm [--confirm-timeout MS]]
              answer ASK_ID TEXT [--prompt [--confirm [--confirm-timeout MS]]] | close ID
plan:         next ID TEXT | next ID --clear | set ID KEY=VALUE... (KEY= deletes) | decide --as ID TEXT | decide --as ID --revoke EVENT_ID
documents:    doc set ID goal|plan [--name NAME] --file PATH (any host) | doc ls ID [--tree] [--kind K] [--versions] [--limit N] | doc get DOC_ID | doc rm DOC_ID --purge | doc backfill [--tree ID] [--dry-run] (any host)
handover:     handover --as ID [--note TEXT] [--out PATH] | adopt ID [--workspace W --tab T --pane P]   (Markdown on stdout)
inbox:        wait [--as ID] [--for KIND[,KIND...]] [--from NAME|ID]... [--ack EVENT_ID] [--timeout MS] [--scan-quota] | ack EVENT_ID --as ID
hooks:        hook <harness> <event> (JSON on stdin)
read:         status [--tree ID] [--all] | asks [--open] [--tree ID] [--owner] [--limit N] | log ID [--tree] [--since EVENT_ID] [--before EVENT_ID] [--limit N]
daemon:       daemon [--stay] [--once] [--status] [--restart]   (the Herdr plugin's event bridge and owner dashboard; one per HOME)
info:         version | help [CMD]
client:       --request-key KEY <command> [args]   (with a server.url: retry a command whose answer was lost)
format:       compact by default; --json or TASKR_FORMAT=json selects legacy JSON (handover/adopt stay Markdown)`

func main() {
	os.Exit(cliMain(os.Args[1:], os.Getenv, os.Stdout, os.Stderr))
}

// cliMain picks the mode: TASKR_DB set is local; otherwise a server.url in
// the state directory makes this CLI an RPC client that never opens a
// local ledger; otherwise local. hub.url plays no part.
func cliMain(args []string, getenv func(string) string, stdout, stderr io.Writer) int {
	if getenv("TASKR_DB") == "" && getenv("HOME") != "" {
		raw, found, err := readServerURL(filepath.Join(getenv("HOME"), ".local", "state", "taskr"))
		if found || err != nil {
			return clientMain(raw, err, args, getenv, stdout, stderr)
		}
	}
	return run(args, getenv, stdout, stderr)
}

func cmdVersion(c *ctx, args []string) (any, int, error) {
	if _, err := parseArgs(c, flag.NewFlagSet("version", flag.ContinueOnError), args, 0, 0); err != nil {
		if errors.Is(err, errHelp) {
			return nil, 0, err
		}
		return nil, exitUsage, usageErr("version: takes no arguments")
	}
	return struct {
		Version string `json:"version"`
		OK      bool   `json:"ok"`
		Server  string `json:"server,omitempty"`
	}{version, true, c.server}, exitOK, nil
}

func run(args []string, getenv func(string) string, stdout, stderr io.Writer) int {
	return runCtx(&ctx{getenv: getenv, out: stdout, errw: stderr}, args)
}

// runCtx runs one command line in c: the local CLI, the client's offline
// commands, and the RPC handler for a remote caller all come through here.
func runCtx(c *ctx, args []string) int {
	stderr := c.errw
	c.json = c.env("TASKR_FORMAT") == "json"
	if len(args) > 0 && args[0] == "--json" {
		c.json = true
		args = args[1:]
	}
	if len(args) > 0 {
		c.cmd = args[0]
	}
	c.code = exitUsage
	if len(args) == 0 || args[0] == "-h" || args[0] == "--help" {
		fmt.Fprintln(stderr, usageText)
		c.emit(map[string]any{"error": "no command", "kind": "usage"})
		return exitUsage
	}
	if args[0] == requestKeyFlag || strings.HasPrefix(args[0], requestKeyFlag+"=") {
		c.emit(map[string]any{"error": requestKeyFlag + " is only for client mode (a server.url without TASKR_DB)", "kind": "usage"})
		return exitUsage
	}
	cmd, ok := commands[args[0]]
	if args[0] == "help" {
		cmd, ok = cmdHelp, true
	}
	if hc := hiddenCommands[args[0]]; hc != nil && c.rpc {
		cmd, ok = hc, true
	}
	if !ok {
		c.emit(map[string]any{"error": "unknown command " + args[0], "kind": "usage", "try": suggestCommand(args[0])})
		return exitUsage
	}
	var res any
	var code int
	var err error
	if c.rpc {
		err = rpcCheckArgs(args)
	}
	if err == nil {
		res, code, err = cmd(c, args[1:])
	}
	c.code = code
	if err != nil {
		if errors.Is(err, errHelp) {
			return exitOK
		}
		var e *exitErr
		if !errors.As(err, &e) {
			e = &exitErr{exitDB, "database", err.Error()}
		}
		if c.json {
			fmt.Fprintf(stderr, "taskr %s: %s\n", args[0], e.msg)
		}
		c.code = e.code
		obj := map[string]any{"error": e.msg, "kind": e.kind}
		if m, ok := res.(map[string]any); ok {
			for k, v := range m {
				obj[k] = v
			}
		}
		c.emit(obj)
		return e.code
	}
	if !c.lines {
		c.emit(res)
	}
	if m, ok := res.(map[string]any); ok && c.cmd == "wait" && !c.json && code == exitTimeout && m["timeout"] == true && m["interrupted"] != true {
		return exitOK
	}
	return code
}

var errHelp = errors.New("help requested")

func cmdHelp(c *ctx, args []string) (any, int, error) {
	fs := flag.NewFlagSet("help", flag.ContinueOnError)
	pos, err := parseArgs(c, fs, args, 0, 1)
	if err != nil {
		return nil, 0, err
	}
	if len(pos) == 0 {
		fmt.Fprintln(c.out, usageText)
		c.lines = true
		return nil, exitOK, nil
	}
	cmd, ok := commands[pos[0]]
	if pos[0] == "help" {
		cmd, ok = cmdHelp, true
	}
	if !ok {
		return map[string]any{"error": "unknown command " + pos[0], "kind": "usage", "try": suggestCommand(pos[0])}, exitUsage, nil
	}
	_, _, err = cmd(c, []string{"--help"})
	return nil, 0, err
}

// parseArgs peels leading positionals, then parses flags, so both
// `ready "text" --report p` and `ack 5 --as 3` work with the standard flag package.
func parseArgs(c *ctx, fs *flag.FlagSet, args []string, min, max int) ([]string, error) {
	// flag's diagnostics duplicate the structured error printed by run.
	fs.SetOutput(io.Discard)
	fs.BoolVar(&c.json, "json", c.json, "legacy JSON output")
	var help, shortHelp bool
	fs.BoolVar(&help, "help", false, "print command help")
	fs.BoolVar(&shortHelp, "h", false, "print command help")
	var pos []string
	for len(args) > 0 {
		if args[0] == "--" {
			pos = append(pos, args[1:]...)
			break
		}
		if !strings.HasPrefix(args[0], "-") || args[0] == "-" {
			pos = append(pos, args[0])
			args = args[1:]
			continue
		}
		end := flagChunkEnd(args, fs)
		if err := flagValueError(fs, args[:end]); err != nil {
			return nil, err
		}
		if err := fs.Parse(args[:end]); err != nil {
			return nil, usageErr("%s", err.Error())
		}
		if err := validateFlagValues(fs); err != nil {
			return nil, err
		}
		args = args[end:]
	}
	if help || shortHelp {
		fmt.Fprintln(c.out, commandUsageLine(fs.Name()))
		fs.SetOutput(c.out)
		fs.PrintDefaults()
		return nil, errHelp
	}
	if len(pos) < min || len(pos) > max {
		return nil, usageErr("%s: expected %d to %d positional arguments, got %d", fs.Name(), min, max, len(pos))
	}
	return pos, nil
}

func flagChunkEnd(args []string, fs *flag.FlagSet) int {
	for i, arg := 0, ""; i < len(args); i++ {
		arg = args[i]
		if arg == "--" || !strings.HasPrefix(arg, "-") || arg == "-" {
			return i
		}
		name, _, hasValue := strings.Cut(strings.TrimLeft(arg, "-"), "=")
		f := fs.Lookup(name)
		if f != nil && !isBoolFlag(f) && !hasValue && i+1 < len(args) {
			i++
		}
	}
	return len(args)
}

func isBoolFlag(f *flag.Flag) bool {
	b, ok := f.Value.(interface{ IsBoolFlag() bool })
	return ok && b.IsBoolFlag()
}

func flagValueError(fs *flag.FlagSet, args []string) error {
	for i := 0; i < len(args); i++ {
		arg := args[i]
		name, value, hasValue := strings.Cut(strings.TrimLeft(arg, "-"), "=")
		f := fs.Lookup(name)
		if f == nil || isBoolFlag(f) {
			continue
		}
		if !hasValue && i+1 < len(args) {
			i++
			value = args[i]
		}
		if strings.HasPrefix(value, "-") {
			name, _, _ := strings.Cut(strings.TrimLeft(value, "-"), "=")
			if fs.Lookup(name) != nil {
				return usageErr("--%s needs a value, got flag --%s", f.Name, name)
			}
		}
	}
	return nil
}

func validateFlagValues(fs *flag.FlagSet) error {
	var err error
	fs.Visit(func(f *flag.Flag) {
		if err != nil || isBoolFlag(f) {
			return
		}
		value := f.Value.String()
		if !strings.HasPrefix(value, "-") {
			return
		}
		name, _, _ := strings.Cut(strings.TrimLeft(value, "-"), "=")
		if fs.Lookup(name) != nil {
			err = usageErr("--%s needs a value, got flag --%s", f.Name, name)
		}
	})
	return err
}

func commandUsageLine(name string) string {
	for _, line := range strings.Split(usageText, "\n") {
		usage := line
		if !strings.HasPrefix(line, " ") && !strings.HasPrefix(line, "\t") {
			if _, rest, ok := strings.Cut(line, ":"); ok {
				usage = rest
			}
		}
		for _, segment := range strings.Split(usage, "|") {
			fields := strings.Fields(segment)
			if len(fields) > 0 && fields[0] == name {
				return line
			}
		}
	}
	return "usage: taskr " + name + " [args]"
}

func suggestCommand(name string) string {
	switch name {
	case "show":
		return "status --tree ID / log ID"
	case "inbox":
		return "asks --open or wait --as ID --timeout 0 (consuming)"
	}
	best, distance := "", 3
	for candidate := range commands {
		if d := editDistance(name, candidate); d < distance || (d == distance && (best == "" || candidate < best)) {
			best, distance = candidate, d
		}
	}
	if d := editDistance(name, "help"); d < distance || (d == distance && (best == "" || "help" < best)) {
		best, distance = "help", d
	}
	if distance <= 2 {
		return best
	}
	return "taskr help"
}

func editDistance(a, b string) int {
	ar, br := []rune(a), []rune(b)
	// ponytail: suggestions stop at distance 2; raise this cutoff if that policy changes.
	if len(ar)-len(br) > 2 || len(br)-len(ar) > 2 {
		return 3
	}
	distance := make([][]int, len(ar)+1)
	for i := range distance {
		distance[i] = make([]int, len(br)+1)
		distance[i][0] = i
	}
	for j := range distance[0] {
		distance[0][j] = j
	}
	for i, r := range ar {
		for j, s := range br {
			cost := 0
			if r != s {
				cost = 1
			}
			distance[i+1][j+1] = min(distance[i][j+1]+1, distance[i+1][j]+1, distance[i][j]+cost)
			if i > 0 && j > 0 && r == br[j-1] && ar[i-1] == s {
				distance[i+1][j+1] = min(distance[i+1][j+1], distance[i-1][j-1]+1)
			}
		}
	}
	return distance[len(ar)][len(br)]
}

func parseID(s, what string) (int64, error) {
	n, err := strconv.ParseInt(s, 10, 64)
	if err != nil || n <= 0 {
		return 0, usageErr("%s must be a positive integer, got %q", what, s)
	}
	return n, nil
}

type kvFlag map[string]string

func (k kvFlag) String() string { return "" }
func (k kvFlag) Set(s string) error {
	key, val, ok := strings.Cut(s, "=")
	if !ok || key == "" {
		return fmt.Errorf("--kv wants KEY=VALUE, got %q", s)
	}
	k[key] = val
	return nil
}
