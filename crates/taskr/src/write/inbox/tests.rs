// Test-only FFI: count BEGIN IMMEDIATE through an SQLite trace hook.
#![allow(unsafe_code)]
use std::{
    ffi::{CStr, c_char, c_int, c_uint, c_void},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
use taskr_core::{db::rusqlite::ffi, store};

static BEGINS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn traced(_: c_uint, _: *mut c_void, _: *mut c_void, sql: *mut c_void) -> c_int {
    let sql = unsafe { CStr::from_ptr(sql as *const c_char) }.to_bytes();
    if sql.len() >= 15 && sql[..15].eq_ignore_ascii_case(b"BEGIN IMMEDIATE") {
        BEGINS.fetch_add(1, Ordering::Relaxed);
    }
    0
}
// Every connection opened after registration counts its BEGIN IMMEDIATE.
unsafe extern "C" fn trace_all(
    db: *mut ffi::sqlite3,
    _: *mut *mut c_char,
    _: *const ffi::sqlite3_api_routines,
) -> c_int {
    unsafe {
        ffi::sqlite3_trace_v2(
            db,
            ffi::SQLITE_TRACE_STMT,
            Some(traced),
            std::ptr::null_mut(),
        )
    };
    ffi::SQLITE_OK
}
fn run(args: &[&str]) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    crate::write::dispatch(true, &args).expect("write command");
}

/// An idle wait takes Go's write locks (two offers per 1 s poll) and still
/// wakes within one poll of a new event. Runs in a child with a scratch ledger.
#[test]
fn wait_polls_like_go() {
    if std::env::var_os("TASKR_WAIT_POLL_CHILD").is_none() {
        let dir = std::env::temp_dir().join(format!("taskr-wait-poll-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "write::inbox::tests::wait_polls_like_go",
                "--nocapture",
            ])
            .env_clear()
            .env("TASKR_WAIT_POLL_CHILD", "1")
            .env("HOME", &dir)
            .env("TASKR_DB", dir.join("taskr.db"))
            .env("HERDR_SOCKET_PATH", dir.join("absent.sock"))
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    run(&["new", "top", "--role", "orchestrator"]);
    unsafe { ffi::sqlite3_auto_extension(Some(trace_all)) };
    let start = Instant::now();
    run(&["wait", "--as", "1", "--timeout", "3000"]);
    let begins = BEGINS.load(Ordering::Relaxed);
    assert!(start.elapsed() >= Duration::from_millis(2900));
    // Go takes 10: open, 7 offers, the waiting_until marker and its clear.
    // Before the fix Rust took about 160 (3 per 50 ms poll).
    assert!(
        begins <= 12,
        "{begins} write transactions in a 3 s idle wait"
    );

    let writer = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(1500));
        let mut db = taskr_core::db::open(&taskr_core::db::path().unwrap()).unwrap();
        store::transaction(&mut db, |tx| {
            store::event(
                tx,
                store::Event {
                    task: 1,
                    to: Some(1),
                    kind: "note",
                    summary: "wake",
                    ..store::Event::default()
                },
            )
        })
        .unwrap();
        Instant::now()
    });
    run(&["wait", "--as", "1", "--timeout", "10000"]);
    let woke = Instant::now();
    let wrote = writer.join().unwrap();
    assert!(
        woke.duration_since(wrote) < Duration::from_millis(1500),
        "woke {:?} after the event",
        woke.duration_since(wrote)
    );
}

/// `wait --for pr` wakes on the poller's kind (code `pu`), and an older `pr` event is
/// coalesced into a newer one for the same PR, filtered or not.
#[test]
fn wait_for_pr_coalesces_per_pr() {
    assert_eq!(super::code("pr"), "pu");
    if std::env::var_os("TASKR_WAIT_PR_CHILD").is_none() {
        let dir = std::env::temp_dir().join(format!("taskr-wait-pr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "write::inbox::tests::wait_for_pr_coalesces_per_pr",
                "--nocapture",
            ])
            .env_clear()
            .env("TASKR_WAIT_PR_CHILD", "1")
            .env("HOME", &dir)
            .env("TASKR_DB", dir.join("taskr.db"))
            .env("HERDR_SOCKET_PATH", dir.join("absent.sock"))
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    run(&["new", "top", "--role", "orchestrator"]);
    let mut db = taskr_core::db::open(&taskr_core::db::path().unwrap()).unwrap();
    let mut ids = vec![];
    for (pr, sub) in [
        ("o/r#1", "blocked"),
        ("o/r#2", "dirty"),
        ("o/r#1", "checks_green"),
        ("o/r#2", "behind"),
    ] {
        ids.push(
            store::transaction(&mut db, |tx| {
                store::event(
                    tx,
                    store::Event {
                        task: 1,
                        to: Some(1),
                        kind: "pr",
                        summary: sub,
                        data: Some(serde_json::json!({"pr":pr,"sub":sub})),
                        ..store::Event::default()
                    },
                )
            })
            .unwrap(),
        );
    }
    let pending = |db: &taskr_core::db::Connection| -> i64 {
        db.query_row("select pending_event_id from tasks where id=1", [], |r| {
            r.get(0)
        })
        .unwrap()
    };
    // Filtered: #1's blocked and #2's dirty are superseded; checks_green comes first.
    run(&["wait", "--as", "1", "--for", "pr", "--timeout", "1000"]);
    assert_eq!(pending(&db), ids[2]);
    // A pending event replays and is never coalesced.
    run(&["wait", "--as", "1", "--timeout", "1000"]);
    assert_eq!(pending(&db), ids[2]);
    run(&[
        "wait",
        "--as",
        "1",
        "--ack",
        &ids[2].to_string(),
        "--timeout",
        "1000",
    ]);
    assert_eq!(pending(&db), ids[3]);
}
