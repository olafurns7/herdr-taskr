package main

import (
	"context"
	"database/sql"
	"embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log"
	"mime"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unicode/utf8"
)

// The owner dashboard: a localhost page served by `taskr daemon` that shows
// every open orchestrator tree and what needs the owner. It is read-only:
// owner asks are answered in the orchestrator's pane, never here. It is a
// passenger: when it cannot bind, the daemon logs one line and carries on.

// The page is the Preact app in web/, built to web/dist and committed so
// go build needs no Node. index.html is served at / as built; the hashed
// files under /assets/ are long-cached.
//
//go:embed web/dist
var webFS embed.FS

var dashboardPage = mustRead(webFS, "web/dist/index.html")

const (
	webDist      = "web/dist/"
	assetsCache  = "public, max-age=31536000, immutable" // hashed names: a change is a new name
	licenceCache = "public, max-age=86400"
	dashboardCSP = "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; img-src 'self' data:; " +
		"connect-src 'self'; base-uri 'none'; frame-ancestors 'none'"
)

var assetTypes = map[string]string{
	".js":    "text/javascript; charset=utf-8",
	".css":   "text/css; charset=utf-8",
	".woff2": "font/woff2",
	".txt":   "text/plain; charset=utf-8",
	".svg":   "image/svg+xml",
}

func mustRead(f embed.FS, name string) string {
	b, err := f.ReadFile(name)
	if err != nil {
		panic(err)
	}
	return string(b)
}

// dashboardDefaultAddr is a variable so tests can bind port 0.
var dashboardDefaultAddr = "127.0.0.1:7788"

const (
	dashboardAddrFile = "dashboard.addr"
	dashboardURLKey   = "dashboard_url"
)

// Caps on /api/state.
const (
	stateAsksMax     = 50
	stateRootsMax    = 50
	stateTasksMax    = 200 // descendants per root
	stateActivityMax = 40
	stateClosedMax   = 20
	stateAskTextMax  = 4000 // runes
	stateNoteMax     = 1000
	stateSummaryMax  = 300
	stateDecisionMax = 50 // decisions in force per root, the latest
)

// The owner's attention model (v0.7), named in one place: what needs the
// owner (PROGRAM), what counts as a milestone (CUE LOG), and the windows
// that light a campaign's tally.
const (
	workWaitingAfter     = 10 * time.Minute // an unacked ready/done/fail older than this is work waiting
	milestoneLitFor      = 10 * time.Minute // a tally is green this long after the campaign's latest milestone
	stateMilestonesMax   = 30               // per machine, and merged on the hub
	stateAttentionMax    = 100              // per machine
	stateUnreadMax       = stateTasksMax + 1
	attentionTextMax     = 200 // runes; one line
	attentionOwnerAsk    = "owner_ask"
	attentionBlocked     = "lane_blocked"
	attentionFailed      = "lane_failed"
	attentionMissing     = "lane_missing"
	attentionStale       = "machine_stale"
	attentionDaemon      = "daemon_unhealthy"
	attentionWorkWaits   = "work_waiting"
	tallyRed, tallyAmber = "red", "amber"
	tallyGreen, tallyOff = "green", "off"
)

// attentionRank orders PROGRAM: most severe first, then oldest first.
var attentionRank = map[string]int{attentionOwnerAsk: 0, attentionBlocked: 1, attentionFailed: 2, attentionMissing: 3,
	attentionStale: 4, attentionDaemon: 5, attentionWorkWaits: 6}

// Milestones: these event kinds, an orchestrator's own notes, and ref sets
// with one of milestoneRefKeys.
var (
	milestoneKinds   = []string{"ready", "done", "fail", "handover", "adopt", "decision"}
	milestoneRefKeys = []string{"pr", "release", "tag", "merged"}
)

// laneReportSkip: event kinds that are not what a task said: plan edits,
// prompts, their outcomes and receipts, and Herdr's own observations (v0.7;
// a lane's last event is its latest report, so a failed lane shows its
// reason, not "agent_status done" or a receipt alarm).
const laneReportSkip = `('next', 'ref', 'herdr', 'got', 'prompt', 'prompt_outcome', 'doc')`

// Lane marks, the state a channel strip shows before any colour.
const (
	markDone, markFailed, markWorking = "done", "failed", "working"
	markPlanned, markReady            = "planned", "ready"
	markBlocked, markMissing          = "blocked", "missing"
)

// dashConfig is <stateDir>/dashboard.addr: a loopback address to bind, and
// whether this machine is the hub that also binds its tailnet address.
type dashConfig struct {
	Addr    string // loopback host:port; "" when off
	Tailnet bool
}

// dashboardConfig reads <stateDir>/dashboard.addr: absent or empty is the
// default, "off" disables the page, "tailnet" or "tailnet:PORT" makes this
// machine the hub (127.0.0.1 plus the Tailscale IPv4, which comes only from
// `tailscale ip -4`), anything else must be a loopback host:port.
func dashboardConfig(dir string) (dashConfig, error) {
	path := filepath.Join(dir, dashboardAddrFile)
	b, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return dashConfig{Addr: dashboardDefaultAddr}, nil
	} else if err != nil {
		return dashConfig{}, err
	}
	v, _, _ := strings.Cut(string(b), "\n")
	switch v = strings.TrimSpace(v); v {
	case "":
		return dashConfig{Addr: dashboardDefaultAddr}, nil
	case "off":
		return dashConfig{}, nil
	}
	if v == "tailnet" || strings.HasPrefix(v, "tailnet:") {
		_, port, _ := net.SplitHostPort(dashboardDefaultAddr)
		if p, ok := strings.CutPrefix(v, "tailnet:"); ok {
			port = p
		}
		if n, err := strconv.Atoi(port); err != nil || n < 0 || n > 65535 {
			return dashConfig{}, fmt.Errorf("%s: bad port %q", path, port)
		}
		return dashConfig{Addr: net.JoinHostPort("127.0.0.1", port), Tailnet: true}, nil
	}
	host, port, err := net.SplitHostPort(v)
	if err != nil {
		return dashConfig{}, fmt.Errorf("%s: %q is not host:port or tailnet", path, v)
	}
	ip := net.ParseIP(host)
	if ip == nil || !(ip.Equal(net.IPv4(127, 0, 0, 1)) || ip.Equal(net.IPv6loopback)) {
		return dashConfig{}, fmt.Errorf("%s: host %q refused: the dashboard binds only 127.0.0.1 or ::1", path, host)
	}
	if n, err := strconv.Atoi(port); err != nil || n < 0 || n > 65535 {
		return dashConfig{}, fmt.Errorf("%s: bad port %q", path, port)
	}
	return dashConfig{Addr: net.JoinHostPort(ip.String(), port)}, nil
}

