//! Safety behaviours formerly exercised only by the Go executable adapter.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;

use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    process::Stdio,
    time::{Duration, Instant},
};
use support::*;
use taskr_core::db::params;

fn rejected(h: &Harness, env: &[(String, String)], args: &[&str], code: i32, message: &str) {
    let before: i64 = h
        .conn()
        .query_row("select count(*) from events", [], |r| r.get(0))
        .unwrap();
    let out = h.output(env, &[&["--json"][..], args].concat(), b"");
    assert_eq!(out.status.code(), Some(code), "{args:?}: {out:?}");
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        value["error"].as_str().unwrap().contains(message),
        "{args:?}: {value}"
    );
    let after: i64 = h
        .conn()
        .query_row("select count(*) from events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, before, "rejected call wrote: {args:?}");
}

fn new_task(h: &Harness, name: &str, role: &str, parent: i64) -> i64 {
    let parent = parent.to_string();
    let mut args = vec!["new", name, "--role", role];
    if parent != "0" {
        args.extend(["--parent", &parent]);
    }
    h.ok(&[], &args)["task_id"].as_i64().unwrap()
}

fn launch(h: &Harness, task: i64) -> Env {
    let id = h.ok(
        &[],
        &[
            "launch",
            &task.to_string(),
            "--provider",
            "claude",
            "--model",
            "test",
            "--effort",
            "medium",
        ],
    )["launch_id"]
        .as_i64()
        .unwrap();
    vec![
        ("TASKR_TASK".into(), task.to_string()),
        ("TASKR_LAUNCH".into(), id.to_string()),
    ]
}

