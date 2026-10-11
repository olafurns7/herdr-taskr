//! `taskr consumer`: the opt-in inbox lease that fences wait/ack. Every command is its
//! own process and every assertion reads the reopened scratch ledger. In-flight waits
//! run as child processes stopped by a lease change at their next poll.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};
use support::*;

#[test]
fn closed_stdout_exits_quietly() {
    let h = Harness::new();
    let root = id(
        &h.ok(&[], &["new", "top", "--role", "orchestrator"]),
        "task_id",
    );
    h.ok(&[], &["note", "fixture", "--as", &root]);
    for args in [
        vec!["consumer", "show", "--as", &root],
        vec!["log", &root],
        vec!["consumer", "--help"],
    ] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let out = h
            .command(&[])
            .args(&args)
            .stdout(writer)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
            .wait_with_output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked"), "{args:?}: {err}");
        assert_eq!(out.status.code(), Some(141), "{args:?}: {err}");
        assert!(err.is_empty(), "{args:?}: {err}");
    }
}

fn run(h: &Harness, env: &[(String, String)], args: &[&str]) -> (Option<i32>, String, String) {
    let out = h.output(env, args, b"");
    (
        out.status.code(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}
fn stdout(h: &Harness, env: &[(String, String)], args: &[&str]) -> String {
    let (code, out, err) = run(h, env, args);
    assert_eq!(code, Some(0), "{args:?}: {out}{err}");
    out
}
fn text(h: &Harness, sql: &str) -> String {
    h.conn()
        .query_row(sql, [], |r| r.get::<_, Option<String>>(0))
        .unwrap()
        .unwrap_or_default()
}
fn lease(h: &Harness) -> String {
    text(
        h,
        "select group_concat(task_id||'|'||fence||'|'||ifnull(holder,'-')||'|'||ifnull(launch_id,'-')||'|'||ifnull(session_gen,'-')||'|'||ifnull(machine,'-')||'|'||acquired_at||'|'||expires_at,' ') from (select * from consumer_leases order by task_id)",
    )
}
fn cursors(h: &Harness) -> String {
    text(
        h,
        "select group_concat(id||':'||acked_event_id||':'||ifnull(pending_event_id,'-'),' ') from (select * from tasks order by id)",
    )
}
/// Lease, inbox, event, attempt, marker and meta state: what a rejection must leave untouched.
fn snapshot(h: &Harness) -> String {
    format!(
        "{} # {} # {} # {} # {} # {}",
        lease(h),
        cursors(h),
        text(h, "select count(*)||'/'||ifnull(max(id),0) from events"),
        text(h, "select count(*)||'' from delivery_attempts"),
        text(
            h,
            "select group_concat(id||':'||ifnull(waiting_until,'-')||':'||status||':'||ifnull(current_launch_id,'-'),' ') from (select * from tasks order by id)"
        ),
        text(
            h,
            "select group_concat(key||'='||value,' ') from (select * from meta order by key)"
        ),
    )
}
/// A worker with a current launch and a live fence passing a root's --session: usage
/// (exit 2) before the ledger is opened, never a rejection (exit 6).
fn worker_session_is_usage(h: &Harness, env: &[(String, String)], w: &str, fence: &str) {
    for args in [
        vec![
            "wait",
            "--as",
            w,
            "--session",
            "0",
            "--fence",
            fence,
            "--timeout",
            "0",
        ],
        vec!["ack", "1", "--as", w, "--session", "0", "--fence", fence],
        vec![
            "consumer",
            "release",
            "--as",
            w,
            "--session",
            "0",
            "--fence",
            fence,
        ],
        vec![
            "consumer",
            "renew",
            "--session",
            "0",
            "--fence",
            fence,
            "--ttl",
            "5000",
        ],
        vec![
            "consumer",
            "take",
            "--as",
            w,
            "--session",
            "0",
            "--holder",
            "x",
            "--ttl",
            "5000",
            "--force",
        ],
    ] {
        let out = rejected(h, env, &args, 2);
        assert!(out.contains("\"k\":\"usage\""), "{args:?}: {out}");
    }
}
fn rejected(h: &Harness, env: &[(String, String)], args: &[&str], want: i32) -> String {
    let before = snapshot(h);
    let (code, out, err) = run(h, env, args);
    assert_eq!(code, Some(want), "{args:?}: {out}{err}");
    assert_eq!(snapshot(h), before, "{args:?} wrote");
    out
}
fn id(v: &serde_json::Value, key: &str) -> String {
    v[key]
        .as_i64()
        .unwrap_or_else(|| panic!("{key}: {v}"))
        .to_string()
}
fn parent(h: &Harness, task: i64) -> String {
    h.count("select parent_id from tasks where id=?", task)
        .to_string()
}
fn acked(h: &Harness, task: &str) -> String {
    text(
        h,
        &format!(
            "select acked_event_id||'/'||ifnull(pending_event_id,'-') from tasks where id={task}"
        ),
    )
}
/// A root's generation as `taskr adopt` reports it on stderr: "...; event EID".
fn adopt(h: &Harness, root: &str, pane: &str) -> String {
    let (code, _, err) = run(h, &[], &["adopt", root, "--pane", pane]);
    assert_eq!(code, Some(0), "{err}");
    err.trim_end().rsplit_once("event ").unwrap().1.to_string()
}
fn relaunch(h: &Harness, w: &str, env: &Env) -> (String, Env) {
    let l = id(
        &h.ok(
            &[],
            &[
                "launch",
                w,
                "--provider",
                "claude",
                "--model",
                "test",
                "--effort",
                "medium",
            ],
        ),
        "launch_id",
    );
    let env = env
        .iter()
        .map(|(k, v)| {
            let v = if k == "TASKR_LAUNCH" {
                l.clone()
            } else {
                v.clone()
            };
            (k.clone(), v)
        })
        .collect();
    (l, env)
}
/// The fields of a compact `e1` wait line: [tag, event, task, launch, kind, replay, ...].
fn event(out: &str) -> Vec<String> {
    let line = out
        .lines()
        .find(|l| l.starts_with("e1\t"))
        .unwrap_or_else(|| panic!("no e1 line: {out}"));
    line.split('\t').map(str::to_string).collect()
}
/// An ask from the worker: an event in the root's inbox.
fn ask(h: &Harness, env: &Env, text: &str) -> String {
    id(&h.ok(env, &["ask", text]), "ask_id")
}
fn spawn(h: &Harness, env: &[(String, String)], args: &[&str]) -> Child {
    h.command(env)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}
fn finish(child: Child) -> (Option<i32>, String) {
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8(out.stdout).unwrap() + &String::from_utf8(out.stderr).unwrap(),
    )
}
/// Until a polling wait of `task` has published a waiting_until marker other than `prev`.
fn marked(h: &Harness, task: &str, prev: &str) {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let now = text(
            h,
            &format!("select waiting_until from tasks where id={task}"),
        );
        if !now.is_empty() && now != prev {
            break;
        }
        assert!(Instant::now() < end, "task {task}'s wait never polled");
        std::thread::sleep(Duration::from_millis(20));
    }
}
#[cfg(feature = "contract")]
fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}
#[allow(clippy::too_many_arguments)]
fn insert_event(
    h: &Harness,
    task: &str,
    to: &str,
    launch: Option<&str>,
    kind: &str,
    summary: Option<&str>,
    data: &str,
    related: Option<&str>,
) -> String {
    let c = h.conn();
    c.execute(
        "insert into events(task_id,recipient_task_id,launch_id,kind,summary,data,related_event_id,created_at) values(?,?,?,?,?,?,?,'2026-01-01T00:00:00.000Z')",
        taskr_core::db::params![task, to, launch, kind, summary, data, related],
    )
    .unwrap();
    c.last_insert_rowid().to_string()
}