// dashboardAddr is the loopback address dashboardConfig binds.
func dashboardAddr(dir string) (string, error) {
	c, err := dashboardConfig(dir)
	return c.Addr, err
}

type dashboard struct {
	db      *sql.DB
	log     *daemonLog
	hosts   map[string]bool // accepted Host headers of the loopback listener: its host:port and localhost:port
	url     string          // the loopback URL; "" when loopback is not bound
	machine string          // this machine's label on the page, unless it is the hub
	hub     atomic.Pointer[hubInfo]
	srv     *http.Server
	mux     *http.ServeMux
	usage   *dashboardUsage

	serverEnv func(string) string // TASKR_DB, HOME and HERDR_SOCKET_PATH for RPC runs; nil is os.Getenv

	stopRetry         context.CancelFunc // ends the tailnet retry loop
	retryDone         chan struct{}
	stopLoopbackRetry context.CancelFunc
	loopbackRetryDone chan struct{}

	mu        sync.Mutex
	listeners []net.Listener // every bound listener, for tests
	tailnetUp int            // tailnet listeners serving; hub_tailnet_url is set while > 0
}

// newDashboard builds the handler for a loopback listener bound at addr
// (host:port). Every route exists from the start; the hub's routes answer
// 404 until a hub is published, so nothing is registered while serving.
func newDashboard(db *sql.DB, lg *daemonLog, addr string) *dashboard {
	_, port, _ := net.SplitHostPort(addr)
	d := &dashboard{db: db, log: lg, url: "http://" + addr + "/", machine: localMachine(), usage: &dashboardUsage{},
		hosts: map[string]bool{strings.ToLower(addr): true, "localhost:" + port: true}}
	d.mux = http.NewServeMux()
	d.mux.HandleFunc("GET /{$}", d.page)
	d.mux.HandleFunc("GET /assets/{name}", d.asset)
	d.mux.HandleFunc("GET /fonts/{name}", d.asset)
	d.mux.HandleFunc("GET /api/state", d.state)
	d.mux.HandleFunc("GET /api/campaigns", d.campaigns)
	d.mux.HandleFunc("GET /api/campaign/{id}", d.campaign)
	d.mux.HandleFunc("GET /api/doc/{id}", d.doc)
	d.mux.HandleFunc("POST "+peerPushPath, d.peerPush) // machine to machine; the page writes nothing
	d.mux.HandleFunc("POST "+rpcPath, d.rpc)           // a remote host's CLI; sets its own write deadline
	d.srv = &http.Server{Handler: dashboardUsageHandler(d, d.usage), ReadHeaderTimeout: 5 * time.Second, ReadTimeout: 10 * time.Second,
		WriteTimeout: 15 * time.Second, IdleTimeout: 60 * time.Second, MaxHeaderBytes: 16 << 10,
		ErrorLog: log.New(io.Discard, "", 0)}
	return d
}

// startDashboard binds the configured address and serves in the background.
// It returns nil, after one log line, when the dashboard is off, refused, or
// (not a hub) cannot bind. A hub binds loopback and each tailnet address as
// separate attempts: any one may fail without the others, and tailnet
// addresses that are unavailable at start are retried in the background.
// Stay also retries an unavailable loopback listener until stop.
func startDashboard(db *sql.DB, lg *daemonLog, dir string, stay bool) *dashboard {
	cfg, err := dashboardConfig(dir)
	if err != nil {
		lg.logf("dashboard: %v; not serving", err)
		return nil
	}
	if cfg.Addr == "" {
		lg.logf("dashboard: off")
		return nil
	}
	_, port, _ := net.SplitHostPort(cfg.Addr)
	var d *dashboard
	ln, err := net.Listen("tcp", cfg.Addr)
	if err != nil {
		lg.logf("dashboard: listen %s failed: %v; the event bridge carries on without it", cfg.Addr, err)
		if !cfg.Tailnet && !stay {
			return nil
		}
		d = newDashboard(db, lg, cfg.Addr)
		d.url = ""
	} else {
		d = newDashboard(db, lg, ln.Addr().String())
		_, port, _ = net.SplitHostPort(ln.Addr().String()) // port 0 binds tailnet on loopback's port
		if err := setMeta(db, dashboardURLKey, d.url); err != nil {
			lg.logf("meta write failed: %v", err)
		}
		d.serve(ln, func() { db.Exec(`delete from meta where key = ?`, dashboardURLKey) })
	}
	if cfg.Tailnet {
		d.startTailnet(port)
	}
	if ln == nil && stay {
		cx, cancel := context.WithCancel(context.Background())
		d.stopLoopbackRetry, d.loopbackRetryDone = cancel, make(chan struct{})
		go func() {
			defer close(d.loopbackRetryDone)
			for sleepCx(cx, 2*time.Second) {
				ln, err := net.Listen("tcp", cfg.Addr)
				if err != nil {
					lg.limited("loopback-retry", time.Minute, "dashboard: listen %s failed: %v; retrying", cfg.Addr, err)
					continue
				}
				url := "http://" + ln.Addr().String() + "/"
				if err := setMeta(db, dashboardURLKey, url); err != nil {
					lg.logf("meta write failed: %v", err)
				}
				d.serve(ln, func() { db.Exec(`delete from meta where key = ?`, dashboardURLKey) })
				lg.logf("dashboard at %s", url)
				return
			}
		}()
	}
	if d.url != "" {
		lg.logf("dashboard at %s", d.url)
	}
	return d
}

// serve runs one listener; if it stops on its own, gone clears its URL.
func (d *dashboard) serve(ln net.Listener, gone func()) {
	d.mu.Lock()
	d.listeners = append(d.listeners, ln)
	d.mu.Unlock()
	go func() {
		if err := d.srv.Serve(ln); err != nil && !errors.Is(err, http.ErrServerClosed) {
			d.log.logf("dashboard listener %s stopped: %v", ln.Addr(), err)
			gone()
		}
	}()
}

