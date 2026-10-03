package main

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// fakeTailscale answers from files next to it: ts.ip for `ip -4`,
// ts.status for `status --json`, ts.whois.<ip> for `whois --json <ip>`
// (with ts.whois.<ip>.sleep and ts.whois.<ip>.exit). A missing file is a
// failure, like a stopped tailscaled. Any other command is refused.
const fakeTailscale = `#!/bin/sh
d="$(dirname "$0")"
printf '%s|' "$@" >> "$d/ts.calls"
echo >> "$d/ts.calls"
case "$1" in
ip)
  [ "$2" = "-4" ] && [ -f "$d/ts.ip" ] && cat "$d/ts.ip" && exit 0
  echo "not running" >&2; exit 1 ;;
status)
  [ "$2" = "--json" ] && [ -f "$d/ts.status" ] && cat "$d/ts.status" && exit 0
  echo "not running" >&2; exit 1 ;;
whois)
  f="$d/ts.whois.$3"
  [ "$2" = "--json" ] || exit 2
  [ -f "$f.sleep" ] && sleep "$(cat "$f.sleep")"
  [ -f "$f.exit" ] && exit "$(cat "$f.exit")"
  [ -f "$f" ] && cat "$f" && exit 0
  echo "no match for IP:port" >&2; exit 1 ;;
*)
  echo "refused" >&2; exit 2 ;;
esac
`

const (
	owner    = "owner@example.com"
	hubIP    = "100.64.0.10"
	hostAIP  = "100.64.0.11"
	otherIP  = "100.100.1.1"
	taggedIP = "100.100.1.2"
	errIP    = "100.100.1.3"
	slowIP   = "100.100.1.4"
	junkIP   = "100.100.1.5"
	// taggedOwnerIP is the owner's own node with a tag: only the tag guard
	// refuses it.
	taggedOwnerIP = "100.100.1.6"
	hubNode       = "nHUB1CNTRL"
	hostANode     = "nHOSTA1CNTRL"
)

func whoisJSON(stable, name, login string, tags []string) string {
	b, _ := json.Marshal(map[string]any{
		"Node":        map[string]any{"ID": 3066361403632065, "StableID": stable, "Name": name, "Tags": tags},
		"UserProfile": map[string]any{"ID": 8506513157120998, "LoginName": login, "DisplayName": "x"},
		"CapMap":      map[string]any{}})
	return string(b)
}

// tailnet writes the fake tailscale's answers into each harness's bin
// (whichever is first on PATH answers): this machine is hub at ip.
func tailnet(ip string, hs ...*harness) { tailnetWith(ip, "", hs...) }

// tailnetWith also lists ip6 among Self's TailscaleIPs.
func tailnetWith(ip, ip6 string, hs ...*harness) {
	ips := []string{}
	if net.ParseIP(ip).To4() != nil {
		ips = append(ips, ip)
	}
	if ip6 != "" {
		ips = append(ips, ip6)
	}
	status, _ := json.Marshal(map[string]any{
		"MagicDNSSuffix": "example.ts.net",
		"Self": map[string]any{"ID": hubNode, "HostName": "hub", "DNSName": "hub.example.ts.net.",
			"UserID": int64(8506513157120998), "TailscaleIPs": ips},
		"User": map[string]any{"8506513157120998": map[string]any{"ID": int64(8506513157120998), "LoginName": owner}},
	})
	hostA := whoisJSON(hostANode, "host-a.example.ts.net.", owner, nil)
	files := map[string]string{
		"ts.ip": ip + "\n", "ts.status": string(status),
		"ts.whois." + hubIP:             whoisJSON(hubNode, "hub.example.ts.net.", owner, nil),
		"ts.whois." + hostAIP:           hostA,
		"ts.whois.::1":                  hostA,                                                 // the test peer, over the fake tailnet
		"ts.whois.hub-::1":              whoisJSON(hubNode, "hub.example.ts.net.", owner, nil), // the peer asking about its hub
		"ts.whois." + taggedOwnerIP:     whoisJSON("nTAGOWN", "ci-runner.example.ts.net.", owner, []string{"tag:ci"}),
		"ts.whois." + otherIP:           whoisJSON("nOTHER", "host-c.example.ts.net.", "user@example.com", nil),
		"ts.whois." + taggedIP:          whoisJSON("nTAG", "ci-runner.example.ts.net.", "tagged-devices", []string{"tag:ci"}),
		"ts.whois." + errIP + ".exit":   "1",
		"ts.whois." + slowIP + ".sleep": "5",
		"ts.whois." + slowIP:            hostA,
		"ts.whois." + junkIP:            "{not json",
	}
	for _, h := range hs {
		for k, v := range files {
			h.write(k, v, 0o644)
		}
	}
}

func (h *harness) tsCalls(prefix string) int {
	b, _ := os.ReadFile(filepath.Join(h.bin, "ts.calls"))
	n := 0
	for _, l := range strings.Split(string(b), "\n") {
		if strings.HasPrefix(l, prefix) {
			n++
		}
	}
	return n
}

// fakeTailnetHooks makes ::1 a tailnet address that goes through whois, so
// two daemons on one host can play hub and peer. Tests only.
func fakeTailnetHooks(t *testing.T) {
	in, lo, arg := inTailnet, loopbackPeer, hubWhoisArg
	inTailnet = func(ip net.IP) bool { return cgnat.Contains(ip) || ip.Equal(net.IPv6loopback) }
	loopbackPeer = func(ip net.IP) bool { return ip.Equal(net.IPv4(127, 0, 0, 1)) }
	hubWhoisArg = func(ip net.IP) string { return "hub-" + ip.String() }
	t.Cleanup(func() { inTailnet, loopbackPeer, hubWhoisArg = in, lo, arg })
}

func setVar[T any](t *testing.T, p *T, v T) {
	old := *p
	*p = v
	t.Cleanup(func() { *p = old })
}

// hubDash is a hub dashboard over h's ledger, bound (on paper) at
// 127.0.0.1:7788 and 100.64.0.10:7788.
func (h *harness) hubDash() *dashboard {
	h.t.Helper()
	tailnet(hubIP, h)
	self, err := tailscaleSelf()
	if err != nil {
		h.t.Fatal(err)
	}
	d := newDashboard(h.openDB(), openDaemonLog(filepath.Join(h.stateDir(), "daemon.log")), testAddr)
	d.enableHub(self, "7788")
	return d
}

func from(ip string, host string) func(*http.Request) {
	return func(r *http.Request) {
		r.RemoteAddr = net.JoinHostPort(ip, "41641")
		r.Host = host
		if o := r.Header.Get("Origin"); o != "" {
			r.Header.Set("Origin", "http://"+host)
		}
	}
}

func TestTailscaleSelf(t *testing.T) {
	h := newHarness(t)
	if _, err := tailscaleSelf(); err == nil || !strings.Contains(err.Error(), "tailscale ip") {
		t.Fatalf("no tailscaled: %v", err)
	}
	tailnet(hubIP, h)
	s, err := tailscaleSelf()
	if err != nil || s.IP.String() != hubIP || s.NodeID != hubNode || s.Short != "hub" ||
		s.DNSName != "hub.example.ts.net" || s.Suffix != "example.ts.net" || s.Login != owner {
		t.Fatalf("self = %+v %v", s, err)
	}
	// The address must be in 100.64.0.0/10, whatever tailscale prints.
	for _, bad := range []string{"192.168.1.5", "100.128.0.1", "::1", "junk"} {
		h.write("ts.ip", bad+"\n", 0o644)
		if _, err := tailscaleSelf(); err == nil {
			t.Errorf("tailscale ip %q accepted", bad)
		}
	}
	// Only read-only subcommands were ever run.
	b, _ := os.ReadFile(filepath.Join(h.bin, "ts.calls"))
	for _, l := range strings.Split(strings.TrimSpace(string(b)), "\n") {
		if !regexp.MustCompile(`^(ip\|-4\||status\|--json\|)$`).MatchString(l) {
			t.Fatalf("unexpected tailscale call %q", l)
		}
	}
}

func TestHubAdmission(t *testing.T) {
	h := newHarness(t)
	setVar(t, &tailscaleTimeout, 500*time.Millisecond)
	d := h.hubDash()
	host := "hub:7788"
	get := func(ip string, mods ...func(*http.Request)) int {
		w, _ := serve(d, req("GET", "/api/state", "", append([]func(*http.Request){from(ip, host)}, mods...)...))
		return w.Code
	}
	if c := get(hostAIP); c != 200 {
		t.Fatalf("owner's laptop = %d", c)
	}
	for name, ip := range map[string]string{"other user": otherIP, "tagged": taggedIP, "whois error": errIP,
		"bad JSON": junkIP, "unknown node": "100.100.9.9"} {
		if c := get(ip); c != 403 {
			t.Errorf("%s: %d, want 403", name, c)
		}
	}
	began := time.Now()
	if c := get(slowIP); c != 403 || time.Since(began) > 3*time.Second {
		t.Fatalf("slow whois: %d after %v", c, time.Since(began))
	}
	// Forwarded headers are never read: only RemoteAddr counts.
	xff := func(r *http.Request) {
		r.Header.Set("X-Forwarded-For", hostAIP)
		r.Header.Set("X-Real-IP", hostAIP)
		r.Header.Set("Forwarded", "for="+hostAIP)
	}
	if c := get(otherIP, xff); c != 403 {
		t.Fatalf("other user claiming the laptop by header = %d", c)
	}
	// Loopback skips whois; the Host check still applies.
	calls := h.tsCalls("whois|")
	if c := get("127.0.0.1", func(r *http.Request) { r.Host = testAddr }); c != 200 {
		t.Fatalf("loopback = %d", c)
	}
	if c := get("127.0.0.1", func(r *http.Request) { r.Host = "evil.example:7788" }); c != 421 {
		t.Fatalf("loopback with a foreign Host = %d", c)
	}
	if h.tsCalls("whois|") != calls {
		t.Fatal("loopback ran whois")
	}
	// A refused node's Host is not even looked at.
	if c := get(otherIP, func(r *http.Request) { r.Host = "evil.example:7788" }); c != 403 {
		t.Fatalf("other user with a foreign Host = %d", c)
	}
	// One log line per refused address per minute.
	log, _ := os.ReadFile(filepath.Join(h.stateDir(), "daemon.log"))
	if n := strings.Count(string(log), "dashboard: refused "+otherIP); n != 1 {
		t.Fatalf("refusal lines for %s = %d:\n%s", otherIP, n, log)
	}
}