#[test]
fn competing_consumers_take_force_and_stale_fences() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let ws = w.to_string();
    let r = parent(&h, w);
    let q = ask(&h, &env, "which way?");
    let a = id(&h.ok(&[], &["answer", &q, "left"]), "answer_id");
    let take = |holder: &'static str, force: bool| {
        let mut args = vec!["consumer", "take", "--holder", holder, "--ttl", "600000"];
        if force {
            args.push("--force");
        }
        args
    };
    // No lease: show is free at fence 0 and a take is fence 1.
    assert_eq!(stdout(&h, &env, &["consumer", "show"]), "cs1 0 free\n");
    let first = h.ok(&env, &take("pi-ext", false));
    assert!(
        first["fence"] == 1
            && first["live"] == true
            && first["holder"] == "pi-ext"
            && first["launch_id"] == h.count("select current_launch_id from tasks where id=?", w)
            && first["session_gen"].is_null()
            && first["machine"].is_null(),
        "{first}"
    );
    // Any second take, the holder's own included, is refused with nothing written.
    rejected(&h, &env, &take("pi-ext", false), 6);
    let out = rejected(&h, &env, &take("cli", false), 6);
    assert!(
        out.contains("held by consumer pi-ext with fence 1"),
        "{out}"
    );
    // While the lease is live, plain consumption is refused and names the holder.
    let out = rejected(&h, &env, &["wait", "--timeout", "0"], 6);
    assert!(
        out.contains("held by consumer pi-ext with fence 1 until ")
            && out.contains("consumer release --fence 1")
            && out.contains("consumer take --force"),
        "{out}"
    );
    rejected(&h, &env, &["wait", "--as", &ws, "--timeout", "0"], 6);
    rejected(&h, &env, &["ack", &a, "--as", &ws], 6);
    rejected(&h, &env, &["delivery", "new", &a, "--adapter", "pi-ext"], 6);
    // The holder consumes with its fence (wait, delivery new, ack).
    let got = stdout(&h, &env, &["wait", "--fence", "1", "--timeout", "0"]);
    assert_eq!(event(&got)[1], a);
    assert_eq!(acked(&h, &ws), format!("0/{a}"));
    h.ok(
        &env,
        &["delivery", "new", &a, "--adapter", "pi-ext", "--fence", "1"],
    );
    // A forced takeover bumps the fence; the old fence is dead everywhere.
    let second = stdout(&h, &env, &take("cli", true));
    assert!(second.starts_with("cs1 2 live cli "), "{second}");
    for args in [
        vec!["wait", "--fence", "1", "--timeout", "0"],
        vec!["wait", "--ack", &a, "--fence", "1", "--timeout", "0"],
        vec!["ack", &a, "--as", &ws, "--fence", "1"],
        vec!["consumer", "renew", "--fence", "1", "--ttl", "5000"],
        vec!["consumer", "release", "--fence", "1"],
        vec!["delivery", "new", &a, "--adapter", "pi-ext", "--fence", "1"],
    ] {
        let out = rejected(&h, &env, &args, 6);
        assert!(out.contains("fence 1 is stale"), "{args:?}: {out}");
    }
    // A wrong task under the worker's identity is refused too.
    rejected(&h, &env, &["ack", &a, "--as", &r, "--fence", "2"], 6);
    // The successor gets the pending event again (replay) and acks it.
    let again = event(&stdout(
        &h,
        &env,
        &["wait", "--fence", "2", "--timeout", "0"],
    ));
    assert!(again[1] == a && again[5] == "1", "{again:?}");
    assert_eq!(
        stdout(&h, &env, &["ack", &a, "--as", &ws, "--fence", "2"]),
        format!("k1 {a}\n")
    );
    assert_eq!(acked(&h, &ws), format!("{a}/-"));
    // Release hands it back: the fence counter stays and plain calls work again.
    assert_eq!(
        stdout(&h, &env, &["consumer", "release", "--fence", "2"]),
        "cs1 2 free\n"
    );
    assert_eq!(stdout(&h, &env, &["consumer", "show"]), "cs1 2 free\n");
    let rows = lease(&h);
    assert!(stdout(&h, &env, &["wait", "--timeout", "0"]).contains("x1 3 timeout"));
    assert_eq!(lease(&h), rows, "a plain wait never touches the lease");

    // A take while a plain wait polls is refused; --force takes over, and the plain wait
    // stops at its next poll without consuming an event that arrives meanwhile.
    let waiter = spawn(&h, &env, &["wait", "--timeout", "60000"]);
    marked(&h, &ws, "");
    let out = rejected(&h, &env, &take("pi-ext", false), 6);
    assert!(out.contains("is marked until"), "{out}");
    assert!(stdout(&h, &env, &take("pi-ext", true)).starts_with("cs1 3 live pi-ext "));
    let a2 = id(
        &h.ok(&[], &["answer", &ask(&h, &env, "still there?"), "yes"]),
        "answer_id",
    );
    let (code, out) = finish(waiter);
    assert!(
        code == Some(6) && out.contains("held by consumer pi-ext with fence 3"),
        "{out}"
    );
    assert_eq!(
        acked(&h, &ws),
        format!("{a}/-"),
        "the stopped plain wait consumed"
    );
    assert!(
        text(
            &h,
            &format!("select waiting_until from tasks where id={ws}")
        )
        .is_empty(),
        "the stopped wait clears its marker"
    );
    // An in-flight fenced wait stops at its next poll after a takeover, likewise.
    assert_eq!(
        event(&stdout(
            &h,
            &env,
            &["wait", "--fence", "3", "--timeout", "0"]
        ))[1],
        a2
    );
    h.ok(&env, &["ack", &a2, "--as", &ws, "--fence", "3"]);
    let fenced = spawn(&h, &env, &["wait", "--fence", "3", "--timeout", "60000"]);
    marked(&h, &ws, "");
    assert!(stdout(&h, &env, &take("cli", true)).starts_with("cs1 4 live cli "));
    h.ok(&[], &["answer", &ask(&h, &env, "and now?"), "no"]);
    let (code, out) = finish(fenced);
    assert!(code == Some(6) && out.contains("fence 3 is stale"), "{out}");
    assert_eq!(
        acked(&h, &ws),
        format!("{a2}/-"),
        "the stopped fenced wait consumed"
    );

    // Marker-publication race: the take landed before a plain wait published its marker;
    // that wait stops at its first poll, leaving no marker and consuming nothing.
    let (code, out) = finish(spawn(&h, &env, &["wait", "--timeout", "60000"]));
    assert!(
        code == Some(6) && out.contains("held by consumer cli with fence 4"),
        "{out}"
    );
    assert!(
        text(
            &h,
            &format!("select waiting_until from tasks where id={ws}")
        )
        .is_empty()
    );
    assert_eq!(acked(&h, &ws), format!("{a2}/-"));
    let a3 = event(&stdout(
        &h,
        &env,
        &["wait", "--fence", "4", "--timeout", "0"],
    ))[1]
        .clone();
    h.ok(&env, &["ack", &a3, "--as", &ws, "--fence", "4"]);
    stdout(&h, &env, &["consumer", "release", "--fence", "4"]);

    // A take and release between two polls of a plain wait: at its next poll the lease is
    // free and the wait carries on as before (legacy identity), then consumes. A poll that
    // lands while the lease is live stops it instead (exit 6), so repeat until the gap.
    let mut carried = false;
    for _ in 0..5 {
        let waiter = spawn(&h, &env, &["wait", "--timeout", "60000"]);
        marked(&h, &ws, "");
        let took = h.ok(
            &env,
            &[
                "consumer", "take", "--holder", "cli", "--ttl", "600000", "--force",
            ],
        );
        h.ok(
            &env,
            &["consumer", "release", "--fence", &id(&took, "fence")],
        );
        let next = id(
            &h.ok(&[], &["answer", &ask(&h, &env, "gap?"), "yes"]),
            "answer_id",
        );
        let (code, out) = finish(waiter);
        if code == Some(0) {
            assert_eq!(event(&out)[1], next, "{out}");
            carried = true;
            break;
        }
        assert!(
            code == Some(6) && out.contains("held by consumer cli"),
            "{out}"
        );
        assert_eq!(
            event(&stdout(&h, &env, &["wait", "--timeout", "0"]))[1],
            next
        );
        h.ok(&env, &["ack", &next, "--as", &ws]);
    }
    assert!(
        carried,
        "a plain wait never slept through a take and release"
    );
}

