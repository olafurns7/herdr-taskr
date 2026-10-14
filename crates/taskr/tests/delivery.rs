//! `taskr delivery`: attempts and receipts for workers (launch-bound) and roots
//! (adopt-generation-bound), each command its own process, every assertion read back
//! from the reopened ledger; and the same through a client host.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use support::*;

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
/// Every task's inbox cursors.
fn cursors(h: &Harness) -> String {
    text(
        h,
        "select group_concat(id||':'||acked_event_id||':'||ifnull(pending_event_id,'-'),' ') from (select * from tasks order by id)",
    )
}
fn attempts(h: &Harness) -> String {
    text(
        h,
        "select group_concat(id||'|'||event_id||'|'||task_id||'|'||ifnull(launch_id,'-')||'|'||ifnull(session_gen,'-')||'|'||ifnull(machine,'-')||'|'||adapter||'|'||created_at||'|'||ifnull(submitted_at,'-')||'|'||ifnull(received_at,'-'),' ') from (select * from delivery_attempts order by id)",
    )
}
/// Delivery, task and event state: what a rejection must leave untouched.
fn snapshot(h: &Harness) -> String {
    format!(
        "{} # {} # {}",
        attempts(h),
        text(h, "select count(*)||'/'||ifnull(max(id),0) from events"),
        text(
            h,
            "select group_concat(id||':'||status||':'||ifnull(current_launch_id,'-')||':'||updated_at,' ') from (select * from tasks order by id)",
        ) + &cursors(h)
    )
}
fn rejected(h: &Harness, env: &[(String, String)], args: &[&str], want: i32) {
    let before = snapshot(h);
    let (code, out, err) = run(h, env, args);
    assert_eq!(code, Some(want), "{args:?}: {out}{err}");
    assert_eq!(snapshot(h), before, "{args:?} wrote");
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
/// A root's generation as `taskr adopt` reports it on stderr: "...; event EID".
fn adopt(h: &Harness, env: &[(String, String)], root: &str, pane: &str) -> String {
    let (code, _, err) = run(h, env, &["adopt", root, "--pane", pane]);
    assert_eq!(code, Some(0), "{err}");
    err.trim_end().rsplit_once("event ").unwrap().1.to_string()
}

#[test]
fn worker_receipts_are_fenced_by_launch_task_and_handled_state() {
    let h = Harness::new();
    let (w, l, env) = h.lane("claude", "implementer");
    let (ws, ls, r) = (w.to_string(), l.to_string(), parent(&h, w));
    let q = id(&h.ok(&env, &["ask", "which way?"]), "ask_id");
    let a = id(&h.ok(&[], &["answer", &q, "left"]), "answer_id");
    let before = cursors(&h);
    let v = h.ok(&env, &["delivery", "new", &a, "--adapter", "pi-ext"]);
    assert!(
        v["state"] == "attempted"
            && v["launch_id"] == l
            && v["session_gen"].is_null()
            && v["machine"].is_null()
            && id(&v, "event_id") == a
            && v["task_id"] == w
            && v["adapter"] == "pi-ext"
            && v["submitted_at"].is_null(),
        "{v}"
    );
    let att = id(&v, "attempt_id");
    for (cmd, want) in [
        ("submitted", format!("dv1 {att} submitted launch={l}\n")),
        ("submitted", format!("dv1 {att} submitted launch={l} dup\n")),
        ("received", format!("dv1 {att} received launch={l}\n")),
        ("received", format!("dv1 {att} received launch={l} dup\n")),
    ] {
        assert_eq!(stdout(&h, &env, &["delivery", cmd, &att]), want);
    }
    assert_eq!(cursors(&h), before, "receipts never touch the inbox cursor");
    let list = stdout(&h, &env, &["delivery", "ls"]);
    assert!(
        list.starts_with("j1 {")
            && list.lines().count() == 1
            && list.contains("\"state\":\"received\""),
        "{list}"
    );
    assert_eq!(stdout(&h, &env, &["delivery", "ls", "--open"]), "");
    assert_eq!(cursors(&h), before);

    let planned = id(
        &h.ok(
            &[],
            &[
                "new",
                "later",
                "--role",
                "implementer",
                "--parent",
                &r,
                "--planned",
            ],
        ),
        "task_id",
    );
    let env_only: Env = vec![("TASKR_TASK".into(), ws.clone())];
    let planned_env: Env = vec![
        ("TASKR_TASK".into(), planned),
        ("TASKR_LAUNCH".into(), ls.clone()),
    ];
    for (env, args, want) in [
        (&env, vec!["delivery", "new", &q, "--adapter", "x"], 6), // addressed to the root
        (&env, vec!["delivery", "new", "999999", "--adapter", "x"], 6),
        (&env, vec!["delivery", "received", "999999"], 6),
        (&env_only, vec!["delivery", "new", &a, "--adapter", "x"], 6),
        (&env_only, vec!["delivery", "received", &att], 6),
        (
            &planned_env,
            vec!["delivery", "new", &a, "--adapter", "x"],
            6,
        ),
        (
            &env,
            vec!["delivery", "received", &att, "--as", &r, "--session", "0"],
            6,
        ),
        (
            &vec![],
            vec!["delivery", "received", &att, "--as", &r, "--session", "0"],
            6,
        ),
        (&vec![], vec!["delivery", "new", &a, "--adapter", "x"], 2),
        (
            &env,
            vec!["delivery", "new", &a, "--adapter", "x", "--session", "0"],
            2,
        ),
        (
            &env,
            vec!["delivery", "new", &a, "--adapter", "x", "--adapter=x"],
            2,
        ),
        (&env, vec!["delivery", "new", &a, "--adapter", "Pi"], 2),
        (&env, vec!["delivery", "new", &a], 2),
        (&env, vec!["delivery", "new"], 2),
        (&env, vec!["delivery", "ls", "--session", "0"], 2),
        (&env, vec!["delivery", "bogus"], 2),
    ] {
        rejected(&h, env, &args, want);
    }

    // Help is a read: usage on stdout, exit 0, nothing written; a repeated help flag
    // (any -h/--help form) is refused rather than run as a write.
    for args in [
        vec!["delivery", "--help"],
        vec!["help", "delivery"],
        vec!["delivery", "new", &a, "--adapter", "pi-ext", "--help"],
    ] {
        let before = snapshot(&h);
        let out = stdout(&h, &env, &args);
        assert!(
            out.contains("delivery new EVENT --adapter NAME [--as ID --session GEN]"),
            "{args:?}: {out}"
        );
        assert_eq!(snapshot(&h), before, "{args:?} wrote");
    }
    for repeat in [
        ["--help", "--help=false"],
        ["-h", "--help=false"],
        ["--help=false", "-h"],
    ] {
        let mut args = vec!["delivery", "new", &a, "--adapter", "pi-ext"];
        args.extend(repeat);
        rejected(&h, &env, &args, 2);
    }

    // Late evidence after ack is kept; a new attempt for a handled event is not.
    let att2 = id(
        &h.ok(&env, &["delivery", "new", &a, "--adapter", "pi-ext"]),
        "attempt_id",
    );
    let rows = attempts(&h);
    assert!(
        stdout(&h, &env, &["wait", "--as", &ws, "--timeout", "0"])
            .starts_with(&format!("e1\t{a}\t"))
    );
    h.ok(&env, &["ack", &a, "--as", &ws]);
    assert_eq!(attempts(&h), rows, "wait and ack never write attempts");
    rejected(&h, &env, &["delivery", "new", &a, "--adapter", "pi-ext"], 6);
    assert_eq!(
        stdout(&h, &env, &["delivery", "received", &att2]),
        format!("dv1 {att2} received launch={l}\n")
    );

    // A relaunch makes the old launch's attempts unreceivable from either launch.
    let l2 = id(
        &h.ok(
            &[],
            &[
                "launch",
                &ws,
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
    let env2: Env = env
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                if k == "TASKR_LAUNCH" {
                    l2.clone()
                } else {
                    v.clone()
                },
            )
        })
        .collect();
    rejected(&h, &env, &["delivery", "submitted", &att2], 6);
    rejected(&h, &env2, &["delivery", "submitted", &att2], 6);
    rejected(&h, &env2, &["delivery", "received", &att], 6);
    let q2 = id(&h.ok(&env2, &["ask", "and now?"]), "ask_id");
    let a2 = id(&h.ok(&[], &["answer", &q2, "right"]), "answer_id");
    let att3 = id(
        &h.ok(&env2, &["delivery", "new", &a2, "--adapter", "pi-ext"]),
        "attempt_id",
    );
    // ls keeps every launch's history; the stale launch cannot read it.
    let all = h.ok(&env2, &["delivery", "ls"])["attempts"].clone();
    let launches: Vec<String> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["launch_id"].to_string())
        .collect();
    assert_eq!(launches, [ls.clone(), ls.clone(), l2.clone()], "{all}");
    rejected(&h, &env, &["delivery", "ls"], 6);
    h.ok(&[], &["close", &ws]);
    rejected(&h, &env2, &["delivery", "received", &att3], 6);
}