func (d *dashboard) stop() {
	if d.stopLoopbackRetry != nil {
		d.stopLoopbackRetry()
		<-d.loopbackRetryDone
	}
	if d.stopRetry != nil {
		d.stopRetry()
		<-d.retryDone
	}
	cx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	if d.srv.Shutdown(cx) != nil {
		d.srv.Close()
	}
}

// ServeHTTP admits a non-loopback request by whois (only a published hub
// serves any), then rejects any Host but the bound ones (DNS rebinding)
// before routing.
func (d *dashboard) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	h := w.Header()
	h.Set("Cache-Control", "no-store")
	h.Set("X-Content-Type-Options", "nosniff")
	h.Set("Referrer-Policy", "no-referrer")
	h.Set("X-Frame-Options", "DENY")
	hub := d.hub.Load()
	var ok bool
	if r, ok = d.admit(w, r, hub); !ok {
		return
	}
	host := strings.ToLower(r.Host)
	if !d.hosts[host] && (hub == nil || !hub.hosts[host]) {
		httpError(w, http.StatusMisdirectedRequest, "unexpected Host header")
		return
	}
	d.mux.ServeHTTP(w, r)
}

func httpJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(code)
	json.NewEncoder(w).Encode(v)
}

func httpError(w http.ResponseWriter, code int, msg string) {
	httpJSON(w, code, map[string]any{"ok": false, "error": msg})
}

func (d *dashboard) page(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Security-Policy", dashboardCSP)
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	io.WriteString(w, dashboardPage)
}

// asset serves one built file from web/dist/assets (hashed, long-cached) or
// web/dist/fonts (the font licence); nothing else is reachable.
func (d *dashboard) asset(w http.ResponseWriter, r *http.Request) {
	dir, cache := "assets/", assetsCache
	if strings.HasPrefix(r.URL.Path, "/fonts/") {
		dir, cache = "fonts/", licenceCache
	}
	name := r.PathValue("name")
	ct, ok := assetTypes[filepath.Ext(name)]
	if !ok || name != filepath.Base(name) || strings.HasPrefix(name, ".") {
		http.NotFound(w, r)
		return
	}
	b, err := webFS.ReadFile(webDist + dir + name)
	if err != nil {
		http.NotFound(w, r)
		return
	}
	w.Header().Set("Cache-Control", cache)
	w.Header().Set("Content-Type", ct)
	w.Write(b)
}

func isJSON(r *http.Request) bool {
	mt, _, err := mime.ParseMediaType(r.Header.Get("Content-Type"))
	return err == nil && mt == "application/json"
}

// decodeStrict reads exactly one JSON object with only known fields, to EOF
// within max bytes, before any transaction.
func decodeStrict(w http.ResponseWriter, r *http.Request, max int64, v any) error {
	dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, max))
	dec.DisallowUnknownFields()
	err := dec.Decode(v)
	if err == nil {
		if err = dec.Decode(&struct{}{}); err == io.EOF {
			err = nil
		} else if err == nil {
			err = errors.New("trailing data after the JSON object")
		}
	}
	return err
}

func tooLarge(err error) bool {
	var e *http.MaxBytesError
	return errors.As(err, &e)
}

func (d *dashboard) state(w http.ResponseWriter, r *http.Request) {
	at := time.Now()
	s, err := readState(r.Context(), d.db, at)
	if err != nil {
		d.log.logf("dashboard state failed: %v", err)
		httpError(w, http.StatusInternalServerError, "database error")
		return
	}
	v, err := d.view(r.Context(), s, at)
	if err != nil {
		d.log.logf("dashboard state failed: %v", err)
		httpError(w, http.StatusInternalServerError, "database error")
		return
	}
	httpJSON(w, http.StatusOK, v)
}

// clip shortens s to at most n runes, keeping its line breaks.
func clip(s string, n int) string {
	if utf8.RuneCountInString(s) <= n {
		return s
	}
	return string([]rune(s)[:n-1]) + "…"
}

type stamped struct {
	Text  string `json:"text"`
	At    string `json:"at"`
	AgeMS int64  `json:"age_ms"`
}

type lastEvent struct {
	Kind    string `json:"kind"`
	Summary string `json:"summary,omitempty"`
	At      string `json:"at"`
	AgeMS   int64  `json:"age_ms"`
}

type taskRef struct {
	ID   int64  `json:"id"`
	Name string `json:"name"`
	Role string `json:"role,omitempty"`
}

type ownerAsk struct {
	ID           int64   `json:"id"`
	Text         string  `json:"text"`
	At           string  `json:"at"`
	AgeMS        int64   `json:"age_ms"`
	Blocking     bool    `json:"blocking"`
	Asker        taskRef `json:"asker"`
	Root         taskRef `json:"root"`
	AskerIsRoot  bool    `json:"asker_is_root"`
	AskerWaiting bool    `json:"asker_waiting"`
	WorkspaceID  string  `json:"workspace_id,omitempty"`
	TabID        string  `json:"tab_id,omitempty"`
	PaneID       string  `json:"pane_id,omitempty"`
}

type stateTask struct {
	ID          int64      `json:"id"`
	ParentID    int64      `json:"parent_id"`
	Depth       int        `json:"depth"`
	Name        string     `json:"name"`
	Role        string     `json:"role"`
	Status      string     `json:"status"`
	Waiting     bool       `json:"waiting"`
	AgentStatus string     `json:"agent_status,omitempty"`
	ObservedAt  string     `json:"observed_at,omitempty"`
	Present     *bool      `json:"present,omitempty"`
	LastEvent   *lastEvent `json:"last_event,omitempty"`
	OpenAsks    int64      `json:"open_asks"`
	ReportPath  string     `json:"report_path,omitempty"`
	PaneID      string     `json:"pane_id,omitempty"`
	Next        *stamped   `json:"next,omitempty"` // v0.6: the orchestrator's next step for this lane
	Refs        []planRef  `json:"refs,omitempty"`
	// v0.7: the Herdr agent name (for `herdr agent read`), and the lane's
	// mark, derived per view (see derive).
	AgentName string `json:"agent_name,omitempty"`
	Mark      string `json:"mark,omitempty"`
}