func TestHubWhoisCache(t *testing.T) {
	h := newHarness(t)
	setVar(t, &whoisTTL, 300*time.Millisecond)
	d := h.hubDash()
	get := func(ip string) int {
		w, _ := serve(d, req("GET", "/api/state", "", from(ip, "hub:7788")))
		return w.Code
	}
	whois := func(ip string) int { return h.tsCalls("whois|--json|" + ip + "|") }
	for i := 0; i < 3; i++ {
		get(hostAIP)
		get(errIP)
	}
	if whois(hostAIP) != 1 || whois(errIP) != 1 {
		t.Fatalf("within the TTL: whois %d and %d times, want 1 and 1", whois(hostAIP), whois(errIP))
	}
	// A cached admission is per IP: another address is asked afresh.
	if get(otherIP) != 403 || whois(otherIP) != 1 {
		t.Fatal("other IP was not looked up")
	}
	time.Sleep(400 * time.Millisecond)
	// Expired: a node that became another user's is refused now.
	h.write("ts.whois."+hostAIP, whoisJSON(hostANode, "host-a.example.ts.net.", "user@example.com", nil), 0o644)
	if c := get(hostAIP); c != 403 || whois(hostAIP) != 2 {
		t.Fatalf("after the TTL: %d, whois %d", c, whois(hostAIP))
	}
	// A failure is cached for the TTL, then asked again.
	if c := get(errIP); c != 403 || whois(errIP) != 2 {
		t.Fatalf("failure after the TTL = %d, whois %d", c, whois(errIP))
	}
	h.write("ts.whois."+errIP, whoisJSON("nERR", "fixed.example.ts.net.", owner, nil), 0o644)
	os.Remove(filepath.Join(h.bin, "ts.whois."+errIP+".exit"))
	if c := get(errIP); c != 403 || whois(errIP) != 2 {
		t.Fatalf("cached failure = %d, whois %d", c, whois(errIP))
	}
	time.Sleep(400 * time.Millisecond)
	if c := get(errIP); c != 200 || whois(errIP) != 3 {
		t.Fatalf("fixed node after the TTL = %d, whois %d", c, whois(errIP))
	}
}

func TestHubHostVariants(t *testing.T) {
	h := newHarness(t)
	d := h.hubDash()
	for _, host := range []string{"127.0.0.1:7788", "localhost:7788", hubIP + ":7788", "hub:7788",
		"hub.example.ts.net:7788", "hub.example.ts.net.:7788", "HUB.example.ts.net:7788"} {
		if w, _ := serve(d, req("GET", "/api/state", "", from(hostAIP, host))); w.Code != 200 {
			t.Errorf("Host %s = %d", host, w.Code)
		}
	}
	for _, host := range []string{"evil.example:7788", "hub:9999", hubIP + ":80", "hub", "localhost:9",
		"other.example.ts.net:7788", "hub.evil.example:7788", "100.64.0.20:7788"} {
		if w, _ := serve(d, req("GET", "/api/state", "", from(hostAIP, host))); w.Code != 421 {
			t.Errorf("Host %s = %d, want 421", host, w.Code)
		}
	}
	// Over the tailnet too, nothing answers: the hub is read-only.
	f := seedDashboard(h)
	for _, path := range []string{fmt.Sprintf("/api/asks/%d/answer", f.rootAsk), fmt.Sprintf("/api/peers/%s/asks/1/answer", hostANode)} {
		if w, _ := serve(d, req("POST", path, `{"text":"from the phone"}`, from(hostAIP, "hub.example.ts.net:7788"))); w.Code != 404 {
			t.Fatalf("tailnet POST %s = %d", path, w.Code)
		}
	}
}

func TestDashboardConfigTailnet(t *testing.T) {
	dir := t.TempDir()
	write := func(s string) {
		os.WriteFile(filepath.Join(dir, dashboardAddrFile), []byte(s), 0o644)
	}
	_, defPort, _ := net.SplitHostPort(dashboardDefaultAddr)
	for in, want := range map[string]dashConfig{
		"tailnet\n":      {Addr: "127.0.0.1:" + defPort, Tailnet: true},
		" tailnet:7799 ": {Addr: "127.0.0.1:7799", Tailnet: true},
		"127.0.0.1:7788": {Addr: "127.0.0.1:7788"},
	} {
		write(in)
		if c, err := dashboardConfig(dir); err != nil || c != want {
			t.Errorf("%q: %+v %v, want %+v", in, c, err, want)
		}
	}
	for _, in := range []string{"tailnet:", "tailnet:x", "tailnet:99999", "100.64.0.10:7788", "tailnet 7788"} {
		write(in)
		if c, err := dashboardConfig(dir); err == nil {
			t.Errorf("%q accepted as %+v", in, c)
		}
	}
}

func TestCheckHubURL(t *testing.T) {
	suffix := func() (string, error) { return "example.ts.net", nil }
	ok := map[string]string{
		"http://hub.example.ts.net:7788":   "http://hub.example.ts.net:7788" + peerPushPath,
		"http://hub.example.ts.net.:7788/": "http://hub.example.ts.net.:7788" + peerPushPath,
		"http://100.64.0.10:7788":          "http://100.64.0.10:7788" + peerPushPath,
	}
	for in, want := range ok {
		if got, err := checkHubURL(in, suffix); err != nil || got != want {
			t.Errorf("%q: %q %v", in, got, err)
		}
	}
	for _, in := range []string{"https://hub.example.ts.net:7788", "http://hub:7788",
		"http://hub.example.ts.net", "http://example.com:7788", "http://evil.example/.example.ts.net:7788",
		"http://a.b.example.ts.net:7788", "http://example.ts.net:7788", "http://10.0.0.1:7788", "http://127.0.0.1:7788",
		"http://[::1]:7788", "http://100.128.0.1:7788", "http://u:p@100.64.0.10:7788", "http://100.64.0.10:7788/x",
		"http://100.64.0.10:7788/?q=1", "ftp://100.64.0.10:7788", "hub.example.ts.net:7788", ""} {
		if got, err := checkHubURL(in, suffix); err == nil {
			t.Errorf("%q accepted as %q", in, got)
		}
	}
	// An IP needs no tailscale; a name does.
	down := func() (string, error) { return "", fmt.Errorf("not running") }
	if _, err := checkHubURL("http://100.64.0.10:7788", down); err != nil {
		t.Fatal(err)
	}
	if _, err := checkHubURL("http://hub.example.ts.net:7788", down); err == nil || !strings.HasPrefix(err.Error(), "cannot check") {
		t.Fatalf("name without tailscale: %v", err)
	}
}

// pushTo posts a peer push from ip and decodes the reply.
func pushTo(t *testing.T, d *dashboard, ip string, body any, mods ...func(*http.Request)) (int, pushReply, string) {
	t.Helper()
	var b []byte
	if s, ok := body.(string); ok {
		b = []byte(s)
	} else {
		b, _ = json.Marshal(body)
	}
	r := req("POST", peerPushPath, string(b), append([]func(*http.Request){from(ip, "hub.example.ts.net:7788")}, mods...)...)
	r.Header.Del("Origin")
	r.Header.Del("Sec-Fetch-Site")
	w, _ := serve(d, r)
	var rep pushReply
	json.Unmarshal(w.Body.Bytes(), &rep)
	return w.Code, rep, w.Body.String()
}

// peerState is a peer's /api/state stand-in with one owner ask.
func peerState(at time.Time, asks ...int64) *dashState {
	s := &dashState{Now: stamp(at), Version: "test", OwnerAsks: []ownerAsk{}, Orchestrators: []orchestrator{},
		Activity: []activity{}, Closed: []closedRoot{}}
	for _, id := range asks {
		s.OwnerAsks = append(s.OwnerAsks, ownerAsk{ID: id, Text: fmt.Sprintf("peer ask %d", id), At: stamp(at.Add(-10 * time.Second)),
			AgeMS: 10000, Asker: taskRef{1, "host-a-orch", "orchestrator"}, Root: taskRef{1, "host-a-orch", "orchestrator"}, AskerIsRoot: true})
	}
	s.Orchestrators = append(s.Orchestrators, orchestrator{ID: 1, Name: "host-a-orch", Role: "orchestrator", Status: "open",
		CreatedAt: stamp(at.Add(-time.Hour)), Tasks: []stateTask{}, Note: &stamped{Text: "on the laptop", At: stamp(at.Add(-time.Minute))}})
	return s
}

// push is the body a peer sends: its state and its clock.
func push(s *dashState, at time.Time) map[string]any {
	return map[string]any{"state": s, "now": at.UTC().Format(time.RFC3339Nano)}
}

func hubView(t *testing.T, d *dashboard) stateView {
	t.Helper()
	w, _ := serve(d, req("GET", "/api/state", "", from("127.0.0.1", testAddr)))
	if w.Code != 200 {
		t.Fatalf("hub state = %d %s", w.Code, w.Body)
	}
	var v stateView
	if err := json.Unmarshal(w.Body.Bytes(), &v); err != nil {
		t.Fatal(err)
	}
	return v
}

func TestHubPeerIdentityFromWhois(t *testing.T) {
	h := newHarness(t)
	d := h.hubDash()
	at := time.Now()
	if c, rep, body := pushTo(t, d, hostAIP, push(peerState(at, 7), at)); c != 200 || rep.Hub.NodeID != hubNode ||
		rep.Hub.Machine != "hub" || rep.Answers == nil {
		t.Fatalf("push = %d %s", c, body)
	}
	var node, machine, login string
	if err := h.openDB().QueryRow(`select node_id, machine, login from peers`).Scan(&node, &machine, &login); err != nil ||
		node != hostANode || machine != "host-a" || login != owner {
		t.Fatalf("peer row = %s %s %s %v", node, machine, login, err)
	}
	// The body cannot name a machine: an extra field is refused outright.
	claim := push(peerState(at), at)
	claim["machine"] = "hub"
	if c, _, _ := pushTo(t, d, hostAIP, claim); c != 400 {
		t.Fatalf("push claiming a machine = %d", c)
	}
	nested := `{"state":{"now":"x","version":"","owner_asks":[],"orchestrators":[],"activity":[],"closed":[],"machine":"hub"},"now":"` +
		at.UTC().Format(time.RFC3339Nano) + `","applied":[]}`
	// Inside state, an unknown field is a newer peer's addition: accepted,
	// but never stored or read, so it cannot name a machine either.
	if c, _, body := pushTo(t, d, hostAIP, nested); c != 200 {
		t.Fatalf("state with an unknown field = %d %s", c, body)
	}
	var snap string
	h.openDB().QueryRow(`select snapshot from peers where node_id = ?`, hostANode).Scan(&snap)
	if strings.Contains(snap, "machine") || strings.Contains(snap, "hub") {
		t.Fatalf("an unknown state field was stored: %s", snap)
	}
	if v := hubView(t, d); v.Machines[1].Machine != "host-a" {
		t.Fatalf("machine = %q", v.Machines[1].Machine)
	}
	for name, c := range map[string]struct {
		ip   string
		body any
		mod  func(*http.Request)
		code int
	}{
		"loopback has no identity": {"127.0.0.1", push(peerState(at), at), func(r *http.Request) { r.Host = testAddr }, 403},
		"the hub itself":           {hubIP, push(peerState(at), at), nil, 403},
		"other user":               {otherIP, push(peerState(at), at), nil, 403},
		"tagged":                   {taggedIP, push(peerState(at), at), nil, 403},
		"text/plain":               {hostAIP, push(peerState(at), at), func(r *http.Request) { r.Header.Set("Content-Type", "text/plain") }, 415},
		"no state":                 {hostAIP, map[string]any{"now": at.Format(time.RFC3339), "applied": []any{}}, nil, 400},
		"bad now":                  {hostAIP, map[string]any{"state": peerState(at), "now": "yesterday"}, nil, 400},
		"trailing data":            {hostAIP, `{"state":{},"now":"2026-09-29T00:00:00Z"}{}`, nil, 400},
		"oversized":                {hostAIP, `{"state":{},"now":"2026-09-29T00:00:00Z","x":"` + strings.Repeat("a", 2<<20) + `"}`, nil, 413},
	} {
		mods := []func(*http.Request){}
		if c.mod != nil {
			mods = append(mods, c.mod)
		}
		if code, _, body := pushTo(t, d, c.ip, c.body, mods...); code != c.code {
			t.Errorf("%s: %d %s, want %d", name, code, body, c.code)
		}
	}
	// A browser on the tailnet cannot turn the push endpoint into a GET.
	if w, _ := serve(d, req("GET", peerPushPath, "", from(hostAIP, "hub:7788"))); w.Code != 405 {
		t.Fatalf("GET push = %d", w.Code)
	}
	// A non-hub dashboard has no push endpoint at all.
	if w, _ := serve(h.dash(), req("POST", peerPushPath, "{}")); w.Code != 404 {
		t.Fatalf("push to a non-hub = %d", w.Code)
	}
}

