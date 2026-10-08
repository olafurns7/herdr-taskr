//go:build taskr_contract

package main

import (
	"testing"
	"time"
)

func TestContractGoldenFrozenClock(t *testing.T) {
	contractGuard(t)
	t.Setenv("TASKR_FROZEN_NOW", "2000-01-01T12:34:56.789Z")
	want, err := time.Parse(time.RFC3339Nano, "2000-01-01T12:34:56.789Z")
	if err != nil {
		t.Fatal(err)
	}
	if got := clockNow(); !got.Equal(want) {
		t.Fatal("clock not frozen", got)
	}
	if got := now(); got != "2000-01-01T12:34:56.789Z" {
		t.Fatal("stamp not frozen", got)
	}
	if got := clockNow().Sub(want.Add(-time.Minute)); got != time.Minute {
		t.Fatal("age not frozen", got)
	}
}
