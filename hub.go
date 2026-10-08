package main

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"strings"
	"time"
)

// The hub: the machine whose dashboard.addr says "tailnet". Its server also
// listens on its Tailscale address, where the client hosts' CLIs and daemons
// reach the one ledger over /api/rpc. Every request from a non-loopback
// address is admitted only by `tailscale whois`: the node must belong to the
// hub's own user and carry no tags.

const hubURLKey = "hub_tailnet_url"

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

// localMachine is this host's short name.
func localMachine() string {
	h, err := os.Hostname()
	if err != nil || h == "" {
		return "this machine"
	}
	return machineLabel(h)
}

// startTailnet makes this server the hub: it tries now, and while
// Tailscale or a tailnet address is unavailable it keeps retrying in the
// background with capped backoff until every address is bound or the
// server stops. Loopback and the event bridge never wait on it.
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