#[test]
fn usage_and_repeated_flags_exit_2_unwritten() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let ws = w.to_string();
    let r = parent(&h, w);
    let q = ask(&h, &env, "go?");
    h.ok(
        &[],
        &[
            "consumer",
            "take",
            "--holder",
            "ch",
            "--ttl",
            "60000",
            "--as",
            &r,
            "--session",
            "0",
        ],
    );
    for (env, args) in [
        // TTL bounds, holder token, fence value.
        (
            &env,
            vec!["consumer", "take", "--holder", "x", "--ttl", "999"],
        ),
        (
            &env,
            vec!["consumer", "take", "--holder", "x", "--ttl", "3600001"],
        ),
        (&env, vec!["consumer", "take", "--holder", "x"]),
        (
            &env,
            vec!["consumer", "take", "--holder", "Bad", "--ttl", "5000"],
        ),
        (&env, vec!["consumer", "take", "--ttl", "5000"]),
        (&env, vec!["consumer", "renew", "--ttl", "5000"]),
        (&env, vec!["consumer", "renew", "--fence", "1"]),
        (&env, vec!["consumer", "release"]),
        (&env, vec!["consumer", "release", "--fence", "0"]),
        (&env, vec!["consumer", "show", "--session", "0"]),
        (&env, vec!["consumer", "show", "extra"]),
        (&env, vec!["consumer", "bogus"]),
        (&env, vec!["consumer"]),
        // A root's --session is required with --as, refused without it.
        (
            &vec![],
            vec![
                "consumer", "take", "--holder", "x", "--ttl", "5000", "--as", &r,
            ],
        ),
        (
            &env,
            vec![
                "consumer",
                "take",
                "--holder",
                "x",
                "--ttl",
                "5000",
                "--session",
                "0",
            ],
        ),
        (
            &vec![],
            vec!["wait", "--as", &r, "--fence", "1", "--timeout", "0"],
        ),
        (
            &vec![],
            vec!["wait", "--as", &r, "--session", "0", "--timeout", "0"],
        ),
        (&vec![], vec!["ack", &q, "--as", &r, "--session", "0"]),
        (&vec![], vec!["ack", &q, "--as", &r, "--fence", "1"]),
        (
            &vec![],
            vec!["ack", &q, "--as", &r, "--fence", "0", "--session", "0"],
        ),
        (
            &env,
            vec!["wait", "--fence", "1", "--session", "0", "--timeout", "0"],
        ),
        (
            &env,
            vec!["delivery", "new", &q, "--adapter", "x", "--fence", "0"],
        ),
        // Repeated consumer flags, -h and --help as one flag.
        (
            &env,
            vec![
                "consumer",
                "take",
                "--holder",
                "x",
                "--holder=x",
                "--ttl",
                "5000",
            ],
        ),
        (
            &env,
            vec![
                "consumer", "take", "--holder", "x", "--ttl", "5000", "-ttl", "5000",
            ],
        ),
        (
            &env,
            vec![
                "consumer",
                "take",
                "--holder",
                "x",
                "--ttl",
                "5000",
                "--force",
                "--force=false",
            ],
        ),
        (
            &env,
            vec![
                "consumer",
                "renew",
                "--fence",
                "1",
                "--fence=1",
                "--ttl",
                "5000",
            ],
        ),
        (
            &env,
            vec![
                "consumer",
                "release",
                "--fence",
                "1",
                "--help",
                "--help=false",
            ],
        ),
        (
            &env,
            vec!["consumer", "release", "--fence", "1", "-h", "--help=false"],
        ),
        (
            &vec![],
            vec![
                "consumer",
                "release",
                "--fence",
                "1",
                "--as",
                &r,
                "--session",
                "0",
                "--session=0",
            ],
        ),
        // Repeated new flags on wait, ack and delivery new.
        (
            &env,
            vec!["wait", "--fence", "1", "--fence", "1", "--timeout", "0"],
        ),
        (
            &vec![],
            vec![
                "wait",
                "--as",
                &r,
                "--fence",
                "1",
                "--session",
                "0",
                "--session",
                "0",
            ],
        ),
        (
            &vec![],
            vec![
                "ack",
                &q,
                "--as",
                &r,
                "--fence",
                "1",
                "--session",
                "0",
                "--help",
                "--help=false",
            ],
        ),
        (
            &env,
            vec![
                "delivery",
                "new",
                &q,
                "--adapter",
                "x",
                "--fence",
                "1",
                "--fence=1",
            ],
        ),
    ] {
        rejected(&h, env, &args, 2);
    }
    // A worker's live fence with a root's --session is a usage error, not lost authority.
    assert!(
        stdout(
            &h,
            &env,
            &["consumer", "take", "--holder", "pi", "--ttl", "600000"]
        )
        .starts_with("cs1 1 live pi ")
    );
    worker_session_is_usage(&h, &env, &ws, "1");
    assert!(stdout(&h, &env, &["consumer", "show"]).starts_with("cs1 1 live pi "));
    // The command-group help refuses a repeated help flag, as check_args does.
    for args in [
        vec!["consumer", "--help", "--help"],
        vec!["consumer", "-h", "--help"],
        vec!["delivery", "--help", "--help"],
    ] {
        rejected(&h, &env, &args, 2);
    }
    // Help is a read; plain wait/ack keep accepting a repeated help flag as before.
    for args in [
        vec!["consumer", "--help"],
        vec!["help", "consumer"],
        vec![
            "consumer", "take", "--holder", "x", "--ttl", "5000", "--help",
        ],
    ] {
        let before = snapshot(&h);
        let out = stdout(&h, &env, &args);
        assert!(
            out.contains("consumer take --holder NAME --ttl MS"),
            "{args:?}: {out}"
        );
        assert_eq!(snapshot(&h), before);
    }
    let before = snapshot(&h);
    let out = stdout(&h, &[], &["wait", "--as", &ws, "--help"]);
    assert!(out.contains("--fence N [--session GEN]"), "{out}");
    assert_eq!(snapshot(&h), before);
}

