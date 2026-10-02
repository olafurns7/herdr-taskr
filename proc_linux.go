package main

import (
	"errors"
	"os"
	"strconv"
	"strings"
	"syscall"
)

// procIdentity reads a process's identity from /proc on Linux: the resolved
// /proc/<pid>/exe link, the NUL-separated cmdline, the start time in clock
// ticks since boot (field 22 of stat), and the owner of /proc/<pid>.
func procIdentity(pid int) (procIdent, error) {
	dir := "/proc/" + strconv.Itoa(pid)
	exe, err := os.Readlink(dir + "/exe")
	if err != nil {
		return procIdent{}, err
	}
	exe = replacedExe(exe)
	cmdline, err := os.ReadFile(dir + "/cmdline")
	if err != nil {
		return procIdent{}, err
	}
	stat, err := os.ReadFile(dir + "/stat")
	if err != nil {
		return procIdent{}, err
	}
	// The command name in field 2 may hold spaces and parentheses; the
	// fields after the last ")" start at field 3.
	i := strings.LastIndexByte(string(stat), ')')
	f := strings.Fields(string(stat)[i+1:])
	if i < 0 || len(f) < 20 {
		return procIdent{}, errors.New("unreadable /proc stat")
	}
	fi, err := os.Stat(dir)
	if err != nil {
		return procIdent{}, err
	}
	st, ok := fi.Sys().(*syscall.Stat_t)
	if !ok {
		return procIdent{}, errors.New("no owner for " + dir)
	}
	return procIdent{Exe: exe, Argv: strings.Split(strings.TrimSuffix(string(cmdline), "\x00"), "\x00"),
		Start: f[19], UID: int(st.Uid)}, nil
}