#[test]
fn root_as_rejects_worker_launch_mismatch_closed_and_missing_identities() {
    let h = Harness::new();
    let (worker, _, env) = h.lane("claude", "implementer");
    let other = new_task(&h, "other", "orchestrator", 0);
    let child = new_task(&h, "child", "implementer", 1);
    let legacy = new_task(&h, "legacy", "orchestrator", 0);
    h.conn().execute("insert into launches(task_id,provider,model,effort,recorded_at) values(?,'claude','test','medium','2026-01-01T00:00:00Z')", [legacy]).unwrap();
    let legacy_launch = h
        .conn()
        .query_row("select id from launches where task_id=?", [legacy], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    h.conn()
        .execute(
            "update tasks set current_launch_id=? where id=?",
            params![legacy_launch, legacy],
        )
        .unwrap();
    let closed = new_task(&h, "closed", "orchestrator", 0);
    h.ok(&[], &["close", &closed.to_string()]);
    for (identity, command, task, message) in [
        (vec![], "note", child, "has parent"),
        (env.clone(), "note", worker, "TASKR_LAUNCH is set"),
        (vec![], "ask", worker, "has parent"),
        (vec![], "note", legacy, "has launch"),
        (
            vec![("TASKR_TASK".into(), other.to_string())],
            "note",
            1,
            "different task",
        ),
        (
            vec![("TASKR_TASK".into(), child.to_string())],
            "ask",
            1,
            "different task",
        ),
        (vec![], "note", closed, "is closed"),
        (vec![], "note", 9999, "does not exist"),
    ] {
        rejected(
            &h,
            &identity,
            &[command, "--as", &task.to_string(), "x"],
            6,
            message,
        );
    }
    rejected(&h, &[], &["note", "--as", "-3", "x"], 2, "");
    h.ok(&env, &["note", "still works"]);
}

#[test]
fn owner_notes_refuse_every_worker_identity_without_writing() {
    let h = Harness::new();
    let (_, _, env) = h.lane("claude", "implementer");
    let sub = new_task(&h, "sub", "sub-orchestrator", 1);
    for (identity, target) in [
        (env.clone(), None),
        (
            vec![
                ("TASKR_TASK".into(), sub.to_string()),
                ("TASKR_LAUNCH".into(), "0".into()),
            ],
            None,
        ),
        (vec![], Some(sub)),
        (
            vec![
                ("TASKR_TASK".into(), "1".into()),
                ("TASKR_LAUNCH".into(), "0".into()),
            ],
            Some(1),
        ),
        (
            vec![
                ("TASKR_TASK".into(), "1".into()),
                ("TASKR_LAUNCH".into(), "0".into()),
            ],
            None,
        ),
        (env.clone(), Some(1)),
        (vec![], None),
    ] {
        let target = target.map(|id| id.to_string());
        let mut args = vec!["note", "for owner", "--owner"];
        if let Some(target) = &target {
            args.extend(["--as", target]);
        }
        rejected(&h, &identity, &args, 6, "");
    }
    h.ok(&env, &["note", "ordinary progress"]);
}

#[test]
fn owner_note_keys_cannot_change_owner_semantics_or_overwrite_text() {
    for owner in [false, true] {
        let h = Harness::new();
        new_task(&h, "root", "orchestrator", 0);
        let mut args = vec!["note", "original", "--key", "note-key", "--as", "1"];
        if owner {
            args.push("--owner");
        }
        let first = h.ok(&[], &args)["event_id"].clone();
        let replay = h.ok(&[], &args);
        assert_eq!(replay["duplicate"], true);
        assert_eq!(replay["event_id"], first);
        let mut changed = vec!["note", "replacement", "--key", "note-key", "--as", "1"];
        if !owner {
            changed.push("--owner");
        }
        rejected(&h, &[], &changed, 6, "--owner");
        let text: String = h
            .conn()
            .query_row(
                "select summary from events where id=?",
                [first.as_i64().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(text, "original");
    }
    let h = Harness::new();
    new_task(&h, "root", "orchestrator", 0);
    let first = h.ok(&[], &["ask", "question", "--key", "ask-key", "--as", "1"]);
    let duplicate = h.ok(
        &[],
        &[
            "ask", "question", "--key", "ask-key", "--as", "1", "--owner",
        ],
    );
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(duplicate["event_id"], first["event_id"]);
}

#[test]
fn reserved_receipt_and_capacity_keys_never_write_ordinary_events() {
    let h = Harness::new();
    let (_, _, env) = h.lane("claude", "implementer");
    for prefix in [
        "got:",
        "no_receipt:",
        "late_receipt:",
        "quota:",
        "capacity:",
    ] {
        let key = format!("{prefix}1");
        rejected(&h, &env, &["note", "x", "--key", &key], 2, "");
        rejected(&h, &[], &["note", "x", "--key", &key, "--as", "1"], 2, "");
        rejected(&h, &env, &["ready", "x", "--key", &key], 2, "");
    }
}

#[test]
fn receipts_are_once_only_bound_to_the_task_launch_and_prompt_kind() {
    let h = Harness::new();
    let (worker, old_launch, mut env) = h.lane("claude", "implementer");
    let native = h
        .home
        .join("accounts/account-b/native")
        .to_string_lossy()
        .into_owned();
    env.push(("CLAUDE_CONFIG_DIR".into(), native.clone()));
    let attempt = h.prompt(worker, true);
    let first = h.ok(&env, &["got", &attempt.to_string()]);
    let duplicate = h.ok(&env, &["got", &attempt.to_string()]);
    assert_eq!(first["round"], 1);
    assert_eq!(duplicate["round"], 1);
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(duplicate["event_id"], first["event_id"]);
    assert_eq!(h.got(attempt), 1);
    let identity: (String, String) = h
        .conn()
        .query_row(
            "select native_home,account from launches where id=?",
            [old_launch],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(identity, (native.clone(), "account-b".into()));
    let receipt: (String, i64, i64, String, String) = h
        .conn()
        .query_row(
            "select kind,recipient_task_id,related_event_id,event_key,data from events where id=?",
            [first["event_id"].as_i64().unwrap()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        (&receipt.0, receipt.1, receipt.2, &receipt.3),
        (&"got".to_string(), 1, attempt, &format!("got:{attempt}"))
    );
    assert!(serde_json::from_str::<Value>(&receipt.4).unwrap()["identity"].is_object());
    let collision = h.prompt(worker, true);
    rejected(
        &h,
        &env,
        &["note", "ordinary", "--key", &format!("got:{collision}")],
        2,
        "",
    );
    rejected(&h, &env, &["ask", "q", "--key", "got:x"], 2, "");
    h.conn().execute("insert into events(task_id,launch_id,kind,summary,event_key,created_at) values(?,?,'note','legacy',?,'x')", params![worker,old_launch,format!("got:{collision}")]).unwrap();
    rejected(
        &h,
        &env,
        &["got", &collision.to_string()],
        6,
        "key collision",
    );
    rejected(&h, &env, &["got", &first["event_id"].to_string()], 6, "");
    rejected(&h, &env, &["got", "9999"], 6, "");
    let other = new_task(&h, "other", "implementer", 1);
    launch(&h, other);
    let foreign = h.prompt(other, true);
    rejected(&h, &env, &["got", &foreign.to_string()], 6, "");
    let mut next_env = launch(&h, worker);
    next_env.push(("CLAUDE_CONFIG_DIR".into(), native));
    h.ok(&next_env, &["start"]);
    let next = h.prompt(worker, true);
    rejected(&h, &next_env, &["got", &attempt.to_string()], 6, "");
    rejected(&h, &env, &["got", &next.to_string()], 6, "");
    h.ok(&next_env, &["got", &next.to_string()]);
    let last = h.prompt(worker, true);
    let got = h.ok(&next_env, &["got", &last.to_string()]);
    assert_eq!(got["round"], 2);
    let data: String = h
        .conn()
        .query_row(
            "select data from events where id=?",
            [got["event_id"].as_i64().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(serde_json::from_str::<Value>(&data).unwrap()["identity"].is_null());
    let status = json_rows(&h, &["status", "--tree", &worker.to_string()]);
    let status = status.iter().find(|row| row["id"] == worker).unwrap();
    assert_eq!(status["round"], 2);
    assert_eq!(status["last_receipt"], last);
    assert_eq!(status["waiting"], false);
}

#[test]
fn worker_wait_cannot_read_another_inbox_or_use_a_stale_launch() {
    let h = Harness::new();
    let (worker, _, env) = h.lane("claude", "implementer");
    let ask = h.ok(&env, &["ask", "which pin?"])["ask_id"].to_string();
    let answer = h.ok(&[], &["answer", &ask, "main"])["answer_id"].clone();
    let event = h.ok(&env, &["wait", "--for", "answer", "--timeout", "0"]);
    assert_eq!(event["event"]["id"], answer);
    rejected(&h, &env, &["wait", "--as", "1", "--timeout", "0"], 6, "");
    rejected(&h, &[], &["wait", "--timeout", "0"], 2, "");
    launch(&h, worker);
    rejected(&h, &env, &["wait", "--timeout", "0"], 6, "");
    rejected(
        &h,
        &env,
        &["wait", "--as", &worker.to_string(), "--timeout", "0"],
        6,
        "",
    );
}

fn cursor(h: &Harness, task: i64) -> (Option<i64>, i64, Option<String>) {
    h.conn()
        .query_row(
            "select pending_event_id,acked_event_id,waiting_until from tasks where id=?",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

#[test]
fn filtered_wait_skips_without_deleting_and_validates_before_ack() {
    let h = Harness::new();
    let (worker, _, env) = h.lane("claude", "implementer");
    let other = new_task(&h, "beta", "implementer", 1);
    let other_env = launch(&h, other);
    let skip_kind = h.ok(&env, &["ask", "which pin?"])["event_id"]
        .as_i64()
        .unwrap();
    let skip_source = h.ok(&other_env, &["ready", "beta"])["event_id"]
        .as_i64()
        .unwrap();
    let matched = h.ok(&env, &["ready", "alpha"])["event_id"]
        .as_i64()
        .unwrap();
    h.ok(&[], &["wait", "--as", "1", "--timeout", "0"]);
    let output = h.output(
        &[],
        &[
            "wait",
            "--as",
            "1",
            "--for",
            "ready,fail",
            "--from",
            "impl",
            "--timeout",
            "0",
        ],
        b"",
    );
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with(&format!("sk1 2\ne1\t{matched}\t")),
        "{text}"
    );
    let replay = h.ok(
        &[],
        &[
            "wait",
            "--as",
            "1",
            "--for",
            "ready",
            "--from",
            &worker.to_string(),
            "--from",
            "beta",
            "--timeout",
            "0",
        ],
    );
    assert_eq!(replay["event"]["id"], matched);
    assert_eq!(replay["replay"], true);
    assert_eq!(cursor(&h, 1), (Some(matched), skip_source, None));
    let retained: i64 = h
        .conn()
        .query_row(
            "select count(*) from events where id in (?,?)",
            params![skip_kind, skip_source],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(retained, 2);
    rejected(
        &h,
        &[],
        &[
            "wait",
            "--as",
            "1",
            "--ack",
            &skip_source.to_string(),
            "--timeout",
            "0",
        ],
        6,
        "",
    );
    rejected(
        &h,
        &[],
        &[
            "wait",
            "--as",
            "1",
            "--ack",
            &matched.to_string(),
            "--for",
            "typo",
            "--timeout",
            "0",
        ],
        2,
        "",
    );
    assert_eq!(cursor(&h, 1), (Some(matched), skip_source, None));
    let next = h.ok(&env, &["fail", "blocked"])["event_id"]
        .as_i64()
        .unwrap();
    let delivered = h.ok(
        &[],
        &[
            "wait",
            "--as",
            "1",
            "--ack",
            &matched.to_string(),
            "--for",
            "fail",
            "--timeout",
            "0",
        ],
    );
    assert_eq!(delivered["event"]["id"], next);
    assert_eq!(delivered["replay"], false);
    let timeout = h.output(
        &[],
        &[
            "--json",
            "wait",
            "--as",
            "1",
            "--ack",
            &next.to_string(),
            "--for",
            "ready",
            "--timeout",
            "0",
        ],
        b"",
    );
    assert_eq!(timeout.status.code(), Some(3));
    rejected(
        &h,
        &[],
        &[
            "wait",
            "--as",
            "1",
            "--ack",
            &next.to_string(),
            "--timeout",
            "0",
        ],
        6,
        "",
    );
    h.ok(&[], &["ack", &next.to_string(), "--as", "1"]);
}

fn inject(h: &Harness, task: i64, launch: i64, kind: &str, data: Value) -> i64 {
    let db = h.conn();
    db.execute("insert into events(task_id,recipient_task_id,launch_id,kind,data,created_at) values(?,1,?,?,?,'2026-01-01T00:00:00Z')", params![task,launch,kind,data.to_string()]).unwrap();
    db.last_insert_rowid()
}

#[test]
fn capacity_quota_and_stall_bypass_filters_and_replay_until_acked() {
    for data in [
        json!({"reason":"model_capacity"}),
        json!({"quota":"limit"}),
        json!({"reason":"stall","last":"blocked"}),
    ] {
        let h = Harness::new();
        let (_, _, env) = h.lane("claude", "implementer");
        let other = new_task(&h, "beta", "implementer", 1);
        let other_env = launch(&h, other);
        let protected = inject(&h, other, other_env[1].1.parse().unwrap(), "herdr", data);
        let ready = h.ok(&env, &["ready", "alpha"])["event_id"].clone();
        let args = [
            "wait",
            "--as",
            "1",
            "--for",
            "ready",
            "--from",
            "impl",
            "--timeout",
            "0",
        ];
        for replay in [false, true] {
            let got = h.ok(&[], &args);
            assert_eq!(got["event"]["id"], protected);
            assert_eq!(got["replay"], replay);
        }
        let next = h.ok(
            &[],
            &[&args[..], &["--ack", &protected.to_string()]].concat(),
        );
        assert_eq!(next["event"]["id"], ready);
        assert_eq!(next["replay"], false);
    }
}

#[test]
fn ordinary_herdr_hints_do_not_bypass_wait_filters() {
    let h = Harness::new();
    let (_, _, env) = h.lane("claude", "implementer");
    let other = new_task(&h, "beta", "implementer", 1);
    let other_env = launch(&h, other);
    for data in [json!({"reason":"other"}), json!({"agent_status":"idle"})] {
        inject(&h, other, other_env[1].1.parse().unwrap(), "herdr", data);
    }
    let ready = h.ok(&env, &["ready", "alpha"])["event_id"]
        .as_i64()
        .unwrap();
    let out = h.output(
        &[],
        &[
            "wait",
            "--as",
            "1",
            "--for",
            "ready",
            "--from",
            "impl",
            "--timeout",
            "0",
        ],
        b"",
    );
    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .starts_with(&format!("sk1 2\ne1\t{ready}\t"))
    );
}

struct Waiter(std::process::Child);
impl Drop for Waiter {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn blocked_wait(h: &Harness, env: &Env, task: i64, extra: &[&str]) -> Waiter {
    let child = h
        .command(env)
        .args([
            "--json",
            "wait",
            "--as",
            &task.to_string(),
            "--timeout",
            "60000",
        ])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let waiter = Waiter(child);
    while cursor(h, task).2.is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "wait marker absent"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    waiter
}
fn wait_result(mut waiter: Waiter) -> (i32, Value) {
    let start = Instant::now();
    let status = loop {
        if let Some(status) = waiter.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "wait never ended"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut waiter.0.stdout.take().unwrap(), &mut bytes).unwrap();
    (
        status.code().unwrap(),
        serde_json::from_slice(&bytes).unwrap(),
    )
}
fn reject_clear(h: &Harness) {
    h.conn().execute_batch("create trigger reject_clear before update of waiting_until on tasks when new.waiting_until is null and old.waiting_until is not null begin select raise(abort,'injected clear failure'); end").unwrap();
}

#[test]
fn cleanup_failure_keeps_the_offered_answer_pending_for_replay() {
    let h = Harness::new();
    let (worker, _, env) = h.lane("claude", "implementer");
    let ask = h.ok(&env, &["ask", "which base?", "--blocking"])["ask_id"].to_string();
    reject_clear(&h);
    let waited = blocked_wait(&h, &env, worker, &[]);
    let answer = h.ok(&[], &["answer", &ask, "main"])["answer_id"]
        .as_i64()
        .unwrap();
    let (code, out) = wait_result(waited);
    assert_eq!(code, 4);
    assert_eq!(out["kind"], "database");
    assert!(out["event"].is_null());
    assert_eq!(out["pending_event_id"], answer);
    assert!(out["stale_waiting_until"].is_string());
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("injected clear failure")
    );
    assert_eq!(cursor(&h, worker).0, Some(answer));
    assert_eq!(cursor(&h, worker).1, 0);
    h.conn().execute_batch("drop trigger reject_clear").unwrap();
    let replay = h.ok(&env, &["wait", "--timeout", "0"]);
    assert_eq!(replay["event"]["id"], answer);
    assert_eq!(replay["replay"], true);
}

#[test]
fn combined_ack_cleanup_failure_reports_both_ids_and_replays_only_the_next() {
    let h = Harness::new();
    let (worker, _, env) = h.lane("claude", "implementer");
    let first_ask = h.ok(&env, &["ask", "first"])["ask_id"].to_string();
    let first = h.ok(&[], &["answer", &first_ask, "one"])["answer_id"]
        .as_i64()
        .unwrap();
    h.ok(&env, &["wait", "--timeout", "0"]);
    let second_ask = h.ok(&env, &["ask", "second"])["ask_id"].to_string();
    reject_clear(&h);
    let waited = blocked_wait(
        &h,
        &env,
        worker,
        &["--ack", &first.to_string(), "--for", "answer"],
    );
    let second = h.ok(&[], &["answer", &second_ask, "two"])["answer_id"]
        .as_i64()
        .unwrap();
    let (code, out) = wait_result(waited);
    assert_eq!(code, 4);
    assert_eq!(out["acked_event_id"], first);
    assert_eq!(out["pending_event_id"], second);
    assert_eq!(cursor(&h, worker).0, Some(second));
    assert_eq!(cursor(&h, worker).1, first);
    h.conn().execute_batch("drop trigger reject_clear").unwrap();
    let replay = h.ok(&env, &["wait", "--for", "answer", "--timeout", "0"]);
    assert_eq!(replay["event"]["id"], second);
    assert_eq!(replay["replay"], true);
}

#[test]
fn timeout_cleanup_failure_is_a_database_error_with_the_marker_retained() {
    let h = Harness::new();
    new_task(&h, "root", "orchestrator", 0);
    reject_clear(&h);
    let out = h.output(&[], &["wait", "--as", "1", "--timeout", "20"], b"");
    assert_eq!(out.status.code(), Some(4));
    assert!(out.stderr.is_empty());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with("x1 4 ") && text.contains("injected clear failure"),
        "{text}"
    );
    assert!(!text.contains("timeout"), "{text}");
    assert!(cursor(&h, 1).2.is_some());
}

#[test]
fn filtered_wait_deadline_retains_the_unscanned_backlog() {
    let h = Harness::new();
    let (worker, launched, _) = h.lane("claude", "implementer");
    let mut db = h.conn();
    let tx = db.transaction().unwrap();
    for _ in 0..2000 {
        tx.execute("insert into events(task_id,recipient_task_id,launch_id,kind,created_at) values(?,1,?,'got','2026-01-01T00:00:00Z')", params![worker,launched]).unwrap();
    }
    tx.execute("insert into events(task_id,recipient_task_id,launch_id,kind,created_at) values(?,1,?,'ready','2026-01-01T00:00:00Z')", params![worker,launched]).unwrap();
    let matched = tx.last_insert_rowid();
    tx.commit().unwrap();
    let out = h.output(
        &[],
        &["wait", "--as", "1", "--for", "ready", "--timeout", "1"],
        b"",
    );
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let skipped: i64 = text
        .lines()
        .next()
        .unwrap()
        .strip_prefix("sk1 ")
        .unwrap()
        .parse()
        .unwrap();
    assert!(skipped > 0 && skipped < 2000, "{text}");
    assert_eq!(
        text,
        format!("sk1 {skipped}\nw1 {{\"owed\":0,\"due\":0}}\nx1 3 timeout\n")
    );
    let (pending, acked, marker) = cursor(&h, 1);
    assert_eq!((pending, marker), (None, None));
    let counts: (i64, i64) = h.conn().query_row("select count(*),count(case when id>? then 1 end) from events where recipient_task_id=1", [acked], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(counts, (2001, 2001 - skipped));
    let next = h.ok(&[], &["wait", "--as", "1", "--timeout", "0"]);
    assert_eq!(next["event"]["kind"], "got");
    assert_eq!(next["replay"], false);
    let out = h.output(
        &[],
        &["wait", "--as", "1", "--for", "ready", "--timeout", "0"],
        b"",
    );
    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .starts_with(&format!("sk1 {}\ne1\t{matched}\t", 2000 - skipped))
    );
}

fn ask(h: &Harness, env: &Env, text: &str, owner: bool) -> i64 {
    let mut args = vec!["ask", text];
    if owner {
        args.push("--owner");
    }
    h.ok(env, &args)["ask_id"].as_i64().unwrap()
}
fn frame_ids(text: &str) -> Vec<i64> {
    text.lines()
        .filter_map(|line| line.strip_prefix("j1 "))
        .map(|row| serde_json::from_str::<Value>(row).unwrap())
        .filter(|row| row["rec"] == "event")
        .map(|row| row["i"].as_i64().unwrap())
        .collect()
}
fn json_rows(h: &Harness, args: &[&str]) -> Vec<Value> {
    let out = h.output(&[], &[&["--json"][..], args].concat(), b"");
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "{args:?}: {out:?}"
    );
    std::str::from_utf8(&out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn ask_ids(h: &Harness, args: &[&str], json_mode: bool) -> Vec<i64> {
    let mut argv = vec![];
    if json_mode {
        argv.push("--json");
    }
    argv.push("asks");
    argv.extend_from_slice(args);
    let out = h.output(&[], &argv, b"");
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "{argv:?}: {out:?}"
    );
    if json_mode {
        std::str::from_utf8(&out.stdout)
            .unwrap()
            .lines()
            .map(|row| {
                serde_json::from_str::<Value>(row).unwrap()["id"]
                    .as_i64()
                    .unwrap()
            })
            .collect()
    } else {
        frame_ids(std::str::from_utf8(&out.stdout).unwrap())
    }
}

#[test]
fn orphan_asks_are_hidden_without_destroying_them_or_their_answers() {
    let h = Harness::new();
    let (worker, _, env) = h.lane("claude", "implementer");
    let closed_ask = ask(&h, &env, "closed asker", true);
    let plain = ask(&h, &env, "closed plain", false);
    let answered = ask(&h, &env, "answered closed", true);
    h.ok(&[], &["answer", &answered.to_string(), "recorded answer"]);
    h.ok(&[], &["close", &worker.to_string()]);
    let live = new_task(&h, "live", "implementer", 1);
    let live_ask = ask(&h, &launch(&h, live), "live", true);
    let sub = new_task(&h, "sub", "sub-orchestrator", 1);
    let child = new_task(&h, "child", "implementer", sub);
    let child_ask = ask(&h, &launch(&h, child), "child", true);
    h.ok(&[], &["close", &sub.to_string()]);
    let closed_root = new_task(&h, "closed-root", "orchestrator", 0);
    let deep_sub = new_task(&h, "deep-sub", "sub-orchestrator", closed_root);
    let deep = new_task(&h, "deep", "implementer", deep_sub);
    let deep_env = launch(&h, deep);
    let root_orphan = ask(&h, &deep_env, "closed root", true);
    let root_answered = ask(&h, &deep_env, "answered under closed root", true);
    h.ok(
        &[],
        &[
            "answer",
            &root_answered.to_string(),
            "another recorded answer",
        ],
    );
    let root_ask = h.ok(
        &[],
        &[
            "ask",
            "root ask",
            "--as",
            &closed_root.to_string(),
            "--owner",
        ],
    )["ask_id"]
        .as_i64()
        .unwrap();
    h.ok(&[], &["close", &closed_root.to_string()]);
    let root = closed_root.to_string();
    for (args, want) in [
        (vec![], vec![answered, live_ask, child_ask, root_answered]),
        (vec!["--open"], vec![live_ask, child_ask]),
        (
            vec!["--owner"],
            vec![answered, live_ask, child_ask, root_answered],
        ),
        (vec!["--open", "--owner"], vec![live_ask, child_ask]),
        (
            vec!["--all"],
            vec![
                closed_ask,
                plain,
                answered,
                live_ask,
                child_ask,
                root_orphan,
                root_answered,
                root_ask,
            ],
        ),
        (
            vec!["--all", "--open"],
            vec![
                closed_ask,
                plain,
                live_ask,
                child_ask,
                root_orphan,
                root_ask,
            ],
        ),
        (
            vec!["--all", "--owner", "--open", "--limit", "1"],
            vec![closed_ask, live_ask, child_ask, root_orphan, root_ask],
        ),
        (vec!["--tree", "1"], vec![answered, live_ask, child_ask]),
        (
            vec!["--all", "--tree", "1", "--owner"],
            vec![closed_ask, answered, live_ask, child_ask],
        ),
        (vec!["--tree", &root], vec![root_answered]),
        (
            vec![
                "--all", "--tree", &root, "--owner", "--open", "--limit", "0",
            ],
            vec![root_orphan, root_ask],
        ),
        (
            vec!["--limit", "1"],
            vec![live_ask, child_ask, root_answered],
        ),
        (
            vec!["--all", "--limit", "1"],
            vec![
                closed_ask,
                plain,
                live_ask,
                child_ask,
                root_orphan,
                root_answered,
                root_ask,
            ],
        ),
    ] {
        for json in [false, true] {
            assert_eq!(ask_ids(&h, &args, json), want, "{args:?} json={json}");
        }
    }
    let all = json_rows(&h, &["asks", "--all"]);
    for (id, text) in [
        (answered, "recorded answer"),
        (root_answered, "another recorded answer"),
    ] {
        assert_eq!(all.iter().find(|r| r["id"] == id).unwrap()["answer"], text);
    }
    h.ok(&[], &["answer", &root_orphan.to_string(), "late answer"]);
    let rows = json_rows(&h, &["asks", "--tree", &root, "--limit", "0"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], root_orphan);
    assert_eq!(rows[0]["answer"], "late answer");
}

#[test]
fn close_warns_once_about_new_orphans_and_never_answers_them() {
    for target in ["lane", "root", "none"] {
        for json_mode in [false, true] {
            let h = Harness::new();
            let (worker, _, env) = h.lane("claude", "implementer");
            let root_ask = h.ok(&[], &["ask", "root ask", "--as", "1", "--owner"])["ask_id"]
                .as_i64()
                .unwrap();
            let lane_ask = ask(&h, &env, "lane ask", true);
            let plain = ask(&h, &env, "plain ask", false);
            let answered = ask(&h, &env, "answered", true);
            h.ok(&[], &["answer", &answered.to_string(), "yes"]);
            let sub = new_task(&h, "sub", "sub-orchestrator", 1);
            let sub_ask = ask(&h, &launch(&h, sub), "already orphaned", true);
            let child = new_task(&h, "child", "implementer", sub);
            let child_ask = ask(&h, &launch(&h, child), "child", true);
            h.ok(&[], &["close", &sub.to_string()]);
            let unrelated = new_task(&h, "unrelated", "orchestrator", 0);
            h.ok(
                &[],
                &[
                    "ask",
                    "unrelated",
                    "--as",
                    &unrelated.to_string(),
                    "--owner",
                ],
            );
            let empty = new_task(&h, "no-owner-asks", "implementer", 1);
            ask(&h, &launch(&h, empty), "plain only", false);
            let (id, want) = match target {
                "lane" => (worker, vec![lane_ask]),
                "root" => (1, vec![root_ask, lane_ask, child_ask]),
                _ => (empty, vec![]),
            };
            let before: i64 = h
                .conn()
                .query_row("select count(*) from events", [], |r| r.get(0))
                .unwrap();
            let id_text = id.to_string();
            let args = if json_mode {
                vec!["--json", "close", &id_text]
            } else {
                vec!["close", &id_text]
            };
            let out = h.output(&[], &args, b"");
            assert!(out.status.success());
            let warning = if want.is_empty() {
                String::new()
            } else {
                format!(
                    "taskr: closing {id} orphans owner ask(s) {}; asks hides them unless --all\n",
                    want.iter()
                        .map(i64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            assert_eq!(String::from_utf8(out.stderr).unwrap(), warning);
            if json_mode {
                let value: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(
                    value["orphaned_owner_asks"],
                    if want.is_empty() {
                        Value::Null
                    } else {
                        json!(want)
                    }
                );
            } else {
                assert_eq!(out.stdout, format!("c1 {id}\n").as_bytes());
            }
            for ask in [root_ask, lane_ask, plain, sub_ask, child_ask] {
                assert_eq!(
                    h.count(
                        "select count(*) from events where id=? and answered_by is null",
                        ask
                    ),
                    1
                );
            }
            let repeat = h.ok(&[], &["close", &id_text]);
            assert_eq!(repeat["already"], true);
            assert!(repeat["orphaned_owner_asks"].is_null());
            let repeat = h.output(&[], &["close", &id_text], b"");
            assert!(repeat.status.success() && repeat.stderr.is_empty());
            assert_eq!(repeat.stdout, format!("c1 {id} already\n").as_bytes());
            let after: i64 = h
                .conn()
                .query_row("select count(*) from events", [], |r| r.get(0))
                .unwrap();
            assert_eq!(after, before + 1);
        }
    }
}

#[test]
fn closing_an_intermediate_task_keeps_its_open_descendants_asks_live() {
    let h = Harness::new();
    new_task(&h, "root", "orchestrator", 0);
    let sub = new_task(&h, "sub", "sub-orchestrator", 1);
    let sub_ask = ask(&h, &launch(&h, sub), "sub", true);
    let child = new_task(&h, "child", "implementer", sub);
    let child_ask = ask(&h, &launch(&h, child), "child", true);
    let closed = h.ok(&[], &["close", &sub.to_string()]);
    assert_eq!(closed["orphaned_owner_asks"], json!([sub_ask]));
    assert_eq!(ask_ids(&h, &["--open", "--owner"], true), vec![child_ask]);
}

#[test]
fn forward_log_budget_cursor_never_skips_or_duplicates_an_event() {
    let h = Harness::new();
    let (worker, _, env) = h.lane("claude", "implementer");
    let mut cursor = h.ok(&env, &["note", "cursor"])["event_id"]
        .as_i64()
        .unwrap();
    let mut expected = BTreeSet::new();
    let body = "x".repeat(2000);
    for _ in 0..40 {
        expected.insert(h.ok(&env, &["note", &body])["event_id"].as_i64().unwrap());
    }
    let mut seen = BTreeSet::new();
    let mut pages = 0;
    loop {
        let out = h.output(
            &[],
            &["log", &worker.to_string(), "--since", &cursor.to_string()],
            b"",
        );
        assert!(out.status.success() && out.stderr.is_empty());
        let text = String::from_utf8(out.stdout).unwrap();
        let ids = frame_ids(&text);
        assert!(!ids.is_empty());
        for id in ids {
            assert!(
                expected.contains(&id) && seen.insert(id),
                "unexpected/duplicate {id}"
            );
        }
        let trailer = text.lines().find_map(|l| l.strip_prefix("m1 "));
        let Some(trailer) = trailer else {
            break;
        };
        assert!(text.len() <= 32 * 1024, "{} bytes", text.len());
        let trailer: Value = serde_json::from_str(trailer).unwrap();
        assert!(trailer["more"].is_number());
        let next = trailer["next"].as_i64().unwrap();
        assert!(next > cursor);
        cursor = next;
        pages += 1;
        assert!(pages <= 10, "paging did not converge");
    }
    assert!(pages > 0, "fixture never hit the budget");
    assert_eq!(seen, expected);
}

#[test]
fn full_spool_refuses_new_records_with_a_retry_and_preserves_all_500() {
    let h = Harness::new();
    let state = h.home.join(".local/state/taskr");
    let queue = state.join("spool/queue");
    std::fs::create_dir_all(&queue).unwrap();
    std::fs::write(state.join("server.url"), "http://127.0.0.1:1\n").unwrap();
    for seq in 1..=500 {
        let key = format!("bound-record-{seq:06}");
        let record = json!({"v":1,"seq":seq,"request_key":key,"queued_at":"2026-01-01T00:00:00Z","request":{"argv":["--json","note","waiting","--as","1"],"cwd":h.home,"request_key":key}});
        std::fs::write(
            queue.join(format!("{seq:06}-{key}.json")),
            record.to_string(),
        )
        .unwrap();
    }
    let snapshot = || -> Vec<(std::path::PathBuf, Vec<u8>)> {
        let mut paths = std::fs::read_dir(&queue)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .into_iter()
            .map(|p| {
                let bytes = std::fs::read(&p).unwrap();
                (p, bytes)
            })
            .collect()
    };
    let before = snapshot();
    let out = h.output(
        &[("TASKR_DB".into(), "".into())],
        &["--json", "note", "new", "--as", "1"],
        b"",
    );
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    let diagnostic = String::from_utf8(out.stderr).unwrap();
    assert!(
        diagnostic.contains("spool full") && diagnostic.contains("retry with:"),
        "{diagnostic}"
    );
    assert_eq!(snapshot(), before);
    assert!(!h.db.exists(), "client command created a local ledger");
}

#[test]
fn bounded_asks_keep_every_open_question_and_the_newest_answered_suffix() {
    let h = Harness::new();
    let (_, _, env) = h.lane("claude", "implementer");
    let mut answered = Vec::new();
    for n in 0..25 {
        let id = ask(&h, &env, &format!("question {n:02}"), false);
        h.ok(&[], &["answer", &id.to_string(), "main"]);
        answered.push(id);
    }
    let open = [
        ask(&h, &env, "open one", false),
        ask(&h, &env, "open two", false),
    ];
    let read = |args: &[&str]| {
        let out = h.output(&[], &[&["asks"][..], args].concat(), b"");
        assert!(out.status.success() && out.stderr.is_empty());
        let text = String::from_utf8(out.stdout).unwrap();
        let ids = frame_ids(&text);
        let trailer = text
            .lines()
            .find_map(|l| l.strip_prefix("m1 "))
            .map(|row| serde_json::from_str::<Value>(row).unwrap());
        (text, ids, trailer)
    };
    let (_, ids, trailer) = read(&[]);
    let mut expected = open.to_vec();
    expected.extend_from_slice(&answered[5..]);
    expected.sort_unstable();
    assert_eq!(ids, expected);
    let trailer = trailer.unwrap();
    assert_eq!(trailer["answered"], 5);
    assert_eq!(trailer["all"], "--limit 0");
    let large = "a".repeat(10000);
    for n in 0..25 {
        let id = ask(&h, &env, &format!("big {n:02}"), false);
        h.ok(&[], &["answer", &id.to_string(), &large]);
        answered.push(id);
    }
    for (args, limit) in [(vec![], 20), (vec!["--limit", "10"], 10)] {
        let (text, ids, trailer) = read(&args);
        assert!(text.len() <= 32 * 1024);
        assert!(ids.starts_with(&open));
        let shown = ids.len() - open.len();
        assert!(shown > 0 && shown < limit);
        assert_eq!(&ids[2..], &answered[answered.len() - shown..]);
        let trailer = trailer.unwrap();
        assert_eq!(trailer["answered"], 50 - shown);
        assert_eq!(trailer["all"], "--limit 0");
    }
    let (_, all, trailer) = read(&["--limit", "0"]);
    assert_eq!(all.len(), 52);
    assert!(trailer.is_none());
    assert_eq!(ask_ids(&h, &[], true).len(), 52);
    assert_eq!(ask_ids(&h, &["--limit", "10"], true).len(), 12);
}
