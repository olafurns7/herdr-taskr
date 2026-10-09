//! A production-poll `taskr wait` returns at once on SIGTERM and clears its marker.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use support::*;

#[test]
fn sigterm_ends_idle_wait_and_clears_marker() {
    let h = Harness::new();
    let root = h.ok(&[], &["new", "orch", "--role", "orchestrator"])["task_id"]
        .as_i64()
        .unwrap();
    let marker = || -> Option<String> {
        h.conn()
            .query_row("select waiting_until from tasks where id=?", [root], |r| {
                r.get(0)
            })
            .unwrap()
    };
    let mut child = h
        .command(&[])
        .args(["wait", "--as", &root.to_string(), "--timeout", "60000"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while marker().is_none() {
        assert!(start.elapsed() < Duration::from_secs(10), "no marker");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Land the signal inside the poll sleep, not at its edge.
    std::thread::sleep(Duration::from_millis(100));
    let sent = Instant::now();
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(3));
            break;
        }
        assert!(
            sent.elapsed() < Duration::from_millis(300),
            "wait still running"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(marker(), None);
}