func TestHubCaps(t *testing.T) {
	h := newHarness(t)
	d := h.hubDash()
	at := time.Now()
	s := peerState(at)
	for i := int64(1); i <= 60; i++ {
		s.OwnerAsks = append(s.OwnerAsks, ownerAsk{ID: i, Text: strings.Repeat("x", 5000), At: stamp(at)})
	}
	for i := int64(2); i <= 70; i++ {
		s.Orchestrators = append(s.Orchestrators, orchestrator{ID: i, Name: "o", CreatedAt: stamp(at), Tasks: []stateTask{}})
	}
	if c, _, body := pushTo(t, d, hostAIP, push(s, at)); c != 200 {
		t.Fatalf("push = %d %s", c, body)
	}
	v := hubView(t, d)
	p := v.Machines[1].State
	if len(v.Machines) != 2 || len(p.OwnerAsks) != stateAsksMax || len(p.Orchestrators) != stateRootsMax ||
		len([]rune(p.OwnerAsks[0].Text)) != stateAskTextMax {
		t.Fatalf("stored snapshot: %d asks (text %d), %d orchestrators", len(p.OwnerAsks), len([]rune(p.OwnerAsks[0].Text)), len(p.Orchestrators))
	}
	// A snapshot past the size cap is refused; the old one stays.
	big := peerState(at)
	for i := 0; i < stateRootsMax; i++ {
		o := orchestrator{ID: int64(i), Name: "o", CreatedAt: stamp(at)}
		for j := 0; j < 100; j++ {
			o.Tasks = append(o.Tasks, stateTask{ID: int64(j), Name: strings.Repeat("n", 300)})
		}
		big.Orchestrators = append(big.Orchestrators, o)
	}
	if c, _, body := pushTo(t, d, hostAIP, push(big, at)); c != 413 {
		t.Fatalf("oversized snapshot = %d %s", c, body)
	}
	if v := hubView(t, d); len(v.Machines[1].State.OwnerAsks) != stateAsksMax {
		t.Fatal("a refused snapshot replaced the stored one")
	}
	// At most peersMax machines pushing in the last week; old rows do not count.
	db := h.openDB()
	for i := 0; i < peersMax; i++ {
		db.Exec(`insert into peers values (?, ?, ?, '{}', ?, ?)`, fmt.Sprintf("n%d", i), fmt.Sprintf("m%d", i), owner, now(), now())
	}
	if c, _, _ := pushTo(t, d, hostAIP, push(peerState(at), at)); c != 200 {
		t.Fatalf("a known machine was refused: %d", c)
	}
	db.Exec(`update peers set received_at = ? where node_id = ?`, stamp(at.Add(-8*24*time.Hour)), hostANode)
	if c, _, body := pushTo(t, d, hostAIP, push(peerState(at), at)); c != 429 {
		t.Fatalf("machine %d = %d %s", peersMax+1, c, body)
	}
	db.Exec(`update peers set received_at = ? where node_id = 'n0'`, stamp(at.Add(-8*24*time.Hour)))
	if c, _, _ := pushTo(t, d, hostAIP, push(peerState(at), at)); c != 200 {
		t.Fatalf("after one machine went quiet for a week: %d", c)
	}
}

// A v0.6 peer still sends the relayed-answer results it applied and reads
// an answers list back. The hub accepts the push, ignores the results,
// writes nothing to the old relay tables, and returns no answers.
func TestHubAcceptsV06PeerApplied(t *testing.T) {
	h := newHarness(t)
	d := h.hubDash()
	db := h.openDB()
	db.Exec(`insert into relay_answers (node_id, ask_id, text, created_at) values (?, 8, 'queued by v0.6', ?)`, hostANode, now())
	at := time.Now()
	old := map[string]any{"state": peerState(at, 8), "now": at.UTC().Format(time.RFC3339Nano),
		"applied": []map[string]any{{"relay_id": 1, "hub_node": hubNode, "result": "ok", "via": "legacy"}, {"relay_id": 2, "hub_node": "nOther", "result": "rejected: x"}}}
	c, rep, body := pushTo(t, d, hostAIP, old)
	if c != 200 || rep.Hub.NodeID != hubNode || rep.Answers == nil || len(rep.Answers) != 0 || !strings.Contains(body, `"answers":[]`) {
		t.Fatalf("v0.6 push = %d %s", c, body)
	}
	var applied, delivered sql.NullString
	db.QueryRow(`select applied_at, delivered_at from relay_answers where node_id = ?`, hostANode).Scan(&applied, &delivered)
	if applied.Valid || delivered.Valid {
		t.Fatalf("the hub touched the old relay row: applied %v, delivered %v", applied, delivered)
	}
	if v := hubView(t, d); len(v.Machines) != 2 || len(v.NeedsYou) != 1 || v.NeedsYou[0].Machine != "host-a" {
		t.Fatalf("v0.6 peer's state = %+v", v.NeedsYou)
	}
	// A push without applied (a v0.7 peer) is the same.
	if c, _, body := pushTo(t, d, hostAIP, push(peerState(at), at)); c != 200 || !strings.Contains(body, `"answers":[]`) {
		t.Fatalf("v0.7 push = %d %s", c, body)
	}
}

func TestHubStaleAndClockSkew(t *testing.T) {
	h := newHarness(t)
	f := seedDashboard(h)
	d := h.hubDash()
	// The peer's clock runs an hour slow: its ask is 10 s old by its clock.
	real := time.Now()
	slow := real.Add(-time.Hour)
	if c, _, body := pushTo(t, d, hostAIP, push(peerState(slow, 7), slow)); c != 200 {
		t.Fatalf("push = %d %s", c, body)
	}
	v := hubView(t, d)
	if len(v.Machines) != 2 || !v.Machines[0].Local || v.Machines[0].Machine != "hub" || v.Machines[1].Machine != "host-a" ||
		v.Machines[1].NodeID != hostANode || v.Machines[1].Stale || v.Machines[1].LastPushAgeMS > 2000 || !v.Hub {
		t.Fatalf("machines = %+v", v.Machines)
	}
	a := v.Machines[1].State.OwnerAsks[0]
	if a.AgeMS < 9000 || a.AgeMS > 13000 || real.Sub(parseTime(a.At)) > 13*time.Second || real.Sub(parseTime(a.At)) < 9*time.Second {
		t.Fatalf("skewed ask: age %d ms at %s (hub now %s)", a.AgeMS, a.At, stamp(real))
	}
	o := v.Machines[1].State.Orchestrators[0]
	if d := real.Sub(parseTime(o.Note.At)); d < 55*time.Second || d > 65*time.Second || o.Note.AgeMS < 55000 {
		t.Fatalf("skewed note: %s, age %d", o.Note.At, o.Note.AgeMS)
	}
	if real.Sub(parseTime(o.CreatedAt)) < 59*time.Minute || real.Sub(parseTime(o.CreatedAt)) > 61*time.Minute {
		t.Fatalf("skewed created_at: %s", o.CreatedAt)
	}
	// Needs you merges both machines, oldest first (the peer's ask is 10 s
	// old by the hub's clock), with machine labels.
	if len(v.NeedsYou) != 3 {
		t.Fatalf("needs_you = %+v", v.NeedsYou)
	}
	var labels []string
	for _, n := range v.NeedsYou {
		labels = append(labels, fmt.Sprintf("%s/%d/%v", n.Machine, n.ID, n.Local))
	}
	if strings.Join(labels, " ") != fmt.Sprintf("host-a/7/false hub/%d/true hub/%d/true", f.workerAsk, f.rootAsk) {
		t.Fatalf("needs_you = %v", labels)
	}
	// The v0.4 fields still describe the hub's own ledger.
	if len(v.OwnerAsks) != 2 || len(v.Orchestrators) != 2 {
		t.Fatalf("top-level state = %+v", v.dashState)
	}
	db := h.openDB()
	db.Exec(`update peers set received_at = ?`, stamp(real.Add(-50*time.Second)))
	v = hubView(t, d)
	if !v.Machines[1].Stale || v.Machines[1].LastPushAgeMS < 50000 || !v.NeedsYou[0].Stale {
		t.Fatalf("after 50 s silence: %+v", v.Machines[1])
	}
	db.Exec(`update peers set received_at = ?`, stamp(real.Add(-8*24*time.Hour)))
	if v = hubView(t, d); len(v.Machines) != 1 || len(v.NeedsYou) != 2 {
		t.Fatalf("a week-silent peer is still shown: %+v", v.Machines)
	}
	var n int
	db.QueryRow(`select count(*) from peers`).Scan(&n)
	if n != 1 {
		t.Fatal("the dropped peer's row was deleted")
	}
}

// A host that moved to client mode proves it with a daemon heartbeat newer
// than its last peer push: the hub hides the old snapshot from machines,
// needs_you and attention, but never deletes the row.
func TestHubHidesClientHostSnapshot(t *testing.T) {
	h := newHarness(t)
	d := h.hubDash()
	db := h.openDB()
	at := time.Now()
	if c, _, body := pushTo(t, d, hostAIP, push(peerState(at, 7), at)); c != 200 {
		t.Fatalf("push = %d %s", c, body)
	}
	// A heartbeat older than the peer's last push changes nothing.
	db.Exec(`insert into meta (key, value) values (?, ?)`, hostHeartbeatKey("host-a"), stamp(at.Add(-time.Minute)))
	if v := hubView(t, d); len(v.Machines) != 2 || len(v.NeedsYou) != 1 {
		t.Fatalf("push newer than heartbeat: %+v asks %+v", v.Machines, v.NeedsYou)
	}
	// The host observed as a client after its last push: hidden.
	db.Exec(`update meta set value = ? where key = ?`, stamp(at.Add(time.Second)), hostHeartbeatKey("host-a"))
	v := hubView(t, d)
	if len(v.Machines) != 1 || len(v.NeedsYou) != 0 {
		t.Fatalf("client host still shown: %+v asks %+v", v.Machines, v.NeedsYou)
	}
	for _, a := range v.Attention {
		if a.Machine == "host-a" {
			t.Fatalf("client host still in attention: %+v", a)
		}
	}
	var n int
	db.QueryRow(`select count(*) from peers`).Scan(&n)
	if n != 1 {
		t.Fatal("the client host's peer row was deleted")
	}
	// No heartbeat row: shown as before, stale after 45 s.
	db.Exec(`delete from meta where key = ?`, hostHeartbeatKey("host-a"))
	db.Exec(`update peers set received_at = ?`, stamp(at.Add(-50*time.Second)))
	v = hubView(t, d)
	if len(v.Machines) != 2 || !v.Machines[1].Stale || len(v.NeedsYou) != 1 || !v.NeedsYou[0].Stale {
		t.Fatalf("no heartbeat: %+v asks %+v", v.Machines[1], v.NeedsYou)
	}
}

