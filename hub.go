package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"sort"
	"strings"
	"time"
)

// The hub: one machine's dashboard (dashboard.addr "tailnet") also listens
// on its Tailscale address. Other machines' daemons push their /api/state
// there; the hub keeps each peer's latest snapshot, shows it read-only, and
// never merges ledgers or writes to a peer. Every request from a
// non-loopback address is admitted only by `tailscale whois`: the node must
// belong to the hub's own user and carry no tags.

const (
	hubURLKey    = "hub_tailnet_url"
	peerPushPath = "/api/peer/push"
	peerPushMax  = 2 << 20
	snapshotMax  = 1 << 20
	peersMax     = 20
)

// Hub timing; package variables so tests can shrink them.
var (
	peerStaleAfter = 45 * time.Second
	peerDropAfter  = 7 * 24 * time.Hour
)

// hubInfo is the hub's identity and names. It is built whole and published
// once through dashboard.hub, before any tailnet listener serves.
type hubInfo struct {
	self  *tsSelf
	whois *whoisCache
	url   string          // http://<MagicDNS name>:<port>/
	addrs []string        // tailnet host:port to bind: the IPv4, then the IPv6 when Self has one
	hosts map[string]bool // accepted Host headers on the tailnet
}

// Tailnet retry timing; package variables so tests can shrink them.
var (
	tailnetRetryBase = 2 * time.Second
	tailnetRetryCap  = 60 * time.Second
)

type identKey struct{}

// localMachine is this host's short name, for the page.
func localMachine() string {
	h, err := os.Hostname()
	if err != nil || h == "" {
		return "this machine"
	}
	return machineLabel(h)
}

// startTailnet makes the dashboard the hub: it tries now, and while
// Tailscale or a tailnet address is unavailable it keeps retrying in the
// background with capped backoff until every address is bound or the
// dashboard stops. Loopback and the event bridge never wait on it.
func (d *dashboard) startTailnet(port string) {
	done, err := d.tryTailnet(port)
	if done {
		return
	}
	if d.tailnetServing() {
		d.log.logf("dashboard: a tailnet address is unavailable (%v); retrying", err)
	} else {
		d.log.logf("dashboard: tailnet unavailable (%v); serving loopback only; retrying", err)
	}
	cx, cancel := context.WithCancel(context.Background())
	d.stopRetry, d.retryDone = cancel, make(chan struct{})
	go func() {
		defer close(d.retryDone)
		for n := 0; ; n++ {
			if !sleepCx(cx, min(tailnetRetryCap, tailnetRetryBase<<min(n, 6))) {
				return
			}
			done, err := d.tryTailnet(port)
			if done {
				return
			}
			d.log.limited("tailnet-retry", 10*time.Minute, "dashboard: tailnet still unavailable (%v); retrying", err)
		}
	}()
}

func (d *dashboard) tailnetServing() bool {
	d.mu.Lock()
	defer d.mu.Unlock()
	return d.tailnetUp > 0
}

// tryTailnet publishes the hub (once) and binds each of its tailnet
// addresses not yet bound, each on its own: IPv4 never waits for IPv6. It
// reports whether all are bound.
func (d *dashboard) tryTailnet(port string) (bool, error) {
	h := d.hub.Load()
	if h == nil {
		self, err := tailscaleSelf()
		if err != nil {
			return false, err
		}
		h = d.enableHub(self, port)
	}
	d.mu.Lock()
	bound := map[string]bool{}
	for _, l := range d.listeners {
		bound[l.Addr().String()] = true
	}
	d.mu.Unlock()
	var errs []error
	for _, addr := range h.addrs {
		if bound[addr] {
			continue
		}
		ln, err := net.Listen("tcp", addr)
		if err != nil {
			errs = append(errs, fmt.Errorf("listen %s: %v", addr, err))
			continue
		}
		d.mu.Lock()
		d.tailnetUp++
		first := d.tailnetUp == 1
		d.mu.Unlock()
		if first {
			if err := setMeta(d.db, hubURLKey, h.url); err != nil {
				d.log.logf("meta write failed: %v", err)
			}
			d.log.logf("dashboard hub at %s", h.url)
		}
		d.log.logf("dashboard: serving the tailnet on %s", addr)
		d.serve(ln, func() {
			d.mu.Lock()
			d.tailnetUp--
			last := d.tailnetUp == 0
			d.mu.Unlock()
			if last {
				d.db.Exec(`delete from meta where key = ?`, hubURLKey)
			}
		})
	}
	return len(errs) == 0, errors.Join(errs...)
}

