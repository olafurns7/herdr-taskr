package main

import (
	"context"
	"database/sql"
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
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unicode/utf8"
)

// The hub's HTTP server, started by `taskr daemon`. It serves one route, the
// RPC a client host's CLI calls, and no page: the owner reads the ledger in
// taskr-tui and the CLI. The names (dashboard.addr, dashboard_url, the
// "dashboard" status fields) date from the web page it once served; hosts
// and the Rust binary rely on them, so they stay. The server is a passenger:
// when it cannot bind, the daemon logs one line and carries on.

// dashboardDefaultAddr is a variable so tests can bind port 0.
var dashboardDefaultAddr = "127.0.0.1:7788"

const (
	dashboardAddrFile = "dashboard.addr"
	dashboardURLKey   = "dashboard_url"
)

// Caps shared by the reads.
const (
	ownerNoteTextMax   = 4000 // runes
	stateSummaryMax    = 300
	attentionTextMax   = 200 // runes; one line
	attentionBlocked   = "lane_blocked"
	attentionMissing   = "lane_missing"
	attentionStale     = "machine_stale"
	attentionWorkWaits = "work_waiting"
)

// attentionRank orders what needs the owner: most severe first.
var attentionRank = map[string]int{"owner_ask": 0, attentionBlocked: 1, "lane_failed": 2, attentionMissing: 3,
	attentionStale: 4, "daemon_unhealthy": 5, attentionWorkWaits: 6}

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

// Lane marks: a lane's state before any colour.
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
// default, "off" disables the server, "tailnet" or "tailnet:PORT" makes this
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
	db    *sql.DB
	log   *daemonLog
	hosts map[string]bool // accepted Host headers of the loopback listener: its host:port and localhost:port
	url   string          // the loopback URL; "" when loopback is not bound
	hub   atomic.Pointer[hubInfo]
	srv   *http.Server
	mux   *http.ServeMux
	usage *dashboardUsage

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
// (host:port).
func newDashboard(db *sql.DB, lg *daemonLog, addr string) *dashboard {
	_, port, _ := net.SplitHostPort(addr)
	d := &dashboard{db: db, log: lg, url: "http://" + addr + "/", usage: &dashboardUsage{},
		hosts: map[string]bool{strings.ToLower(addr): true, "localhost:" + port: true}}
	d.mux = http.NewServeMux()
	d.mux.HandleFunc("POST "+rpcPath, d.rpc) // a remote host's CLI; sets its own write deadline
	d.srv = &http.Server{Handler: dashboardUsageHandler(d, d.usage), ReadHeaderTimeout: 5 * time.Second, ReadTimeout: 10 * time.Second,
		WriteTimeout: 15 * time.Second, IdleTimeout: 60 * time.Second, MaxHeaderBytes: 16 << 10,
		ErrorLog: log.New(io.Discard, "", 0)}
	return d
}

// startDashboard binds the configured address and serves in the background.
// It returns nil, after one log line, when the server is off, refused, or
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

// laneMark is a lane's state as the views show it: a report first (failed,
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

// oneLine keeps the first non-empty line of s.
func oneLine(s string) string {
	for _, l := range strings.Split(s, "\n") {
		if l = strings.TrimSpace(l); l != "" {
			return l
		}
	}
	return ""
}
