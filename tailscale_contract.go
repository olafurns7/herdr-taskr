//go:build taskr_contract

package main

import (
	"net"
	"os"
	"strconv"
	"time"
)

// Executable contract fixtures reuse the same loopback hooks as fakeTailnetHooks.
// This file is absent from production binaries.
func init() {
	if os.Getenv("TASKR_CONTRACT_TAILNET") != "1" {
		return
	}
	inTailnet = func(ip net.IP) bool { return cgnat.Contains(ip) || ip.Equal(net.IPv6loopback) }
	loopbackPeer = func(ip net.IP) bool { return ip.Equal(net.IPv4(127, 0, 0, 1)) }
	hubWhoisArg = func(ip net.IP) string { return "hub-" + ip.String() }
}

func init() {
	if raw := os.Getenv("TASKR_CONTRACT_RETRY_MS"); raw != "" {
		if ms, err := strconv.ParseInt(raw, 10, 64); err == nil && ms >= 0 {
			rpcRetryWindow = func([]string) time.Duration { return time.Duration(ms) * time.Millisecond }
		}
	}
}
