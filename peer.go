package main

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"net"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"time"
)

// Reaching the hub from another host: the URL check and the verified dial
// that the RPC client uses. hub.url is the pre-0.9 peer setting; the daemon
// no longer pushes anything to the hub named there, and `daemon --status`
// still reports it as role "peer".

const (
	hubURLFile = "hub.url"
	peerOKKey  = "peer_last_push_ok_at" // read by --status; no longer written
	peerErrKey = "peer_last_push_error"
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
// name). It returns http://HOST:PORT.
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
	return "http://" + u.Host, nil
}

// verifiedConn is a connection to a hub that whois admitted before any
// request byte was written on it.
type verifiedConn struct {
	net.Conn
	id     peerIdent
	handed atomic.Bool // given to the transport; a connection serves one request
}

type verifiedConnKey struct{}

// transportDial hands the transport the connection dialHub dialled and
// verified for this request, once. The transport never dials on its own,
// so no request can go out on an unverified or reused connection.
func transportDial(cx context.Context, _, _ string) (net.Conn, error) {
	if vc, ok := cx.Value(verifiedConnKey{}).(*verifiedConn); ok && vc.handed.CompareAndSwap(false, true) {
		return vc, nil
	}
	return nil, errors.New("no verified hub connection for this request")
}

// dialHub connects to the hub, then runs whois on the address it actually
// reached and refuses it unless it is a tailnet node of this machine's user
// without tags. Nothing is sent to a hub that fails this.
func dialHub(cx context.Context, whois *whoisCache, network, addr string) (net.Conn, error) {
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
	id, err := whois.admit(ta.IP)
	if err != nil {
		c.Close()
		return nil, fmt.Errorf("hub not verified: %v", err)
	}
	return &verifiedConn{Conn: c, id: id}, nil
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