type orchestrator struct {
	ID             int64       `json:"id"`
	Name           string      `json:"name"`
	Role           string      `json:"role"`
	Status         string      `json:"status"`
	WorkspaceID    string      `json:"workspace_id,omitempty"`
	TabID          string      `json:"tab_id,omitempty"`
	PaneID         string      `json:"pane_id,omitempty"`
	CreatedAt      string      `json:"created_at"`
	Waiting        bool        `json:"waiting"`
	Unacked        int64       `json:"unacked"`
	OpenAsks       int64       `json:"open_asks"`
	Note           *stamped    `json:"note"`
	LastEvent      *lastEvent  `json:"last_event,omitempty"`
	Tasks          []stateTask `json:"tasks"`
	ClosedTasks    int         `json:"closed_tasks"`              // closed descendants: counted, not listed
	TasksTruncated int         `json:"tasks_truncated,omitempty"` // open descendants past the cap
	// v0.6, absent from older peers: the root's own next step and refs, the
	// rules in force, and its latest handover (text: its note).
	Next         *stamped       `json:"next,omitempty"`
	Refs         []planRef      `json:"refs,omitempty"`
	Decisions    []planDecision `json:"decisions,omitempty"`
	LastHandover *stamped       `json:"last_handover,omitempty"`
	// v0.7, absent from older peers: per inbox in this tree (the root's and
	// each sub-orchestrator's) its oldest unacked ready/done/fail; and the
	// tally, derived per view.
	Unread []unreadWork `json:"unread_work,omitempty"`
	Tally  string       `json:"tally,omitempty"`
}

// unreadWork is the oldest ready/done/fail event a recipient in the tree
// has not acked, and how many such events it holds.
type unreadWork struct {
	EventID   int64   `json:"event_id"`
	Kind      string  `json:"kind"`
	Summary   string  `json:"summary,omitempty"`
	At        string  `json:"at"`
	AgeMS     int64   `json:"age_ms"`
	Count     int64   `json:"count"`
	Recipient taskRef `json:"recipient"`
	From      taskRef `json:"from"`
}

// milestone is one checkpoint for the CUE LOG.
type milestone struct {
	ID           int64  `json:"id"`
	Kind         string `json:"kind"` // an event kind; "note" is an orchestrator's own note, "ref" a pr/release/tag/merged ref
	Text         string `json:"text"`
	Key          string `json:"key,omitempty"` // ref key
	At           string `json:"at"`
	AgeMS        int64  `json:"age_ms"`
	RootID       int64  `json:"root_id"`
	Orchestrator string `json:"orchestrator"`
	LaneID       int64  `json:"lane_id,omitempty"` // absent when the orchestrator itself wrote it
	Lane         string `json:"lane,omitempty"`
	Role         string `json:"role,omitempty"`
	Machine      string `json:"machine,omitempty"` // set per view
	NodeID       string `json:"node_id,omitempty"`
}

// daemonHealth is this machine's event bridge as its heartbeat shows it:
// "fresh", "stale" or "none"; At is the last heartbeat, only when stale, so
// a live daemon's state does not change between pushes.
type daemonHealth struct {
	State string `json:"state"`
	At    string `json:"at,omitempty"`
	AgeMS int64  `json:"age_ms,omitempty"`
}

// attentionItem is one PROGRAM cue: something that needs the owner.
type attentionItem struct {
	Kind         string       `json:"kind"`
	Severity     int          `json:"severity"` // attentionRank: 0 is the most severe
	Machine      string       `json:"machine"`
	NodeID       string       `json:"node_id,omitempty"`
	Local        bool         `json:"local"`
	Orchestrator *taskRef     `json:"orchestrator,omitempty"`
	Lane         *taskRef     `json:"lane,omitempty"`
	Since        string       `json:"since,omitempty"`
	AgeMS        int64        `json:"age_ms"`
	Text         string       `json:"text"`
	Action       string       `json:"action"`            // what to do, in words
	Command      string       `json:"command,omitempty"` // the exact command, when one helps
	PaneID       string       `json:"pane_id,omitempty"`
	Report       string       `json:"report,omitempty"`
	AskID        int64        `json:"ask_id,omitempty"` // owner_ask: answer in the orchestrator's pane
	Blocking     bool         `json:"blocking,omitempty"`
	AskerWaiting bool         `json:"asker_waiting,omitempty"`
	Also         []laneSignal `json:"also,omitempty"` // owner_ask: the asking lane's own cues, folded in
}

// laneSignal is a lane cue folded into that lane's owner ask: PROGRAM shows
// one cue per lane, the ask first, instead of two cues about one agent.
type laneSignal struct {
	Kind    string `json:"kind"`
	Since   string `json:"since,omitempty"`
	AgeMS   int64  `json:"age_ms"`
	Text    string `json:"text"`
	Action  string `json:"action"`
	Command string `json:"command,omitempty"`
	Report  string `json:"report,omitempty"`
}

type activity struct {
	ID       int64   `json:"id"`
	Kind     string  `json:"kind"`
	Summary  string  `json:"summary,omitempty"`
	At       string  `json:"at"`
	AgeMS    int64   `json:"age_ms"`
	Task     taskRef `json:"task"`
	RootID   int64   `json:"root_id"`
	RootName string  `json:"root_name"`
}

type closedRoot struct {
	ID       int64    `json:"id"`
	Name     string   `json:"name"`
	ClosedAt string   `json:"closed_at"`
	AgeMS    int64    `json:"age_ms"`
	Note     *stamped `json:"note"`
}

type dashState struct {
	Now           string         `json:"now"`
	Version       string         `json:"version"`
	OwnerAsks     []ownerAsk     `json:"owner_asks"`
	Orchestrators []orchestrator `json:"orchestrators"`
	Activity      []activity     `json:"activity"`
	Closed        []closedRoot   `json:"closed"`
	// v0.7, absent from older peers.
	Daemon     *daemonHealth   `json:"daemon,omitempty"`
	Milestones []milestone     `json:"milestones"`
	Attention  []attentionItem `json:"attention"` // derived per view; a push carries none
}

// openTrees maps every task of a non-closed root's tree to that root, with
// its depth and a sortable path (parents before children, siblings by id).
var openTrees = `with recursive tree(id, root, depth, path) as (
	select id, id, 0, printf('%012d', id) from
		(select id from tasks where parent_id is null and status != 'closed' order by id limit ` + strconv.Itoa(stateRootsMax) + `)
	union all
	select t.id, tree.root, tree.depth + 1, tree.path || '/' || printf('%012d', t.id)
	from tasks t join tree on t.parent_id = tree.id)`