// enableHub builds the hub's identity, names and addresses and publishes
// them at once. The IPv6 address is Self's exact tailnet address, never a
// wildcard.
func (d *dashboard) enableHub(self *tsSelf, port string) *hubInfo {
	h := &hubInfo{self: self, whois: newWhoisCache(self.Login), url: "http://" + self.DNSName + ":" + port + "/",
		addrs: []string{net.JoinHostPort(self.IP.String(), port)}, hosts: map[string]bool{}}
	names := []string{self.IP.String(), self.Short, self.DNSName, self.DNSName + "."}
	hostsOf := []string{net.JoinHostPort(self.IP.String(), port)}
	if self.IP6 != nil {
		h.addrs = append(h.addrs, net.JoinHostPort(self.IP6.String(), port))
		hostsOf = append(hostsOf, net.JoinHostPort(self.IP6.String(), port))
		names = append(names, "["+self.IP6.String()+"]")
	}
	for _, n := range names[1:4] {
		hostsOf = append(hostsOf, n+":"+port)
	}
	if port == "80" {
		hostsOf = append(hostsOf, names...)
	}
	for _, x := range hostsOf {
		h.hosts[strings.ToLower(x)] = true
	}
	d.hub.Store(h)
	return h
}

// admit runs whois for a non-loopback RemoteAddr (never a forwarded header)
// and refuses with 403 unless the node is the owner's and untagged. Only a
// hub serves non-loopback addresses; without one they are refused. It
// returns the request carrying the identity.
func (d *dashboard) admit(w http.ResponseWriter, r *http.Request, hub *hubInfo) (*http.Request, bool) {
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	ip := net.ParseIP(host)
	if err != nil || ip == nil {
		httpError(w, http.StatusForbidden, "unknown remote address")
		return r, false
	}
	if loopbackPeer(ip) {
		return r, true
	}
	if hub == nil {
		httpError(w, http.StatusForbidden, "this dashboard serves loopback only")
		return r, false
	}
	id, err := hub.whois.admit(ip)
	if err != nil {
		d.log.limited("whois:"+ip.String(), time.Minute, "dashboard: refused %s: %v", ip, err)
		httpError(w, http.StatusForbidden, "this tailnet node is not admitted")
		return r, false
	}
	return r.WithContext(context.WithValue(r.Context(), identKey{}, id)), true
}

// pushBody is the strict envelope; state is decoded apart, leniently, so a
// newer peer's additive fields are dropped instead of refusing the push.
// Applied is a v0.6 peer's report of relayed answers it applied: accepted
// so those peers keep pushing, and ignored, since the hub relays nothing.
type pushBody struct {
	State   json.RawMessage `json:"state"`
	Now     string          `json:"now"`
	Applied json.RawMessage `json:"applied,omitempty"`
}

// decodePeerState reads a pushed state: an object whose known fields must
// have their types; unknown fields are ignored, never stored or read.
func decodePeerState(raw json.RawMessage) (*dashState, error) {
	if len(raw) == 0 || raw[0] != '{' {
		return nil, errors.New("state must be a JSON object")
	}
	var s dashState
	if err := json.Unmarshal(raw, &s); err != nil {
		return nil, err
	}
	return &s, nil
}

type hubRef struct {
	NodeID  string `json:"node_id"`
	Machine string `json:"machine"`
}

// pushReply names the hub. Answers is always empty: a v0.6 peer reads it,
// and the hub no longer queues owner answers for peers.
type pushReply struct {
	Hub     hubRef     `json:"hub"`
	Answers []struct{} `json:"answers"`
}

