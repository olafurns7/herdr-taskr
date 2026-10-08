#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};
use support::*;
const FLOWS: [(&str, &str, &str, &str, &str, &str, &str); 4] = [
    (
        "claude",
        "SessionStart",
        "UserPromptSubmit",
        "StopFailure",
        "claude-session-start.json",
        "claude-user-prompt-submit.json",
        "claude-stop-failure.json",
    ),
    (
        "codex",
        "SessionStart",
        "UserPromptSubmit",
        "Stop",
        "codex-session-start.json",
        "codex-user-prompt-submit.json",
        "codex-stop.json",
    ),
    (
        "opencode",
        "session.created",
        "chat.message",
        "session.error",
        "opencode-session-created.json",
        "opencode-chat-message.json",
        "opencode-session-error.json",
    ),
    (
        "pi",
        "session_start",
        "input",
        "agent_settled",
        "pi-session-start.json",
        "pi-input.json",
        "pi-agent-settled-error.json",
    ),
];
#[test]
fn fixture_flows_and_receipts() {
    for (name, start, prompt, stop, start_file, prompt_file, stop_file) in FLOWS {
        let h = Harness::new();
        let (w, l, env) = h.lane(name, "implementer");
        h.hook(&env, name, start, fixture(start_file));
        let (session, source, transcript): (String, String, Option<String>) = h
            .conn()
            .query_row(
                "select session_ref,session_source,transcript_path from launches where id=?",
                [l],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(session, format!("{name}-session"));
        assert_eq!(source, format!("hook:{start}"));
        let expected = (name != "opencode").then(|| format!("/tmp/taskr-hooks/{name}.jsonl"));
        assert_eq!(transcript, expected);
        let a = h.prompt(w, true);
        h.hook(&env, name, prompt, prompt_fixture(prompt_file, a));
        assert_eq!(h.got(a), 1);
        let got = h.ok(&env, &["got", &a.to_string()]);
        assert_eq!(got["duplicate"], true);
        let data: String = h
            .conn()
            .query_row(
                "select data from events where id=?",
                [got["event_id"].as_i64().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        let data: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(data["identity"]["session_ref"], session);
        assert_eq!(data["identity"]["session_source"], format!("hook:{prompt}"));
        assert_eq!(data["identity"]["transcript_path"], json!(expected));
        h.hook(&env, name, stop, fixture(stop_file));
        if name == "opencode" {
            h.hook(
                &env,
                name,
                "session.idle",
                fixture("opencode-session-idle.json"),
            );
        }
        if name == "pi" {
            h.hook(
                &env,
                name,
                "agent_settled",
                fixture("pi-agent-settled.json"),
            );
        }
        assert_eq!(h.stall(a)["reason"], "stall");
        let error = match name {
            "claude" => json!("rate_limit"),
            "opencode" => json!("server_overloaded"),
            "pi" => json!("ECONNRESET"),
            _ => Value::Null,
        };
        assert_eq!(h.stall(a)["error"], error);
        h.hook(&env, name, stop, fixture(stop_file));
        assert_eq!(h.stalls(a), 1);
        let a2 = h.prompt(w, false);
        assert!(h.ok(&env, &["got", &a2.to_string()])["duplicate"].is_null());
        h.hook(&env, name, prompt, prompt_fixture(prompt_file, a2));
        assert_eq!(h.got(a2), 1);
        let report = h.home.join("report.md");
        fs::write(&report, "ready").unwrap();
        h.ok(
            &env,
            &["ready", "slice", "--report", report.to_str().unwrap()],
        );
        h.hook(&env, name, stop, fixture(stop_file));
        assert_eq!(h.stall(a2)["last"], "r");
        assert_eq!(h.stall(a2)["error"], error);
    }
}
#[test]
fn session_and_pane_binding() {
    let h = Harness::new();
    let (w, l, env) = h.lane("claude", "implementer");
    h.hook(
        &env,
        "claude",
        "SessionStart",
        fixture("claude-session-start.json"),
    );
    let mut nested = fixture("claude-session-start.json");
    nested["session_id"] = json!("nested-session");
    h.hook(&env, "claude", "SessionStart", nested);
    let bound: String = h
        .conn()
        .query_row("select session_ref from launches where id=?", [l], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(bound, "claude-session");
    let a = h.prompt(w, true);
    let mut quoted = prompt_fixture("claude-user-prompt-submit.json", a);
    quoted["prompt"] = json!(format!("Please see First taskr got {a}."));
    h.hook(&env, "claude", "UserPromptSubmit", quoted);
    assert_eq!(h.got(a), 0);
    let a = h.prompt(w, true);
    h.hook(
        &env,
        "claude",
        "UserPromptSubmit",
        prompt_fixture("claude-user-prompt-submit.json", a),
    );
    assert_eq!(h.got(a), 1);
    let a = h.prompt(w, true);
    let mut wrong = env.clone();
    wrong.push(("HERDR_PANE_ID".into(), "w9:p2".into()));
    h.hook(
        &wrong,
        "claude",
        "UserPromptSubmit",
        prompt_fixture("claude-user-prompt-submit.json", a),
    );
    assert_eq!(h.got(a), 0);
    let mut wrong_session = prompt_fixture("claude-user-prompt-submit.json", a);
    wrong_session["session_id"] = json!("nested-session");
    h.hook(&env, "claude", "UserPromptSubmit", wrong_session);
    assert_eq!(h.got(a), 0);
    let a = h.prompt(w, true);
    let new = h.ok(
        &[],
        &[
            "launch",
            &w.to_string(),
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
    assert_ne!(l, new);
    h.hook(
        &env,
        "claude",
        "UserPromptSubmit",
        prompt_fixture("claude-user-prompt-submit.json", a),
    );
    assert_eq!(h.got(a), 0);
}
#[test]
fn no_stall_after_done_fail_or_ask() {
    for outcome in ["done", "fail", "ask"] {
        let h = Harness::new();
        let (w, _, env) = h.lane("claude", "implementer");
        h.hook(
            &env,
            "claude",
            "SessionStart",
            fixture("claude-session-start.json"),
        );
        let a = h.prompt(w, false);
        h.ok(&env, &[outcome, "result"]);
        h.hook(&env, "claude", "Stop", fixture("claude-stop.json"));
        assert_eq!(h.stalls(a), 0, "{outcome}");
    }
}
#[test]
fn role_stalls() {
    for role in ["sub-orchestrator", "orchestrator", "reviewer"] {
        let h = Harness::new();
        let (w, _, env) = h.lane("claude", role);
        h.hook(
            &env,
            "claude",
            "SessionStart",
            fixture("claude-session-start.json"),
        );
        let a = h.prompt(w, false);
        h.hook(
            &env,
            "claude",
            "UserPromptSubmit",
            prompt_fixture("claude-user-prompt-submit.json", a),
        );
        h.hook(&env, "claude", "Stop", fixture("claude-stop.json"));
        assert_eq!(h.stalls(a), i64::from(role == "reviewer"));
        if role != "reviewer" {
            let a = h.prompt(w, false);
            h.hook(
                &env,
                "claude",
                "UserPromptSubmit",
                prompt_fixture("claude-user-prompt-submit.json", a),
            );
            h.hook(
                &env,
                "claude",
                "StopFailure",
                fixture("claude-stop-failure.json"),
            );
            assert_eq!(h.stalls(a), 1);
            assert_eq!(h.stall(a)["error"], "rate_limit");
        }
    }
}
fn uncoded(provider: &str, role: &str) {
    let (name, start, prompt, stop, sf, pf, ef) =
        FLOWS.iter().find(|f| f.0 == provider).copied().unwrap();
    let h = Harness::new();
    let (w, _, env) = h.lane(provider, role);
    h.hook(&env, name, start, fixture(sf));
    let a = h.prompt(w, false);
    h.hook(&env, name, prompt, prompt_fixture(pf, a));
    let mut v = fixture(ef);
    match provider {
        "claude" => v["error"] = json!("API Error: 529 overloaded"),
        "opencode" => v["properties"]["error"] = json!("API Error: 529 overloaded"),
        "pi" => {
            v["message"] = json!({"stopReason":"error","errorMessage":"API Error: 529 overloaded"})
        }
        _ => unreachable!(),
    }
    h.hook(&env, name, stop, v);
    assert_eq!(h.stalls(a), 1);
    assert_eq!(h.stall(a)["error"], "unknown");
}
#[test]
fn uncoded_stop_failure_stalls() {
    for role in ["orchestrator", "sub-orchestrator", "implementer"] {
        uncoded("claude", role);
    }
}
#[test]
fn opencode_uncoded_error_stalls() {
    uncoded("opencode", "implementer");
}
#[test]
fn pi_uncoded_error_stalls() {
    uncoded("pi", "implementer");
}
#[test]
fn silent_noops() {
    let h = Harness::new();
    let (_, _, env) = h.lane("claude", "implementer");
    let before: i64 = h
        .conn()
        .query_row("select count(*) from events", [], |r| r.get(0))
        .unwrap();
    h.hook(
        &[],
        "claude",
        "SessionStart",
        fixture("claude-session-start.json"),
    );
    h.hook_bytes(&env, "claude", "SessionStart", b"{broken");
    for (name, event, file) in [
        ("unknown", "SessionStart", "claude-session-start.json"),
        ("claude", "SessionEnd", "claude-session-start.json"),
        ("opencode", "session.status", "opencode-session-status.json"),
        ("pi", "agent_start", "pi-agent-start.json"),
        ("pi", "input", "pi-agent-start.json"),
    ] {
        h.hook(&env, name, event, fixture(file));
    }
    assert_eq!(
        before,
        h.conn()
            .query_row("select count(*) from events", [], |r| r.get::<_, i64>(0))
            .unwrap()
    );
    let path = h.home.join("missing/taskr.db");
    let mut missing = env;
    missing.push(("TASKR_DB".into(), path.to_str().unwrap().into()));
    h.hook(
        &missing,
        "claude",
        "SessionStart",
        fixture("claude-session-start.json"),
    );
    assert!(!path.exists());
}
#[test]
fn older_binary_shell_guard() {
    let h = Harness::new();
    h.script("taskr", "#!/bin/sh\necho unknown command >&2\nexit 2\n");
    let out = Command::new("/bin/sh")
        .args(["-c", "taskr hook claude Stop >/dev/null 2>&1; true"])
        .env_clear()
        .env("PATH", h.home.join("bin"))
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
}
#[test]
fn deadline_with_held_db_lock() {
    let h = Harness::new();
    let (_, l, env) = h.lane("claude", "implementer");
    let db = h.conn();
    db.execute_batch("begin immediate; insert into meta(key,value) values('hook-lock','held')")
        .unwrap();
    let start = Instant::now();
    h.hook(
        &env,
        "claude",
        "SessionStart",
        fixture("claude-session-start.json"),
    );
    assert!(
        start.elapsed() < Duration::from_millis(450),
        "{:?}",
        start.elapsed()
    );
    let session: Option<String> = db
        .query_row("select session_ref from launches where id=?", [l], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(session.is_none());
    db.execute_batch("rollback").unwrap();
}
#[test]
fn migration_from_v010() {
    let h = Harness::new();
    let legacy = taskr_core::schema::SCHEMA.replace(
        "session_ref  text, session_kind text, session_source text, transcript_path text,",
        "session_ref  text, session_kind text, session_source text,",
    );
    assert_ne!(legacy, taskr_core::schema::SCHEMA);
    let db = h.conn();
    db.execute_batch(&legacy).unwrap();
    db.execute_batch("insert into tasks(id,name,role,status,created_at,updated_at) values(1,'old','implementer','open','2026-10-01T00:00:00Z','2026-10-01T00:00:00Z'); insert into launches(id,task_id,observed_version,recorded_at) values(1,1,19,'2026-10-01T00:00:00Z')").unwrap();
    drop(db);
    let output = h.output(&[], &["--json", "status"], b"");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        h.conn()
            .query_row(
                "select count(*) from pragma_table_info('launches') where name='transcript_path'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        h.count("select observed_version from launches where id=?", 1),
        19
    );
}
#[test]
fn hooked_liveness_rule() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    h.hook(
        &env,
        "claude",
        "SessionStart",
        fixture("claude-session-start.json"),
    );
    h.prompt(w, false);
    for (status, seq) in [("idle", 2), ("unknown", 3), ("done", 4)] {
        h.agents(Some(status), seq);
        h.daemon();
        let sql = format!(
            "select coalesce((select cast(value as integer) from meta where key like 'hint_suppressed:%:{status}:hooked'),0)"
        );
        assert_eq!(
            h.conn()
                .query_row(&sql, [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    assert_eq!(
        h.count(
            "select count(*) from events where task_id=? and kind='herdr'",
            w
        ),
        0
    );
    h.prompt(w, true);
    h.agents(None, 0);
    h.daemon();
    h.agents(Some("blocked"), 4);
    h.daemon();
    assert_eq!(
        h.count(
            "select count(*) from events where task_id=? and kind='herdr'",
            w
        ),
        2
    );
    let h = Harness::new();
    let (w, _, _) = h.lane("claude", "implementer");
    h.prompt(w, false);
    h.agents(Some("done"), 1);
    h.daemon();
    assert_eq!(
        h.count(
            "select count(*) from events where task_id=? and kind='herdr'",
            w
        ),
        1
    );
}
const SESSION: &str = "019a0f91-fbbd-7051-83dd-74a92bc55288";
fn codex_payload(h: &Harness, file: &str, attempt: i64) -> Value {
    let mut v = fixture(file);
    v["session_id"] = json!(SESSION);
    v["transcript_path"] = json!(rollout_path(h));
    if attempt > 0 {
        v["prompt"] = json!(format!("First taskr got {attempt}. Continue."));
    }
    v
}
fn rollout_path(h: &Harness) -> PathBuf {
    h.home
        .join("codex/sessions/2026/10/01")
        .join(format!("rollout-2026-10-01T20-00-00-{SESSION}.jsonl"))
}
fn codex_lane(h: &Harness, role: &str) -> (i64, i64, Env, Env) {
    let (w, l, hook) = h.lane("codex", role);
    fs::create_dir_all(rollout_path(h).parent().unwrap()).unwrap();
    h.hook(
        &hook,
        "codex",
        "SessionStart",
        codex_payload(h, "codex-session-start.json", 0),
    );
    let mut worker = hook.clone();
    worker.retain(|(k, _)| k != "HERDR_ENV");
    worker.extend([
        (
            "CODEX_HOME".into(),
            h.home.join("codex").to_str().unwrap().into(),
        ),
        ("CODEX_THREAD_ID".into(), "env-thread".into()),
    ]);
    (w, l, hook, worker)
}
#[test]
fn codex_binding_survives_worker_identity() {
    for case in ["start", "model-first got", "nested rebind"] {
        let h = Harness::new();
        let (w, l, hook, worker) = codex_lane(&h, "implementer");
        let a = h.prompt(w, false);
        if case == "model-first got" {
            h.ok(&worker, &["got", &a.to_string()]);
        } else {
            h.hook(
                &hook,
                "codex",
                "UserPromptSubmit",
                codex_payload(&h, "codex-user-prompt-submit.json", a),
            );
            h.ok(&worker, &["start"]);
        }
        let (session,kind,source,home):(String,String,String,String)=h.conn().query_row("select session_ref,session_kind,session_source,coalesce(native_home,'') from launches where id=?",[l],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(session, SESSION);
        assert_eq!(kind, "thread_id");
        assert_eq!(source, "hook:SessionStart");
        assert_eq!(home, h.home.join("codex").to_str().unwrap());
        if case == "nested rebind" {
            let mut v = codex_payload(&h, "codex-session-start.json", 0);
            v["session_id"] = json!("nested-session");
            h.hook(&hook, "codex", "SessionStart", v);
            let bound: String = h
                .conn()
                .query_row("select session_ref from launches where id=?", [l], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(bound, SESSION);
        } else {
            h.hook(
                &hook,
                "codex",
                "Stop",
                codex_payload(&h, "codex-stop.json", 0),
            );
            assert_eq!(h.stalls(a), 1);
        }
    }
}
fn prompt_at(h: &Harness, a: i64) -> time::OffsetDateTime {
    let s: String = h
        .conn()
        .query_row("select created_at from events where id=?", [a], |r| {
            r.get(0)
        })
        .unwrap();
    time::OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).unwrap()
}
fn error_turn(at: time::OffsetDateTime, timestamp: bool, uncoded: bool) -> Value {
    let mut v = json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","last_agent_message":null,"completed_at":at.unix_timestamp(),"error":{"message":"Selected model is at capacity.","codex_error_info":"server_overloaded"}}});
    if timestamp {
        v["timestamp"] = json!(
            at.format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        );
    }
    if uncoded {
        v["payload"]["error"] = json!("API Error: 529 overloaded");
    }
    v
}
fn write_rollout(h: &Harness, rows: &[Value]) {
    fs::write(
        rollout_path(h),
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
}
#[test]
fn codex_rollout_fallback() {
    for (case, role, want) in [
        (
            "error after prompt",
            "implementer",
            Some("server_overloaded"),
        ),
        (
            "completed_at fallback",
            "implementer",
            Some("server_overloaded"),
        ),
        ("later user message", "implementer", None),
        ("error older than prompt", "implementer", None),
        (
            "sub-orchestrator error after prompt",
            "sub-orchestrator",
            Some("server_overloaded"),
        ),
        (
            "sub-orchestrator uncoded error after prompt",
            "sub-orchestrator",
            Some("unknown"),
        ),
    ] {
        let h = Harness::new();
        let (w, _, hook, worker) = codex_lane(&h, role);
        let a = h.prompt(w, false);
        h.hook(
            &hook,
            "codex",
            "UserPromptSubmit",
            codex_payload(&h, "codex-user-prompt-submit.json", a),
        );
        assert_eq!(h.ok(&worker, &["got", &a.to_string()])["duplicate"], true);
        let p = prompt_at(&h, a);
        let user = json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"First taskr got 1."}]}});
        let error = error_turn(
            p + time::Duration::seconds(if case == "error older than prompt" {
                -1
            } else {
                2
            }),
            case != "completed_at fallback",
            case.contains("uncoded"),
        );
        let rows = if case == "later user message" {
            vec![error, user]
        } else {
            vec![user, error]
        };
        write_rollout(&h, &rows);
        h.agents(Some("idle"), 2);
        h.daemon();
        assert_eq!(h.stalls(a), i64::from(want.is_some()), "{case}");
        if let Some(error) = want {
            assert_eq!(h.stall(a)["error"], error, "{case}");
            assert_eq!(h.stall(a)["reason"], "stall");
        }
    }
}
#[test]
fn codex_rollout_reprompt() {
    let h = Harness::new();
    let (w, _, hook, worker) = codex_lane(&h, "implementer");
    let a = h.prompt(w, false);
    h.hook(
        &hook,
        "codex",
        "UserPromptSubmit",
        codex_payload(&h, "codex-user-prompt-submit.json", a),
    );
    h.ok(&worker, &["got", &a.to_string()]);
    write_rollout(&h, &[error_turn(prompt_at(&h, a), true, false)]);
    h.agents(Some("idle"), 2);
    h.daemon();
    assert_eq!(h.stalls(a), 1);
    let a2 = h.prompt(w, false);
    assert!(prompt_at(&h, a2) > prompt_at(&h, a));
    h.agents(Some("working"), 3);
    h.daemon();
    assert_eq!(h.stalls(a2), 0);
}
#[test]
fn codex_rollout_fixtures() {
    for (file, want) in [
        ("codex-rollout-error.jsonl", 1),
        ("codex-rollout-reprompt.jsonl", 0),
    ] {
        let h = Harness::new();
        let (w, _, hook, _) = codex_lane(&h, "implementer");
        let a = h.prompt(w, false);
        h.hook(
            &hook,
            "codex",
            "UserPromptSubmit",
            codex_payload(&h, "codex-user-prompt-submit.json", a),
        );
        h.conn()
            .execute(
                "update events set created_at='2026-10-01T20:00:00.000Z' where id=?",
                [a],
            )
            .unwrap();
        fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../testdata/hooks")
                .join(file),
            rollout_path(&h),
        )
        .unwrap();
        h.agents(Some("idle"), 2);
        h.daemon();
        assert_eq!(h.stalls(a), want, "{file}");
        if want == 1 {
            assert_eq!(h.stall(a)["error"], "server_overloaded");
        }
    }
}
#[test]
fn pi_worker_ignores_inherited_codex_thread() {
    let h = Harness::new();
    let (_, l, mut env) = h.lane("pi", "implementer");
    env.push(("CODEX_THREAD_ID".into(), "inherited-thread".into()));
    assert!(h.ok(&env, &["start"])["session_ref"].is_null());
    h.hook(
        &env,
        "pi",
        "session_start",
        fixture("pi-session-start.json"),
    );
    let got: (String, String, String) = h
        .conn()
        .query_row(
            "select session_ref,session_kind,session_source from launches where id=?",
            [l],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        got,
        (
            "pi-session".into(),
            "id".into(),
            "hook:session_start".into()
        )
    );
}
