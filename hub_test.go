package main

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
	"time"
)

// fakeTailscale answers from files next to it: ts.ip for `ip -4`,
// ts.status for `status --json`, ts.whois.<ip> for `whois --json <ip>`
// (with ts.whois.<ip>.sleep and ts.whois.<ip>.exit). A missing file is a
// failure, like a stopped tailscaled. Any other command is refused.
const fakeTailscale = `#!/bin/sh
d="${0%/*}"
printf '%s|' "$@" >> "$d/ts.calls"
echo >> "$d/ts.calls"
case "$1" in
ip)
  [ "$2" = "-4" ] && [ -f "$d/ts.ip" ] && exec cat "$d/ts.ip"
  echo "not running" >&2; exit 1 ;;
status)
  [ "$2" = "--json" ] && [ -f "$d/ts.status" ] && exec cat "$d/ts.status"
  echo "not running" >&2; exit 1 ;;
whois)
  f="$d/ts.whois.$3"
  [ "$2" = "--json" ] || exit 2
  [ -f "$f.sleep" ] && sleep "$(cat "$f.sleep")"
  [ -f "$f.exit" ] && exit "$(cat "$f.exit")"
  [ -f "$f" ] && exec cat "$f"
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
	t.Setenv("TASKR_CONTRACT_TAILNET", "1")
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
	contractGuard(t)
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
	contractGuard(t)
	h := newHarness(t)
	setVar(t, &tailscaleTimeout, 500*time.Millisecond)
	d := h.hubDash()
	host := "hub:7788"
	get := func(ip string, mods ...func(*http.Request)) int {
		w, _ := serve(d, req("GET", probePath, "", append([]func(*http.Request){from(ip, host)}, mods...)...))
		return w.Code
	}
	if c := get(hostAIP); c != admitted {
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
	if c := get("127.0.0.1", func(r *http.Request) { r.Host = testAddr }); c != admitted {
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
	contractGuard(t)
	h := newHarness(t)
	setVar(t, &whoisTTL, 300*time.Millisecond)
	d := h.hubDash()
	get := func(ip string) int {
		w, _ := serve(d, req("GET", probePath, "", from(ip, "hub:7788")))
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
	if c := get(errIP); c != admitted || whois(errIP) != 3 {
		t.Fatalf("fixed node after the TTL = %d, whois %d", c, whois(errIP))
	}
}

func TestHubHostVariants(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	d := h.hubDash()
	for _, host := range []string{"127.0.0.1:7788", "localhost:7788", hubIP + ":7788", "hub:7788",
		"hub.example.ts.net:7788", "hub.example.ts.net.:7788", "HUB.example.ts.net:7788"} {
		if w, _ := serve(d, req("GET", probePath, "", from(hostAIP, host))); w.Code != admitted {
			t.Errorf("Host %s = %d", host, w.Code)
		}
	}
	for _, host := range []string{"evil.example:7788", "hub:9999", hubIP + ":80", "hub", "localhost:9",
		"other.example.ts.net:7788", "hub.evil.example:7788", "100.64.0.20:7788"} {
		if w, _ := serve(d, req("GET", probePath, "", from(hostAIP, host))); w.Code != 421 {
			t.Errorf("Host %s = %d, want 421", host, w.Code)
		}
	}
	// Over the tailnet too, the old answer routes are gone.
	f := seedDashboard(h)
	for _, path := range []string{fmt.Sprintf("/api/asks/%d/answer", f.rootAsk), fmt.Sprintf("/api/peers/%s/asks/1/answer", hostANode)} {
		if w, _ := serve(d, req("POST", path, `{"text":"from the phone"}`, from(hostAIP, "hub.example.ts.net:7788"))); w.Code != 404 {
			t.Fatalf("tailnet POST %s = %d", path, w.Code)
		}
	}
}

func TestDashboardConfigTailnet(t *testing.T) {
	contractGuard(t)
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
	contractGuard(t)
	suffix := func() (string, error) { return "example.ts.net", nil }
	ok := map[string]string{
		"http://hub.example.ts.net:7788":   "http://hub.example.ts.net:7788",
		"http://hub.example.ts.net.:7788/": "http://hub.example.ts.net.:7788",
		"http://100.64.0.10:7788":          "http://100.64.0.10:7788",
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

func TestHubWithoutTailscaleServesLoopback(t *testing.T) {
	contractGuard(t)
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
	s.shutdown()
	p.wait(t)
	if l := dashLines(h.daemonLog()); len(l) != 2 || !strings.Contains(l[0], "dashboard: tailnet unavailable (tailscale ip") ||
		!strings.Contains(l[1], "dashboard at") {
		t.Fatalf("dashboard lines = %q", l)
	}
}

func TestLocalRole(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	if st := h.ok(nil, "daemon", "--status"); st["role"] != "local" || st["tailnet_url"] != nil || st["hub_url"] != nil {
		t.Fatalf("status = %v", st)
	}
}

var _ = sql.ErrNoRows

// Same-login tagged node: only the tag guard refuses it. Each whois cache
// map is cleared at its cap before one more entry. Port 80 accepts bare
// names.
func TestHubTaggedOwnerCacheCapAndPort80(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	d := h.hubDash()
	if w, _ := serve(d, req("GET", probePath, "", from(taggedOwnerIP, "hub:7788"))); w.Code != 403 {
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
		if w, _ := serve(d80, req("GET", probePath, "", from(hostAIP, host))); w.Code != admitted {
			t.Errorf("port 80 Host %s = %d", host, w.Code)
		}
	}
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
	r, _ := http.NewRequest("GET", "http://[::1]:"+port+probePath, nil)
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
	contractGuard(t)
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
	if c := tailnetGet(t, port, "hub:"+port); c != admitted {
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
	contractGuard(t)
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
	contractGuard(t)
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
	time.Sleep(300 * time.Millisecond) // several failed retries
	tailnet("::1", h)
	eventually(t, "tailnet up after tailscale", func() bool {
		return h.ok(nil, "daemon", "--status")["tailnet_url"] != nil
	})
	if code := tailnetGet(t, port, "hub.example.ts.net:"+port); code != admitted {
		t.Fatalf("tailnet GET = %d", code)
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
	contractGuard(t)
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
	if w, _ := serve(d, req("GET", probePath, "", from(hostAIP, "[fd7a:115c:a1e0::1234:5678]:7788"))); w.Code != admitted {
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
	if code := tailnetGet(t, port, "[::1]:"+port); code != admitted {
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
	if code := tailnetGet(t, port, "hub:"+port); code != admitted {
		t.Fatalf("IPv4 GET = %d", code)
	}
	s.shutdown()
	p.wait(t)
	if log := h4.daemonLog(); !strings.Contains(log, "a tailnet address is unavailable (listen [fd7a:115c:a1e0::dead]:"+port) {
		t.Fatalf("log:\n%s", log)
	}
}

// A server that is not (yet) the hub serves loopback only: any other
// remote address is refused before whois or routing, so no tailnet request
// can arrive ahead of the hub's admission.
func TestNonHubRefusesRemote(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	d := h.dash()
	for _, ip := range []string{hostAIP, "192.0.2.1", "fd7a:115c:a1e0::1"} {
		if w, _ := serve(d, req("GET", probePath, "", func(r *http.Request) { r.RemoteAddr = net.JoinHostPort(ip, "41641") })); w.Code != 403 {
			t.Errorf("%s on a non-hub = %d", ip, w.Code)
		}
	}
	if h.tsCalls("whois|") != 0 {
		t.Fatal("a non-hub ran whois")
	}
	if w, _ := serve(d, req("GET", probePath, "")); w.Code != admitted {
		t.Fatalf("loopback on a non-hub = %d", w.Code)
	}
}
