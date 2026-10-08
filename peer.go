package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net"
	"net/http"
	"net/http/httptrace"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"time"
)

// A peer: a machine whose <stateDir>/hub.url names the hub. Its daemon
// pushes its own /api/state there after each pass that changed it and at
// least every peerPushEvery. It only reads this ledger: the hub shows the
// state and writes nothing back (a v0.6 hub's queued answers are ignored;
// owner asks are answered in the orchestrator's pane). The pusher is a
// passenger like the dashboard: it never touches the event bridge or the
// heartbeat, and an unavailable hub costs only backoff.

const (
	hubURLFile       = "hub.url"
	peerOKKey        = "peer_last_push_ok_at"
	peerErrKey       = "peer_last_push_error"
	peerReplyMax     = 1 << 20
	peerHubNodeMax   = 64
	peerURLErrPrefix = "hub.url refused: "
)

// Peer timing; package variables so tests can shrink them.
var (
	peerPushEvery   = 15 * time.Second
	peerCheckEvery  = 2 * time.Second
	peerTimeout     = 5 * time.Second
	peerBackoffBase = time.Second
	peerBackoffCap  = 60 * time.Second
)

// readHubURL returns the one line of <stateDir>/hub.url, or "" without one.
func readHubURL(dir string) (string, error) {
	b, err := os.ReadFile(filepath.Join(dir, hubURLFile))
	if errors.Is(err, fs.ErrNotExist) {
		return "", nil
	} else if err != nil {
		return "", err
	}
	v, _, _ := strings.Cut(string(b), "\n")
	return strings.TrimSpace(v), nil
}

// checkHubURL accepts only http://HOST:PORT[/] where HOST is a 100.64.0.0/10
// address or a MagicDNS name under the tailnet suffix (needed only for a
// name). It returns the push endpoint.
func checkHubURL(raw string, suffix func() (string, error)) (string, error) {
	u, err := url.Parse(raw)
	if err != nil || u.Scheme != "http" || u.User != nil || u.Opaque != "" || u.RawQuery != "" || u.Fragment != "" ||
		(u.Path != "" && u.Path != "/") {
		return "", fmt.Errorf("%q is not http://HOST:PORT", raw)
	}
	host, port := u.Hostname(), u.Port()
	if host == "" || port == "" {
		return "", fmt.Errorf("%q needs a host and a port", raw)
	}
	if ip := net.ParseIP(host); ip != nil {
		if !inTailnet(ip) {
			return "", fmt.Errorf("%s is not a tailnet address (100.64.0.0/10)", host)
		}
	} else {
		sfx, err := suffix()
		if err != nil {
			return "", fmt.Errorf("cannot check %s against the tailnet: %v", host, err)
		}
		name := strings.ToLower(strings.TrimSuffix(host, "."))
		if !strings.HasSuffix(name, "."+sfx) || strings.Count(name, ".") != strings.Count(sfx, ".")+1 {
			return "", fmt.Errorf("%s is not a MagicDNS name under %s", host, sfx)
		}
	}
	return "http://" + u.Host + peerPushPath, nil
}

func tailnetSuffix() (string, error) {
	s, err := tailscaleIdentity()
	if err != nil {
		return "", err
	}
	return s.Suffix, nil
}

type pusher struct {
	db       *sql.DB
	log      *daemonLog
	raw      string      // hub.url as written
	target   string      // the push endpoint, once hub.url passed its check
	hostPort string      // its host:port, dialled by push itself
	hubWhois *whoisCache // admits the hub: this machine's user, no tags
	hubNode  string      // the stable id of the last verified hub
	client   *http.Client
	kick     chan struct{}

	lastDigest [32]byte
	lastOK     time.Time
	fails      int
}

// verifiedConn is a connection to a hub that whois admitted before any
// request byte was written on it.
type verifiedConn struct {
	net.Conn
	id     peerIdent
	handed atomic.Bool // given to the transport; a connection serves one request
}

type verifiedConnKey struct{}

// transportDial hands the transport the connection push dialled and
// verified for this request, once. The transport never dials on its own,
// so no request can go out on an unverified or reused connection.
func transportDial(cx context.Context, _, _ string) (net.Conn, error) {
	if vc, ok := cx.Value(verifiedConnKey{}).(*verifiedConn); ok && vc.handed.CompareAndSwap(false, true) {
		return vc, nil
	}
	return nil, errors.New("no verified hub connection for this request")
}