#[test]
fn stale_bindings_free_the_lease_and_stop_in_flight_waits() {
    let h = Harness::new();
    // A working agent: a root's polling wait observes no Herdr change to wake on.
    h.agents(Some("working"), 1);
    let (w, _, env) = h.lane("claude", "implementer");
    let ws = w.to_string();
    let r = parent(&h, w);
    let q = ask(&h, &env, "go?");
    let root = |args: &[&str], gen_: &str| -> Vec<String> {
        args.iter()
            .map(|s| s.to_string())
            .chain(["--as".into(), r.clone(), "--session".into(), gen_.into()])
            .collect()
    };
    let call = |args: &[&str], gen_: &str| {
        let a = root(args, gen_);
        stdout(&h, &[], &a.iter().map(String::as_str).collect::<Vec<_>>())
    };
    let refuse = |args: &[&str], gen_: &str| {
        let a = root(args, gen_);
        rejected(
            &h,
            &[],
            &a.iter().map(String::as_str).collect::<Vec<_>>(),
            6,
        )
    };
    // A root holds with generation 0 before any adopt; another generation is refused.
    assert!(
        call(
            &["consumer", "take", "--holder", "ch", "--ttl", "600000"],
            "0"
        )
        .starts_with("cs1 1 live ch ")
    );
    assert!(
        call(
            &["consumer", "renew", "--fence", "1", "--ttl", "600000"],
            "0"
        )
        .contains(" session=0")
    );
    refuse(&["consumer", "renew", "--fence", "1", "--ttl", "5000"], "7");
    refuse(&["ack", &q, "--fence", "1"], "7");
    refuse(&["wait", "--fence", "1", "--timeout", "0"], "7");
    rejected(&h, &[], &["wait", "--as", &r, "--timeout", "0"], 6);
    // A worker's environment cannot take its root's lease or consume its inbox.
    rejected(&h, &env, &["ack", &q, "--as", &r, "--fence", "1"], 6);
    // An in-flight fenced root wait stops at its next poll after adopt; the lease is free.
    let waiter = spawn(
        &h,
        &[],
        &root(
            &[
                "wait",
                "--fence",
                "1",
                "--timeout",
                "60000",
                "--for",
                "done",
            ],
            "0",
        )
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>(),
    );
    // The filtered waiter skips q (nonmatching): the legacy filter, not the lease, moved it.
    marked(&h, &r, "");
    assert_eq!(acked(&h, &r), format!("{q}/-"));
    let g = adopt(&h, &r, "w1:p7");
    let (code, out) = finish(waiter);
    assert!(
        code == Some(6) && out.contains("session generation 0 is not root"),
        "{out}"
    );
    assert_eq!(acked(&h, &r), format!("{q}/-"), "{out}");
    let q2 = ask(&h, &env, "second?");
    assert_eq!(
        stdout(&h, &[], &["consumer", "show", "--as", &r]),
        "cs1 1 free\n"
    );
    for args in [
        vec!["ack", &q2, "--fence", "1"],
        vec!["wait", "--fence", "1", "--timeout", "0"],
        vec!["consumer", "renew", "--fence", "1", "--ttl", "5000"],
        vec!["consumer", "release", "--fence", "1"],
    ] {
        refuse(&args, "0");
        refuse(&args, &g);
    }
    // The plain wait consumes again; the new generation takes fence 2 and gets a replay.
    assert_eq!(
        event(&stdout(&h, &[], &["wait", "--as", &r, "--timeout", "0"]))[1],
        q2
    );
    assert!(
        call(
            &["consumer", "take", "--holder", "ch", "--ttl", "600000"],
            &g
        )
        .starts_with("cs1 2 live ch ")
    );
    let again = event(&call(&["wait", "--fence", "2", "--timeout", "0"], &g));
    assert!(again[1] == q2 && again[5] == "1", "{again:?}");
    refuse(&["ack", &q2, "--fence", "2"], "0");
    assert_eq!(
        call(&["ack", &q2, "--fence", "2"], &g),
        format!("k1 {q2}\n")
    );

    // Worker: a relaunch stops the old launch's fenced wait and frees the lease.
    assert!(
        stdout(
            &h,
            &env,
            &["consumer", "take", "--holder", "pi", "--ttl", "600000"]
        )
        .starts_with("cs1 1 live pi ")
    );
    let waiter = spawn(&h, &env, &["wait", "--fence", "1", "--timeout", "60000"]);
    marked(&h, &ws, "");
    let (_, env2) = relaunch(&h, &ws, &env);
    let (code, out) = finish(waiter);
    assert!(
        code == Some(6) && out.contains("is not the current launch"),
        "{out}"
    );
    assert_eq!(stdout(&h, &env2, &["consumer", "show"]), "cs1 1 free\n");
    for args in [
        vec!["wait", "--fence", "1", "--timeout", "0"],
        vec!["consumer", "renew", "--fence", "1", "--ttl", "5000"],
        vec![
            "consumer", "take", "--holder", "pi", "--ttl", "5000", "--force",
        ],
    ] {
        rejected(&h, &env, &args, 6);
    }
    // The new launch's plain wait works; it takes fence 2.
    assert!(stdout(&h, &env2, &["wait", "--timeout", "0"]).contains("x1 3 timeout"));
    assert!(
        stdout(
            &h,
            &env2,
            &["consumer", "take", "--holder", "pi", "--ttl", "600000"]
        )
        .starts_with("cs1 2 live pi ")
    );
    assert_eq!(
        stdout(&h, &env2, &["consumer", "release", "--fence", "2"]),
        "cs1 2 free\n"
    );

    // A killed plain wait leaves a stale marker: take is refused, --force recovers, also
    // after another relaunch; force never skips identity (the stale launch stays refused).
    let mut killed = spawn(&h, &env2, &["wait", "--timeout", "600000"]);
    marked(&h, &ws, "");
    killed.kill().unwrap();
    killed.wait().unwrap();
    let (_, env3) = relaunch(&h, &ws, &env2);
    assert!(
        !text(
            &h,
            &format!("select waiting_until from tasks where id={ws}")
        )
        .is_empty()
    );
    let out = rejected(
        &h,
        &env3,
        &["consumer", "take", "--holder", "pi", "--ttl", "5000"],
        6,
    );
    assert!(out.contains("it may be stale"), "{out}");
    rejected(
        &h,
        &env2,
        &[
            "consumer", "take", "--holder", "pi", "--ttl", "5000", "--force",
        ],
        6,
    );
    assert!(
        stdout(
            &h,
            &env3,
            &[
                "consumer", "take", "--holder", "pi", "--ttl", "600000", "--force"
            ]
        )
        .starts_with("cs1 3 live pi ")
    );

    // Close stops a fenced wait; the row and its fence survive, nothing consumes.
    let stale = text(
        &h,
        &format!("select waiting_until from tasks where id={ws}"),
    );
    let waiter = spawn(&h, &env3, &["wait", "--fence", "3", "--timeout", "60000"]);
    marked(&h, &ws, &stale);
    h.ok(&[], &["close", &ws]);
    let (code, out) = finish(waiter);
    assert!(code == Some(6) && out.contains("is closed"), "{out}");
    assert!(lease(&h).contains(&format!("{ws}|3|pi|")), "{}", lease(&h));
    rejected(&h, &env3, &["consumer", "release", "--fence", "3"], 6);
}

