//go:build taskr_contract

package main

import (
	"os"
	"time"
)

// Contract-only clock. Deadlines still advance in normal oracle invocations.
func clockNow() time.Time {
	if raw := os.Getenv("TASKR_FROZEN_NOW"); raw != "" {
		frozen, err := time.Parse(time.RFC3339Nano, raw)
		if err != nil {
			panic("invalid TASKR_FROZEN_NOW: " + err.Error())
		}
		return frozen
	}
	return time.Now()
}
