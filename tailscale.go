package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Tailscale is read, never driven: taskr runs only `tailscale ip -4`,
// `tailscale status --json` and `tailscale whois --json <ip>`, each from an
// argument array with a deadline. Identity comes from these answers alone;
// no file or request can name a machine.

// tailscaleFallbacks are tried when `tailscale` is not on PATH (the daemon
// inherits the Herdr server's environment). Tests clear it.
var tailscaleFallbacks = []string{"/usr/bin/tailscale", "/usr/local/bin/tailscale",
	"/Applications/Tailscale.app/Contents/MacOS/Tailscale"}

var tailscaleTimeout = 3 * time.Second

const tailscaleOutMax = 4 << 20

// cgnat is Tailscale's IPv4 range, 100.64.0.0/10; tsULA its IPv6 range,
// fd7a:115c:a1e0::/48.
var (
	cgnat = &net.IPNet{IP: net.IPv4(100, 64, 0, 0).To4(), Mask: net.CIDRMask(10, 32)}
	tsULA = &net.IPNet{IP: net.ParseIP("fd7a:115c:a1e0::"), Mask: net.CIDRMask(48, 128)}
)

// Test hooks: which addresses count as the tailnet, which remote addresses
// skip whois as loopback, and the whois argument a peer uses for its hub.
// Production uses the Tailscale ranges, the IP loopback and the plain IP;
// tests bind a fake tailnet on ::1, treat only 127.0.0.1 as loopback (so
// requests over ::1 go through whois), and name the hub apart from the
// peer, since both ends of a one-host test are ::1.
var (
	inTailnet    = func(ip net.IP) bool { return cgnat.Contains(ip) }
	inTailnet6   = func(ip net.IP) bool { return ip.To4() == nil && tsULA.Contains(ip) }
	loopbackPeer = func(ip net.IP) bool { return ip.IsLoopback() }
	hubWhoisArg  = func(ip net.IP) string { return ip.String() }
)

func tailscaleBin() (string, error) {
	if p, err := exec.LookPath("tailscale"); err == nil {
		return p, nil
	}
	for _, p := range tailscaleFallbacks {
		if st, err := os.Stat(p); err == nil && st.Mode().IsRegular() && st.Mode()&0o111 != 0 {
			return p, nil
		}
	}
	return "", errors.New("tailscale not found on PATH or in the usual places")
}

// tailscaleOut runs one tailscale command and returns its stdout. Stderr is
// dropped; a non-zero exit, a timeout, or oversized output is an error.
func tailscaleOut(args ...string) ([]byte, error) {
	bin, err := tailscaleBin()
	if err != nil {
		return nil, err
	}
	cx, cancel := context.WithTimeout(context.Background(), tailscaleTimeout)
	defer cancel()
	cmd := exec.CommandContext(cx, bin, args...)
	cmd.WaitDelay = herdrWaitDelay
	var out capBuffer
	cmd.Stdout = &out
	err = cmd.Run()
	if cx.Err() != nil {
		return nil, fmt.Errorf("tailscale %s: timed out after %v", args[0], tailscaleTimeout)
	}
	if err != nil {
		return nil, fmt.Errorf("tailscale %s: %v", args[0], err)
	}
	if out.over {
		return nil, fmt.Errorf("tailscale %s: output too large", args[0])
	}
	return out.Bytes(), nil
}

// capBuffer keeps at most tailscaleOutMax bytes.
type capBuffer struct {
	bytes.Buffer
	over bool
}

func (b *capBuffer) Write(p []byte) (int, error) {
	if b.Len()+len(p) > tailscaleOutMax {
		b.over = true
		return len(p), nil
	}
	return b.Buffer.Write(p)
}

// tsSelf is this machine as Tailscale sees it.
type tsSelf struct {
	IP      net.IP // from `tailscale ip -4`, inside the tailnet range
	IP6     net.IP // Self's address in fd7a:115c:a1e0::/48, or nil
	NodeID  string // stable node id
	DNSName string // full MagicDNS name, no trailing dot
	Short   string // its first label: the machine label
	Suffix  string // the tailnet's MagicDNS suffix
	Login   string // the owning user's login name
}

type tsStatus struct {
	MagicDNSSuffix string
	Self           *struct {
		ID           string
		DNSName      string
		UserID       int64
		TailscaleIPs []string
	}
	User map[string]struct {
		LoginName string
	}
}