// dial connects to the hub, then runs whois on the address it actually
// reached and refuses it unless it is a tailnet node of this machine's user
// without tags. Nothing is sent to a hub that fails this.
func (p *pusher) dial(cx context.Context, network, addr string) (net.Conn, error) {
	var nd net.Dialer
	c, err := nd.DialContext(cx, network, addr)
	if err != nil {
		return nil, err
	}
	ta, _ := c.RemoteAddr().(*net.TCPAddr)
	if ta == nil || !(inTailnet(ta.IP) || inTailnet6(ta.IP)) {
		c.Close()
		return nil, fmt.Errorf("hub address %v is not on the tailnet", c.RemoteAddr())
	}
	id, err := p.hubWhois.admit(ta.IP)
	if err != nil {
		c.Close()
		return nil, fmt.Errorf("hub not verified: %v", err)
	}
	return &verifiedConn{Conn: c, id: id}, nil
}

// newPusher reads hub.url; it returns nil without one, and nil after one log
// line when the file is unreadable or the URL is refused.
func newPusher(db *sql.DB, lg *daemonLog, dir string) *pusher {
	raw, err := readHubURL(dir)
	if err != nil {
		lg.logf("peer: %v; not pushing", err)
		return nil
	}
	if raw == "" {
		return nil
	}
	p := &pusher{db: db, log: lg, raw: raw, kick: make(chan struct{}, 1)}
	// No proxy from the inherited environment and no redirects: the push
	// goes straight to the verified tailnet address in hub.url.
	// Keep-alives are off: every push dials and passes whois (through its
	// bounded cache) before a byte is sent, so a hub that stops being
	// admitted is refused on the next push, not kept on an open connection.
	p.client = &http.Client{Timeout: peerTimeout, Transport: &http.Transport{Proxy: nil, DialContext: transportDial,
		DisableKeepAlives: true, TLSHandshakeTimeout: peerTimeout, ResponseHeaderTimeout: peerTimeout},
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	return p
}

// poke asks for a push when the state may have changed; it never blocks.
func (p *pusher) poke() {
	select {
	case p.kick <- struct{}{}:
	default:
	}
}

// run pushes until cx ends: at start, on each poke, and every peerPushEvery;
// after a failure it waits out a capped exponential backoff, ignoring pokes.
func (p *pusher) run(cx context.Context) {
	// The hub is checked against this machine's Tailscale identity (the
	// suffix for a name, the user for whois); until Tailscale answers, the
	// check is retried like a failed push. A malformed URL is refused at once.
	for p.target == "" {
		var self *tsSelf
		suffix := func() (string, error) {
			s, err := tailscaleIdentity()
			if err != nil {
				return "", err
			}
			self = s
			return s.Suffix, nil
		}
		target, err := checkHubURL(p.raw, suffix)
		if err != nil && !strings.HasPrefix(err.Error(), "cannot check") {
			p.log.logf("peer: %s%v; not pushing", peerURLErrPrefix, err)
			setMeta(p.db, peerErrKey, now()+" "+peerURLErrPrefix+err.Error())
			return
		}
		if err == nil && self == nil { // an IP needs no suffix, but whois needs the user
			if self, err = tailscaleIdentity(); err != nil {
				err = fmt.Errorf("cannot check the hub without Tailscale: %v", err)
			}
		}
		if err == nil {
			u, _ := url.Parse(target)
			p.target, p.hostPort, p.hubWhois = target, u.Host, newWhoisCache(self.Login)
			p.hubWhois.arg = hubWhoisArg
			p.log.logf("peer: pushing to %s", p.raw)
			break
		}
		p.failed(err)
		if !sleepCx(cx, p.backoff()) {
			return
		}
	}
	force, due := true, clockNow()
	for {
		if cx.Err() != nil {
			return
		}
		// Between forced pushes, a cheap check every peerCheckEvery sends a
		// changed state: CLI writes (a new ask, a note) cause no pass.
		wait, check := time.Until(due), false
		if p.fails == 0 && wait > peerCheckEvery {
			wait, check = peerCheckEvery, true
		}
		if wait > 0 {
			t := time.NewTimer(wait)
			select {
			case <-cx.Done():
				t.Stop()
				return
			case <-t.C:
				force = !check
			case <-p.kick:
				t.Stop()
				if p.fails > 0 {
					continue // a poke does not shorten a backoff
				}
			}
		}
		// Whatever woke the loop (a check timer that fired late on a busy
		// or just-woken machine, a poke), a push that is due is forced.
		// Otherwise an unchanged state skips it, due stays in the past, and
		// the loop spins on skipped pushes without ever pushing or waiting.
		if !clockNow().Before(due) {
			force = true
		}
		sent := p.push(force)
		force = false
		switch {
		case p.fails > 0:
			due = clockNow().Add(p.backoff())
		case sent:
			due = clockNow().Add(peerPushEvery)
		}
	}
}

func sleepCx(cx context.Context, d time.Duration) bool {
	if d <= 0 {
		return cx.Err() == nil
	}
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-cx.Done():
		return false
	case <-t.C:
		return true
	}
}

