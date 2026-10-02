package main

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"

	"golang.org/x/sys/unix"
)

// procIdentity reads a process's identity from the kernel on macOS:
// kern.procargs2 gives the path the kernel executed and the exact argv;
// kern.proc.pid gives the start time (microseconds) and the effective uid.
// The executable path is the exec path from procargs2 (the path execve was
// given, which for a daemon started by absolute path is the binary's path),
// resolved like the daemon resolves its own.
func procIdentity(pid int) (procIdent, error) {
	raw, err := unix.SysctlRaw("kern.procargs2", pid)
	if err != nil {
		return procIdent{}, err
	}
	if len(raw) < 4 {
		return procIdent{}, errors.New("short kern.procargs2")
	}
	argc := int(binary.LittleEndian.Uint32(raw[:4]))
	rest := raw[4:]
	i := bytes.IndexByte(rest, 0)
	if i <= 0 {
		return procIdent{}, errors.New("kern.procargs2 has no exec path")
	}
	id := procIdent{Exe: string(rest[:i])}
	rest = bytes.TrimLeft(rest[i:], "\x00")
	for len(id.Argv) < argc {
		j := bytes.IndexByte(rest, 0)
		if j < 0 {
			return procIdent{}, errors.New("kern.procargs2 argv is truncated")
		}
		id.Argv, rest = append(id.Argv, string(rest[:j])), rest[j+1:]
	}
	kp, err := unix.SysctlKinfoProc("kern.proc.pid", pid)
	if err != nil {
		return procIdent{}, err
	}
	if int(kp.Proc.P_pid) != pid {
		return procIdent{}, fmt.Errorf("no process %d", pid)
	}
	st := kp.Proc.P_starttime
	id.Start = fmt.Sprintf("%d.%06d", st.Sec, st.Usec)
	id.UID = int(kp.Eproc.Ucred.Uid)
	return id, nil
}