// startHubAndPeer runs two daemons in one process: the hub on 127.0.0.1
// plus the fake tailnet [::1], and a peer pushing to [::1].
func startHubAndPeer(t *testing.T, every time.Duration) (hub, peer *harness, hubURL string) {
	fakeTailnetHooks(t)
	setVar(t, &peerPushEvery, every)
	setVar(t, &peerCheckEvery, 100*time.Millisecond)
	setVar(t, &peerBackoffBase, 50*time.Millisecond)
	setVar(t, &peerBackoffCap, 200*time.Millisecond)
	hub = newHarness(t)
	peer = newHarness(t) // its bin is first on PATH now
	// Registered first, so it runs last: after both daemons stopped.
	t.Cleanup(func() {
		if t.Failed() {
			t.Logf("hub daemon.log:\n%s\npeer daemon.log:\n%s\nfake tailscale calls:\n%s", hub.daemonLog(), peer.daemonLog(),
				func() string { b, _ := os.ReadFile(filepath.Join(peer.bin, "ts.calls")); return string(b) }())
		}
	})
	tailnet("::1", hub, peer)
	hub.writeAddr("tailnet")
	hs := newFakeSocket(t)
	hub.startDaemon(hs)
	c, _, _ := hs.next()
	c.Write([]byte(ack))
	var st map[string]any
	eventually(t, "hub up", func() bool {
		st = hub.ok(nil, "daemon", "--status")
		return st["dashboard"] == "up" && st["tailnet_url"] != nil
	})
	if st["role"] != "hub" || !regexp.MustCompile(`^http://hub\.example\.ts\.net:\d+/$`).MatchString(st["tailnet_url"].(string)) {
		t.Fatalf("hub status = %v", st)
	}
	_, port, _ := net.SplitHostPort(strings.TrimSuffix(strings.TrimPrefix(st["tailnet_url"].(string), "http://"), "/"))
	os.WriteFile(filepath.Join(peer.stateDir(), hubURLFile), []byte("http://[::1]:"+port+"\n"), 0o644)
	peer.writeAddr("off")
	ps := newFakeSocket(t)
	peer.startDaemon(ps)
	c, _, _ = ps.next()
	c.Write([]byte(ack))
	return hub, peer, st["dashboard_url"].(string)
}

func getJSON(t *testing.T, url string, v any) {
	t.Helper()
	resp, err := http.Get(url)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if err := json.NewDecoder(resp.Body).Decode(v); err != nil {
		t.Fatal(err)
	}
}

func TestHubAndPeerEndToEnd(t *testing.T) {
	hub, peer, url := startHubAndPeer(t, 300*time.Millisecond)
	// The peer's orchestrator asks the owner.
	top := peer.newTask("peer-root", "orchestrator", 0)
	askID := num(peer.ok(nil, "ask", "--as", id(top), "Merge the laptop branch?", "--owner"), "ask_id")
	var v stateView
	eventually(t, "peer snapshot on the hub", func() bool {
		getJSON(t, url+"api/state", &v)
		return len(v.Machines) == 2 && len(v.NeedsYou) == 1
	})
	if v.Machines[1].Machine != "host-a" || v.Machines[1].NodeID != hostANode || v.NeedsYou[0].Machine != "host-a" ||
		v.NeedsYou[0].ID != askID || v.NeedsYou[0].Text != "Merge the laptop branch?" || v.NeedsYou[0].Local {
		t.Fatalf("hub view = %+v", v)
	}
	st := peer.ok(nil, "daemon", "--status")
	if st["role"] != "peer" || st["last_push_ok_at"] == nil || st["last_push_error"] != nil || !strings.HasPrefix(st["hub_url"].(string), "http://[::1]:") {
		t.Fatalf("peer status = %v", st)
	}
	// The hub offers no way to answer: the owner answers in the
	// orchestrator's pane, on the peer, and the ask leaves the hub.
	r, _ := http.NewRequest("POST", fmt.Sprintf("%sapi/peers/%s/asks/%d/answer", url, hostANode, askID), strings.NewReader(`{"text":"Yes, merge."}`))
	r.Header.Set("Content-Type", "application/json")
	r.Header.Set("Origin", strings.TrimSuffix(url, "/"))
	resp, err := http.DefaultClient.Do(r)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != 404 {
		t.Fatalf("POST a peer answer to the hub = %d", resp.StatusCode)
	}
	peer.ok(nil, "answer", id(askID), "Yes, merge.")
	eventually(t, "answered ask leaves the hub", func() bool {
		getJSON(t, url+"api/state", &v)
		return len(v.NeedsYou) == 0
	})
	// The hub's own ledger was never written, and the peer has only the
	// pane's answer.
	var n int
	hub.openDB().QueryRow(`select count(*) from events`).Scan(&n)
	if n != 0 {
		t.Fatalf("hub ledger has %d events", n)
	}
	peer.openDB().QueryRow(`select count(*) from events where kind in ('answer', 'owner_answer')`).Scan(&n)
	if n != 1 {
		t.Fatalf("peer answers = %d", n)
	}
	if log := hub.daemonLog(); !strings.Contains(log, "dashboard hub at http://hub.example.ts.net:") {
		t.Fatalf("hub log:\n%s", log)
	}
}

// A CLI write on the peer (no daemon pass) reaches the hub by the change
// check, long before the next forced push; an unchanged ledger is not re-sent.
func TestPeerPushesChangesBetweenForcedPushes(t *testing.T) {
	hub, peer, url := startHubAndPeer(t, time.Hour)
	var v stateView
	// The peer's daemon health is state: its first heartbeat after the
	// first push is a change, pushed once.
	eventually(t, "first push with a live peer daemon", func() bool {
		getJSON(t, url+"api/state", &v)
		return len(v.Machines) == 2 && v.Machines[1].State.Daemon != nil && v.Machines[1].State.Daemon.State == "fresh"
	})
	received := func() string {
		var r string
		hub.openDB().QueryRow(`select received_at from peers`).Scan(&r)
		return r
	}
	first := received()
	time.Sleep(500 * time.Millisecond)
	if received() != first {
		t.Fatal("an unchanged ledger was pushed again")
	}
	top := peer.newTask("late", "orchestrator", 0)
	peer.ok(nil, "ask", "--as", id(top), "Now?", "--owner")
	eventually(t, "the new ask on the hub", func() bool {
		getJSON(t, url+"api/state", &v)
		return len(v.NeedsYou) == 1
	})
}

// A hub that is away costs the peer only backoff: the event bridge and the
// heartbeat carry on, and the failure is logged once and shown in --status.
func TestPeerPushFailureKeepsHeartbeat(t *testing.T) {
	fakeTailnetHooks(t)
	setVar(t, &peerPushEvery, 100*time.Millisecond)
	setVar(t, &peerBackoffBase, 100*time.Millisecond)
	setVar(t, &peerBackoffCap, 400*time.Millisecond)
	h := newHarness(t)
	tailnet("::1", h)
	var hits atomic.Int32
	ln, err := net.Listen("tcp", "[::1]:0")
	if err != nil {
		t.Fatal(err)
	}
	srv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		httpError(w, http.StatusServiceUnavailable, "hub asleep")
	})}
	go srv.Serve(ln)
	defer srv.Close()
	os.WriteFile(filepath.Join(h.stateDir(), hubURLFile), []byte("http://"+ln.Addr().String()+"\n"), 0o644)
	h.writeAddr("off")
	s := newFakeSocket(t)
	p := h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(ack))
	eventually(t, "a failed push", func() bool { return hits.Load() >= 2 })
	began, start := time.Now(), hits.Load()
	for time.Since(began) < 2*time.Second {
		c.Write([]byte(eventLines(1))) // passes poke the pusher; a backoff ignores them
		if st := h.ok(nil, "daemon", "--status"); st["daemon"] != "fresh" {
			t.Fatalf("heartbeat while the hub is away: %v", st)
		}
		time.Sleep(100 * time.Millisecond)
	}
	if n := hits.Load() - start; n > 8 {
		t.Fatalf("%d pushes in 2 s with a 400 ms backoff cap", n)
	}
	st := h.ok(nil, "daemon", "--status")
	if st["role"] != "peer" || !strings.Contains(fmt.Sprint(st["last_push_error"]), "hub answered 503 hub asleep") || st["last_push_ok_at"] != nil {
		t.Fatalf("status = %v", st)
	}
	if n := strings.Count(h.daemonLog(), "peer: push failed"); n != 1 {
		t.Fatalf("push failure lines = %d", n)
	}
	s.shutdown()
	p.wait(t)
}

func TestHubWithoutTailscaleServesLoopback(t *testing.T) {
	setVar(t, &tailnetRetryBase, time.Hour) // pin the log to the first attempt
	h := newHarness(t)
	h.writeAddr("tailnet")
	s := newFakeSocket(t)
	p := h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(ack))
	var st map[string]any
	eventually(t, "dashboard up", func() bool {
		st = h.ok(nil, "daemon", "--status")
		return st["dashboard"] == "up"
	})
	if st["role"] != "hub" || st["tailnet_url"] != nil || !regexp.MustCompile(`^http://127\.0\.0\.1:\d+/$`).MatchString(st["dashboard_url"].(string)) {
		t.Fatalf("status = %v", st)
	}
	var v stateView
	getJSON(t, st["dashboard_url"].(string)+"api/state", &v)
	if v.Hub || len(v.Machines) != 1 {
		t.Fatalf("state = %+v", v)
	}
	s.shutdown()
	p.wait(t)
	if l := dashLines(h.daemonLog()); len(l) != 2 || !strings.Contains(l[0], "dashboard: tailnet unavailable (tailscale ip") ||
		!strings.Contains(l[1], "dashboard at") {
		t.Fatalf("dashboard lines = %q", l)
	}
}

func TestPeerBadHubURLRefused(t *testing.T) {
	for _, bad := range []string{"https://hub.example.ts.net:7788", "http://example.com:7788", "http://10.0.0.1:7788",
		"http://hub.other.ts.net:7788"} {
		h := newHarness(t)
		tailnet(hubIP, h)
		os.WriteFile(filepath.Join(h.stateDir(), hubURLFile), []byte(bad+"\n"), 0o644)
		h.writeAddr("off")
		s := newFakeSocket(t)
		p := h.startDaemon(s)
		c, _, _ := s.next()
		c.Write([]byte(ack))
		eventually(t, "refusal", func() bool {
			return strings.Contains(h.daemonLog(), "hub.url refused") && h.ok(nil, "daemon", "--status")["daemon"] == "fresh"
		})
		st := h.ok(nil, "daemon", "--status")
		if st["role"] != "peer" || !strings.Contains(fmt.Sprint(st["last_push_error"]), "hub.url refused") || st["daemon"] != "fresh" {
			t.Fatalf("%s: status = %v", bad, st)
		}
		s.shutdown()
		p.wait(t)
		if n := strings.Count(h.daemonLog(), "peer:"); n != 1 {
			t.Fatalf("%s: peer lines = %d\n%s", bad, n, h.daemonLog())
		}
	}
	// A hub ignores its own hub.url.
	h := newHarness(t)
	h.writeAddr("tailnet")
	os.WriteFile(filepath.Join(h.stateDir(), hubURLFile), []byte("http://100.64.0.10:7788\n"), 0o644)
	if st := h.ok(nil, "daemon", "--status"); st["role"] != "hub" || st["hub_url"] != nil {
		t.Fatalf("hub status = %v", st)
	}
}

