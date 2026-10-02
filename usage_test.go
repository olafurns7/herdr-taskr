package main

import (
	"encoding/json"
	"testing"
	"time"
)

func TestDashboardUsageFlushMergesPrunesAndSkipsIdle(t *testing.T) {
	h := newHarness(t)
	db := h.openDB()
	at := time.Date(2026, 10, 1, 12, 30, 0, 0, time.UTC)
	idle := &dashboardUsage{}
	if err := idle.flush(db, at); err != nil {
		t.Fatal(err)
	}
	var n int
	if err := db.QueryRow(`select count(*) from meta where key like 'usage:%'`).Scan(&n); err != nil || n != 0 {
		t.Fatalf("idle flush usage keys = %d, %v", n, err)
	}

	hour := at.Add(-time.Hour).Truncate(time.Hour)
	key := "usage:" + hour.Format("2006-01-02T15")
	if err := setMeta(db, key, `{"page":5,"refused":1}`); err != nil {
		t.Fatal(err)
	}
	old31 := "usage:" + at.Add(-31*24*time.Hour).Format("2006-01-02T15")
	old29 := "usage:" + at.Add(-29*24*time.Hour).Format("2006-01-02T15")
	if err := setMeta(db, old31, `{"other":9}`); err != nil {
		t.Fatal(err)
	}
	if err := setMeta(db, old29, `{"other":8}`); err != nil {
		t.Fatal(err)
	}
	u := &dashboardUsage{}
	u.record(usagePage, hour)
	u.record(usageOther, hour)
	u.record(usageOther, hour)
	if err := u.flush(db, at); err != nil {
		t.Fatal(err)
	}

	var counts map[string]int64
	if raw, ok, err := getMeta(db, key); err != nil || !ok || json.Unmarshal([]byte(raw), &counts) != nil {
		t.Fatalf("merged usage = %q, present %v, err %v", raw, ok, err)
	}
	if counts[usagePage] != 6 || counts[usageRefused] != 1 || counts[usageOther] != 2 {
		t.Fatalf("merged usage counts = %v", counts)
	}
	if _, ok, err := getMeta(db, old31); err != nil || ok {
		t.Fatalf("31-day key present = %v, err %v", ok, err)
	}
	if _, ok, err := getMeta(db, old29); err != nil || !ok {
		t.Fatalf("29-day key present = %v, err %v", ok, err)
	}
	u.mu.Lock()
	left := len(u.pending)
	u.mu.Unlock()
	if left != 0 {
		t.Fatalf("pending counts after successful flush = %d", left)
	}
}

func TestDashboardUsageFlushRetainsCountsOnFailure(t *testing.T) {
	h := newHarness(t)
	db := h.openDB()
	at := time.Date(2026, 10, 1, 12, 0, 0, 0, time.UTC)
	hour := at.Format("2006-01-02T15")
	if err := setMeta(db, "usage:"+hour, "not json"); err != nil {
		t.Fatal(err)
	}
	replacement := &dashboardUsage{}
	replacement.record(usagePage, at)
	if err := replacement.flush(db, at); err != nil {
		t.Fatal(err)
	}
	if raw, ok, err := getMeta(db, "usage:"+hour); err != nil || !ok || raw != `{"page":1}` {
		t.Fatalf("replaced usage = %q, present %v, err %v", raw, ok, err)
	}

	failureAt := at.Add(time.Hour)
	failureHour := failureAt.Format("2006-01-02T15")
	u := &dashboardUsage{}
	u.record(usagePage, failureAt)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	if err := u.flush(db, failureAt); err == nil {
		t.Fatal("flush succeeded with a closed database")
	}
	u.mu.Lock()
	left := u.pending[failureHour][usagePage]
	u.mu.Unlock()
	if left != 1 {
		t.Fatalf("pending page count after failed flush = %d, want 1", left)
	}
	db = h.openDB()
	if err := u.flush(db, failureAt); err != nil {
		t.Fatal(err)
	}
	var counts map[string]int64
	raw, _, err := getMeta(db, "usage:"+failureHour)
	if err != nil || json.Unmarshal([]byte(raw), &counts) != nil || counts[usagePage] != 1 {
		t.Fatalf("recovered usage = %q, counts %v, err %v", raw, counts, err)
	}
}
