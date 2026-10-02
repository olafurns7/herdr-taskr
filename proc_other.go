//go:build !darwin && !linux

package main

import "errors"

// procIdentity is unknown here, so daemon --restart refuses to signal.
func procIdentity(pid int) (procIdent, error) {
	return procIdent{}, errors.New("process identity is not available on this platform")
}
