package main

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"math"
	"net/http"
	"strings"
	"sync"
	"time"
)

// The hourly request counters of the hub server. Only refused and other are
// counted now; the page, state and push classes belonged to the removed web
// page and peer push, and stay in `daemon --status` as zeros (old hours keep
// their counts) because the status shape is shared with the Rust binary.
const (
	usagePage          = "page"
	usageStateLoopback = "state_loopback"
	usageStateTailnet  = "state_tailnet"
	usagePushAccepted  = "push_accepted"
	usageRefused       = "refused"
	usageOther         = "other"
)

var usageClasses = [...]string{usagePage, usageStateLoopback, usageStateTailnet, usagePushAccepted, usageRefused, usageOther}

type dashboardUsage struct {
	mu      sync.Mutex
	flushMu sync.Mutex
	pending map[string]map[string]int64
}

func (u *dashboardUsage) record(class string, at time.Time) {
	if u == nil {
		return
	}
	hour := at.UTC().Format("2006-01-02T15")
	u.mu.Lock()
	defer u.mu.Unlock()
	if u.pending == nil {
		u.pending = make(map[string]map[string]int64)
	}
	if u.pending[hour] == nil {
		u.pending[hour] = make(map[string]int64)
	}
	u.pending[hour][class]++
}

type statusRecorder struct {
	http.ResponseWriter
	status int
}

func (w *statusRecorder) WriteHeader(status int) {
	if w.status != 0 {
		return
	}
	w.status = status
	w.ResponseWriter.WriteHeader(status)
}

func (w *statusRecorder) Write(p []byte) (int, error) {
	if w.status == 0 {
		w.WriteHeader(http.StatusOK)
	}
	return w.ResponseWriter.Write(p)
}

func (w *statusRecorder) Unwrap() http.ResponseWriter { return w.ResponseWriter }

func dashboardUsageHandler(next http.Handler, usage *dashboardUsage) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		recorder := &statusRecorder{ResponseWriter: w}
		next.ServeHTTP(recorder, r)
		status := recorder.status
		if status == 0 {
			status = http.StatusOK
		}
		usage.record(classifyDashboardUsage(r, status), clockNow())
	})
}

func classifyDashboardUsage(r *http.Request, status int) string {
	if status == http.StatusForbidden || status == http.StatusMisdirectedRequest {
		return usageRefused
	}
	if status < http.StatusOK || status >= http.StatusMultipleChoices {
		return usageOther
	}
	return usageOther
}

func (u *dashboardUsage) flush(db *sql.DB, at time.Time) error {
	if u == nil {
		return nil
	}
	u.flushMu.Lock()
	defer u.flushMu.Unlock()

	u.mu.Lock()
	pending := make(map[string]map[string]int64, len(u.pending))
	for hour, counts := range u.pending {
		pending[hour] = make(map[string]int64, len(counts))
		for class, count := range counts {
			pending[hour][class] = count
		}
	}
	u.mu.Unlock()
	if len(pending) == 0 {
		return nil
	}

	cutoff := at.UTC().Add(-30 * 24 * time.Hour)
	err := withTx(db, func(tx *sql.Tx) error {
		for hour, counts := range pending {
			key := "usage:" + hour
			value, ok, err := getMeta(tx, key)
			if err != nil {
				return err
			}
			merged := map[string]int64{}
			if ok {
				if err := json.Unmarshal([]byte(value), &merged); err != nil || merged == nil {
					merged = map[string]int64{}
				}
			}
			for class, count := range counts {
				merged[class] += count
			}
			encoded, err := json.Marshal(merged)
			if err != nil {
				return err
			}
			if _, err := tx.Exec(`insert into meta (key, value) values (?, ?)
				on conflict(key) do update set value = excluded.value`, key, string(encoded)); err != nil {
				return err
			}
		}

		rows, err := tx.Query(`select key from meta where key like 'usage:%'`)
		if err != nil {
			return err
		}
		var expired []string
		for rows.Next() {
			var key string
			if err := rows.Scan(&key); err != nil {
				rows.Close()
				return err
			}
			hour, err := time.Parse("2006-01-02T15", strings.TrimPrefix(key, "usage:"))
			if err == nil && hour.Before(cutoff) {
				expired = append(expired, key)
			}
		}
		if err := rows.Err(); err != nil {
			rows.Close()
			return err
		}
		if err := rows.Close(); err != nil {
			return err
		}
		for _, key := range expired {
			if _, err := tx.Exec(`delete from meta where key = ?`, key); err != nil {
				return err
			}
		}
		return nil
	})
	if err != nil {
		return err
	}

	u.mu.Lock()
	defer u.mu.Unlock()
	for hour, counts := range pending {
		for class, count := range counts {
			u.pending[hour][class] -= count
			if u.pending[hour][class] == 0 {
				delete(u.pending[hour], class)
			}
		}
		if len(u.pending[hour]) == 0 {
			delete(u.pending, hour)
		}
	}
	return nil
}

// dashboardUsageStatus reads only hourly meta keys; the daemon's minute
// flush means these totals can lag current traffic by up to one minute.
func dashboardUsageStatus(q queryer, at time.Time) (map[string]any, error) {
	h24, d7 := emptyUsageCounts(), emptyUsageCounts()
	rows, err := q.Query(`select key, value from meta where key like 'usage:%'`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	base := at.UTC().Truncate(time.Hour)
	h24Start, d7Start := base.Add(-23*time.Hour), base.Add(-167*time.Hour)
	for rows.Next() {
		var key, value string
		if err := rows.Scan(&key, &value); err != nil {
			return nil, err
		}
		hour, err := time.Parse("2006-01-02T15", strings.TrimPrefix(key, "usage:"))
		if err != nil || hour.Before(d7Start) {
			continue
		}
		var counts map[string]int64
		if err := json.Unmarshal([]byte(value), &counts); err != nil {
			return nil, fmt.Errorf("decode %s: %w", key, err)
		}
		if counts == nil {
			return nil, fmt.Errorf("decode %s: expected an object", key)
		}
		for _, class := range usageClasses {
			if hourCounts := counts[class]; hourCounts != 0 {
				d7[class] += hourCounts
				if !hour.Before(h24Start) {
					h24[class] += hourCounts
				}
			}
		}
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	return map[string]any{
		"h24": h24, "d7": d7,
		"visible_minutes_h24": visibleMinutes(h24),
		"visible_minutes_d7":  visibleMinutes(d7),
	}, nil
}

func emptyUsageCounts() map[string]int64 {
	return map[string]int64{usagePage: 0, usageStateLoopback: 0, usageStateTailnet: 0,
		usagePushAccepted: 0, usageRefused: 0, usageOther: 0}
}

func visibleMinutes(counts map[string]int64) float64 {
	seconds := float64(counts[usageStateLoopback]+counts[usageStateTailnet]) * 3
	return math.Round(seconds/60*10) / 10
}