func TestLocalRoleAndStateShape(t *testing.T) {
	h := newHarness(t)
	if st := h.ok(nil, "daemon", "--status"); st["role"] != "local" || st["tailnet_url"] != nil || st["hub_url"] != nil {
		t.Fatalf("status = %v", st)
	}
	seedDashboard(h)
	raw, _ := getState(t, h.dash())
	var v stateView
	json.Unmarshal([]byte(raw), &v)
	if v.Hub || len(v.Machines) != 1 || !v.Machines[0].Local || len(v.NeedsYou) != 2 || v.Machine == "" {
		t.Fatalf("local state = %s", raw)
	}
	if bytes.Contains([]byte(raw), []byte(`"relays"`)) {
		t.Fatalf("state still carries relays: %s", raw)
	}
	// The page renders the merged view of every machine.
	for file, wants := range map[string][]string{
		"web/src/work.ts": {"s.machines", "s.attention"},
	} {
		src, err := os.ReadFile(file)
		if err != nil {
			t.Fatal(err)
		}
		for _, want := range wants {
			if !strings.Contains(string(src), want) {
				t.Errorf("%s lacks %s", file, want)
			}
		}
	}
}

func TestPeerClientIgnoresProxyEnv(t *testing.T) {
	h := newHarness(t)
	os.WriteFile(filepath.Join(h.stateDir(), hubURLFile), []byte("http://100.64.0.10:7788\n"), 0o644)
	p := newPusher(h.openDB(), &daemonLog{}, h.stateDir())
	if tr, ok := p.client.Transport.(*http.Transport); !ok || tr.Proxy != nil || p.client.CheckRedirect == nil {
		t.Fatalf("peer transport = %#v", p.client.Transport)
	}
}

// The push reply's digest ignores the clock, so an unchanged ledger is not
// re-sent on every pass.
func TestDigestIgnoresClock(t *testing.T) {
	a := peerState(time.Now(), 1)
	b := peerState(time.Now().Add(time.Minute), 1)
	b.OwnerAsks[0].At, b.OwnerAsks[0].AgeMS = a.OwnerAsks[0].At, 99999
	b.Orchestrators[0].CreatedAt, b.Orchestrators[0].Note.At = a.Orchestrators[0].CreatedAt, a.Orchestrators[0].Note.At
	if digest(a) != digest(b) {
		t.Fatal("digest depends on now or ages")
	}
	b.OwnerAsks[0].Text = "changed"
	if digest(a) == digest(b) {
		t.Fatal("digest missed a change")
	}
}

var _ = sql.ErrNoRows

// Forward compatibility: the envelope stays strict; nested state ignores
// unknown fields but keeps the known fields' types and every cap.
func TestHubStateForwardCompatible(t *testing.T) {
	h := newHarness(t)
	d := h.hubDash()
	at := time.Now()
	nowS := `"now":"` + at.UTC().Format(time.RFC3339Nano) + `","applied":[]`
	additive := `{"state":{"now":"x","version":"9","future":{"a":[1,2]},"owner_asks":[{"id":7,"text":"q","at":"` + stamp(at) +
		`","asker":{"id":1,"name":"o","role":"orchestrator","colour":"red"},"root":{"id":1,"name":"o"},"urgency":3}],` +
		`"orchestrators":[{"id":1,"name":"o","role":"orchestrator","status":"open","created_at":"` + stamp(at) +
		`","tasks":[{"id":2,"name":"w","sidebar":"x"}],"spaces":2}],"activity":[],"closed":[]},` + nowS + `}`
	if c, _, body := pushTo(t, d, hostAIP, additive); c != 200 {
		t.Fatalf("additive nested fields = %d %s", c, body)
	}
	v := hubView(t, d)
	if p := v.Machines[1].State; len(p.OwnerAsks) != 1 || p.OwnerAsks[0].ID != 7 || len(p.Orchestrators[0].Tasks) != 1 {
		t.Fatalf("known fields lost: %+v", p)
	}
	var snap string
	h.openDB().QueryRow(`select snapshot from peers`).Scan(&snap)
	for _, k := range []string{"future", "urgency", "colour", "sidebar", "spaces"} {
		if strings.Contains(snap, k) {
			t.Fatalf("unknown field %s stored: %s", k, snap)
		}
	}
	for name, body := range map[string]string{
		"asks not a list":      `{"state":{"owner_asks":"x"},` + nowS + `}`,
		"ask id a string":      `{"state":{"owner_asks":[{"id":"7"}]},` + nowS + `}`,
		"tasks not a list":     `{"state":{"orchestrators":[{"id":1,"tasks":{}}]},` + nowS + `}`,
		"age a string":         `{"state":{"activity":[{"id":1,"age_ms":"old"}]},` + nowS + `}`,
		"state null":           `{"state":null,` + nowS + `}`,
		"state a list":         `{"state":[],` + nowS + `}`,
		"state missing":        `{` + nowS + `}`,
		"unknown top level":    `{"state":{},"machine":"hub",` + nowS + `}`,
		"trailing value":       `{"state":{},` + nowS + `}{}`,
		"state trailing bytes": `{"state":{}} ,` + nowS + `}`,
	} {
		if c, _, out := pushTo(t, d, hostAIP, body); c != 400 {
			t.Errorf("%s: %d %s, want 400", name, c, out)
		}
	}
	// Caps apply to the leniently decoded state as before (see TestHubCaps);
	// the body bound too.
	big := `{"state":{"future":"` + strings.Repeat("a", 2<<20) + `"},` + nowS + `}`
	if c, _, _ := pushTo(t, d, hostAIP, big); c != 413 {
		t.Fatalf("oversized body with an unknown nested field = %d", c)
	}
}

// Same-login tagged node: only the tag guard refuses it. Each whois cache
// map is cleared at its cap before one more entry. Port 80 accepts bare
// names.
func TestHubTaggedOwnerCacheCapAndPort80(t *testing.T) {
	h := newHarness(t)
	d := h.hubDash()
	if w, _ := serve(d, req("GET", "/api/state", "", from(taggedOwnerIP, "hub:7788"))); w.Code != 403 {
		t.Fatalf("the owner's tagged node = %d", w.Code)
	}
	c := d.hub.Load().whois
	for i := 0; i < whoisCacheMax; i++ {
		c.ok[fmt.Sprint("seed-ok-", i)] = whoisEntry{at: time.Now()}
		c.bad[fmt.Sprint("seed-bad-", i)] = whoisEntry{at: time.Now()}
	}
	if _, err := c.admit(net.ParseIP(hostAIP)); err != nil {
		t.Fatal(err)
	}
	if _, err := c.admit(net.ParseIP(otherIP)); err == nil {
		t.Fatal("other user admitted")
	}
	if len(c.ok) > whoisCacheMax || len(c.bad) > whoisCacheMax || len(c.ok) != 1 || len(c.bad) != 1 {
		t.Fatalf("cache sizes after the cap: ok %d, bad %d", len(c.ok), len(c.bad))
	}
	self, _ := tailscaleSelf()
	d80 := newDashboard(h.openDB(), &daemonLog{}, "127.0.0.1:80")
	d80.enableHub(self, "80")
	for _, host := range []string{hubIP, "hub", "hub.example.ts.net", "hub.example.ts.net.", hubIP + ":80"} {
		if w, _ := serve(d80, req("GET", "/api/state", "", from(hostAIP, host))); w.Code != 200 {
			t.Errorf("port 80 Host %s = %d", host, w.Code)
		}
	}
}

func TestRebaseBothDirections(t *testing.T) {
	for _, delta := range []time.Duration{-8 * time.Minute, 8 * time.Minute} {
		at := time.Now().UTC().Truncate(time.Millisecond)
		peer := at.Add(delta)
		s := peerState(peer, 7)
		walkTimes(s, func(ts *string, age *int64) {
			*ts = stamp(peer.Add(-10 * time.Second))
			if age != nil {
				*age = 1
			}
		})
		rebase(s, peer, at, at.Add(5*time.Second))
		walkTimes(s, func(ts *string, age *int64) {
			if *ts != stamp(at.Add(-10*time.Second)) {
				t.Fatalf("delta %v: timestamp %s", delta, *ts)
			}
			if age != nil && *age != 15000 {
				t.Fatalf("delta %v: age %d", delta, *age)
			}
		})
	}
}

// Twenty peers at the full snapshot size still serve /api/state.
func TestHubStateAtCap(t *testing.T) {
	h := newHarness(t)
	d := h.hubDash()
	at := time.Now()
	s := peerState(at, 7)
	s.Orchestrators = nil
	for i := 0; i < stateRootsMax; i++ {
		o := orchestrator{ID: int64(i + 1), Name: "root", CreatedAt: stamp(at), Tasks: []stateTask{}}
		for j := 0; j < 80; j++ {
			o.Tasks = append(o.Tasks, stateTask{ID: int64(j + 1), Name: "worker", Role: "implementer", Status: "open",
				ObservedAt: stamp(at), LastEvent: &lastEvent{Kind: "ready", At: stamp(at)}})
		}
		s.Orchestrators = append(s.Orchestrators, o)
	}
	b, _ := json.Marshal(s)
	if len(b) > snapshotMax {
		t.Fatalf("base snapshot %d bytes", len(b))
	}
	s.Version = strings.Repeat("x", snapshotMax-len(b)+len(s.Version))
	if b, _ = json.Marshal(s); len(b) != snapshotMax {
		t.Fatalf("snapshot %d bytes", len(b))
	}
	if c, _, body := pushTo(t, d, hostAIP, push(s, at)); c != 200 {
		t.Fatalf("push at the cap = %d %s", c, body)
	}
	for i := 1; i < peersMax; i++ {
		if _, err := d.db.Exec(`insert into peers values (?, ?, ?, ?, ?, ?)`, fmt.Sprint("node", i), fmt.Sprint("machine", i),
			owner, string(b), stamp(at), stamp(at)); err != nil {
			t.Fatal(err)
		}
	}
	began := time.Now()
	w, _ := serve(d, req("GET", "/api/state", "", from("127.0.0.1", testAddr)))
	if w.Code != 200 {
		t.Fatalf("state = %d", w.Code)
	}
	t.Logf("%d peers at %d bytes: %d bytes of state in %v", peersMax, len(b), w.Body.Len(), time.Since(began))
}

// A hub served over [::1] by httptest, as seen by the peer's dial.
func hubServer(t *testing.T, d http.Handler) string {
	t.Helper()
	ln, err := net.Listen("tcp", "[::1]:0")
	if err != nil {
		t.Fatal(err)
	}
	srv := &http.Server{Handler: d}
	go srv.Serve(ln)
	t.Cleanup(func() { srv.Close() })
	return "http://" + ln.Addr().String() + peerPushPath
}

// newTestPusher is a pusher over h's ledger aimed at target, verifying the
// hub as the fake whois describes "hub-::1".
func newTestPusher(t *testing.T, h *harness, target string) *pusher {
	t.Helper()
	os.WriteFile(filepath.Join(h.stateDir(), hubURLFile), []byte("http://100.64.0.10:7788\n"), 0o644)
	p := newPusher(h.openDB(), &daemonLog{}, h.stateDir())
	u, _ := url.Parse(target)
	p.target, p.hostPort, p.hubWhois = target, u.Host, newWhoisCache(owner)
	p.hubWhois.arg = hubWhoisArg
	return p
}