// peerPush stores a peer's snapshot.
// The peer is whoever whois says it is; the body cannot name a machine. It
// is machine-to-machine, so it needs no page token or same-origin headers.
func (d *dashboard) peerPush(w http.ResponseWriter, r *http.Request) {
	hub := d.hub.Load()
	if hub == nil {
		http.NotFound(w, r)
		return
	}
	id, ok := r.Context().Value(identKey{}).(peerIdent)
	if !ok {
		httpError(w, http.StatusForbidden, "a push needs a tailnet identity")
		return
	}
	if id.NodeID == hub.self.NodeID {
		httpError(w, http.StatusForbidden, "the hub does not push to itself")
		return
	}
	if !isJSON(r) {
		httpError(w, http.StatusUnsupportedMediaType, "Content-Type must be application/json")
		return
	}
	var body pushBody
	if err := decodeStrict(w, r, peerPushMax, &body); err != nil {
		d.log.limited("push:"+id.NodeID, time.Minute, "dashboard: push from %s refused: %v", id.Machine, err)
		if tooLarge(err) {
			httpError(w, http.StatusRequestEntityTooLarge, "push body too large")
			return
		}
		httpError(w, http.StatusBadRequest, "body must be {\"state\", \"now\"}: "+truncate(err.Error(), 200))
		return
	}
	peerNow, err := time.Parse(time.RFC3339Nano, body.Now)
	if err != nil {
		httpError(w, http.StatusBadRequest, "push needs an RFC 3339 now")
		return
	}
	state, err := decodePeerState(body.State)
	if err != nil {
		d.log.limited("push:"+id.NodeID, time.Minute, "dashboard: push from %s refused: state: %v", id.Machine, err)
		httpError(w, http.StatusBadRequest, "push state is not a taskr state: "+truncate(err.Error(), 200))
		return
	}
	capState(state)
	snap, _ := json.Marshal(state)
	if len(snap) > snapshotMax {
		d.log.limited("push:"+id.NodeID, time.Minute, "dashboard: push from %s refused: snapshot of %d bytes", id.Machine, len(snap))
		httpError(w, http.StatusRequestEntityTooLarge, "snapshot too large")
		return
	}
	reply := pushReply{Hub: hubRef{NodeID: hub.self.NodeID, Machine: hub.self.Short}, Answers: []struct{}{}}
	full := false
	err = withTx(d.db, func(tx *sql.Tx) error {
		at := time.Now()
		var known, active int
		if err := tx.QueryRow(`select count(*) from peers where node_id = ? and received_at > ?`, id.NodeID,
			stamp(at.Add(-peerDropAfter))).Scan(&known); err != nil {
			return err
		}
		if err := tx.QueryRow(`select count(*) from peers where received_at > ?`, stamp(at.Add(-peerDropAfter))).Scan(&active); err != nil {
			return err
		}
		if known == 0 && active >= peersMax {
			full = true
			return nil
		}
		_, err := tx.Exec(`insert into peers (node_id, machine, login, snapshot, peer_now, received_at) values (?, ?, ?, ?, ?, ?)
			on conflict(node_id) do update set machine = excluded.machine, login = excluded.login, snapshot = excluded.snapshot,
			peer_now = excluded.peer_now, received_at = excluded.received_at`,
			id.NodeID, id.Machine, id.Login, string(snap), stamp(peerNow), stamp(at))
		return err
	})
	switch {
	case err != nil:
		d.log.logf("dashboard: push from %s failed: %v", id.Machine, err)
		httpError(w, http.StatusInternalServerError, "database error")
	case full:
		d.log.limited("push-full", time.Minute, "dashboard: push from %s refused: %d machines already", id.Machine, peersMax)
		httpError(w, http.StatusTooManyRequests, fmt.Sprintf("the hub already shows %d machines", peersMax))
	default:
		httpJSON(w, http.StatusOK, reply)
	}
}

