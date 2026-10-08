//go:build !taskr_contract

package main

import "time"

// clockNow is real time in production; only contract builds can freeze it.
func clockNow() time.Time { return time.Now() }