// The peer verifies the hub before sending anything: another user's node,
// a tagged node, or a whois failure gets no bytes; a reply naming another
// hub than whois found applies nothing.
func TestPeerVerifiesHubBeforeSending(t *testing.T) {
	fakeTailnetHooks(t)
	setVar(t, &whoisTTL, time.Nanosecond)
	h := newHarness(t)
	tailnet(hubIP, h)
	f := seedDashboard(h)
	var hits atomic.Int32
	var reply atomic.Value
	reply.Store(`{"hub":{"node_id":"nEVIL","machine":"evil"},"answers":[{"relay_id":1,"ask_id":` + id(f.workerAsk) + `,"text":"injected"}]}`)
	target := hubServer(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		io.Copy(io.Discard, r.Body)
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, reply.Load().(string))
	}))
	p := newTestPusher(t, h, target)
	for name, whois := range map[string]string{
		"other user": whoisJSON("nOTHER", "host-c.example.ts.net.", "user@example.com", nil),
		"tagged":     whoisJSON("nTAG", "ci-runner.example.ts.net.", owner, []string{"tag:ci"}),
		"garbage":    "{",
	} {
		h.write("ts.whois.hub-::1", whois, 0o644)
		if sent := p.push(true); sent || hits.Load() != 0 {
			t.Fatalf("%s: pushed to an unverified hub (hits %d)", name, hits.Load())
		}
	}
	h.write("ts.whois.hub-::1.exit", "1", 0o644)
	if sent := p.push(true); sent || hits.Load() != 0 {
		t.Fatal("pushed although whois failed")
	}
	var e string
	h.openDB().QueryRow(`select value from meta where key = ?`, peerErrKey).Scan(&e)
	if !strings.Contains(e, "hub not verified") {
		t.Fatalf("last push error = %q", e)
	}
	// A verified connection whose reply names another hub: nothing applied.
	os.Remove(filepath.Join(h.bin, "ts.whois.hub-::1.exit"))
	h.write("ts.whois.hub-::1", whoisJSON(hubNode, "hub.example.ts.net.", owner, nil), 0o644)
	if sent := p.push(true); sent || hits.Load() != 1 {
		t.Fatalf("mismatched reply accepted (hits %d)", hits.Load())
	}
	h.openDB().QueryRow(`select value from meta where key = ?`, peerErrKey).Scan(&e)
	var answered sql.NullInt64
	h.openDB().QueryRow(`select answered_by from events where id = ?`, f.workerAsk).Scan(&answered)
	if answered.Valid || !strings.Contains(e, "names node nEVIL") {
		t.Fatalf("answered %v, error %q", answered, e)
	}
	// The same hub naming itself is accepted. Answers a v0.6 hub queued in
	// its reply are ignored: the peer never writes what the hub sends.
	reply.Store(`{"hub":{"node_id":"` + hubNode + `","machine":"hub"},"answers":[{"relay_id":1,"ask_id":` + id(f.workerAsk) + `,"text":"relayed"}]}`)
	if sent := p.push(true); !sent || p.hubNode != hubNode {
		t.Fatalf("verified hub refused: hub %q", p.hubNode)
	}
	h.openDB().QueryRow(`select answered_by from events where id = ?`, f.workerAsk).Scan(&answered)
	if answered.Valid {
		t.Fatalf("a v0.6 hub's relayed answer was applied: event %d", answered.Int64)
	}
}

// Daemon level: an unverified hub gets no push, the bridge and heartbeat
// carry on, and --status shows why, logged once.
func TestPeerDaemonUnverifiedHub(t *testing.T) {
	fakeTailnetHooks(t)
	setVar(t, &peerBackoffBase, 50*time.Millisecond)
	setVar(t, &peerBackoffCap, 200*time.Millisecond)
	h := newHarness(t)
	tailnet(hubIP, h)
	h.write("ts.whois.hub-::1", whoisJSON("nOTHER", "host-c.example.ts.net.", "user@example.com", nil), 0o644)
	var hits atomic.Int32
	target := hubServer(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { hits.Add(1) }))
	os.WriteFile(filepath.Join(h.stateDir(), hubURLFile), []byte(strings.TrimSuffix(target, peerPushPath)+"\n"), 0o644)
	h.writeAddr("off")
	s := newFakeSocket(t)
	p := h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(ack))
	eventually(t, "refused hub", func() bool {
		st := h.ok(nil, "daemon", "--status")
		return strings.Contains(fmt.Sprint(st["last_push_error"]), "belongs to another user") && st["daemon"] == "fresh"
	})
	time.Sleep(500 * time.Millisecond)
	s.shutdown()
	p.wait(t)
	if hits.Load() != 0 || strings.Count(h.daemonLog(), "peer: push failed") != 1 {
		t.Fatalf("hits %d, log:\n%s", hits.Load(), h.daemonLog())
	}
}

// A slow hub times the push out; the event bridge keeps passing.
func TestPeerSlowHubKeepsBridge(t *testing.T) {
	fakeTailnetHooks(t)
	setVar(t, &peerPushEvery, 100*time.Millisecond)
	setVar(t, &peerBackoffBase, 100*time.Millisecond)
	setVar(t, &peerBackoffCap, 400*time.Millisecond)
	setVar(t, &peerTimeout, 200*time.Millisecond)
	h := newHarness(t)
	tailnet("::1", h)
	top := h.newTask("top", "orchestrator", 0)
	wid := h.newTask("impl", "implementer", top, "--pane", "w9:p1")
	h.launch(wid)
	h.setAgents("w9:p1/idle/1")
	var hits atomic.Int32
	target := hubServer(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		time.Sleep(400 * time.Millisecond)
		httpError(w, http.StatusServiceUnavailable, "hub asleep")
	}))
	os.WriteFile(filepath.Join(h.stateDir(), hubURLFile), []byte(strings.TrimSuffix(target, peerPushPath)+"\n"), 0o644)
	h.writeAddr("off")
	s := newFakeSocket(t)
	p := h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(ack))
	eventually(t, "a failed push", func() bool { return hits.Load() >= 2 })
	before := len(h.calls("agent|list|"))
	began, start := time.Now(), hits.Load()
	for time.Since(began) < 2*time.Second {
		c.Write([]byte(eventLines(1)))
		if st := h.ok(nil, "daemon", "--status"); st["daemon"] != "fresh" {
			t.Fatalf("heartbeat while the hub is slow: %v", st)
		}
		time.Sleep(100 * time.Millisecond)
	}
	if len(h.calls("agent|list|")) <= before {
		t.Fatal("the bridge did not pass during the timeouts")
	}
	if n := hits.Load() - start; n > 8 {
		t.Fatalf("%d pushes in 2 s", n)
	}
	if st := h.ok(nil, "daemon", "--status"); !strings.Contains(fmt.Sprint(st["last_push_error"]), "Timeout") {
		t.Fatalf("status = %v", st)
	}
	s.shutdown()
	p.wait(t)
}

// startHubDaemon runs a hub daemon on h with dashboard.addr "tailnet:PORT".
func (h *harness) startHubDaemon(port string) (*daemonProc, *fakeSocket, net.Conn) {
	h.writeAddr("tailnet:" + port)
	s := newFakeSocket(h.t)
	p := h.startDaemon(s)
	c, _, _ := s.next()
	c.Write([]byte(ack))
	return p, s, c
}

// freePort is a port free on both 127.0.0.1 and [::1] right now.
func freePort(t *testing.T) string {
	t.Helper()
	for i := 0; i < 20; i++ {
		ln, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			t.Fatal(err)
		}
		_, port, _ := net.SplitHostPort(ln.Addr().String())
		ln.Close()
		if l6, err := net.Listen("tcp", "[::1]:"+port); err == nil {
			l6.Close()
			return port
		}
	}
	t.Fatal("no port free on both loopbacks")
	return ""
}

func tailnetGet(t *testing.T, port, host string) int {
	t.Helper()
	r, _ := http.NewRequest("GET", "http://[::1]:"+port+"/api/state", nil)
	r.Host = host
	resp, err := http.DefaultClient.Do(r)
	if err != nil {
		return 0
	}
	resp.Body.Close()
	return resp.StatusCode
}

// Loopback taken by something else: the tailnet listener still serves, and
// --status says loopback is down and the tailnet is up.
func TestHubLoopbackBusyTailnetServes(t *testing.T) {
	fakeTailnetHooks(t)
	h := newHarness(t)
	tailnet("::1", h)
	port := freePort(t)
	busy, err := net.Listen("tcp", "127.0.0.1:"+port)
	if err != nil {
		t.Fatal(err)
	}
	defer busy.Close()
	p, s, _ := h.startHubDaemon(port)
	var st map[string]any
	eventually(t, "tailnet up", func() bool {
		st = h.ok(nil, "daemon", "--status")
		return st["tailnet_url"] != nil && st["daemon"] == "fresh"
	})
	if st["role"] != "hub" || st["dashboard"] != "down" || st["tailnet_url"] != "http://hub.example.ts.net:"+port+"/" {
		t.Fatalf("status = %v", st)
	}
	if c := tailnetGet(t, port, "hub:"+port); c != 200 {
		t.Fatalf("tailnet GET = %d", c)
	}
	s.shutdown()
	p.wait(t)
	if out := p.lines(t); out[0]["dashboard_url"] != nil || out[0]["tailnet_url"] == nil {
		t.Fatalf("start line = %v", out[0])
	}
	if l := dashLines(h.daemonLog()); !strings.Contains(l[0], "listen 127.0.0.1:"+port+" failed") {
		t.Fatalf("log = %q", l)
	}
}

// Each listener's URL is set and cleared on its own.
func TestHubListenerURLsIndependent(t *testing.T) {
	fakeTailnetHooks(t)
	h := newHarness(t)
	tailnet("::1", h)
	h.writeAddr("tailnet:" + freePort(t))
	db := h.openDB()
	d := startDashboard(db, openDaemonLog(filepath.Join(h.stateDir(), "daemon.log")), h.stateDir(), false)
	if d == nil || d.hub.Load() == nil {
		t.Fatal("hub did not start")
	}
	defer d.stop()
	meta := func(k string) bool { _, ok, _ := getMeta(db, k); return ok }
	if !meta(dashboardURLKey) || !meta(hubURLKey) {
		t.Fatal("URLs not recorded")
	}
	d.mu.Lock()
	lo := d.listeners[0]
	d.mu.Unlock()
	lo.Close() // the loopback listener dies on its own
	eventually(t, "loopback URL cleared", func() bool { return !meta(dashboardURLKey) })
	if !meta(hubURLKey) {
		t.Fatal("the tailnet URL went with the loopback listener")
	}
	d.mu.Lock()
	tl := d.listeners[1]
	d.mu.Unlock()
	tl.Close()
	eventually(t, "tailnet URL cleared", func() bool { return !meta(hubURLKey) })
}