// readState reads the whole dashboard in one read transaction.
func readState(cx context.Context, db *sql.DB, at time.Time) (*dashState, error) {
	tx, err := db.BeginTx(cx, &sql.TxOptions{ReadOnly: true})
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()
	nowS := stamp(at)
	age := func(ts string) int64 { return max(0, at.Sub(parseTime(ts)).Milliseconds()) }
	s := &dashState{Now: nowS, Version: version, OwnerAsks: []ownerAsk{}, Orchestrators: []orchestrator{},
		Activity: []activity{}, Closed: []closedRoot{}, Milestones: []milestone{}, Attention: []attentionItem{}}

	// Open owner asks on non-closed tasks: the ones the page shows.
	rows, err := tx.Query(`select e.id, coalesce(e.summary, ''), e.created_at, coalesce(json_extract(e.data, '$.blocking'), 0),
		t.id, t.name, t.role, coalesce(t.workspace_id, ''), coalesce(t.tab_id, ''), coalesce(l.pane_id, t.pane_id, ''),
		coalesce(t.waiting_until > ?, 0)
		from events e join tasks t on t.id = e.task_id left join launches l on l.id = t.current_launch_id
		where e.kind = 'ask' and e.answered_by is null and json_extract(e.data, '$.owner') = 1 and t.status != 'closed'
		order by e.id limit ?`, nowS, stateAsksMax)
	if err != nil {
		return nil, err
	}
	for rows.Next() {
		var a ownerAsk
		if err := rows.Scan(&a.ID, &a.Text, &a.At, &a.Blocking, &a.Asker.ID, &a.Asker.Name, &a.Asker.Role,
			&a.WorkspaceID, &a.TabID, &a.PaneID, &a.AskerWaiting); err != nil {
			rows.Close()
			return nil, err
		}
		a.Text, a.AgeMS = clip(a.Text, stateAskTextMax), age(a.At)
		s.OwnerAsks = append(s.OwnerAsks, a)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, err
	}
	for i := range s.OwnerAsks {
		a := &s.OwnerAsks[i]
		root, err := rootOf(tx, a.Asker.ID)
		if err != nil {
			return nil, err
		}
		a.Root.ID, a.AskerIsRoot = root, root == a.Asker.ID
		if err := tx.QueryRow(`select name, role from tasks where id = ?`, root).Scan(&a.Root.Name, &a.Root.Role); err != nil {
			return nil, err
		}
	}

	// Open trees: each root, then its descendants depth-first.
	rows, err = tx.Query(openTrees+`
		select tree.root, tree.depth, t.id, coalesce(t.parent_id, 0), t.name, t.role, t.status,
		coalesce(t.workspace_id, ''), coalesce(t.tab_id, ''), coalesce(l.pane_id, t.pane_id, ''), coalesce(t.report_path, ''),
		t.created_at, coalesce(t.waiting_until > ?, 0), t.acked_event_id,
		coalesce(l.observed_status, ''), coalesce(l.observed_at, ''), l.present,
		(select count(*) from events a where a.task_id = t.id and a.kind = 'ask' and a.answered_by is null),
		le.kind, le.summary, le.created_at, coalesce(t.agent_name, '')
		from tree join tasks t on t.id = tree.id left join launches l on l.id = t.current_launch_id
		left join events le on le.id = (select max(x.id) from events x where x.task_id = t.id and x.kind not in `+laneReportSkip+`)
		order by tree.root, tree.path`, nowS)
	if err != nil {
		return nil, err
	}
	var acked []int64
	for rows.Next() {
		var root, ack int64
		var st stateTask
		var ws, tab, created string
		var present sql.NullBool
		var leKind, leSummary, leAt sql.NullString
		if err := rows.Scan(&root, &st.Depth, &st.ID, &st.ParentID, &st.Name, &st.Role, &st.Status, &ws, &tab, &st.PaneID,
			&st.ReportPath, &created, &st.Waiting, &ack, &st.AgentStatus, &st.ObservedAt, &present, &st.OpenAsks,
			&leKind, &leSummary, &leAt, &st.AgentName); err != nil {
			rows.Close()
			return nil, err
		}
		if present.Valid {
			st.Present = &present.Bool
		}
		if leKind.Valid {
			st.LastEvent = &lastEvent{Kind: leKind.String, Summary: clip(leSummary.String, stateSummaryMax),
				At: leAt.String, AgeMS: age(leAt.String)}
		}
		if st.Depth == 0 {
			s.Orchestrators = append(s.Orchestrators, orchestrator{ID: st.ID, Name: st.Name, Role: st.Role,
				Status: st.Status, WorkspaceID: ws, TabID: tab, PaneID: st.PaneID, CreatedAt: created,
				Waiting: st.Waiting, OpenAsks: st.OpenAsks, LastEvent: st.LastEvent, Tasks: []stateTask{}})
			acked = append(acked, ack)
			continue
		}
		// Closed descendants are only counted, so old history cannot use up
		// the cap; the traversal still reaches open tasks under a closed one.
		o := &s.Orchestrators[len(s.Orchestrators)-1]
		if st.Status == "closed" {
			o.ClosedTasks++
			continue
		}
		if len(o.Tasks) >= stateTasksMax {
			o.TasksTruncated++
			continue
		}
		o.Tasks = append(o.Tasks, st)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, err
	}
	for i := range s.Orchestrators {
		o := &s.Orchestrators[i]
		if err := tx.QueryRow(`select count(*) from events where recipient_task_id = ? and id > ?`, o.ID, acked[i]).
			Scan(&o.Unacked); err != nil {
			return nil, err
		}
		if o.Note, err = lastNote(tx, o.ID, age); err != nil {
			return nil, err
		}
		if err := readPlan(tx, o, age); err != nil {
			return nil, err
		}
	}
	if err := readLanePlans(tx, s.Orchestrators, age); err != nil {
		return nil, err
	}
	if err := readUnread(tx, s.Orchestrators, age); err != nil {
		return nil, err
	}
	if s.Milestones, err = readMilestones(tx, age); err != nil {
		return nil, err
	}
	state, hb, _, err := daemonState(tx)
	if err != nil {
		return nil, err
	}
	s.Daemon = &daemonHealth{State: state}
	if state == "stale" {
		s.Daemon.At, s.Daemon.AgeMS = hb, age(hb)
	}

	// Activity across open trees, newest first.
	rows, err = tx.Query(openTrees+`
		select e.id, e.kind, coalesce(e.summary, ''), e.created_at, t.id, t.name, t.role, tree.root, r.name
		from events e join tree on tree.id = e.task_id join tasks t on t.id = e.task_id join tasks r on r.id = tree.root
		order by e.id desc limit ?`, stateActivityMax)
	if err != nil {
		return nil, err
	}
	for rows.Next() {
		var a activity
		if err := rows.Scan(&a.ID, &a.Kind, &a.Summary, &a.At, &a.Task.ID, &a.Task.Name, &a.Task.Role, &a.RootID, &a.RootName); err != nil {
			rows.Close()
			return nil, err
		}
		a.Summary, a.AgeMS = clip(a.Summary, stateSummaryMax), age(a.At)
		s.Activity = append(s.Activity, a)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, err
	}

	// Roots closed in the last 24 hours.
	rows, err = tx.Query(`select id, name, closed_at from tasks where parent_id is null and status = 'closed'
		and closed_at > ? order by closed_at desc limit ?`, stamp(at.Add(-24*time.Hour)), stateClosedMax)
	if err != nil {
		return nil, err
	}
	for rows.Next() {
		var c closedRoot
		if err := rows.Scan(&c.ID, &c.Name, &c.ClosedAt); err != nil {
			rows.Close()
			return nil, err
		}
		c.AgeMS = age(c.ClosedAt)
		s.Closed = append(s.Closed, c)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, err
	}
	for i := range s.Closed {
		if s.Closed[i].Note, err = lastNote(tx, s.Closed[i].ID, age); err != nil {
			return nil, err
		}
	}
	return s, nil
}