#[test]
fn root_receipts_are_fenced_by_the_adopt_generation() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let r = parent(&h, w);
    let q = id(&h.ok(&env, &["ask", "go?"]), "ask_id");
    let before = cursors(&h);
    let root = |gen_: &str| {
        [
            "--as".to_string(),
            r.clone(),
            "--session".into(),
            gen_.into(),
        ]
    };
    let args = |head: &[&str], gen_: &str| -> Vec<String> {
        head.iter()
            .map(|s| s.to_string())
            .chain(root(gen_))
            .collect()
    };
    let call = |head: &[&str], gen_: &str| {
        let a = args(head, gen_);
        h.ok(&[], &a.iter().map(String::as_str).collect::<Vec<_>>())
    };
    let refuse = |head: &[&str], gen_: &str, want: i32| {
        let a = args(head, gen_);
        rejected(
            &h,
            &[],
            &a.iter().map(String::as_str).collect::<Vec<_>>(),
            want,
        );
    };
    // Bootstrap generation 0 before any adopt.
    let v = call(&["delivery", "new", &q, "--adapter", "claude-channel"], "0");
    assert!(
        v["session_gen"] == 0 && v["launch_id"].is_null() && v["machine"].is_null(),
        "{v}"
    );
    let ra = id(&v, "attempt_id");
    // received before submitted: a later submitted fills submitted_at, keeps received.
    let got = call(&["delivery", "received", &ra], "0");
    assert!(
        got["state"] == "received" && got["submitted_at"].is_null(),
        "{got}"
    );
    let sub = call(&["delivery", "submitted", &ra], "0");
    assert!(
        sub["state"] == "received"
            && !sub["submitted_at"].is_null()
            && sub["received_at"] == got["received_at"],
        "{sub}"
    );
    assert_eq!(
        call(&["delivery", "submitted", &ra], "0")["duplicate"],
        true
    );
    assert_eq!(cursors(&h), before);
    assert_eq!(
        call(
            &["delivery", "state", &q, "--adapter", "claude-channel"],
            "0"
        )["received"],
        true
    );
    refuse(&["delivery", "received", &ra], "7", 6);
    // GEN omitted, --as with a launch, or an env-only root identity.
    rejected(
        &h,
        &[],
        &["delivery", "new", &q, "--adapter", "x", "--as", &r],
        2,
    );
    rejected(
        &h,
        &[("TASKR_TASK".into(), r.clone())],
        &["delivery", "new", &q, "--adapter", "x"],
        6,
    );
    rejected(
        &h,
        &[
            ("TASKR_TASK".into(), r.clone()),
            ("TASKR_LAUNCH".into(), "1".into()),
        ],
        &args(&["delivery", "new", &q, "--adapter", "x"], "0")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        6,
    );
    rejected(
        &h,
        &[],
        &[
            "delivery",
            "new",
            &q,
            "--adapter",
            "x",
            "--as",
            &r,
            "--session",
            "0",
            "--session=0",
        ],
        2,
    );

    // After adopt: the old generation, and the new one on an old attempt, are refused.
    let g = adopt(&h, &[], &r, "w1:p7");
    assert_eq!(cursors(&h), before, "adopt keeps the cursors");
    refuse(&["delivery", "received", &ra], "0", 6);
    refuse(&["delivery", "new", &q, "--adapter", "x"], "0", 6);
    refuse(
        &["delivery", "state", &q, "--adapter", "claude-channel"],
        "0",
        6,
    );
    assert_eq!(
        call(
            &["delivery", "state", &q, "--adapter", "claude-channel"],
            &g
        ),
        serde_json::json!({"received":false,"watermark":0,"attempt_id":null})
    );
    refuse(&["delivery", "submitted", &ra], &g, 6);
    let rb = id(
        &call(&["delivery", "new", &q, "--adapter", "claude-channel"], &g),
        "attempt_id",
    );
    // ls is history: both generations, without a GEN.
    let all = h.ok(&[], &["delivery", "ls", "--as", &r])["attempts"].clone();
    assert!(
        all[0]["session_gen"] == 0 && id(&all[1], "session_gen") == g,
        "{all}"
    );
    let open = h.ok(&[], &["delivery", "ls", "--as", &r, "--open"])["attempts"].clone();
    assert!(
        open.as_array().unwrap().len() == 1 && id(&open[0], "attempt_id") == rb,
        "{open}"
    );
    assert_eq!(cursors(&h), before);

    // Late receipt after ack is accepted; a new attempt for a handled event is not.
    assert!(
        stdout(&h, &[], &["wait", "--as", &r, "--timeout", "0"]).starts_with(&format!("e1\t{q}\t"))
    );
    h.ok(&[], &["ack", &q, "--as", &r]);
    assert_eq!(
        call(&["delivery", "received", &rb], &g)["state"],
        "received"
    );
    refuse(&["delivery", "new", &q, "--adapter", "x"], &g, 6);
    // A later adopt or close ends that generation's receipts.
    let g2 = adopt(&h, &[], &r, "w1:p8");
    refuse(&["delivery", "submitted", &rb], &g, 6);
    refuse(&["delivery", "submitted", &rb], &g2, 6);
    h.ok(&[], &["close", &r]);
    refuse(&["delivery", "received", &rb], &g2, 6);
}

