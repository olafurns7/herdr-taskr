package main

import (
	"context"
	"errors"
	"net"
	"os"
	"path/filepath"
	"testing"
)

// herdrCommand itself refuses without a server, whatever its caller checked:
// a stale socket (listener gone, file kept) and an absent one give an error,
// no command, and no herdr run.
func TestHerdrCommandRefusesWithoutServer(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	dir, err := os.MkdirTemp("", "hs")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	stale := filepath.Join(dir, "stale.sock")
	ln, err := net.ListenUnix("unix", &net.UnixAddr{Name: stale, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	ln.SetUnlinkOnClose(false)
	ln.Close()
	for name, sock := range map[string]string{"stale": stale, "absent": filepath.Join(dir, "absent.sock"), "empty": ""} {
		cmd, err := herdrCommand(context.Background(), sock, "agent", "list")
		if !errors.Is(err, errNoServer) || cmd != nil {
			t.Fatalf("%s socket: cmd %v, err %v; want no command and errNoServer", name, cmd, err)
		}
	}
	if calls := h.calls(""); len(calls) > 1 {
		t.Fatalf("herdr calls without a server: %q", calls)
	}

	// With a server the command is built and runs the (fake) herdr.
	cmd, err := herdrCommand(context.Background(), h.herdrSock, "agent", "list")
	if err != nil || cmd == nil {
		t.Fatalf("live socket: cmd %v, err %v", cmd, err)
	}
	if err := cmd.Run(); err != nil {
		t.Fatal(err)
	}
	if n := len(h.calls("agent|list|")); n != 1 {
		t.Fatalf("herdr calls with a server = %d, want 1", n)
	}
}