// Tailscale down at start: loopback and the bridge serve, the hub comes up
// in the background when Tailscale does, without a restart.
func TestHubTailnetRetryUntilAvailable(t *testing.T) {
	fakeTailnetHooks(t)
	setVar(t, &tailnetRetryBase, 50*time.Millisecond)
	setVar(t, &tailnetRetryCap, 200*time.Millisecond)
	h := newHarness(t)
	port := freePort(t)
	p, s, c := h.startHubDaemon(port)
	var st map[string]any
	eventually(t, "loopback up", func() bool {
		st = h.ok(nil, "daemon", "--status")
		return st["dashboard"] == "up" && st["daemon"] == "fresh"
	})
	if st["tailnet_url"] != nil || tailnetGet(t, port, "hub:"+port) != 0 {
		t.Fatalf("tailnet up without tailscale: %v", st)
	}
	var v stateView
	getJSON(t, st["dashboard_url"].(string)+"api/state", &v)
	if v.Hub {
		t.Fatal("hub published without tailscale")
	}
	time.Sleep(300 * time.Millisecond) // several failed retries
	tailnet("::1", h)
	eventually(t, "tailnet up after tailscale", func() bool {
		return h.ok(nil, "daemon", "--status")["tailnet_url"] != nil
	})
	if code := tailnetGet(t, port, "hub.example.ts.net:"+port); code != 200 {
		t.Fatalf("tailnet GET = %d", code)
	}
	getJSON(t, st["dashboard_url"].(string)+"api/state", &v)
	if !v.Hub || v.Machine != "hub" {
		t.Fatalf("state after the hub came up: hub %v machine %s", v.Hub, v.Machine)
	}
	c.Write([]byte(eventLines(1)))
	if st := h.ok(nil, "daemon", "--status"); st["daemon"] != "fresh" || st["dashboard"] != "up" {
		t.Fatalf("status = %v", st)
	}
	s.shutdown()
	p.wait(t)
	l := dashLines(h.daemonLog())
	if len(l) < 3 || !strings.Contains(l[0], "tailnet unavailable (tailscale ip") || !strings.Contains(l[1], "dashboard at") ||
		!strings.Contains(strings.Join(l, "\n"), "dashboard hub at http://hub.example.ts.net:"+port+"/") ||
		strings.Count(strings.Join(l, "\n"), "still unavailable") > 1 {
		t.Fatalf("dashboard lines = %q", l)
	}
}

// IPv6: Self's exact fd7a:115c:a1e0::/48 address is its own listener with
// its bracketed Host; IPv4 and IPv6 never wait on each other.
func TestHubIPv6Listener(t *testing.T) {
	fakeTailnetHooks(t)
	setVar(t, &tailnetRetryBase, 50*time.Millisecond)
	setVar(t, &tailnetRetryCap, 200*time.Millisecond)
	// Unit: the address comes from Self, validated in the range.
	h := newHarness(t)
	tailnetWith(hubIP, "fd7a:115c:a1e0::1234:5678", h)
	self, err := tailscaleSelf()
	if err != nil || self.IP6.String() != "fd7a:115c:a1e0::1234:5678" {
		t.Fatalf("self = %+v %v", self, err)
	}
	d := newDashboard(h.openDB(), &daemonLog{}, testAddr)
	hub := d.enableHub(self, "7788")
	if len(hub.addrs) != 2 || hub.addrs[1] != "[fd7a:115c:a1e0::1234:5678]:7788" {
		t.Fatalf("addrs = %v", hub.addrs)
	}
	if w, _ := serve(d, req("GET", "/api/state", "", from(hostAIP, "[fd7a:115c:a1e0::1234:5678]:7788"))); w.Code != 200 {
		t.Fatalf("bracketed IPv6 Host = %d", w.Code)
	}
	for _, bad := range []string{"fd7a:115c:a1e1::1", "fd00::1", "::", "2001:db8::1"} {
		tailnetWith(hubIP, bad, h)
		if s, _ := tailscaleSelf(); s.IP6 != nil {
			t.Errorf("IPv6 %s accepted", bad)
		}
	}

	// IPv6 up while IPv4 cannot bind (use an unused tailnet-range address).
	addrs, err := net.InterfaceAddrs()
	if err != nil {
		t.Fatal(err)
	}
	localIPs := map[string]bool{}
	for _, addr := range addrs {
		switch addr := addr.(type) {
		case *net.IPNet:
			localIPs[addr.IP.String()] = true
		case *net.IPAddr:
			localIPs[addr.IP.String()] = true
		}
	}
	ip4 := ""
	for n := uint32(127<<16 | 255<<8 | 254); n >= 64<<16; n-- {
		ip := net.IPv4(100, byte(n>>16), byte(n>>8), byte(n))
		if !localIPs[ip.String()] {
			ip4 = ip.String()
			break
		}
	}
	if ip4 == "" {
		t.Skip("no non-local IPv4 address in 100.64.0.0/10")
	}
	in6 := inTailnet6
	inTailnet6 = func(ip net.IP) bool { return in6(ip) || ip.Equal(net.IPv6loopback) }
	t.Cleanup(func() { inTailnet6 = in6 })
	h6 := newHarness(t)
	tailnetWith(ip4, "::1", h6)
	port := freePort(t)
	p, s, _ := h6.startHubDaemon(port)
	eventually(t, "IPv6 tailnet up", func() bool { return h6.ok(nil, "daemon", "--status")["tailnet_url"] != nil })
	if code := tailnetGet(t, port, "[::1]:"+port); code != 200 {
		t.Fatalf("IPv6 GET = %d", code)
	}
	s.shutdown()
	p.wait(t)
	if log := h6.daemonLog(); !strings.Contains(log, "serving the tailnet on [::1]:"+port) ||
		!strings.Contains(log, "a tailnet address is unavailable (listen "+ip4+":"+port) {
		t.Fatalf("log:\n%s", log)
	}

	// IPv4 up while IPv6 cannot bind (a range address this host lacks).
	h4 := newHarness(t)
	tailnetWith("::1", "fd7a:115c:a1e0::dead", h4)
	port = freePort(t)
	p, s, _ = h4.startHubDaemon(port)
	eventually(t, "IPv4 tailnet up", func() bool { return h4.ok(nil, "daemon", "--status")["tailnet_url"] != nil })
	if code := tailnetGet(t, port, "hub:"+port); code != 200 {
		t.Fatalf("IPv4 GET = %d", code)
	}
	s.shutdown()
	p.wait(t)
	if log := h4.daemonLog(); !strings.Contains(log, "a tailnet address is unavailable (listen [fd7a:115c:a1e0::dead]:"+port) {
		t.Fatalf("log:\n%s", log)
	}
}

// A dashboard that is not (yet) the hub serves loopback only: any other
// remote address is refused before whois or routing, so no tailnet request
// can arrive ahead of the hub's admission.
func TestNonHubRefusesRemote(t *testing.T) {
	h := newHarness(t)
	d := h.dash()
	for _, ip := range []string{hostAIP, "192.0.2.1", "fd7a:115c:a1e0::1"} {
		if w, _ := serve(d, req("GET", "/api/state", "", func(r *http.Request) { r.RemoteAddr = net.JoinHostPort(ip, "41641") })); w.Code != 403 {
			t.Errorf("%s on a non-hub = %d", ip, w.Code)
		}
	}
	if h.tsCalls("whois|") != 0 {
		t.Fatal("a non-hub ran whois")
	}
	if w, _ := serve(d, req("GET", "/api/state", "")); w.Code != 200 {
		t.Fatalf("loopback on a non-hub = %d", w.Code)
	}
}

// Keep-alives are off: a hub whose whois admission lapses (here it gains a
// tag after the cache TTL) is refused on the very next push, not kept on an
// open connection. (Review round 2, finding 1.)
func TestPeerReusedConnectionExpiresIdentity(t *testing.T) {
	fakeTailnetHooks(t)
	setVar(t, &whoisTTL, 20*time.Millisecond)
	h := newHarness(t)
	tailnet(hubIP, h)
	f := seedDashboard(h)
	var hits atomic.Int32
	target := hubServer(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.Copy(io.Discard, r.Body)
		n := hits.Add(1)
		reply := map[string]any{"hub": hubRef{NodeID: hubNode, Machine: "hub"}, "answers": []any{}}
		if n > 1 { // a v0.6 hub's queued answer: never applied
			reply["answers"] = []any{map[string]any{"relay_id": 88, "ask_id": f.workerAsk, "text": "answer from a now-refused hub"}}
		}
		httpJSON(w, 200, reply)
	}))
	p := newTestPusher(t, h, target)
	defer p.client.CloseIdleConnections()
	if sent := p.push(true); !sent {
		t.Fatal("initial push failed")
	}
	h.write("ts.whois.hub-::1", whoisJSON(hubNode, "hub.example.ts.net.", owner, []string{"tag:ci"}), 0o644)
	time.Sleep(30 * time.Millisecond)
	before := h.tsCalls("whois|")
	sent := p.push(true)
	var n int
	h.openDB().QueryRow(`select count(*) from events where kind = 'answer'`).Scan(&n)
	if sent || hits.Load() != 1 || n != 0 || h.tsCalls("whois|") != before+1 {
		t.Fatalf("lapsed identity reused: sent=%v requests=%d answers=%d whois %d -> %d", sent, hits.Load(), n, before, h.tsCalls("whois|"))
	}
	// Within the TTL, a push still dials anew but the cache answers.
	h.write("ts.whois.hub-::1", whoisJSON(hubNode, "hub.example.ts.net.", owner, nil), 0o644)
	time.Sleep(30 * time.Millisecond) // the cached refusal expires
	if sent := p.push(true); !sent {
		t.Fatal("re-admitted hub refused")
	}
	setVar(t, &whoisTTL, time.Minute)
	calls := h.tsCalls("whois|")
	p.push(true)
	if h.tsCalls("whois|") != calls || hits.Load() != 3 {
		t.Fatalf("cached admission: whois %d -> %d, requests %d", calls, h.tsCalls("whois|"), hits.Load())
	}
}

// A wake-up that finds the forced push already due must force it, whichever
// timer or poke woke the loop. Before this held, a late check timer or a
// poke at the edge left `due` in the past with force off: the loop then
// spun on skipped pushes, never pushed again and never saw cancellation
// (the round-2 end-to-end timeout and its "daemon did not exit"). Here
// pokes arrive constantly while the state never changes.
func TestPeerLoopKeepsForcedPushesUnderPokes(t *testing.T) {
	fakeTailnetHooks(t)
	setVar(t, &peerPushEvery, 30*time.Millisecond)
	setVar(t, &peerCheckEvery, 10*time.Millisecond)
	h := newHarness(t)
	tailnet(hubIP, h)
	var hits atomic.Int32
	target := hubServer(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.Copy(io.Discard, r.Body)
		hits.Add(1)
		httpJSON(w, 200, pushReply{Hub: hubRef{NodeID: hubNode, Machine: "hub"}, Answers: []struct{}{}})
	}))
	p := newTestPusher(t, h, target)
	cx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() { defer close(done); p.run(cx) }()
	end := time.Now().Add(1500 * time.Millisecond)
	for time.Now().Before(end) {
		p.poke()
		time.Sleep(50 * time.Microsecond)
	}
	n := hits.Load()
	cancel()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatalf("the pusher did not stop after cancel (%d pushes)", n)
	}
	if n < 10 { // about 50 at one per 30 ms
		t.Fatalf("%d forced pushes in 1.5 s at a 30 ms interval", n)
	}
}