// lastNote is a task's latest note, or nil.
func lastNote(tx *sql.Tx, id int64, age func(string) int64) (*stamped, error) {
	var n stamped
	err := tx.QueryRow(`select coalesce(summary, ''), created_at from events where task_id = ? and kind = 'note'
		order by id desc limit 1`, id).Scan(&n.Text, &n.At)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	} else if err != nil {
		return nil, err
	}
	n.Text, n.AgeMS = clip(n.Text, stateNoteMax), age(n.At)
	return &n, nil
}

// readPlan adds a root's decisions in force (the latest stateDecisionMax)
// and its latest handover.
func readPlan(tx *sql.Tx, o *orchestrator, age func(string) int64) error {
	ds, err := decisionsInForce(tx, o.ID)
	if err != nil {
		return err
	}
	if len(ds) > stateDecisionMax {
		ds = ds[len(ds)-stateDecisionMax:]
	}
	for i := range ds {
		ds[i].Text, ds[i].Answer, ds[i].AgeMS = clip(ds[i].Text, stateNoteMax), clip(ds[i].Answer, stateNoteMax), age(ds[i].At)
	}
	o.Decisions = ds
	marks, err := handoverMarks(tx, o.ID, 1)
	if err != nil || len(marks) == 0 {
		return err
	}
	o.LastHandover = &stamped{Text: clip(marks[0].Note, stateNoteMax), At: marks[0].At, AgeMS: age(marks[0].At)}
	return nil
}

// readLanePlans adds every open tree's next steps and refs in two queries.
func readLanePlans(tx *sql.Tx, orchs []orchestrator, age func(string) int64) error {
	next := map[int64]*stamped{}
	rows, err := tx.Query(openTrees + `
		select e.task_id, coalesce(e.summary, ''), e.created_at, coalesce(json_extract(e.data, '$.clear'), 0) from events e
		where e.id in (select max(x.id) from events x join tree on tree.id = x.task_id where x.kind = 'next' group by x.task_id)`)
	if err != nil {
		return err
	}
	for rows.Next() {
		var id int64
		var n stamped
		var clear bool
		if err := rows.Scan(&id, &n.Text, &n.At, &clear); err != nil {
			rows.Close()
			return err
		}
		if !clear {
			n.Text, n.AgeMS = clip(n.Text, stateNoteMax), age(n.At)
			next[id] = &n
		}
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return err
	}
	refs := map[int64][]planRef{}
	rows, err = tx.Query(openTrees + `
		select e.task_id, json_extract(e.data, '$.key'), coalesce(json_extract(e.data, '$.value'), '') from events e
		where e.id in (select max(x.id) from events x join tree on tree.id = x.task_id where x.kind = 'ref'
			group by x.task_id, json_extract(x.data, '$.key'))
		order by e.task_id, 2`)
	if err != nil {
		return err
	}
	for rows.Next() {
		var id int64
		var r planRef
		if err := rows.Scan(&id, &r.Key, &r.Value); err != nil {
			rows.Close()
			return err
		}
		if r.Value != "" {
			refs[id] = append(refs[id], r)
		}
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return err
	}
	for i := range orchs {
		o := &orchs[i]
		o.Next, o.Refs = next[o.ID], refs[o.ID]
		for j := range o.Tasks {
			t := &o.Tasks[j]
			t.Next, t.Refs = next[t.ID], refs[t.ID]
		}
	}
	return nil
}

// readUnread adds, per open tree, each non-closed recipient's oldest
// unacked ready/done/fail event (the root's inbox and each
// sub-orchestrator's), with how many it holds.
func readUnread(tx *sql.Tx, orchs []orchestrator, age func(string) int64) error {
	at := make(map[int64]int, len(orchs))
	for i := range orchs {
		at[orchs[i].ID] = i
	}
	const work = `x.recipient_task_id = r.id and x.id > r.acked_event_id and x.kind in ('ready', 'done', 'fail')`
	rows, err := tx.Query(openTrees + `
		select tree.root, r.id, r.name, r.role, e.id, e.kind, coalesce(e.summary, ''), e.created_at, s.id, s.name, s.role,
		(select count(*) from events x where ` + work + `)
		from tree join tasks r on r.id = tree.id
		join events e on e.id = (select min(x.id) from events x where ` + work + `)
		join tasks s on s.id = e.task_id
		where r.status != 'closed' order by tree.root, e.id`)
	if err != nil {
		return err
	}
	defer rows.Close()
	for rows.Next() {
		var root int64
		var u unreadWork
		if err := rows.Scan(&root, &u.Recipient.ID, &u.Recipient.Name, &u.Recipient.Role, &u.EventID, &u.Kind, &u.Summary,
			&u.At, &u.From.ID, &u.From.Name, &u.From.Role, &u.Count); err != nil {
			return err
		}
		i, ok := at[root]
		if !ok || len(orchs[i].Unread) >= stateUnreadMax {
			continue
		}
		u.Summary, u.AgeMS = clip(u.Summary, stateSummaryMax), age(u.At)
		orchs[i].Unread = append(orchs[i].Unread, u)
	}
	return rows.Err()
}

