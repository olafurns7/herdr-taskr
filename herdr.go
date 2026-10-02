package main

import (
	"bufio"
	"context"
	"encoding/json"
	"io"
	"net"
	"os"
	"os/exec"
	"time"
)

const capacityReplyLimit = 256 * 1024

// This read-only path never executes the CLI, even if the server disappears.
func herdrCapacityRequest(sock, method string, params map[string]any, deadline time.Time, result any) error {
	if method != "agent.get" && method != "agent.read" {
		return herdrErr("unsupported capacity method")
	}
	if limit := time.Now().Add(500 * time.Millisecond); limit.Before(deadline) {
		deadline = limit
	}
	cx, cancel := context.WithDeadline(context.Background(), deadline)
	defer cancel()
	var dialer net.Dialer
	conn, err := dialer.DialContext(cx, "unix", sock)
	if err != nil {
		return herdrErr("capacity socket unavailable")
	}
	defer conn.Close()
	if err := conn.SetDeadline(deadline); err != nil {
		return herdrErr("capacity socket deadline failed")
	}
	const requestID = "taskr-capacity"
	if err := json.NewEncoder(conn).Encode(map[string]any{"id": requestID, "method": method, "params": params}); err != nil {
		return herdrErr("capacity request failed")
	}
	line, err := bufio.NewReader(io.LimitReader(conn, capacityReplyLimit+1)).ReadBytes('\n')
	if err != nil || len(line) > capacityReplyLimit {
		return herdrErr("capacity response unavailable or oversized")
	}
	var reply struct {
		ID     string          `json:"id"`
		Error  json.RawMessage `json:"error"`
		Result json.RawMessage `json:"result"`
	}
	if json.Unmarshal(line, &reply) != nil || reply.ID != requestID ||
		(len(reply.Error) != 0 && string(reply.Error) != "null") || len(reply.Result) == 0 || string(reply.Result) == "null" {
		return herdrErr("invalid capacity response")
	}
	if json.Unmarshal(reply.Result, result) != nil {
		return herdrErr("invalid capacity result")
	}
	return nil
}

// Owner rule: nothing taskr runs may start a Herdr server. The herdr CLI
// starts one when no server answers on its socket, and that server (and every
// pane it later opens) would inherit the calling agent's environment. So every
// herdr CLI call goes through herdrCommand, which first dials the socket and
// runs nothing when no server accepts.

var serverDialTimeout = 500 * time.Millisecond

// serverUp reports whether a Herdr server accepts on sock: a dial with a
// short timeout, closed at once.
func serverUp(sock string) bool {
	if sock == "" {
		return false
	}
	c, err := net.DialTimeout("unix", sock, serverDialTimeout)
	if err != nil {
		return false
	}
	c.Close()
	return true
}

// errNoServer is returned by herdrCommand when no Herdr server accepts.
var errNoServer = herdrErr("Herdr server not reachable; no herdr command run")

// herdrCommand is the only way taskr builds a herdr CLI command: it returns
// errNoServer, and no command, unless a server accepts on sock.
func herdrCommand(cx context.Context, sock string, args ...string) (*exec.Cmd, error) {
	if !serverUp(sock) {
		return nil, errNoServer
	}
	cmd := exec.CommandContext(cx, "herdr", args...)
	cmd.Env = append(os.Environ(), "HERDR_SOCKET_PATH="+sock) // the server just checked, not herdr's own default
	return cmd, nil
}