// The hub derives every machine's attention, marks and tallies on its own
// clock: an older peer (no v0.7 fields) still gets its asks and failed and
// blocked lanes; a newer peer adds work waiting, daemon health and
// milestones; a pushed attention list is ignored; a stale peer shows as
// machine_stale instead of its daemon's health.
func TestHubAttentionOldAndNewPeers(t *testing.T) {
	h := newHarness(t)
	d := h.hubDash()
	h.openDB().Exec(`insert into meta (key, value) values (?, ?)`, heartbeatKey, now())
	const lnxIP, lnxNode = "100.100.1.7", "nLNX1CNTRL"
	h.write("ts.whois."+lnxIP, whoisJSON(lnxNode, "host-b.example.ts.net.", owner, nil), 0o644)
	at := time.Now()
	ts := func(dd time.Duration) string { return stamp(at.Add(-dd)) }

	old := `{"state":{"now":"` + stamp(at) + `","version":"v0.6.0","owner_asks":[{"id":7,"text":"Ship it?","at":"` + ts(time.Minute) +
		`","asker":{"id":1,"name":"host-a-orch","role":"orchestrator"},"root":{"id":1,"name":"host-a-orch","role":"orchestrator"},"asker_is_root":true}],` +
		`"orchestrators":[{"id":1,"name":"host-a-orch","role":"orchestrator","status":"open","created_at":"` + ts(time.Hour) + `","tasks":[` +
		`{"id":2,"parent_id":1,"depth":1,"name":"impl-x","role":"implementer","status":"failed","last_event":{"kind":"fail","summary":"broke","at":"` + ts(3*time.Minute) + `","age_ms":1}},` +
		`{"id":3,"parent_id":1,"depth":1,"name":"rev-x","role":"reviewer","status":"open","agent_status":"blocked","present":true,"observed_at":"` + ts(2*time.Minute) + `"}]}],` +
		`"activity":[],"closed":[]},"now":"` + at.UTC().Format(time.RFC3339Nano) + `","applied":[]}`
	if c, _, body := pushTo(t, d, hostAIP, old); c != 200 {
		t.Fatalf("old peer push = %d %s", c, body)
	}
	ns := peerState(at)
	ns.Orchestrators[0].Name = "host-b-orch"
	ns.Orchestrators[0].Unread = []unreadWork{{EventID: 40, Kind: "done", Summary: "slice", At: ts(15 * time.Minute), Count: 1,
		Recipient: taskRef{ID: 1, Name: "host-b-orch"}, From: taskRef{ID: 4, Name: "impl-y"}}}
	ns.Milestones = []milestone{{ID: 41, Kind: "ready", Text: "APPROVE", At: ts(2 * time.Minute), RootID: 1, Orchestrator: "host-b-orch", LaneID: 4, Lane: "impl-y"}}
	ns.Daemon = &daemonHealth{State: "stale", At: ts(5 * time.Minute)}
	ns.Attention = []attentionItem{{Kind: "owner_ask", AskID: 999, Text: "forged"}}
	if c, _, body := pushTo(t, d, lnxIP, push(ns, at)); c != 200 {
		t.Fatalf("new peer push = %d %s", c, body)
	}

	v := hubView(t, d)
	by := map[string]machineView{}
	for _, m := range v.Machines {
		by[m.Machine] = m
	}
	hostA, lnx := by["host-a"], by["host-b"]
	if hostA.State == nil || lnx.State == nil || len(v.Machines) != 3 {
		t.Fatalf("machines = %+v", v.Machines)
	}
	if got := kindsOf(hostA.State.Attention); got != "owner_ask lane_blocked lane_failed" {
		t.Fatalf("old peer attention = %s", got)
	}
	mo := hostA.State.Orchestrators[0]
	if mo.Tally != tallyRed || mo.Tasks[0].Mark != markFailed || mo.Tasks[1].Mark != markBlocked || hostA.State.Milestones == nil {
		t.Fatalf("old peer orch = %+v milestones %v", mo, hostA.State.Milestones)
	}
	ask := hostA.State.Attention[0]
	if ask.AskID != 7 || ask.Local || ask.NodeID != hostANode || ask.Machine != "host-a" || ask.AgeMS < 55000 {
		t.Fatalf("old peer ask cue = %+v", ask)
	}
	if got := kindsOf(lnx.State.Attention); got != "daemon_unhealthy work_waiting" {
		t.Fatalf("new peer attention = %s", got)
	}
	if lnx.State.Orchestrators[0].Tally != tallyAmber {
		t.Fatalf("new peer tally = %s", lnx.State.Orchestrators[0].Tally)
	}
	// Merged: severity first, every machine's cues, the hub's own daemon healthy.
	if got := kindsOf(v.Attention); got != "owner_ask lane_blocked lane_failed daemon_unhealthy work_waiting" {
		t.Fatalf("merged attention = %s", got)
	}
	if len(v.Milestones) != 1 || v.Milestones[0].Machine != "host-b" || v.Milestones[0].NodeID != lnxNode || v.Milestones[0].AgeMS < 110000 {
		t.Fatalf("merged milestones = %+v", v.Milestones)
	}
	h.openDB().Exec(`update peers set received_at = ? where node_id = ?`, ts(time.Minute), lnxNode)
	v = hubView(t, d)
	if got := kindsOf(v.Attention); got != "owner_ask lane_blocked lane_failed machine_stale work_waiting" {
		t.Fatalf("with a stale peer = %s", got)
	}
}

// fixtureShape is the set of JSON paths and leaf types in v, arrays
// collapsed, so order and values do not count, only the contract.
func fixtureShape(v any, path string, out map[string]bool) {
	switch x := v.(type) {
	case map[string]any:
		out[path+":object"] = true
		for k, e := range x {
			fixtureShape(e, path+"."+k, out)
		}
	case []any:
		out[path+":array"] = true
		for _, e := range x {
			fixtureShape(e, path+"[]", out)
		}
	case nil:
		out[path+":null"] = true
	default:
		out[fmt.Sprintf("%s:%T", path, x)] = true
	}
}

// TestWebStateFixture writes web/src/fixtures/state.json: a hub's
// /api/state over a seeded ledger with every attention and milestone kind,
// an older and a newer peer, with times, ages and
// paths normalized. `pnpm --dir web typecheck` checks it against
// web/src/api.ts, so a renamed Go field breaks the web build. The file is
// rewritten, and this test fails, only when the shape changes.
func TestWebStateFixture(t *testing.T) {
	h := newHarness(t)
	f := seedDashboard(h)
	d := h.hubDash()
	db := h.openDB()
	db.Exec(`insert into meta (key, value) values (?, ?)`, heartbeatKey, stamp(time.Now().Add(-5*time.Minute)))
	h.ok(nil, "next", id(f.a), "merge after the review")
	h.ok(nil, "next", id(f.rev), "post the verdict")
	h.ok(nil, "set", id(f.impl), "pr=3712", "commit=6c85fa1")
	h.ok(nil, "decide", "--as", id(f.a), "Review before merging.")
	for _, k := range []string{"handover", "adopt"} {
		db.Exec(`insert into events (task_id, kind, summary, created_at) values (?, ?, ?, ?)`, f.a, k, k+" note", now())
	}
	planned := h.newTask("gate-a", "gate", f.a, "--planned")
	h.ok(nil, "next", id(planned), "run after the merge")
	waitE := num(h.ok(as(f.bw, f.bwL), "ready", "slice b"), "event_id")
	backdate(t, db, waitE, workWaitingAfter+time.Minute)
	failed := h.newTask("impl-fail", "implementer", f.b, "--pane", "w2:p3", "--report", "r/fail.md")
	h.ok(as(failed, h.launch(failed)), "fail", "tests red")
	db.Exec(`update launches set observed_status = 'blocked', observed_at = ? where task_id = ?`, now(), f.rev)
	db.Exec(`update launches set present = 0, observed_at = ? where task_id = ?`, now(), f.impl)
	db.Exec(`update tasks set agent_name = 'impl-a' where id = ?`, f.impl)
	// impl-a's missing cue folds into its owner ask; this lane has none.
	lost := h.newTask("rs-lost", "researcher", f.b, "--pane", "w2:p4")
	h.launch(lost)
	db.Exec(`update launches set present = 0, observed_at = ? where task_id = ?`, now(), lost)

	at := time.Now()
	old := `{"state":{"now":"` + stamp(at) + `","version":"v0.6.0","owner_asks":[{"id":7,"text":"Ship it?","at":"` + stamp(at) +
		`","asker":{"id":1,"name":"host-a-orch","role":"orchestrator"},"root":{"id":1,"name":"host-a-orch","role":"orchestrator"},"asker_is_root":true}],` +
		`"orchestrators":[{"id":1,"name":"host-a-orch","role":"orchestrator","status":"open","created_at":"` + stamp(at) + `","note":null,"tasks":[]}],` +
		`"activity":[],"closed":[]},"now":"` + at.UTC().Format(time.RFC3339Nano) + `","applied":[]}`
	if c, _, body := pushTo(t, d, hostAIP, old); c != 200 {
		t.Fatalf("old peer push = %d %s", c, body)
	}
	const lnxIP, lnxNode = "100.100.1.7", "nLNX1CNTRL"
	h.write("ts.whois."+lnxIP, whoisJSON(lnxNode, "host-b.example.ts.net.", owner, nil), 0o644)
	ns := peerState(at)
	ns.Milestones = []milestone{{ID: 41, Kind: "ref", Key: "release", Text: "release=v0.7.0", At: stamp(at), RootID: 1, Orchestrator: "host-a-orch"}}
	ns.Daemon = &daemonHealth{State: "fresh"}
	if c, _, body := pushTo(t, d, lnxIP, push(ns, at)); c != 200 {
		t.Fatalf("new peer push = %d %s", c, body)
	}
	db.Exec(`update peers set received_at = ? where node_id = ?`, stamp(at.Add(-time.Minute)), lnxNode)

	w, _ := serve(d, req("GET", "/api/state", "", from("127.0.0.1", testAddr)))
	raw := w.Body.String()
	raw = strings.ReplaceAll(raw, h.dir, "/ledger")
	raw = regexp.MustCompile(`\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z`).ReplaceAllString(raw, "2026-09-29T12:00:00.000Z")
	raw = regexp.MustCompile(`"(age_ms|last_push_age_ms)":\d+`).ReplaceAllString(raw, `"$1":60000`)
	var v map[string]any
	if err := json.Unmarshal([]byte(raw), &v); err != nil {
		t.Fatal(err)
	}
	kinds := map[string]bool{}
	for _, a := range v["attention"].([]any) {
		kinds[a.(map[string]any)["kind"].(string)] = true
		also, _ := a.(map[string]any)["also"].([]any)
		for _, x := range also {
			kinds["also:"+x.(map[string]any)["kind"].(string)] = true
		}
	}
	for _, m := range v["milestones"].([]any) {
		kinds["m:"+m.(map[string]any)["kind"].(string)] = true
	}
	for _, k := range []string{attentionOwnerAsk, attentionBlocked, attentionFailed, attentionMissing, attentionStale, attentionDaemon,
		attentionWorkWaits, "also:" + attentionMissing, "m:ready", "m:fail", "m:decision", "m:handover", "m:adopt", "m:ref", "m:note"} {
		if !kinds[k] {
			t.Fatalf("fixture lacks %s: %v", k, kinds)
		}
	}
	pretty, _ := json.MarshalIndent(v, "", "  ")
	pretty = append(pretty, '\n')
	const path = "web/src/fixtures/state.json"
	want := map[string]bool{}
	fixtureShape(v, "", want)
	have := map[string]bool{}
	if b, err := os.ReadFile(path); err == nil {
		var old any
		if json.Unmarshal(b, &old) == nil {
			fixtureShape(old, "", have)
		}
	}
	if fmt.Sprint(want) != fmt.Sprint(have) {
		if err := os.WriteFile(path, pretty, 0o644); err != nil {
			t.Fatal(err)
		}
		t.Errorf("%s had a different shape and was rewritten: run pnpm --dir web typecheck and commit it", path)
	}
}