// readMilestones reads the machine's latest stateMilestonesMax milestones,
// newest first, across every tree (a closed campaign's last checkpoints
// stay in the log).
func readMilestones(tx *sql.Tx, age func(string) int64) ([]milestone, error) {
	kinds := `'` + strings.Join(milestoneKinds, `', '`) + `'`
	keys := `'` + strings.Join(milestoneRefKeys, `', '`) + `'`
	rows, err := tx.Query(`select e.id, e.kind, coalesce(e.summary, ''), e.created_at, t.id, t.name, t.role, t.parent_id is null,
		coalesce(json_extract(e.data, '$.key'), '')
		from events e join tasks t on t.id = e.task_id
		where e.kind in (`+kinds+`) or (e.kind = 'note' and t.parent_id is null)
			or (e.kind = 'ref' and json_extract(e.data, '$.key') in (`+keys+`) and coalesce(json_extract(e.data, '$.value'), '') != '')
		order by e.id desc limit ?`, stateMilestonesMax)
	if err != nil {
		return nil, err
	}
	out := []milestone{}
	for rows.Next() {
		var m milestone
		var isRoot bool
		var laneName, role, key string
		if err := rows.Scan(&m.ID, &m.Kind, &m.Text, &m.At, &m.LaneID, &laneName, &role, &isRoot, &key); err != nil {
			rows.Close()
			return nil, err
		}
		m.Text, m.AgeMS = clip(m.Text, stateSummaryMax), age(m.At)
		if m.Kind == "ref" {
			m.Key = key
		}
		if isRoot {
			m.RootID, m.Orchestrator, m.LaneID = m.LaneID, laneName, 0
		} else {
			m.Lane, m.Role = laneName, role
		}
		out = append(out, m)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, err
	}
	names := map[int64]string{}
	for i := range out {
		m := &out[i]
		if m.LaneID == 0 {
			continue
		}
		if m.RootID, err = rootOf(tx, m.LaneID); err != nil {
			return nil, err
		}
		if _, ok := names[m.RootID]; !ok {
			var n string
			if err := tx.QueryRow(`select name from tasks where id = ?`, m.RootID).Scan(&n); err != nil {
				return nil, err
			}
			names[m.RootID] = n
		}
		m.Orchestrator = names[m.RootID]
	}
	return out, nil
}

// machineCtx names the machine a state belongs to on this page.
type machineCtx struct {
	Name          string
	NodeID        string
	Local         bool
	Stale         bool
	LastPushAgeMS int64
}

// derive fills a machine's attention list, each lane's mark and each
// orchestrator's tally from the state alone, at time at. It runs on every
// view, for this machine and (on the hub) for each peer after rebase, so a
// peer's cues age on the hub's clock and an older peer, which sends no v0.7
// fields, still gets its asks and failed, blocked and missing lanes.
func derive(s *dashState, m machineCtx, at time.Time) {
	ageOf := func(ts string) int64 {
		t := parseTime(ts)
		if t.IsZero() {
			return 0
		}
		return max(0, at.Sub(t).Milliseconds())
	}
	items := []attentionItem{}
	add := func(it attentionItem) {
		it.Severity, it.Machine, it.NodeID, it.Local = attentionRank[it.Kind], m.Name, m.NodeID, m.Local
		if it.Kind != attentionOwnerAsk {
			it.Text = clip(oneLine(it.Text), attentionTextMax)
		}
		if it.Since != "" {
			it.AgeMS = ageOf(it.Since)
		}
		items = append(items, it)
	}
	ref := func(r taskRef) *taskRef { return &r }

	// The page is read-only: an owner ask is answered in its orchestrator's
	// pane, which passes the answer on with `taskr answer`.
	rootPane := map[int64]string{}
	for _, o := range s.Orchestrators {
		rootPane[o.ID] = o.PaneID
	}
	for _, a := range s.OwnerAsks {
		action := "Answer in " + a.Root.Name + "'s pane"
		if p := rootPane[a.Root.ID]; p != "" {
			action += " " + p
		}
		it := attentionItem{Kind: attentionOwnerAsk, Orchestrator: ref(a.Root), Since: a.At, Text: a.Text,
			Action: action, PaneID: rootPane[a.Root.ID], AskID: a.ID, Blocking: a.Blocking, AskerWaiting: a.AskerWaiting}
		if !a.AskerIsRoot {
			it.Lane = ref(a.Asker)
		}
		add(it)
	}
	for i := range s.Orchestrators {
		o := &s.Orchestrators[i]
		root := taskRef{ID: o.ID, Name: o.Name, Role: o.Role}
		panes := map[int64]string{o.ID: o.PaneID}
		for j := range o.Tasks {
			t := &o.Tasks[j]
			t.Mark = laneMark(t)
			panes[t.ID] = t.PaneID
			lane := taskRef{ID: t.ID, Name: t.Name, Role: t.Role}
			// A lane's last report, but never its own question: an ask's
			// text belongs to the ask's cue alone.
			last := ""
			if t.LastEvent != nil {
				last = t.LastEvent.Summary
				if t.LastEvent.Kind == "ask" {
					last = "it asked a question"
				}
			}
			read := readCommand(t)
			switch t.Mark {
			case markFailed:
				it := attentionItem{Kind: attentionFailed, Orchestrator: ref(root), Lane: ref(lane),
					Text: firstNonEmpty(last, "the lane reported fail"), Action: "Read its report; relaunch or close the lane",
					Command: read, PaneID: t.PaneID, Report: t.ReportPath}
				if t.LastEvent != nil {
					it.Since = t.LastEvent.At
				}
				add(it)
			case markBlocked:
				text := "Herdr sees an approval or question dialog in its pane"
				if last != "" {
					text += "; last: " + last
				}
				add(attentionItem{Kind: attentionBlocked, Orchestrator: ref(root), Lane: ref(lane), Since: t.ObservedAt,
					Text: text, Action: "Answer the dialog in its pane", Command: read, PaneID: t.PaneID})
			case markMissing:
				add(attentionItem{Kind: attentionMissing, Orchestrator: ref(root), Lane: ref(lane), Since: t.ObservedAt,
					Text:   "pane " + firstNonEmpty(t.PaneID, "?") + " is gone; last: " + firstNonEmpty(last, "nothing reported"),
					Action: "Have " + o.Name + " relaunch or close it", Command: fmt.Sprintf("taskr log %d", t.ID), PaneID: t.PaneID})
			}
		}
		waiting := false
		for _, u := range o.Unread {
			if ageOf(u.At) < workWaitingAfter.Milliseconds() {
				continue
			}
			waiting = true
			n := ""
			if u.Count > 1 {
				n = fmt.Sprintf(" (%d unread)", u.Count)
			}
			it := attentionItem{Kind: attentionWorkWaits, Orchestrator: ref(root), Lane: ref(u.From), Since: u.At,
				Text:   u.Recipient.Name + " has not taken " + u.From.Name + "'s " + u.Kind + n + ": " + firstNonEmpty(u.Summary, "no summary"),
				Action: "Check " + u.Recipient.Name + "'s pane", PaneID: panes[u.Recipient.ID]}
			if p := panes[u.Recipient.ID]; p != "" {
				it.Command = "herdr pane read " + p + " --source recent-unwrapped --lines 60"
			}
			add(it)
		}
		red := false
		for _, it := range items {
			red = red || (it.Orchestrator != nil && it.Orchestrator.ID == o.ID && it.Kind != attentionWorkWaits)
		}
		lit := false
		for _, ms := range s.Milestones {
			lit = lit || (ms.RootID == o.ID && ageOf(ms.At) <= milestoneLitFor.Milliseconds())
		}
		switch {
		case red:
			o.Tally = tallyRed
		case waiting:
			o.Tally = tallyAmber
		case lit:
			o.Tally = tallyGreen
		default:
			o.Tally = tallyOff
		}
	}
	if m.Stale {
		since := stamp(at.Add(-time.Duration(m.LastPushAgeMS) * time.Millisecond))
		add(attentionItem{Kind: attentionStale, Since: since, Text: m.Name + " has not pushed its state to the hub",
			Action: "Check the taskr daemon on " + m.Name, Command: "taskr daemon --status"})
	} else if s.Daemon != nil && s.Daemon.State != "fresh" {
		text := "the taskr daemon has no live Herdr connection; no heartbeat"
		if s.Daemon.State == "stale" {
			text = "the taskr daemon's heartbeat stopped; Herdr events are not arriving"
		}
		add(attentionItem{Kind: attentionDaemon, Since: s.Daemon.At, Text: text,
			Action: "Check the taskr daemon on " + m.Name, Command: "taskr daemon --status"})
	}
	for i := range s.Milestones {
		s.Milestones[i].Machine, s.Milestones[i].NodeID = m.Name, m.NodeID
	}
	items = foldIntoAsks(items)
	sortAttention(items)
	s.Attention = capList(items, stateAttentionMax)
}