#[test]
fn handoff_replays_pending_events_within_the_cursor_rules() {
    let h = Harness::new();
    h.agents(Some("working"), 1);
    let (w, l, env) = h.lane("claude", "implementer");
    let ws = w.to_string();
    let r = parent(&h, w);
    let as_root = |args: &[&str], gen_: &str| -> Vec<String> {
        args.iter()
            .map(|s| s.to_string())
            .chain(["--as".into(), r.clone(), "--session".into(), gen_.into()])
            .collect()
    };
    let call = |args: &[&str]| {
        let a = as_root(args, "0");
        stdout(&h, &[], &a.iter().map(String::as_str).collect::<Vec<_>>())
    };
    let take = |fence: &str| {
        let out = call(&[
            "consumer", "take", "--holder", "ch", "--ttl", "600000", "--force",
        ]);
        assert!(out.starts_with(&format!("cs1 {fence} live ")), "{out}");
    };
    let q1 = ask(&h, &env, "one?");
    // Offered to holder 1, then released: the plain wait gets it again (replay).
    take("1");
    assert_eq!(
        event(&call(&["wait", "--fence", "1", "--timeout", "0"]))[1],
        q1
    );
    assert_eq!(
        call(&["consumer", "release", "--fence", "1"]),
        "cs1 1 free\n"
    );
    let plain = event(&stdout(&h, &[], &["wait", "--as", &r, "--timeout", "0"]));
    assert!(plain[1] == q1 && plain[5] == "1", "{plain:?}");
    // Then taken: the next holder gets it again too, and only its ack moves the cursor.
    take("2");
    let again = event(&call(&["wait", "--fence", "2", "--timeout", "0"]));
    assert!(again[1] == q1 && again[5] == "1", "{again:?}");
    assert_eq!(acked(&h, &r), format!("0/{q1}"));
    call(&["ack", &q1, "--fence", "2"]);
    // A late event while held reaches only the fenced consumer.
    let q2 = ask(&h, &env, "two?");
    rejected(&h, &[], &["wait", "--as", &r, "--timeout", "0"], 6);
    assert_eq!(acked(&h, &r), format!("{q1}/-"));
    let late = event(&call(&["wait", "--fence", "2", "--timeout", "0"]));
    assert!(late[1] == q2 && late[5] == "0", "{late:?}");
    // Takeover between an offer and the filtered ack (as one sequence): the old holder's
    // ack is refused and the event stays pending for the successor.
    take("3");
    rejected(
        &h,
        &[],
        &as_root(&["ack", &q2, "--fence", "2"], "0")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        6,
    );
    assert_eq!(acked(&h, &r), format!("{q1}/{q2}"));
    // A filtered successor discards the nonmatching pending event: the legacy filter.
    let out = call(&["wait", "--fence", "3", "--for", "done", "--timeout", "0"]);
    assert!(out.starts_with("sk1 1\n") && !out.contains("e1\t"), "{out}");
    assert_eq!(acked(&h, &r), format!("{q2}/-"));

    // A superseded pending Herdr hint is coalesced by a filtered successor (legacy rule).
    let hint = insert_event(
        &h,
        &ws,
        &r,
        Some(&l.to_string()),
        "herdr",
        None,
        r#"{"status":"idle"}"#,
        None,
    );
    assert_eq!(
        event(&call(&["wait", "--fence", "3", "--timeout", "0"]))[1],
        hint
    );
    let (l2, env2) = relaunch(&h, &ws, &env);
    take("4");
    let out = call(&["wait", "--fence", "4", "--for", "ask", "--timeout", "0"]);
    assert!(out.starts_with("sk1 ") && !out.contains("e1\t"), "{out}");
    assert!(
        h.count(
            "select acked_event_id from tasks where id=?",
            r.parse().unwrap()
        ) >= hint.parse().unwrap(),
        "{}",
        acked(&h, &r)
    );

    // A pending pr event is protected from storm coalescing across a handoff.
    let p1 = insert_event(
        &h,
        &ws,
        &r,
        None,
        "pr",
        Some("opened"),
        r#"{"pr":"o/r#1","sub":"opened"}"#,
        None,
    );
    assert_eq!(
        event(&call(&["wait", "--fence", "4", "--timeout", "0"]))[1],
        p1
    );
    let p2 = insert_event(
        &h,
        &ws,
        &r,
        None,
        "pr",
        Some("merged"),
        r#"{"pr":"o/r#1","sub":"merged"}"#,
        None,
    );
    take("5");
    let again = event(&call(&["wait", "--fence", "5", "--timeout", "0"]));
    assert!(again[1] == p1 && again[5] == "1", "{again:?}");
    call(&["ack", &p1, "--fence", "5"]);
    assert_eq!(
        event(&call(&["wait", "--fence", "5", "--timeout", "0"]))[1],
        p2
    );
    call(&["ack", &p2, "--fence", "5"]);

    // A pending no_receipt is protected as well; unpending it would be coalesced.
    let prompt = insert_event(&h, &ws, &ws, Some(&l2), "prompt", None, "{}", None);
    insert_event(&h, &ws, &ws, Some(&l2), "got", None, "{}", Some(&prompt));
    let nr = insert_event(
        &h,
        &ws,
        &r,
        Some(&l2),
        "prompt_outcome",
        Some("no_receipt"),
        r#"{"outcome":"no_receipt"}"#,
        Some(&prompt),
    );
    assert_eq!(
        event(&call(&["wait", "--fence", "5", "--timeout", "0"]))[1],
        nr
    );
    take("6");
    let again = event(&call(&[
        "wait",
        "--fence",
        "6",
        "--for",
        "prompt_outcome",
        "--timeout",
        "0",
    ]));
    assert!(again[1] == nr && again[5] == "1", "{again:?}");

    // Takeover between wait --ack and its next offer: exit 6, the ack stays committed.
    let waiter = spawn(
        &h,
        &[],
        &as_root(
            &["wait", "--ack", &nr, "--fence", "6", "--timeout", "60000"],
            "0",
        )
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>(),
    );
    marked(&h, &r, "");
    take("7");
    let (code, out) = finish(waiter);
    assert!(
        code == Some(6) && out.contains(&format!("\"acked_event_id\":{nr}")),
        "{out}"
    );
    assert_eq!(acked(&h, &r), format!("{nr}/-"));
    drop(env2);
}

