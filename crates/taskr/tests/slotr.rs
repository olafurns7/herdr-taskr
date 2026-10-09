//! `taskr slotr`: slotr's status passed through with the ledger's roots, its environment,
//! and the failures that answer `available:false`, with a fake slotr on PATH.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use serde_json::Value;
use std::time::{Duration, Instant};
use support::*;

/// A slotr that prints what it saw: its arguments and XDG_RUNTIME_DIR, and rows whose
/// tasks are `task`, an id the ledger does not have, and no id at all.
fn fake(h: &Harness, task: i64) {
    h.script(
        "slotr",
        &format!(
            "#!/bin/sh\nprintf '{{\"schema_version\":1,\"seen_args\":\"%s\",\"seen_xdg\":\"%s\",\"stats\":{{\"available_mib\":2048.5}},\"pools\":{{\"runtime\":{{\"slots\":2,\"holders\":[{{\"run\":\"r1\",\"task\":\"{task}\",\"pane\":\"w9:p1\",\"anon_mib\":null}}],\"queue\":[{{\"position\":1,\"task\":\"999999\"}},{{\"position\":2,\"task\":\"\",\"wait_reason\":\"psi\"}}]}}}}}}\\n' \"$*\" \"$XDG_RUNTIME_DIR\"\n"
        ),
    );
}

fn slotr(h: &Harness, env: &[(String, String)]) -> Value {
    h.ok(env, &["slotr"])
}

#[test]
fn passes_slotr_through_with_roots_and_its_runtime_dir() {
    let h = Harness::new();
    let (lane, _, _) = h.lane("claude", "implementer");
    let sub = h.ok(
        &[],
        &[
            "new",
            "sub",
            "--role",
            "implementer",
            "--parent",
            &lane.to_string(),
        ],
    )["task_id"]
        .as_i64()
        .unwrap();
    fake(&h, sub);
    let uid = String::from_utf8(
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let v = slotr(&h, &[]);
    assert_eq!(v["available"], true, "{v}");
    assert_eq!(
        (v["host"].as_str(), v["seen_args"].as_str()),
        (Some(""), Some("status --json"))
    );
    // The hub child's environment is cleared: slotr gets the runtime dir systemd would.
    assert_eq!(v["seen_xdg"], format!("/run/user/{}", uid.trim()));
    assert_eq!(v["stats"]["available_mib"], 2048.5);
    assert!(v["now"].as_str().unwrap().ends_with('Z'), "{v}");
    let pool = &v["pools"]["runtime"];
    let holder = &pool["holders"][0];
    // Two levels up from the sub-lane: the campaign's root.
    let root = h.count("select parent_id from tasks where id=?", lane);
    assert_eq!(
        (holder["root_id"].as_i64(), holder["root_name"].as_str()),
        (Some(root), Some("top"))
    );
    assert_eq!(
        (holder["pane"].as_str(), holder["anon_mib"].is_null()),
        (Some("w9:p1"), true)
    );
    for waiter in pool["queue"].as_array().unwrap() {
        assert!(waiter.get("root_id").is_none(), "{waiter}");
    }
    // The caller's own runtime dir wins.
    let v = slotr(&h, &[("XDG_RUNTIME_DIR".into(), "/tmp/xdg-test".into())]);
    assert_eq!(v["seen_xdg"], "/tmp/xdg-test");
    // Compact mode is one j1 line.
    let out = h.output(&[], &["slotr"], b"");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.status.success() && text.starts_with("j1 {") && text.lines().count() == 1,
        "{text}"
    );
}

#[test]
fn a_slotr_that_fails_is_unavailable_with_exit_zero() {
    let h = Harness::new();
    // Missing.
    let v = slotr(&h, &[]);
    assert_eq!(v["available"], false, "{v}");
    assert!(
        v["error"].as_str().unwrap().starts_with("cannot run slotr"),
        "{v}"
    );
    assert!(v["now"].is_string() && v["host"] == "", "{v}");
    // Failing: its first stderr line.
    h.script(
        "slotr",
        "#!/bin/sh\necho 'slotr: no systemd user bus here' >&2\necho 'second line' >&2\nexit 2\n",
    );
    let v = slotr(&h, &[]);
    assert_eq!(
        (v["available"].as_bool(), v["error"].as_str()),
        (Some(false), Some("slotr: no systemd user bus here"))
    );
    // Not JSON.
    h.script("slotr", "#!/bin/sh\necho 'runtime: 1 holder'\n");
    let v = slotr(&h, &[]);
    assert_eq!(v["error"], "slotr status printed no JSON object", "{v}");
    // Hanging: cut at 2 s, its process group with it.
    h.script("slotr", "#!/bin/sh\nsleep 30\n");
    let start = Instant::now();
    let v = slotr(&h, &[]);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(v["error"], "slotr status took over 2s", "{v}");
    // No arguments: a client can make slotr run nothing else.
    let out = h.output(&[], &["--json", "slotr", "stop"], b"");
    assert_eq!(out.status.code(), Some(2));
}

#[cfg(feature = "contract")]
#[test]
fn forwarded_calls_store_nothing_and_log_nothing() {
    use std::{
        fs,
        io::{BufRead, BufReader},
        process::Stdio,
    };
    let h = Harness::new();
    h.script("tailscale", include_str!("hook/tailscale.sh"));
    fake(&h, 1);
    fs::create_dir_all(h.home.join(".local/state/taskr")).unwrap();
    let mut hub = h
        .command(&[
            ("NET_ID".into(), "hub".into()),
            ("TASKR_CONTRACT_TAILNET".into(), "1".into()),
            ("TASKR_CONTRACT_ORACLE".into(), "1".into()),
            ("XDG_RUNTIME_DIR".into(), "/tmp/hub-daemon-xdg".into()),
        ])
        .arg("--contract-hub")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut line = String::new();
    BufReader::new(hub.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _hub = Kill(hub);
    assert!(line.starts_with("http://[::1]:"), "{line}");
    fs::create_dir_all(h.home.join("client/.local/state/taskr")).unwrap();
    fs::write(h.home.join("client/.local/state/taskr/server.url"), &line).unwrap();
    let client: Env = vec![
        (
            "HOME".into(),
            h.home.join("client").to_str().unwrap().into(),
        ),
        ("TASKR_DB".into(), "".into()),
        ("NET_ID".into(), "host-a".into()),
        ("TASKR_CONTRACT_TAILNET".into(), "1".into()),
        ("TASKR_CONTRACT_ORACLE".into(), "1".into()),
    ];
    let root = h.ok(&client, &["new", "top", "--role", "orchestrator"])["task_id"]
        .as_i64()
        .unwrap();
    let uid = String::from_utf8(
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let requests = || h.count("select count(*) from requests where length(key)>?", 0);
    let before = requests();
    for _ in 0..10 {
        let v = slotr(&h, &client);
        assert_eq!(v["available"], true, "{v}");
        // The hub child runs with a cleared environment, so not the daemon's dir either.
        assert_eq!(v["seen_xdg"], format!("/run/user/{}", uid.trim()), "{v}");
        assert_eq!(v["pools"]["runtime"]["holders"][0]["root_id"], root, "{v}");
    }
    assert_eq!(requests(), before, "a slotr read is not a stored request");
    assert!(h.output(&client, &["status"], b"").status.success());
    let log = fs::read_to_string(h.home.join(".local/state/taskr/daemon.log")).unwrap_or_default();
    assert!(log.contains("cmd=status "), "the RPC log is written: {log}");
    assert!(!log.contains("cmd=slotr"), "{log}");
}