// foldIntoAsks merges a lane's blocked, failed and missing cues into the
// owner ask that lane has open, so PROGRAM shows one cue per agent.
//
// Every observed signal is kept, after the ask's own answer action. Herdr's
// "blocked" means it recognised an approval or question UI in the pane, and
// the ledger cannot tell which question that is: asker_waiting is only a
// lease (waiting_until > now), which a dead wait can leave behind, not proof
// that the dialog is this ask. So a blocked dialog keeps its pane action;
// while the asker holds a wait lease it is worded as a condition.
func foldIntoAsks(items []attentionItem) []attentionItem {
	asks := map[int64]int{} // lane id -> its first owner ask
	for i, it := range items {
		if _, ok := asks[laneOf(it)]; it.Kind == attentionOwnerAsk && it.Lane != nil && !ok {
			asks[it.Lane.ID] = i
		}
	}
	folded := map[int]bool{}
	for j, it := range items {
		i, ok := asks[laneOf(it)]
		if !ok || (it.Kind != attentionBlocked && it.Kind != attentionFailed && it.Kind != attentionMissing) {
			continue
		}
		folded[j] = true
		action := it.Action
		if it.Kind == attentionBlocked && items[i].AskerWaiting {
			action = "If the dialog is not this question, answer it in its pane"
		}
		items[i].Also = append(items[i].Also, laneSignal{Kind: it.Kind, Since: it.Since, AgeMS: it.AgeMS, Text: it.Text,
			Action: action, Command: it.Command, Report: it.Report})
	}
	out := make([]attentionItem, 0, len(items)-len(folded))
	for j, it := range items {
		if !folded[j] {
			out = append(out, it)
		}
	}
	return out
}

func laneOf(it attentionItem) int64 {
	if it.Lane == nil {
		return 0
	}
	return it.Lane.ID
}

// laneMark is a lane's state as the wall shows it: a report first (failed,
// done), then what Herdr observed of an open lane (missing, blocked).
func laneMark(t *stateTask) string {
	switch t.Status {
	case "failed":
		return markFailed
	case "done":
		return markDone
	case "planned":
		return markPlanned
	}
	switch {
	case t.Status == "open" && t.Present != nil && !*t.Present:
		return markMissing
	case t.AgentStatus == "blocked" && (t.Present == nil || *t.Present):
		return markBlocked
	case t.Status == "ready":
		return markReady
	}
	return markWorking
}

// readCommand is how the owner looks at a lane's agent.
func readCommand(t *stateTask) string {
	switch {
	case t.AgentName != "":
		return "herdr agent read " + t.AgentName + " --source recent-unwrapped --lines 60"
	case t.PaneID != "":
		return "herdr pane read " + t.PaneID + " --source recent-unwrapped --lines 60"
	}
	return fmt.Sprintf("taskr log %d", t.ID)
}

// sortAttention orders by severity, then oldest first.
func sortAttention(items []attentionItem) {
	sort.SliceStable(items, func(i, j int) bool {
		if items[i].Severity != items[j].Severity {
			return items[i].Severity < items[j].Severity
		}
		return items[i].AgeMS > items[j].AgeMS
	})
}

// oneLine keeps the first non-empty line of s.
func oneLine(s string) string {
	for _, l := range strings.Split(s, "\n") {
		if l = strings.TrimSpace(l); l != "" {
			return l
		}
	}
	return ""
}