#[cfg(feature = "contract")]
#[test]
fn expiry_follows_the_frozen_clock() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let r = parent(&h, w);
    let at = |ms: &str| -> Env {
        vec![(
            "TASKR_FROZEN_NOW".into(),
            format!("2026-01-01T00:00:0{ms}Z"),
        )]
    };
    let root = |args: &[&str]| -> Vec<String> {
        args.iter()
            .map(|s| s.to_string())
            .chain(["--as".into(), r.clone(), "--session".into(), "0".into()])
            .collect()
    };
    let show = |when: &str| stdout(&h, &at(when), &["consumer", "show", "--as", &r]);
    let call = |when: &str, args: &[&str]| {
        let a = root(args);
        stdout(
            &h,
            &at(when),
            &a.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    };
    let refuse = |when: &str, args: &[&str]| {
        let a = root(args);
        rejected(
            &h,
            &at(when),
            &a.iter().map(String::as_str).collect::<Vec<_>>(),
            6,
        )
    };
    let q = ask(&h, &env, "go?");
    assert_eq!(
        call(
            "0.000",
            &["consumer", "take", "--holder", "ch", "--ttl", "1000"]
        ),
        "cs1 1 live ch 2026-01-01T00:00:01.000Z session=0\n"
    );
    // Before the deadline the lease is live: plain wait refused, the holder consumes.
    assert!(show("0.999").starts_with("cs1 1 live"));
    rejected(&h, &at("0.999"), &["wait", "--as", &r, "--timeout", "0"], 6);
    assert_eq!(
        event(&call("0.999", &["wait", "--fence", "1", "--timeout", "0"]))[1],
        q
    );
    // At the deadline it has expired: the old fence is refused, the plain wait replays.
    assert_eq!(show("1.000"), "cs1 1 free\n");
    refuse(
        "1.000",
        &["consumer", "renew", "--fence", "1", "--ttl", "1000"],
    );
    refuse("1.000", &["ack", &q, "--fence", "1"]);
    refuse("1.000", &["wait", "--fence", "1", "--timeout", "0"]);
    let plain = event(&stdout(
        &h,
        &at("1.000"),
        &["wait", "--as", &r, "--timeout", "0"],
    ));
    assert!(plain[1] == q && plain[5] == "1", "{plain:?}");
    // After it a new take gets fence 2; renew extends from now.
    assert!(
        call(
            "1.001",
            &["consumer", "take", "--holder", "ch", "--ttl", "1000"]
        )
        .starts_with("cs1 2 live ch 2026-01-01T00:00:02.001Z")
    );
    assert_eq!(
        call(
            "1.500",
            &["consumer", "renew", "--fence", "2", "--ttl", "1000"]
        ),
        "cs1 2 live ch 2026-01-01T00:00:02.500Z session=0\n"
    );
    assert!(show("2.499").starts_with("cs1 2 live"));
    let again = event(&call("2.499", &["wait", "--fence", "2", "--timeout", "0"]));
    assert!(again[1] == q && again[5] == "1", "{again:?}");
    assert_eq!(show("2.500"), "cs1 2 free\n");
    assert_eq!(
        acked(&h, &r),
        format!("0/{q}"),
        "expiry never moves the cursor"
    );
}