func (p *pusher) backoff() time.Duration {
	return min(peerBackoffCap, peerBackoffBase<<min(max(p.fails-1, 0), 6))
}

func (p *pusher) failed(err error) {
	p.fails++
	msg := truncate(err.Error(), 300)
	key := "push-fail" // an unverified hub is logged apart from an unreachable one
	if strings.Contains(msg, "hub not verified") || strings.Contains(msg, "whois says the hub") {
		key = "push-verify"
	}
	p.log.limited(key, time.Minute, "peer: push failed: %s", msg)
	if err := setMeta(p.db, peerErrKey, now()+" "+msg); err != nil {
		p.log.limited("push-meta", time.Minute, "peer: meta write failed: %v", err)
	}
}

// digest fingerprints a state without its clock and ages, so an unchanged
// ledger is not re-sent on every pass.
func digest(s *dashState) [32]byte {
	c := *s
	c.Now = ""
	b, _ := json.Marshal(c)
	var z dashState
	json.Unmarshal(b, &z)
	walkTimes(&z, func(_ *string, age *int64) {
		if age != nil {
			*age = 0
		}
	})
	b, _ = json.Marshal(z)
	return sha256.Sum256(b)
}

// push sends one snapshot unless nothing changed and force is false. It
// returns whether it sent one that the hub took.
func (p *pusher) push(force bool) bool {
	at := clockNow()
	s, err := readState(context.Background(), p.db, at)
	if err != nil {
		p.failed(fmt.Errorf("reading state: %v", err))
		return false
	}
	dg := digest(s)
	if !force && dg == p.lastDigest && p.fails == 0 {
		return false
	}
	dialCx, cancel := context.WithTimeout(context.Background(), peerTimeout)
	conn, err := p.dial(dialCx, "tcp", p.hostPort)
	cancel()
	if err != nil {
		p.failed(fmt.Errorf("connect %s: %w", p.hostPort, err))
		return false
	}
	vc := conn.(*verifiedConn)
	defer vc.Close()
	verified := vc.id
	state, _ := json.Marshal(s)
	body, _ := json.Marshal(pushBody{State: state, Now: at.UTC().Format(time.RFC3339Nano)})
	usedOurs := false
	trace := &httptrace.ClientTrace{GotConn: func(info httptrace.GotConnInfo) { usedOurs = info.Conn == net.Conn(vc) }}
	cx := context.WithValue(httptrace.WithClientTrace(context.Background(), trace), verifiedConnKey{}, vc)
	req, _ := http.NewRequestWithContext(cx, http.MethodPost, p.target, bytes.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	resp, err := p.client.Do(req)
	if err != nil {
		p.failed(err)
		return false
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, peerReplyMax+1))
	if err != nil || len(raw) > peerReplyMax {
		p.failed(fmt.Errorf("hub reply unreadable or too large"))
		return false
	}
	if resp.StatusCode != http.StatusOK {
		var e struct {
			Error string `json:"error"`
		}
		json.Unmarshal(raw, &e)
		p.failed(fmt.Errorf("hub answered %d %s", resp.StatusCode, truncate(e.Error, 200)))
		return false
	}
	var reply pushReply
	if err := json.Unmarshal(raw, &reply); err != nil || reply.Hub.NodeID == "" || len(reply.Hub.NodeID) > peerHubNodeMax {
		p.failed(fmt.Errorf("hub reply is not a push reply"))
		return false
	}
	switch {
	case !usedOurs:
		p.failed(fmt.Errorf("the request did not use the verified hub connection"))
		return false
	case reply.Hub.NodeID != verified.NodeID:
		p.failed(fmt.Errorf("hub reply names node %s, but whois says the hub is %s (%s)",
			truncate(reply.Hub.NodeID, peerHubNodeMax), verified.NodeID, verified.Machine))
		return false
	}
	if p.fails > 0 {
		p.log.logf("peer: push ok again after %d failures", p.fails)
	}
	if verified.NodeID != p.hubNode {
		p.log.logf("peer: hub verified as %s (%s)", verified.Machine, verified.NodeID)
	}
	p.fails, p.lastDigest, p.lastOK, p.hubNode = 0, dg, at, verified.NodeID
	if err := setMeta(p.db, peerOKKey, stamp(at)); err != nil {
		p.log.limited("push-meta", time.Minute, "peer: meta write failed: %v", err)
	}
	p.db.Exec(`delete from meta where key = ?`, peerErrKey)
	return true
}
