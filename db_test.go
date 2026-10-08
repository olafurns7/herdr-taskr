package main

import (
	"database/sql"
	"testing"
	"time"
)

// A panic inside withTx must not leave the immediate transaction holding the
// writer lock: net/http recovers handler panics and the daemon keeps writing.
func TestWithTxPanicReleasesWriterLock(t *testing.T) {
	contractGuard(t)
	h := newHarness(t)
	db := h.openDB()
	func() {
		defer func() {
			if recover() == nil {
				t.Fatal("fn did not panic")
			}
		}()
		withTx(db, func(tx *sql.Tx) error {
			if _, err := tx.Exec(`insert into meta (key, value) values ('panicked', 'x')`); err != nil {
				t.Fatal(err)
			}
			panic("fault")
		})
	}()
	start := time.Now()
	if err := setMeta(db, heartbeatKey, now()); err != nil {
		t.Fatalf("write after a panicked transaction: %v", err)
	}
	if d := time.Since(start); d > time.Second {
		t.Fatalf("write after a panicked transaction took %v (busy_timeout is 5 s)", d)
	}
	if _, ok, _ := getMeta(db, "panicked"); ok {
		t.Fatal("the panicked transaction's write was committed")
	}
}