// capState trims a pushed snapshot to the caps readState applies.
func capState(s *dashState) {
	notes := []ownerNote{}
	counts := map[int64]int{}
	for _, n := range s.OwnerNotes {
		if counts[n.RootID] < stateOwnerNotesMax {
			n.Text = clip(n.Text, ownerNoteTextMax)
			notes = append(notes, n)
			counts[n.RootID]++
		}
	}
	s.OwnerNotes = notes
	s.OwnerAsks = capList(s.OwnerAsks, stateAsksMax)
	s.Orchestrators = capList(s.Orchestrators, stateRootsMax)
	s.Activity = capList(s.Activity, stateActivityMax)
	s.Closed = capList(s.Closed, stateClosedMax)
	// v0.7: an older peer sends no milestones (empty here) and no daemon
	// (nil); a pushed attention list is dropped, since the hub derives it.
	s.Milestones = capList(s.Milestones, stateMilestonesMax)
	for i := range s.Milestones {
		s.Milestones[i].Text = clip(s.Milestones[i].Text, stateSummaryMax)
	}
	s.Attention = []attentionItem{}
	for i := range s.OwnerAsks {
		s.OwnerAsks[i].Text = clip(s.OwnerAsks[i].Text, stateAskTextMax)
	}
	for i := range s.Orchestrators {
		o := &s.Orchestrators[i]
		o.Tasks = capList(o.Tasks, stateTasksMax)
		// v0.6 fields stay absent (nil) when an older peer did not send them.
		if len(o.Decisions) > stateDecisionMax {
			o.Decisions = o.Decisions[len(o.Decisions)-stateDecisionMax:]
		}
		if len(o.Refs) > refKeysMax {
			o.Refs = o.Refs[:refKeysMax]
		}
		for j := range o.Tasks {
			if len(o.Tasks[j].Refs) > refKeysMax {
				o.Tasks[j].Refs = o.Tasks[j].Refs[:refKeysMax]
			}
		}
		if len(o.Unread) > stateUnreadMax {
			o.Unread = o.Unread[:stateUnreadMax]
		}
	}
}

func capList[T any](l []T, n int) []T {
	if l == nil {
		return []T{}
	}
	return l[:min(len(l), n)]
}

// walkTimes calls f on every timestamp in s, with its age field when it has
// one.
func walkTimes(s *dashState, f func(at *string, age *int64)) {
	le := func(e *lastEvent) {
		if e != nil {
			f(&e.At, &e.AgeMS)
		}
	}
	note := func(n *stamped) {
		if n != nil {
			f(&n.At, &n.AgeMS)
		}
	}
	for i := range s.OwnerAsks {
		f(&s.OwnerAsks[i].At, &s.OwnerAsks[i].AgeMS)
	}
	for i := range s.OwnerNotes {
		f(&s.OwnerNotes[i].At, &s.OwnerNotes[i].AgeMS)
	}
	for i := range s.Orchestrators {
		o := &s.Orchestrators[i]
		f(&o.CreatedAt, nil)
		note(o.Note)
		le(o.LastEvent)
		note(o.Next)
		note(o.LastHandover)
		for j := range o.Decisions {
			f(&o.Decisions[j].At, &o.Decisions[j].AgeMS)
		}
		for j := range o.Tasks {
			t := &o.Tasks[j]
			if t.ObservedAt != "" {
				f(&t.ObservedAt, nil)
			}
			le(t.LastEvent)
			note(t.Next)
		}
		for j := range o.Unread {
			f(&o.Unread[j].At, &o.Unread[j].AgeMS)
		}
	}
	if s.Daemon != nil && s.Daemon.At != "" {
		f(&s.Daemon.At, &s.Daemon.AgeMS)
	}
	for i := range s.Milestones {
		f(&s.Milestones[i].At, &s.Milestones[i].AgeMS)
	}
	for i := range s.Attention {
		if s.Attention[i].Since != "" {
			f(&s.Attention[i].Since, &s.Attention[i].AgeMS)
		}
	}
	for i := range s.Activity {
		f(&s.Activity[i].At, &s.Activity[i].AgeMS)
	}
	for i := range s.Closed {
		f(&s.Closed[i].ClosedAt, &s.Closed[i].AgeMS)
		note(s.Closed[i].Note)
	}
}

// rebase moves a peer snapshot onto the hub's clock: each timestamp shifts
// by received_at - peer_now, and each age is recomputed at the hub's time.
// A skewed peer clock then neither ages nor rejuvenates its tasks.
func rebase(s *dashState, peerNow, receivedAt, at time.Time) {
	offset := receivedAt.Sub(peerNow)
	walkTimes(s, func(ts *string, age *int64) {
		t := parseTime(*ts)
		if t.IsZero() {
			return
		}
		t = t.Add(offset)
		*ts = stamp(t)
		if age != nil {
			*age = max(0, at.Sub(t).Milliseconds())
		}
	})
	s.Now = stamp(at)
}

type machineView struct {
	Machine       string     `json:"machine"`
	NodeID        string     `json:"node_id,omitempty"`
	Local         bool       `json:"local"`
	LastPushAgeMS int64      `json:"last_push_age_ms"`
	Stale         bool       `json:"stale"`
	State         *dashState `json:"state"`
}

