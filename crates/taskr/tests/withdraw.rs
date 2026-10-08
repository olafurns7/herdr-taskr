//! `answer --withdraw` authority, its stored data, and handover, through the CLI.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use serde_json::{Value, json};
use support::*;

fn ask(h: &Harness, lane: &Env) -> i64 {
    h.ok(lane, &["ask", "Merge old change?", "--owner", "--blocking"])["event_id"]
        .as_i64()
        .unwrap()
}
fn state(h: &Harness) -> (i64, i64) {
    h.conn()
        .query_row(
            "select (select count(*) from events),(select count(*) from events where answered_by is not null)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}
fn rejected(h: &Harness, env: &Env, args: &[&str]) {
    let before = state(h);
    let out = h.output(env, args, b"");
    assert_eq!(
        out.status.code(),
        Some(6),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(state(h), before, "{args:?} wrote");
}

#[test]
fn withdraw_authority_and_handover() {
    let h = Harness::new();
    let (_w, _l, lane) = h.lane("claude", "implementer");
    let a = ask(&h, &lane).to_string();
    // A lane naming its root with --as.
    rejected(
        &h,
        &lane,
        &["answer", &a, "--withdraw", "stale", "--as", "1"],
    );
    // Another root's TASKR_TASK naming a hub root.
    let other = h.ok(&[], &["new", "other", "--role", "orchestrator"])["task_id"]
        .as_i64()
        .unwrap()
        .to_string();
    rejected(
        &h,
        &vec![("TASKR_TASK".into(), other.clone())],
        &["answer", &a, "--withdraw", "stale", "--as", "1"],
    );
    // The asker's root may.
    h.ok(&[], &["answer", &a, "--withdraw", "stale", "--as", "1"]);
    let data: String = h
        .conn()
        .query_row("select data from events where kind='answer'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&data).unwrap(),
        json!({"owner":false,"withdrawn":true})
    );
    // A withdrawn ask is not an owner decision in the handover; a real answer still is.
    let b = ask(&h, &lane).to_string();
    h.ok(&[], &["answer", &b, "ship it", "--as", "1"]);
    let out = h.output(&[], &["handover", "--as", "1"], b"");
    assert!(out.status.success());
    let md = String::from_utf8(out.stdout).unwrap();
    assert!(!md.contains("Answer: stale"), "{md}");
    assert!(
        md.contains(&format!("- Owner answer to ask {b} from impl")),
        "{md}"
    );
    assert!(md.contains("Answer: ship it"), "{md}");
}