#[test]
fn delivery_new_follows_the_consumer_lease_and_receipts_ignore_it() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let q = id(&h.ok(&env, &["ask", "which way?"]), "ask_id");
    let a = id(&h.ok(&[], &["answer", &q, "left"]), "answer_id");
    // Without a live lease: unchanged, and a supplied fence is refused.
    rejected(
        &h,
        &env,
        &["delivery", "new", &a, "--adapter", "pi-ext", "--fence", "1"],
        6,
    );
    let free = id(
        &h.ok(&env, &["delivery", "new", &a, "--adapter", "pi-ext"]),
        "attempt_id",
    );
    // While held: only the live fence of the same binding creates an attempt.
    h.ok(
        &env,
        &["consumer", "take", "--holder", "pi-ext", "--ttl", "600000"],
    );
    rejected(&h, &env, &["delivery", "new", &a, "--adapter", "pi-ext"], 6);
    rejected(
        &h,
        &env,
        &["delivery", "new", &a, "--adapter", "pi-ext", "--fence", "2"],
        6,
    );
    let held = h.ok(
        &env,
        &["delivery", "new", &a, "--adapter", "pi-ext", "--fence", "1"],
    );
    assert_eq!(
        held["launch_id"],
        h.count("select current_launch_id from tasks where id=?", w)
    );
    let held = id(&held, "attempt_id");
    // A takeover under the same binding: receipts and ls stay valid evidence.
    h.ok(
        &env,
        &[
            "consumer", "take", "--holder", "cli", "--ttl", "600000", "--force",
        ],
    );
    rejected(
        &h,
        &env,
        &["delivery", "new", &a, "--adapter", "pi-ext", "--fence", "1"],
        6,
    );
    for att in [&free, &held] {
        assert_eq!(
            h.ok(&env, &["delivery", "received", att])["state"],
            "received"
        );
        assert_eq!(
            h.ok(&env, &["delivery", "submitted", att])["state"],
            "received"
        );
    }
    assert_eq!(
        h.ok(&env, &["delivery", "ls"])["attempts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn crash_boundaries_leave_rows_and_the_inbox_offers_until_ack() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let r = parent(&h, w);
    let q = id(&h.ok(&env, &["ask", "go?"]), "ask_id");
    let root = ["--as", r.as_str(), "--session", "0"];
    let offered = || {
        stdout(&h, &[], &["wait", "--as", &r, "--timeout", "0"]).starts_with(&format!("e1\t{q}\t"))
    };
    let open = || stdout(&h, &[], &["delivery", "ls", "--as", &r, "--open"]);
    let mut new = vec!["delivery", "new", q.as_str(), "--adapter", "claude-channel"];
    new.extend(root);
    let a = id(&h.ok(&[], &new), "attempt_id");
    // Each step is its own process; the reopened ledger holds what it left.
    let state = || {
        text(
            &h,
            &format!(
                "select (submitted_at is not null)||(received_at is not null) from delivery_attempts where id={a}"
            ),
        )
    };
    for (step, stored, listed) in [
        ("new", "00", "attempted"),
        ("submitted", "10", "submitted"),
        ("received", "11", ""),
    ] {
        if step != "new" {
            let mut args = vec!["delivery", step, a.as_str()];
            args.extend(root);
            let c = cursors(&h);
            stdout(&h, &[], &args);
            assert_eq!(cursors(&h), c, "{step} touched the cursor");
        }
        assert_eq!(state(), stored, "after {step}");
        let rows = open();
        if listed.is_empty() {
            assert_eq!(rows, "", "after {step}");
        } else {
            assert!(
                rows.lines().count() == 1 && rows.contains(&format!("\"state\":\"{listed}\"")),
                "after {step}: {rows}"
            );
        }
        assert!(offered(), "the inbox still offers {q} after {step}");
    }
    let rows = attempts(&h);
    h.ok(&[], &["ack", &q, "--as", &r]);
    assert!(!offered(), "ack hands the event off");
    assert_eq!(attempts(&h), rows, "ack writes no attempt");
}

/// A synthetic contract hub, killed on drop.
#[cfg(feature = "contract")]
struct Hub(std::process::Child);
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
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
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
fn a_client_call_is_host_bound_replayed_by_key_and_never_queued() {
    let h = Harness::new();
    let (_hub, url, client, server) = start_hub(&h);
    // Only the unknown and unreachable cases skip the client's retry window.
    let mut fast = client.clone();
    fast.push(("TASKR_CONTRACT_RETRY_MS".into(), "0".into()));
    // A hub-local tree, and a tree created from host-a.
    let (_, _, local) = h.lane("claude", "implementer");
    let local_q = id(&h.ok(&local, &["ask", "local?"]), "ask_id");
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
    let q = id(&h.ok(&worker, &["ask", "remote?"]), "ask_id");
    let a = id(&h.ok(&client, &["answer", &q, "yes"]), "answer_id");
    let requests = || h.count("select count(*) from requests where length(key)>?", 0);

    // Stored writes, bound to the authenticated caller; ls is a read.
    let n = requests();
    let wa = h.ok(&worker, &["delivery", "new", &a, "--adapter", "pi-ext"]);
    assert_eq!(wa["machine"], "host-a", "{wa}");
    let wa = id(&wa, "attempt_id");
    let ra = h.ok(
        &client,
        &[
            "delivery",
            "new",
            &q,
            "--adapter",
            "claude-channel",
            "--as",
            &r,
            "--session",
            "0",
        ],
    );
    assert_eq!(ra["machine"], "host-a", "{ra}");
    let ra = id(&ra, "attempt_id");
    assert_eq!(requests(), n + 2);
    h.ok(&client, &["delivery", "ls", "--as", &r]);
    h.ok(&worker, &["delivery", "ls", "--open"]);
    assert_eq!(requests(), n + 2, "delivery ls is not stored");

    // The hub decides the host: another caller, or a hub-local task, is refused.
    std::fs::write(h.home.join("whois-host"), "host-b").unwrap();
    rejected(&h, &worker, &["delivery", "received", &wa], 6);
    rejected(
        &h,
        &client,
        &["delivery", "received", &ra, "--as", &r, "--session", "0"],
        6,
    );
    std::fs::remove_file(h.home.join("whois-host")).unwrap();
    let top = parent(&h, local[0].1.parse().unwrap());
    rejected(
        &h,
        &client,
        &[
            "delivery",
            "new",
            &local_q,
            "--adapter",
            "x",
            "--as",
            &top,
            "--session",
            "0",
        ],
        6,
    );
    let mut stale = local.clone();
    stale.extend(client.iter().cloned());
    rejected(&h, &stale, &["delivery", "ls"], 6);

    // A lost response: the same key replays the committed result, even after adopt.
    let replay = [
        "--request-key",
        "delivery-replay-01",
        "--json",
        "delivery",
        "new",
        &q,
        "--adapter",
        "claude-channel",
        "--as",
        &r,
        "--session",
        "0",
    ];
    let first = stdout(&h, &client, &replay);
    let rows = attempts(&h);
    let g = adopt(&h, &client, &r, "w1:p7");
    assert_eq!(stdout(&h, &client, &replay), first);
    assert_eq!(attempts(&h), rows, "a replay is not a new attempt");
    // A fresh read under a reused key sees the change.
    let read = [
        "--request-key",
        "delivery-read-01",
        "--json",
        "delivery",
        "ls",
        "--as",
        &r,
    ];
    let before = stdout(&h, &client, &read);
    h.ok(
        &client,
        &[
            "delivery",
            "new",
            &q,
            "--adapter",
            "claude-channel",
            "--as",
            &r,
            "--session",
            &g,
        ],
    );
    assert_ne!(stdout(&h, &client, &read), before);
    assert_eq!(
        h.count(
            "select count(*) from requests where key='delivery-read-01' and ?",
            1
        ),
        0
    );

    // In flight on the hub: outcome unknown, nothing written.
    let argv: Vec<String> = [
        "--json",
        "delivery",
        "submitted",
        &ra,
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
            "insert into requests(key,machine,argv_sha,state,created_at) values('delivery-running-01','host-a',?,'running',?)",
            [taskr_core::request_hash(Some(&argv), None).unwrap(), taskr_core::store::now()],
        )
        .unwrap();
    let mut running = vec!["--request-key", "delivery-running-01"];
    running.extend(argv.iter().map(String::as_str));
    let before = snapshot(&h);
    let (code, out, _) = run(&h, &fast, &running);
    assert_eq!(code, Some(5), "{out}");
    assert!(out.contains("outcome unknown"), "{out}");
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
            "delivery",
            "new",
            &q,
            "--adapter",
            "claude-channel",
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
    let retry = err
        .lines()
        .find_map(|l| l.split_once("retry with: taskr "))
        .map(|(_, r)| r.to_string())
        .unwrap();
    assert!(retry.starts_with("--request-key "), "{err}");
    assert_eq!(snapshot(&h), before);
    std::fs::write(&server, &url).unwrap();
    let retry: Vec<&str> = retry.split(' ').collect();
    let once = stdout(&h, &client, &retry);
    let rows = attempts(&h);
    assert_eq!(stdout(&h, &client, &retry), once);
    assert_eq!(attempts(&h), rows);
    assert_eq!(rows.split(' ').count(), 5, "{rows}");
}

/// Forward RPCs to the hub; read the first reply and drop it, as if the connection
/// died after the hub committed. Every request body is sent back for inspection.
#[cfg(feature = "contract")]
fn drop_first_reply(hub_port: u16) -> (u16, std::sync::mpsc::Receiver<serde_json::Value>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (send, seen) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for (n, client) in listener.incoming().enumerate() {
            let mut client = client.unwrap();
            let mut reader = BufReader::new(client.try_clone().unwrap());
            let (mut head, mut len) = (String::new(), 0);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if lower.starts_with("host:") {
                    line = format!("Host: [::1]:{hub_port}\r\n");
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let mut hub =
                std::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, hub_port)).unwrap();
            hub.write_all(head.as_bytes()).unwrap();
            hub.write_all(&body).unwrap();
            let mut reply = Vec::new();
            hub.read_to_end(&mut reply).unwrap();
            send.send(serde_json::from_slice(&body).unwrap()).unwrap();
            if n > 0 {
                client.write_all(&reply).unwrap();
            }
        }
    });
    (port, seen)
}
/// One RPC straight to the hub, as the authenticated caller host-a.
#[cfg(feature = "contract")]
fn post(port: u16, req: &serde_json::Value) -> serde_json::Value {
    use std::io::{Read, Write};
    let body = req.to_string();
    let mut stream = std::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
    write!(stream, "POST /api/rpc HTTP/1.1\r\nHost: [::1]:{port}\r\nX-Taskr-RPC: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[cfg(feature = "contract")]
#[test]
fn a_dropped_reply_replays_one_attempt_and_repeated_help_never_writes() {
    let h = Harness::new();
    let (_hub, url, client, server) = start_hub(&h);
    let hub_port: u16 = url.trim().rsplit_once(':').unwrap().1.parse().unwrap();
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
    worker.extend([("TASKR_TASK".into(), w), ("TASKR_LAUNCH".into(), l)]);
    let q = id(&h.ok(&worker, &["ask", "remote?"]), "ask_id");
    let done = |key: &str| {
        h.conn()
            .query_row(
                "select count(*) from requests where key=? and state='done'",
                [key],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
    };

    // The hub commits, the reply is lost, the client retries the same key: one attempt.
    let (proxy, seen) = drop_first_reply(hub_port);
    std::fs::write(&server, format!("http://[::1]:{proxy}\n")).unwrap();
    let key = "delivery-dropped-01";
    let v: serde_json::Value = serde_json::from_str(&stdout(
        &h,
        &client,
        &[
            "--request-key",
            key,
            "--json",
            "delivery",
            "new",
            &q,
            "--adapter",
            "claude-channel",
            "--as",
            &r,
            "--session",
            "0",
        ],
    ))
    .unwrap();
    std::fs::write(&server, &url).unwrap();
    let sent: Vec<serde_json::Value> = seen.try_iter().collect();
    assert!(
        sent.len() == 2
            && sent
                .iter()
                .all(|s| s["request_key"] == key && s["argv"] == sent[0]["argv"]),
        "{sent:?}"
    );
    assert_eq!(
        h.count("select count(*) from delivery_attempts where ?", 1),
        1
    );
    assert_eq!(
        id(&v, "attempt_id"),
        text(&h, "select cast(id as text) from delivery_attempts")
    );
    assert_eq!(done(key), 1);

    // A repeated help flag would run as a write but classify as a help read: refused,
    // on every send, with no attempt and no stored request.
    let write = |key: &str, extra: &[&str]| {
        let mut argv = vec![
            "--json",
            "delivery",
            "new",
            &q,
            "--adapter",
            "synthetic",
            "--as",
            &r,
            "--session",
            "0",
        ];
        argv.extend(extra);
        serde_json::json!({"argv": argv, "request_key": key, "cwd": h.home, "env": {}})
    };
    let before = snapshot(&h);
    for extra in [
        &["--help", "--help=false"][..],
        &["-h", "-help=false"],
        &["--help=false", "-h=true"],
    ] {
        for _ in 0..2 {
            let rep = post(hub_port, &write("delivery-help-twice", extra));
            assert_eq!(rep["exit"], 2, "{extra:?}: {rep}");
            assert!(
                rep["stdout"]
                    .as_str()
                    .unwrap()
                    .contains("repeated RPC flag --help"),
                "{rep}"
            );
        }
    }
    assert_eq!(snapshot(&h), before);
    assert_eq!(
        h.count(
            "select count(*) from requests where key='delivery-help-twice' and ?",
            1
        ),
        0
    );
    // One help flag is a fresh help read: usage, no write, no stored request.
    let rep = post(hub_port, &write("delivery-help-once", &["--help"]));
    assert!(
        rep["exit"] == 0
            && rep["stdout"]
                .as_str()
                .unwrap()
                .contains("delivery new EVENT --adapter NAME"),
        "{rep}"
    );
    assert_eq!(snapshot(&h), before);
    assert_eq!(
        h.count(
            "select count(*) from requests where key='delivery-help-once' and ?",
            1
        ),
        0
    );
    // The CLI refuses the repeated form before it reaches the hub.
    rejected(
        &h,
        &client,
        &[
            "delivery",
            "new",
            &q,
            "--adapter",
            "synthetic",
            "--as",
            &r,
            "--session",
            "0",
            "--help",
            "--help=false",
        ],
        2,
    );
}

#[test]
fn internal_state_scopes_receipts_and_reconciles_only_new_eligible_attempts() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let q = id(&h.ok(&env, &["ask", "state?"]), "ask_id");
    let e = id(&h.ok(&[], &["answer", &q, "yes"]), "answer_id");
    let state = |after: &str| {
        h.ok(
            &env,
            &[
                "delivery",
                "state",
                &e,
                "--adapter",
                "claude-channel",
                "--after",
                after,
            ],
        )
    };
    assert_eq!(
        state("0"),
        serde_json::json!({"received":false,"watermark":0,"attempt_id":null})
    );
    let new = |adapter: &str| {
        id(
            &h.ok(&env, &["delivery", "new", &e, "--adapter", adapter]),
            "attempt_id",
        )
    };
    let old = new("claude-channel");
    assert_eq!(state(&old)["attempt_id"], serde_json::Value::Null);
    let eligible = new("claude-channel");
    let other = new("codex-queue");
    let submitted = new("claude-channel");
    h.ok(&env, &["delivery", "submitted", &submitted]);
    assert_eq!(
        state(&old),
        serde_json::json!({"received":false,"watermark":submitted.parse::<i64>().unwrap(),"attempt_id":eligible.parse::<i64>().unwrap()})
    );
    // A foreign host's receipt is audit history, not this loop's receipt or watermark.
    let foreign = new("claude-channel");
    h.conn()
        .execute(
            "update delivery_attempts set machine='other-host',received_at='r' where id=?",
            [&foreign],
        )
        .unwrap();
    assert_eq!(state(&old)["received"], false);
    assert_eq!(
        state(&old)["watermark"].as_i64().unwrap().to_string(),
        submitted
    );
    // Receipt suppression spans adapters; a receipt before submitted is enough.
    h.ok(&env, &["delivery", "received", &other]);
    assert_eq!(state(&old)["received"], true);
    assert_eq!(state(&eligible)["attempt_id"], serde_json::Value::Null);
    // Identity is derived from the caller, never from a supplied task or launch.
    rejected(
        &h,
        &env,
        &[
            "delivery",
            "state",
            &e,
            "--adapter",
            "claude-channel",
            "--as",
            &w.to_string(),
            "--session",
            "0",
        ],
        6,
    );
    rejected(
        &h,
        &env,
        &[
            "delivery",
            "state",
            &e,
            "--adapter",
            "claude-channel",
            "--after",
            "-1",
        ],
        2,
    );
    let next = id(
        &h.ok(
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
        ),
        "launch_id",
    );
    rejected(
        &h,
        &env,
        &["delivery", "state", &e, "--adapter", "claude-channel"],
        6,
    );
    let next_env: Env = env
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                if k == "TASKR_LAUNCH" {
                    next.clone()
                } else {
                    v.clone()
                },
            )
        })
        .collect();
    assert_eq!(
        h.ok(
            &next_env,
            &["delivery", "state", &e, "--adapter", "claude-channel"]
        ),
        serde_json::json!({"received":false,"watermark":0,"attempt_id":null})
    );
}