type needAsk struct {
	ownerAsk
	Machine string `json:"machine"`
	NodeID  string `json:"node_id,omitempty"`
	Local   bool   `json:"local"`
	Stale   bool   `json:"stale"`
}

// stateView is /api/state: the v0.4 fields for this machine, then every
// machine (this one first) and the owner asks of all of them. Attention and Milestones (v0.7) are every machine's,
// merged; they shadow this machine's own lists, which stay under machines.
type stateView struct {
	dashState
	Machine    string          `json:"machine"`
	Hub        bool            `json:"hub"`
	Machines   []machineView   `json:"machines"`
	NeedsYou   []needAsk       `json:"needs_you"`
	Attention  []attentionItem `json:"attention"`
	Milestones []milestone     `json:"milestones"`
}

// view adds the machines to this machine's state: on the hub, each peer's
// latest snapshot pushed in the last peerDropAfter, rebased to this clock.
func (d *dashboard) view(cx context.Context, s *dashState, at time.Time) (*stateView, error) {
	hub, machine := d.hub.Load(), d.machine
	if hub != nil {
		machine = hub.self.Short
	}
	v := &stateView{dashState: *s, Machine: machine, Hub: hub != nil,
		Machines: []machineView{{Machine: machine, Local: true, State: s}}}
	if hub != nil {
		if err := d.peerViews(cx, v, at); err != nil {
			return nil, err
		}
	}
	v.Attention, v.Milestones = []attentionItem{}, []milestone{}
	for _, m := range v.Machines {
		derive(m.State, machineCtx{Name: m.Machine, NodeID: m.NodeID, Local: m.Local, Stale: m.Stale,
			LastPushAgeMS: m.LastPushAgeMS}, at)
		for _, a := range m.State.OwnerAsks {
			v.NeedsYou = append(v.NeedsYou, needAsk{ownerAsk: a, Machine: m.Machine, NodeID: m.NodeID, Local: m.Local, Stale: m.Stale})
		}
		v.Attention = append(v.Attention, m.State.Attention...)
		v.Milestones = append(v.Milestones, m.State.Milestones...)
	}
	if v.NeedsYou == nil {
		v.NeedsYou = []needAsk{}
	}
	sort.SliceStable(v.NeedsYou, func(i, j int) bool { return v.NeedsYou[i].At < v.NeedsYou[j].At })
	sortAttention(v.Attention)
	sort.SliceStable(v.Milestones, func(i, j int) bool { return v.Milestones[i].AgeMS < v.Milestones[j].AgeMS })
	v.Milestones = capList(v.Milestones, stateMilestonesMax)
	return v, nil
}

func (d *dashboard) peerViews(cx context.Context, v *stateView, at time.Time) error {
	tx, err := d.db.BeginTx(cx, &sql.TxOptions{ReadOnly: true})
	if err != nil {
		return err
	}
	defer tx.Rollback()
	rows, err := tx.Query(`select node_id, machine, snapshot, peer_now, received_at from peers
		where received_at > ? order by machine, node_id limit ?`, stamp(at.Add(-peerDropAfter)), peersMax)
	if err != nil {
		return err
	}
	for rows.Next() {
		var m machineView
		var snap, peerNow, received string
		if err := rows.Scan(&m.NodeID, &m.Machine, &snap, &peerNow, &received); err != nil {
			rows.Close()
			return err
		}
		var s dashState
		if err := json.Unmarshal([]byte(snap), &s); err != nil {
			d.log.limited("snapshot:"+m.NodeID, time.Minute, "dashboard: stored snapshot of %s unreadable: %v", m.Machine, err)
			continue
		}
		if s.OwnerNotes == nil {
			s.OwnerNotes = []ownerNote{}
		}
		rt := parseTime(received)
		// The host's daemon now observes to the hub as a client; its old
		// peer snapshot is history, shown nowhere, but never deleted.
		if hb, ok, err := getMeta(tx, hostHeartbeatKey(m.Machine)); err != nil {
			rows.Close()
			return err
		} else if ok && parseTime(hb).After(rt) {
			continue
		}
		rebase(&s, parseTime(peerNow), rt, at)
		m.State, m.LastPushAgeMS = &s, max(0, at.Sub(rt).Milliseconds())
		m.Stale = at.Sub(rt) > peerStaleAfter
		v.Machines = append(v.Machines, m)
	}
	rows.Close()
	return rows.Err()
}
