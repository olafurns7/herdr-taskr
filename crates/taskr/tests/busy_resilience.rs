#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use std::time::{Duration, Instant};
use support::Harness;

#[test]
fn current_ledger_read_succeeds_while_writer_is_locked() {
    let h = Harness::new();
    h.ok(&[], &["new", "read-root", "--role", "orchestrator"]);
    let blocker = h.conn();
    blocker.execute_batch("begin immediate").unwrap();
    let result = h.output(&[], &["--json", "status"], b"");
    blocker.execute_batch("rollback").unwrap();
    assert!(result.status.success(), "{result:?}");
    assert!(String::from_utf8_lossy(&result.stdout).contains("read-root"));
}

#[test]
fn cli_write_waits_past_five_seconds() {
    let h = Harness::new();
    let root = h.ok(&[], &["new", "write-root", "--role", "orchestrator"])["task_id"]
        .as_i64()
        .unwrap();
    let blocker = h.conn();
    blocker.execute_batch("begin immediate").unwrap();
    let (stop, release) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _ = release.recv_timeout(Duration::from_secs(8));
        blocker.execute_batch("rollback").unwrap();
    });
    let started = Instant::now();
    let result = h.output(&[], &["note", "after-lock", "--as", &root.to_string()], b"");
    let _ = stop.send(());
    holder.join().unwrap();
    assert!(result.status.success(), "{result:?}");
    assert!(started.elapsed() >= Duration::from_secs(7));
    assert_eq!(
        h.count(
            "select count(*) from events where kind='note' and summary='after-lock' and task_id=?",
            root
        ),
        1
    );
}

#[test]
fn current_version_open_repairs_search() {
    use taskr_core::schema::{SCHEMA_VERSION, SEARCH_TRIGGER};
    for missing in ["table", "trigger", "old-trigger", "secure-delete"] {
        let h = Harness::new();
        let root = h.ok(&[], &["new", "repair-root", "--role", "orchestrator"])["task_id"]
            .as_i64()
            .unwrap();
        let root_arg = root.to_string();
        let file = h.home.join("goal.md");
        std::fs::write(&file, "repairtoken document").unwrap();
        h.ok(
            &[],
            &[
                "doc",
                "set",
                &root_arg,
                "goal",
                "--file",
                file.to_str().unwrap(),
            ],
        );
        h.ok(&[], &["note", "repairtoken old note", "--as", &root_arg]);
        let db = h.conn();
        if missing == "old-trigger" {
            let old = SEARCH_TRIGGER.replace(", 'owner_answer'", "");
            db.execute_batch("drop trigger search_events_insert")
                .unwrap();
            db.execute_batch(&old).unwrap();
        }
        db.execute("insert into events(task_id,kind,summary,created_at) values(?,'owner_answer','repairtoken old owner answer','2026-10-09T00:00:00Z')", [root]).unwrap();
        match missing {
            "table" => db.execute_batch("drop table search_fts").unwrap(),
            "trigger" => db
                .execute_batch("drop trigger search_events_insert")
                .unwrap(),
            "secure-delete" => db
                .execute_batch(
                    "insert into search_fts(search_fts, rank) values('secure-delete', 0)",
                )
                .unwrap(),
            _ => {}
        }
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        let status = h.output(&[], &["--json", "status"], b"");
        assert!(status.status.success(), "{missing}: {status:?}");
        h.ok(&[], &["note", "repairtoken new note", "--as", &root_arg]);
        let search = h.output(&[], &["--json", "search", "repairtoken"], b"");
        assert!(search.status.success(), "{missing}: {search:?}");
        assert_eq!(
            String::from_utf8(search.stdout).unwrap().lines().count(),
            4,
            "{missing}"
        );
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(
            db.query_row(
                "select sql from sqlite_master where name='search_events_insert'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            SEARCH_TRIGGER,
            "{missing}"
        );
        assert_eq!(
            db.query_row(
                "select v from search_fts_config where k='secure-delete'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1,
            "{missing}"
        );
        // Distinctive rowids detect an unnecessary rebuild on the next current-version open.
        db.execute_batch("update search_fts set rowid=rowid+1000")
            .unwrap();
        let status = h.output(&[], &["--json", "status"], b"");
        assert!(status.status.success(), "{missing}: {status:?}");
        assert_eq!(
            db.query_row(
                "select count(*), coalesce(sum(rowid>1000),0) from search_fts",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            )
            .unwrap(),
            (4, 4),
            "{missing}"
        );
        let search = h.output(&[], &["--json", "search", "repairtoken"], b"");
        assert!(search.status.success(), "{missing}: {search:?}");
        assert_eq!(
            String::from_utf8(search.stdout).unwrap().lines().count(),
            4,
            "{missing}"
        );
    }
}