// tailscaleIdentity reads this node's user, name and suffix from `status`.
func tailscaleIdentity() (*tsSelf, error) {
	b, err := tailscaleOut("status", "--json")
	if err != nil {
		return nil, err
	}
	var st tsStatus
	if err := json.Unmarshal(b, &st); err != nil {
		return nil, fmt.Errorf("tailscale status: %v", err)
	}
	if st.Self == nil || st.Self.ID == "" || st.Self.DNSName == "" {
		return nil, errors.New("tailscale status: no Self node (logged out?)")
	}
	u, ok := st.User[strconv.FormatInt(st.Self.UserID, 10)]
	if !ok || u.LoginName == "" {
		return nil, errors.New("tailscale status: Self user not in User")
	}
	s := &tsSelf{NodeID: st.Self.ID, DNSName: strings.ToLower(strings.TrimSuffix(st.Self.DNSName, ".")), Login: u.LoginName,
		Suffix: strings.ToLower(strings.Trim(st.MagicDNSSuffix, "."))}
	for _, a := range st.Self.TailscaleIPs {
		if ip := net.ParseIP(a); ip != nil && !ip.IsUnspecified() && inTailnet6(ip) {
			s.IP6 = ip
			break
		}
	}
	s.Short, _, _ = strings.Cut(s.DNSName, ".")
	if s.Suffix == "" {
		_, s.Suffix, _ = strings.Cut(s.DNSName, ".")
	}
	if s.Short == "" || s.Suffix == "" {
		return nil, fmt.Errorf("tailscale status: cannot read a MagicDNS name from %q", st.Self.DNSName)
	}
	return s, nil
}

// tailscaleSelf is tailscaleIdentity plus this node's tailnet IPv4.
func tailscaleSelf() (*tsSelf, error) {
	b, err := tailscaleOut("ip", "-4")
	if err != nil {
		return nil, err
	}
	first, _, _ := strings.Cut(strings.TrimSpace(string(b)), "\n")
	ip := net.ParseIP(strings.TrimSpace(first))
	if ip == nil || !inTailnet(ip) {
		return nil, fmt.Errorf("tailscale ip -4: %q is not a tailnet address", truncate(first, 60))
	}
	s, err := tailscaleIdentity()
	if err != nil {
		return nil, err
	}
	s.IP = ip
	return s, nil
}

// machineLabel is a node name without the tailnet suffix: its first label.
func machineLabel(name string) string {
	short, _, _ := strings.Cut(strings.ToLower(strings.TrimSuffix(name, ".")), ".")
	return clip(short, 63)
}

// peerIdent is a whois answer that was admitted.
type peerIdent struct {
	NodeID  string
	Machine string
	Login   string
}

type whoisEntry struct {
	id  peerIdent
	err error
	at  time.Time
}

var whoisTTL = 60 * time.Second

const whoisCacheMax = 1024

// whoisCache admits a remote tailnet address when its node belongs to the
// hub's own user and carries no tags. Answers are cached per IP for
// whoisTTL, admissions and refusals in separate maps.
type whoisCache struct {
	login string
	arg   func(net.IP) string // the whois argument for an IP
	mu    sync.Mutex
	ok    map[string]whoisEntry
	bad   map[string]whoisEntry
}

func newWhoisCache(login string) *whoisCache {
	return &whoisCache{login: login, arg: net.IP.String, ok: map[string]whoisEntry{}, bad: map[string]whoisEntry{}}
}

type tsWhois struct {
	Node *struct {
		StableID string
		Name     string
		Tags     []string
	}
	UserProfile *struct {
		LoginName string
	}
}

// admit returns the identity behind ip, or why it is refused.
func (c *whoisCache) admit(ip net.IP) (peerIdent, error) {
	key := ip.String()
	c.mu.Lock()
	for _, m := range []map[string]whoisEntry{c.ok, c.bad} {
		if e, ok := m[key]; ok {
			if clockNow().Sub(e.at) < whoisTTL {
				c.mu.Unlock()
				return e.id, e.err
			}
			delete(m, key)
		}
	}
	c.mu.Unlock()
	id, err := c.lookup(c.arg(ip))
	c.mu.Lock()
	m := c.ok
	if err != nil {
		m = c.bad
	}
	if len(m) >= whoisCacheMax {
		clear(m)
	}
	m[key] = whoisEntry{id: id, err: err, at: clockNow()}
	c.mu.Unlock()
	return id, err
}

func (c *whoisCache) lookup(ip string) (peerIdent, error) {
	b, err := tailscaleOut("whois", "--json", ip)
	if err != nil {
		return peerIdent{}, err
	}
	var w tsWhois
	if err := json.Unmarshal(b, &w); err != nil {
		return peerIdent{}, fmt.Errorf("whois %s: bad JSON", ip)
	}
	switch {
	case w.Node == nil || w.UserProfile == nil || w.Node.StableID == "" || w.Node.Name == "":
		return peerIdent{}, fmt.Errorf("whois %s: incomplete answer", ip)
	case len(w.Node.Tags) > 0:
		return peerIdent{}, fmt.Errorf("whois %s: tagged node %s", ip, machineLabel(w.Node.Name))
	case w.UserProfile.LoginName != c.login:
		return peerIdent{}, fmt.Errorf("whois %s: node %s belongs to another user", ip, machineLabel(w.Node.Name))
	}
	return peerIdent{NodeID: clip(w.Node.StableID, 64), Machine: machineLabel(w.Node.Name), Login: w.UserProfile.LoginName}, nil
}