/// A synthetic contract hub, killed on drop.
#[cfg(feature = "contract")]
struct Hub(Child);
#[cfg(feature = "contract")]
impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
/// Start the hub; return it, its URL, the client env (host-a) and the client's server.url.
#[cfg(feature = "contract")]
fn start_hub(h: &Harness) -> (Hub, String, Env, std::path::PathBuf) {
    use std::io::BufRead;
    h.script("tailscale", include_str!("hook/tailscale.sh"));
    let contract = [
        ("TASKR_CONTRACT_TAILNET".to_string(), "1".to_string()),
        ("TASKR_CONTRACT_ORACLE".to_string(), "1".to_string()),
    ];
    let mut hub_env = contract.to_vec();
    hub_env.push(("NET_ID".into(), "hub".into()));
    let mut hub = Hub(h
        .command(&hub_env)
        .arg("--contract-hub")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap());
    let mut url = String::new();
    let mut out = std::io::BufReader::new(hub.0.stdout.take().unwrap());
    out.read_line(&mut url).unwrap();
    // Keep the hub's stdout open: a closed pipe would fail its later writes.
    std::thread::spawn(move || std::io::copy(&mut out, &mut std::io::sink()));
    assert!(url.starts_with("http://[::1]:"), "{url:?}");
    let server = h.home.join("client/.local/state/taskr/server.url");
    std::fs::create_dir_all(server.parent().unwrap()).unwrap();
    std::fs::write(&server, &url).unwrap();
    let mut client = contract.to_vec();
    client.extend([
        (
            "HOME".into(),
            h.home.join("client").to_str().unwrap().into(),
        ),
        ("TASKR_DB".into(), "".into()),
        ("NET_ID".into(), "host-a".into()),
    ]);
    (hub, url, client, server)
}

#[cfg(feature = "contract")]
#[test]
fn rpc_leases_are_host_bound_stored_by_key_and_never_queued() {
    let h = Harness::new();
    let (_hub, url, client, server) = start_hub(&h);
    let mut fast = client.clone();
    fast.push(("TASKR_CONTRACT_RETRY_MS".into(), "0".into()));
    let r = id(
        &h.ok(&client, &["new", "remote", "--role", "orchestrator"]),
        "task_id",
    );
    let w = id(
        &h.ok(
            &client,
            &[
                "new",
                "impl",
                "--role",
                "implementer",
                "--parent",
                &r,
                "--cwd",
                h.home.to_str().unwrap(),
                "--pane",
                "w9:p1",
            ],
        ),
        "task_id",
    );
    let l = id(
        &h.ok(
            &client,
            &[
                "launch",
                &w,
                "--provider",
                "claude",
                "--model",
                "test",
                "--effort",
                "medium",
            ],
        ),
        "launch_id",
    );
    let mut worker = client.clone();
    worker.extend([
        ("TASKR_TASK".into(), w.clone()),
        ("TASKR_LAUNCH".into(), l.clone()),
    ]);
    let requests = || h.count("select count(*) from requests where length(key)>?", 0);
    let keyed = |key: &str, args: &[&str]| -> Vec<String> {
        ["--request-key", key]
            .iter()
            .chain(args)
            .map(|s| s.to_string())
            .collect()
    };

    // Stored writes bound to the authenticated caller; show is a fresh read.
    let n = requests();
    let take1 = keyed(
        "consumer-take-0001",
        &["consumer", "take", "--holder", "pi", "--ttl", "600000"],
    );
    let first = stdout(&h, &worker, &strs(&take1));
    assert!(first.starts_with("cs1 1 live pi "), "{first}");
    assert!(
        lease(&h).contains(&format!("|{l}|-|host-a|")),
        "{}",
        lease(&h)
    );
    assert_eq!(requests(), n + 1);
    assert_eq!(stdout(&h, &worker, &["consumer", "show"]), first);
    assert_eq!(requests(), n + 1, "show is not stored");
    // Over RPC too: a worker's --session is usage (2), as is a repeated group help flag.
    worker_session_is_usage(&h, &worker, &w, "1");
    for args in [
        vec!["consumer", "--help", "--help"],
        vec!["consumer", "-h", "--help"],
    ] {
        rejected(&h, &worker, &args, 2);
    }
    assert!(stdout(&h, &worker, &["consumer", "--help"]).contains("consumer take --holder NAME"));
    // The hub decides the host: another caller is refused, nothing written.
    std::fs::write(h.home.join("whois-host"), "host-b").unwrap();
    rejected(
        &h,
        &worker,
        &["consumer", "renew", "--fence", "1", "--ttl", "5000"],
        6,
    );
    rejected(&h, &worker, &["wait", "--fence", "1", "--timeout", "0"], 6);
    rejected(
        &h,
        &client,
        &[
            "consumer",
            "take",
            "--holder",
            "x",
            "--ttl",
            "5000",
            "--as",
            &r,
            "--session",
            "0",
        ],
        6,
    );
    std::fs::remove_file(h.home.join("whois-host")).unwrap();
    // A repeated flag is refused by the hub too, unwritten (also over raw argv checks).
    rejected(
        &h,
        &worker,
        &[
            "consumer",
            "take",
            "--holder",
            "x",
            "--ttl",
            "5000",
            "--force",
            "--force=false",
        ],
        2,
    );

    // Replay after takeover: the same key returns the historical take, acquires nothing.
    assert!(
        stdout(
            &h,
            &worker,
            &[
                "consumer", "take", "--holder", "cli", "--ttl", "600000", "--force"
            ]
        )
        .starts_with("cs1 2 live cli ")
    );
    let rows = lease(&h);
    assert_eq!(stdout(&h, &worker, &strs(&take1)), first);
    assert_eq!(lease(&h), rows, "a replayed take is not a new lease");
    // Renew, then takeover, then replay: no extension of the successor.
    let renew = keyed(
        "consumer-renew-0001",
        &["consumer", "renew", "--fence", "2", "--ttl", "900000"],
    );
    let renewed = stdout(&h, &worker, &strs(&renew));
    assert!(
        stdout(
            &h,
            &worker,
            &[
                "consumer", "take", "--holder", "pi", "--ttl", "600000", "--force"
            ]
        )
        .starts_with("cs1 3 live pi ")
    );
    let rows = lease(&h);
    assert_eq!(stdout(&h, &worker, &strs(&renew)), renewed);
    assert_eq!(lease(&h), rows);
    // A replayed release never clears the successor's lease.
    let release = keyed(
        "consumer-release-0001",
        &["consumer", "release", "--fence", "3"],
    );
    assert_eq!(stdout(&h, &worker, &strs(&release)), "cs1 3 free\n");
    assert!(
        stdout(
            &h,
            &worker,
            &["consumer", "take", "--holder", "cli", "--ttl", "600000"]
        )
        .starts_with("cs1 4 live cli ")
    );
    assert_eq!(stdout(&h, &worker, &strs(&release)), "cs1 3 free\n");
    assert!(stdout(&h, &worker, &["consumer", "show"]).starts_with("cs1 4 live cli "));
    // A fenced ack replayed after takeover acks nothing new.
    let q = id(&h.ok(&worker, &["ask", "remote?"]), "ask_id");
    let a = id(&h.ok(&client, &["answer", &q, "yes"]), "answer_id");
    assert_eq!(
        event(&stdout(
            &h,
            &worker,
            &["wait", "--fence", "4", "--timeout", "0"]
        ))[1],
        a
    );
    let ack = keyed(
        "consumer-ack-00001",
        &["ack", &a, "--as", &w, "--fence", "4"],
    );
    let acked_out = stdout(&h, &worker, &strs(&ack));
    let a2 = id(
        &h.ok(
            &client,
            &[
                "answer",
                &id(&h.ok(&worker, &["ask", "again?"]), "ask_id"),
                "no",
            ],
        ),
        "answer_id",
    );
    assert_eq!(
        event(&stdout(
            &h,
            &worker,
            &["wait", "--fence", "4", "--timeout", "0"]
        ))[1],
        a2
    );
    assert!(
        stdout(
            &h,
            &worker,
            &[
                "consumer", "take", "--holder", "pi", "--ttl", "600000", "--force"
            ]
        )
        .starts_with("cs1 5 live pi ")
    );
    let before = cursors(&h);
    assert_eq!(stdout(&h, &worker, &strs(&ack)), acked_out);
    assert_eq!(cursors(&h), before, "a replayed ack moves no cursor");
    // Fresh, the same ack with the dead fence is refused.
    rejected(&h, &worker, &["ack", &a2, "--as", &w, "--fence", "4"], 6);

    // delivery new under a root lease, replayed after adopt and after expiry.
    let q3 = id(&h.ok(&worker, &["ask", "root?"]), "ask_id");
    assert!(
        stdout(
            &h,
            &client,
            &[
                "consumer",
                "take",
                "--holder",
                "ch",
                "--ttl",
                "600000",
                "--as",
                &r,
                "--session",
                "0"
            ]
        )
        .starts_with("cs1 1 live ch ")
    );
    let dv = keyed(
        "consumer-dv-00001",
        &[
            "delivery",
            "new",
            &q3,
            "--adapter",
            "ch",
            "--fence",
            "1",
            "--as",
            &r,
            "--session",
            "0",
        ],
    );
    let created = stdout(&h, &client, &strs(&dv));
    let attempts = || text(&h, "select count(*)||'' from delivery_attempts");
    let n_att = attempts();
    let g = {
        let (code, _, err) = run(&h, &client, &["adopt", &r, "--pane", "w1:p7"]);
        assert_eq!(code, Some(0), "{err}");
        err.trim_end().rsplit_once("event ").unwrap().1.to_string()
    };
    assert_eq!(stdout(&h, &client, &strs(&dv)), created);
    assert_eq!(
        attempts(),
        n_att,
        "a replayed delivery new is not a new attempt"
    );
    let take_g = keyed(
        "consumer-take-0002",
        &[
            "consumer",
            "take",
            "--holder",
            "ch",
            "--ttl",
            "600000",
            "--as",
            &r,
            "--session",
            &g,
        ],
    );
    let taken = stdout(&h, &client, &strs(&take_g));
    // Expired (the deadline moved into the past in the scratch ledger): replay is history.
    h.conn()
        .execute(
            "update consumer_leases set expires_at='2000-01-01T00:00:00.000Z' where task_id=?",
            [r.parse::<i64>().unwrap()],
        )
        .unwrap();
    let rows = lease(&h);
    assert_eq!(stdout(&h, &client, &strs(&take_g)), taken);
    assert_eq!(lease(&h), rows);
    assert_eq!(
        stdout(&h, &client, &["consumer", "show", "--as", &r]),
        "cs1 2 free\n"
    );

    // In flight on the hub: outcome unknown, nothing written.
    let argv: Vec<String> = [
        "--json",
        "consumer",
        "take",
        "--holder",
        "x",
        "--ttl",
        "5000",
        "--as",
        &r,
        "--session",
        &g,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    h.conn()
        .execute(
            "insert into requests(key,machine,argv_sha,state,created_at) values('consumer-running-1','host-a',?,'running',?)",
            [taskr_core::request_hash(Some(&argv), None).unwrap(), taskr_core::store::now()],
        )
        .unwrap();
    let mut running = vec!["--request-key", "consumer-running-1"];
    running.extend(argv.iter().map(String::as_str));
    let before = snapshot(&h);
    let (code, out, _) = run(&h, &fast, &running);
    assert!(code == Some(5) && out.contains("outcome unknown"), "{out}");
    assert_eq!(snapshot(&h), before);

    // Server unreachable: exit 5 with the retry line, never qd1; the retry commits once.
    let dead = std::net::TcpListener::bind("[::1]:0").unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);
    std::fs::write(&server, format!("http://[::1]:{port}\n")).unwrap();
    let before = snapshot(&h);
    let (code, out, err) = run(
        &h,
        &fast,
        &[
            "consumer",
            "take",
            "--holder",
            "ch",
            "--ttl",
            "600000",
            "--as",
            &r,
            "--session",
            &g,
        ],
    );
    assert_eq!(code, Some(5), "{out}{err}");
    assert!(
        !out.starts_with("qd1")
            && !h
                .home
                .join("client/.local/state/taskr/spool/queue")
                .exists(),
        "{out}"
    );
    assert_eq!(snapshot(&h), before);
    let retry = err
        .lines()
        .find_map(|l| l.split_once("retry with: taskr "))
        .map(|(_, r)| r.to_string())
        .unwrap();
    std::fs::write(&server, &url).unwrap();
    let retry: Vec<&str> = retry.split(' ').collect();
    let once = stdout(&h, &client, &retry);
    assert!(once.starts_with("cs1 3 live ch "), "{once}");
    let rows = lease(&h);
    assert_eq!(stdout(&h, &client, &retry), once);
    assert_eq!(lease(&h), rows);
}

#[test]
fn wait_clears_its_marker_before_a_failing_skipped_print() {
    let h = Harness::new();
    h.agents(Some("working"), 1);
    let (w, _, env) = h.lane("claude", "implementer");
    let r = parent(&h, w);
    ask(&h, &env, "skipped?");
    // The nonmatching ask is skipped before the marker is set; SIGTERM then ends the
    // wait and the `sk1` print hits a stdout that was closed before spawn.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let child = h
        .command(&[])
        .args(["wait", "--as", &r, "--for", "done", "--timeout", "60000"])
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    marked(&h, &r, "");
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let (code, out) = finish(child);
    assert_eq!(code, Some(141), "{out}");
    assert_eq!(
        text(&h, &format!("select waiting_until from tasks where id={r}")),
        "",
        "{out}"
    );
    let (_, out, _) = run(&h, &[], &["wait", "--as", &r, "--timeout", "0"]);
    assert!(!out.contains("e1\t"), "the ask was skipped: {out}");
}
