//! `taskr tmp`: lane and root paths, --mkdir's 0700 dirs and refusals, an unknown task, the
//! compact launch line, and a client that makes the dir on its own host.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
};
use support::*;

fn base_env(base: &Path) -> Env {
    vec![("TASKR_TMP_BASE".into(), base.to_str().unwrap().into())]
}

/// (root, lane) ids of a fresh campaign.
fn campaign(h: &Harness) -> (i64, i64) {
    let (lane, _, _) = h.lane("claude", "implementer");
    (
        h.count("select parent_id from tasks where id=?", lane),
        lane,
    )
}

fn run(h: &Harness, env: &Env, args: &[&str]) -> (i32, String) {
    let out = h.output(env, args, b"");
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
    )
}

fn mode(p: &Path) -> u32 {
    fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn lane_and_root_paths() {
    let h = Harness::new();
    let (root, lane) = campaign(&h);
    let base = h.home.join("tb");
    let env = base_env(&base);
    let v = h.ok(&env, &["tmp", &lane.to_string()]);
    let want = base.join(root.to_string()).join(lane.to_string());
    assert_eq!(
        (
            v["root_id"].as_i64(),
            v["task_id"].as_i64(),
            v["tmpdir"].as_str()
        ),
        (Some(root), Some(lane), want.to_str())
    );
    // Compact output is the bare path, for $(taskr tmp ID).
    assert_eq!(
        run(&h, &env, &["tmp", &lane.to_string()]),
        (0, format!("{}\n", want.display()))
    );
    // A root's own dir is <root>/<root>.
    let v = h.ok(&env, &["tmp", &root.to_string()]);
    assert_eq!(
        v["tmpdir"].as_str(),
        base.join(root.to_string()).join(root.to_string()).to_str()
    );
    // Printing creates nothing.
    assert!(!base.exists());
    // The default base is /tmp/taskr-<uid>.
    let uid = uid();
    let v = h.ok(&base_env(Path::new("")), &["tmp", &lane.to_string()]);
    assert_eq!(v["tmpdir"], format!("/tmp/taskr-{uid}/{root}/{lane}"));
}

fn uid() -> String {
    let out = std::process::Command::new("id").arg("-u").output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().into()
}

#[test]
fn mkdir_makes_0700_dirs_and_is_idempotent() {
    let h = Harness::new();
    let (root, lane) = campaign(&h);
    let base = h.home.join("tb");
    let env = base_env(&base);
    let want = base.join(root.to_string()).join(lane.to_string());
    let mut marker_inode = None;
    for _ in 0..2 {
        assert_eq!(
            run(&h, &env, &["tmp", &lane.to_string(), "--mkdir"]),
            (0, format!("{}\n", want.display()))
        );
        for p in [&base, &base.join(root.to_string()), &want] {
            assert_eq!(mode(p), 0o700, "{}", p.display());
        }
        let marker = base.join(".taskr-tmp");
        assert_eq!(fs::read(&marker).unwrap(), b"taskr tmp base v1\n");
        assert_eq!(mode(&marker), 0o600);
        let inode = fs::symlink_metadata(&marker).unwrap().ino();
        if let Some(before) = marker_inode {
            assert_eq!(inode, before);
        }
        marker_inode = Some(inode);
    }
    // A file left in the lane dir survives a second --mkdir.
    fs::write(want.join("keep"), "x").unwrap();
    assert_eq!(run(&h, &env, &["tmp", &lane.to_string(), "--mkdir"]).0, 0);
    assert!(want.join("keep").exists());
}

#[test]
fn unmarked_base_is_preserved_until_explicit_mkdir() {
    let h = Harness::new();
    let (root, lane) = campaign(&h);
    let base = h.home.join("home-like");
    let env = base_env(&base);
    let dir = base.join(root.to_string()).join(lane.to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&base, fs::Permissions::from_mode(0o750)).unwrap();
    fs::write(dir.join("precious"), "user data").unwrap();
    h.ok(&env, &["close", &root.to_string()]);
    age_close(&h, root, 660);
    h.ok(&env, &["daemon", "--once"]);
    assert!(dir.join("precious").exists());
    assert!(!base.join(".taskr-tmp").exists());
    assert_eq!(
        run(&h, &env, &["close", &root.to_string(), "--clean-tmp"]).0,
        1
    );
    assert!(dir.join("precious").exists());
    assert!(!base.join(".taskr-tmp").exists());
    h.ok(&env, &["tmp", &lane.to_string(), "--mkdir"]);
    assert!(base.join(".taskr-tmp").is_file());
    assert!(dir.join("precious").exists());
    h.ok(&env, &["close", &root.to_string(), "--clean-tmp"]);
    assert!(!base.join(root.to_string()).exists());
}

#[test]
fn python_fixture_isolation() {
    let h = Harness::new();
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/tmp_isolation.py"
        ))
        .arg(&h.home)
        .env_remove("TASKR_TASK")
        .env_remove("TASKR_LAUNCH")
        .output()
        .expect("run process-free Python fixture isolation gate");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn refused(h: &Harness, base: &Path, lane: i64, why: &str) {
    let (code, out) = run(h, &base_env(base), &["tmp", &lane.to_string(), "--mkdir"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.starts_with("x1 1 {") && out.contains(why), "{out}");
}

#[test]
fn mkdir_refuses_unsafe_paths() {
    let h = Harness::new();
    let (root, lane) = campaign(&h);
    let real = h.home.join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    // The base is a symlink, even to a good dir, in every spelling: bare, trailing slash
    // and a terminal dot (components() drops both, so the link stays the final component).
    let link = h.home.join("link");
    symlink(&real, &link).unwrap();
    refused(&h, &link, lane, "is a symlink");
    refused(
        &h,
        Path::new(&format!("{}/", link.display())),
        lane,
        "is a symlink",
    );
    refused(
        &h,
        Path::new(&format!("{}/.", link.display())),
        lane,
        "is a symlink",
    );
    assert!(!real.join(root.to_string()).exists());
    // A campaign component is a symlink: nothing is made through it.
    let base = h.home.join("tb");
    fs::create_dir(&base).unwrap();
    fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&real, base.join(root.to_string())).unwrap();
    refused(&h, &base, lane, "is a symlink");
    assert!(!real.join(lane.to_string()).exists());
    // A lane component is a symlink.
    fs::remove_file(base.join(root.to_string())).unwrap();
    fs::create_dir(base.join(root.to_string())).unwrap();
    symlink(&real, base.join(root.to_string()).join(lane.to_string())).unwrap();
    refused(&h, &base, lane, "is a symlink");
    // Group- or world-writable (wrong owner needs root to set up).
    let open = h.home.join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o770)).unwrap();
    refused(&h, &open, lane, "group- or world-writable");
    fs::set_permissions(&open, fs::Permissions::from_mode(0o707)).unwrap();
    refused(&h, &open, lane, "group- or world-writable");
    // Not a directory.
    let file = h.home.join("file");
    fs::write(&file, "x").unwrap();
    refused(&h, &file, lane, "is not a directory");
    // A '..' component in the base.
    refused(&h, &real.join("x/.."), lane, "'..'");
    // A relative base.
    refused(&h, Path::new("rel/base"), lane, "absolute path");
    let (code, out) = run(&h, &base_env(Path::new("rel")), &["--json", "tmp", "1"]);
    assert_eq!(code, 1, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["kind"], "tmp", "{v}");
}

#[test]
fn unknown_task_is_rejected() {
    let h = Harness::new();
    campaign(&h);
    let env = base_env(&h.home.join("tb"));
    let (code, out) = run(&h, &env, &["tmp", "999999", "--mkdir"]);
    assert_eq!(code, 6, "{out}");
    assert!(out.contains("does not exist"), "{out}");
    assert!(!h.home.join("tb").exists());
    assert_eq!(run(&h, &env, &["tmp", "abc"]).0, 2);
    assert_eq!(run(&h, &env, &["tmp"]).0, 2);
}

#[test]
fn launch_shows_root_and_tmpdir_in_compact_mode() {
    let h = Harness::new();
    let (root, lane) = campaign(&h);
    let base = h.home.join("tb");
    let env = base_env(&base);
    let launch = [
        "launch",
        &lane.to_string(),
        "--provider",
        "claude",
        "--model",
        "test",
        "--effort",
        "medium",
    ]
    .map(String::from);
    let args: Vec<&str> = launch.iter().map(String::as_str).collect();
    let (code, out) = run(&h, &env, &args);
    assert_eq!(code, 0, "{out}");
    let v: Value = serde_json::from_str(out.strip_prefix("l1 ").unwrap()).unwrap();
    assert_eq!(v["root_id"], root, "{v}");
    let want = base.join(root.to_string()).join(lane.to_string());
    assert_eq!(v["tmpdir"].as_str(), want.to_str(), "{v}");
    // Informational: launch makes nothing.
    assert!(!base.exists());
    // The --json object stays as the contract golden pins it.
    let v = h.ok(&env, &args);
    assert!(
        v.get("tmpdir").is_none() && v.get("root_id").is_none(),
        "{v}"
    );
}

#[cfg(feature = "contract")]
#[test]
fn a_client_makes_the_dir_on_its_own_host() {
    use std::{
        io::{BufRead, BufReader},
        process::Stdio,
    };
    let h = Harness::new();
    h.script("tailscale", include_str!("hook/tailscale.sh"));
    fs::create_dir_all(h.home.join(".local/state/taskr")).unwrap();
    let hub_base = h.home.join("hub-tmp");
    let mut hub = h
        .command(&[
            ("NET_ID".into(), "hub".into()),
            ("TASKR_CONTRACT_TAILNET".into(), "1".into()),
            ("TASKR_CONTRACT_ORACLE".into(), "1".into()),
            ("TASKR_TMP_BASE".into(), hub_base.to_str().unwrap().into()),
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
    let client_base = h.home.join("client-tmp");
    let mut client: Env = vec![
        (
            "HOME".into(),
            h.home.join("client").to_str().unwrap().into(),
        ),
        ("TASKR_DB".into(), "".into()),
        ("NET_ID".into(), "host-a".into()),
        ("TASKR_CONTRACT_TAILNET".into(), "1".into()),
        ("TASKR_CONTRACT_ORACLE".into(), "1".into()),
    ];
    client.extend(base_env(&client_base));
    let root = h.ok(&client, &["new", "top", "--role", "orchestrator"])["task_id"]
        .as_i64()
        .unwrap();
    let lane = h.ok(
        &client,
        &[
            "new",
            "impl",
            "--role",
            "implementer",
            "--parent",
            &root.to_string(),
        ],
    )["task_id"]
        .as_i64()
        .unwrap();
    let requests = || h.count("select count(*) from requests where length(key)>?", 0);
    let before = requests();
    let want = client_base.join(root.to_string()).join(lane.to_string());
    assert_eq!(
        run(&h, &client, &["tmp", &lane.to_string(), "--mkdir"]),
        (0, format!("{}\n", want.display()))
    );
    assert_eq!(mode(&want), 0o700);
    assert!(!hub_base.exists(), "the hub made a dir");
    let v = h.ok(&client, &["tmp", &root.to_string()]);
    assert_eq!(
        (v["root_id"].as_i64(), v["tmpdir"].as_str()),
        (
            Some(root),
            client_base
                .join(root.to_string())
                .join(root.to_string())
                .to_str()
        )
    );
    assert_eq!(requests(), before, "a tmp read is not a stored request");
    // An unknown task is rejected through the hub, in either format.
    let (code, out) = run(&h, &client, &["tmp", "999999", "--mkdir"]);
    assert_eq!(code, 6, "{out}");
    assert!(
        out.starts_with("x1 6 ") && out.contains("does not exist"),
        "{out}"
    );
    let (code, out) = run(&h, &client, &["--json", "tmp", "999999"]);
    assert_eq!(code, 6, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["kind"], "rejected", "{v}");
    let (code, help) = run(
        &h,
        &client,
        &["close", &lane.to_string(), "--clean-tmp", "--help"],
    );
    assert_eq!(code, 0, "{help}");
    assert!(help.contains("clean-tmp"));
    assert_eq!(
        h.count("select status='open' from tasks where id=?", lane),
        1
    );
    let no_clean = h.ok(
        &client,
        &[
            "new",
            "no-clean",
            "--role",
            "implementer",
            "--parent",
            &root.to_string(),
        ],
    )["task_id"]
        .as_i64()
        .unwrap();
    h.ok(&client, &["tmp", &no_clean.to_string(), "--mkdir"]);
    h.ok(
        &client,
        &["close", &no_clean.to_string(), "--clean-tmp=false"],
    );
    assert!(
        client_base
            .join(root.to_string())
            .join(no_clean.to_string())
            .exists()
    );
    // Client close never forwards the deletion flag to the hub.
    fs::create_dir_all(hub_base.join(root.to_string()).join(lane.to_string())).unwrap();
    let hub_sentinel = hub_base
        .join(root.to_string())
        .join(lane.to_string())
        .join("sentinel");
    fs::write(&hub_sentinel, "hub").unwrap();
    h.ok(&client, &["set", &root.to_string(), "tmp.cleanup=keep"]);
    let report = want.join("report.md");
    fs::write(&report, "client report in tmp").unwrap();
    h.conn()
        .execute(
            "update tasks set report_path=? where id=?",
            taskr_core::db::params![report.to_str().unwrap(), lane],
        )
        .unwrap();
    h.ok(&client, &["close", &lane.to_string(), "--clean-tmp"]);
    let doc = h.count("select id from documents where task_id=? and kind='report' and captured=1 order by id desc limit 1",lane);
    assert_eq!(
        run(&h, &client, &["doc", "get", &doc.to_string()]),
        (0, "client report in tmp".into())
    );
    assert!(!want.exists());
    assert!(hub_sentinel.exists());
    h.ok(&client, &["tmp", &lane.to_string(), "--mkdir"]);
    // Legacy closed rows without a timestamp still authorize an explicit close cleanup.
    h.conn()
        .execute("update tasks set closed_at=null where id=?", [lane])
        .unwrap();
    h.ok(&client, &["close", &lane.to_string(), "--clean-tmp"]);
    assert!(!want.exists());
    fs::copy(
        h.home.join("agents.json"),
        h.home.join("client/agents.json"),
    )
    .unwrap();
    // A client daemon reads closure/policy over RPC and sweeps only its own base.
    h.ok(&client, &["set", &root.to_string(), "tmp.cleanup=on-close"]);
    h.ok(&client, &["tmp", &lane.to_string(), "--mkdir"]);
    age_close(&h, lane, 660);
    h.ok(&client, &["daemon", "--once"]);
    assert!(!want.exists());
    assert!(hub_sentinel.exists());
    let offline = h.ok(
        &client,
        &[
            "new",
            "offline",
            "--role",
            "implementer",
            "--parent",
            &root.to_string(),
        ],
    )["task_id"]
        .as_i64()
        .unwrap();
    h.ok(&client, &["tmp", &offline.to_string(), "--mkdir"]);
    let sentinel = client_base
        .join(root.to_string())
        .join(offline.to_string())
        .join("sentinel");
    fs::write(&sentinel, "offline").unwrap();
    let mut hub = _hub;
    hub.0.kill().unwrap();
    hub.0.wait().unwrap();
    let out = h.output(
        &client,
        &["close", &offline.to_string(), "--clean-tmp"],
        b"",
    );
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("qd1 "));
    assert!(String::from_utf8_lossy(&out.stderr).contains("immediate cleanup skipped"));
    assert!(sentinel.exists());
    assert_eq!(
        h.count("select status='open' from tasks where id=?", offline),
        1
    );
    let spool = h.home.join("client/.local/state/taskr/spool/queue");
    for entry in fs::read_dir(spool).unwrap() {
        let v: Value = serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).unwrap();
        assert!(
            !v["request"]["argv"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "--clean-tmp")
        );
        assert!(!v.to_string().contains(client_base.to_str().unwrap()));
    }
    // Unreachable sweep is fail-closed too (daemon's observation may exit 5).
    let _ = h.output(&client, &["daemon", "--once"], b"");
    assert!(sentinel.exists());
}

#[test]
fn cleanup_policy_values_defaults_root_only_and_budget() {
    let h = Harness::new();
    let (root, lane) = campaign(&h);
    let env = base_env(&h.home.join("policy-base"));
    assert_eq!(
        h.ok(&env, &["tmp", &lane.to_string()])["cleanup"]["policy"],
        "on-close"
    );
    for policy in ["on-close", "root-close", "keep"] {
        h.ok(
            &env,
            &["set", &root.to_string(), &format!("tmp.cleanup={policy}")],
        );
        assert_eq!(
            h.ok(&env, &["tmp", &lane.to_string()])["cleanup"]["policy"],
            policy
        );
        assert_eq!(
            run(
                &h,
                &env,
                &["set", &lane.to_string(), &format!("tmp.cleanup={policy}")]
            )
            .0,
            2
        );
    }
    for bad in ["", "invalid", "ON-CLOSE"] {
        assert_eq!(
            run(
                &h,
                &env,
                &["set", &root.to_string(), &format!("tmp.cleanup={bad}")]
            )
            .0,
            2
        );
        assert_eq!(
            h.ok(&env, &["tmp", &lane.to_string()])["cleanup"]["policy"],
            "keep"
        );
    }
    for n in 0..19 {
        h.ok(&env, &["set", &root.to_string(), &format!("ref{n}=x")]);
    }
    assert_eq!(run(&h, &env, &["set", &root.to_string(), "extra=x"]).0, 6);
}

#[test]
fn close_clean_tmp_is_immediate_local_and_repeatable() {
    let h = Harness::new();
    let (root, lane) = campaign(&h);
    let base = h.home.join("cleanup-base");
    let env = base_env(&base);
    h.ok(&env, &["set", &root.to_string(), "tmp.cleanup=keep"]);
    let path = base.join(root.to_string()).join(lane.to_string());
    h.ok(&env, &["tmp", &lane.to_string(), "--mkdir"]);
    fs::write(path.join("sentinel"), "x").unwrap();
    assert_eq!(run(&h, &env, &["close", "999999", "--clean-tmp"]).0, 6);
    assert!(path.join("sentinel").exists());
    h.ok(&env, &["close", &lane.to_string(), "--clean-tmp"]);
    assert!(!path.exists());
    h.ok(&env, &["close", &lane.to_string(), "--clean-tmp"]);
    h.ok(&env, &["tmp", &lane.to_string(), "--mkdir"]);
    h.ok(&env, &["close", &lane.to_string(), "--clean-tmp"]);
    assert!(!path.exists());
    h.ok(&env, &["tmp", &root.to_string(), "--mkdir"]);
    let open_lane = h.ok(
        &env,
        &[
            "new",
            "open",
            "--role",
            "implementer",
            "--parent",
            &root.to_string(),
        ],
    )["task_id"]
        .as_i64()
        .unwrap();
    h.ok(&env, &["tmp", &open_lane.to_string(), "--mkdir"]);
    h.ok(&env, &["close", &root.to_string(), "--clean-tmp"]);
    assert!(!base.join(root.to_string()).exists());
}

#[test]
fn close_clean_tmp_refuses_unsafe_base_after_recording_close() {
    let h = Harness::new();
    let (root, lane) = campaign(&h);
    let base = h.home.join("cleanup-base");
    let env = base_env(&base);
    h.ok(&env, &["tmp", &lane.to_string(), "--mkdir"]);
    let sentinel = base
        .join(root.to_string())
        .join(lane.to_string())
        .join("sentinel");
    fs::write(&sentinel, "x").unwrap();
    let link = h.home.join("cleanup-link");
    symlink(&base, &link).unwrap();
    for spelling in [link.clone(), link.join(".")] {
        let out = h.output(
            &base_env(&spelling),
            &["close", &lane.to_string(), "--clean-tmp"],
            b"",
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stdout).contains("close succeeded"));
        assert!(sentinel.exists());
        assert_eq!(
            h.count("select status='closed' from tasks where id=?", lane),
            1
        );
    }
}

fn age_close(h: &Harness, task: i64, seconds: i64) {
    let at = taskr_core::store::stamp(
        time::OffsetDateTime::now_utc() - time::Duration::seconds(seconds),
    );
    h.conn()
        .execute(
            "update tasks set closed_at=? where id=?",
            taskr_core::db::params![at, task],
        )
        .unwrap();
}

#[test]
fn daemon_sweeps_ledger_state_with_grace_on_this_host() {
    let h = Harness::new();
    let base = h.home.join("sweep-base");
    let env = base_env(&base);
    for policy in ["on-close", "root-close", "keep"] {
        let (root, lane) = campaign(&h);
        if policy != "on-close" {
            h.ok(
                &env,
                &["set", &root.to_string(), &format!("tmp.cleanup={policy}")],
            );
        }
        h.ok(&env, &["tmp", &lane.to_string(), "--mkdir"]);
        let path = base.join(root.to_string()).join(lane.to_string());
        fs::write(path.join("sentinel"), "x").unwrap();
        h.ok(&env, &["daemon", "--once"]);
        assert!(path.exists(), "open {policy}");
        h.ok(&env, &["close", &lane.to_string()]);
        h.ok(&env, &["daemon", "--once"]);
        assert!(path.exists(), "grace {policy}");
        age_close(&h, lane, 660);
        fs::create_dir_all(base.join(root.to_string()).join("999999")).unwrap();
        fs::create_dir_all(base.join(root.to_string()).join("nonnumeric")).unwrap();
        h.ok(&env, &["daemon", "--once"]);
        assert_eq!(path.exists(), policy != "on-close", "lane {policy}");
        assert!(base.join(root.to_string()).join("999999").exists());
        assert!(base.join(root.to_string()).join("nonnumeric").exists());
        h.ok(&env, &["close", &root.to_string()]);
        h.ok(&env, &["daemon", "--once"]);
        assert!(base.join(root.to_string()).exists(), "root grace {policy}");
        age_close(&h, root, 660);
        h.ok(&env, &["daemon", "--once"]);
        assert!(
            base.join(root.to_string()).exists(),
            "unknown lane retains root {policy}"
        );
        assert!(base.join(root.to_string()).join("999999").exists());
        fs::remove_dir(base.join(root.to_string()).join("999999")).unwrap();
        h.ok(&env, &["daemon", "--once"]);
        assert_eq!(
            base.join(root.to_string()).exists(),
            policy == "keep",
            "confirmed root {policy}"
        );
    }
    fs::create_dir_all(base.join("888888/1")).unwrap();
    fs::create_dir_all(base.join("nonnumeric/1")).unwrap();
    h.ok(&env, &["daemon", "--once"]);
    assert!(base.join("888888/1").exists());
    assert!(base.join("nonnumeric/1").exists());
}

#[test]
fn fixture_commands_keep_tmp_work_inside_scratch() {
    let h = Harness::new();
    let cmd = h.command(&[]);
    assert_eq!(
        cmd.get_envs()
            .find(|(k, _)| *k == "TASKR_TMP_BASE")
            .unwrap()
            .1,
        Some(h.home.join("taskr-tmp").as_os_str())
    );
    let base = h.home.join("override");
    let cmd = h.command(&base_env(&base));
    assert_eq!(
        cmd.get_envs()
            .find(|(k, _)| *k == "TASKR_TMP_BASE")
            .unwrap()
            .1,
        Some(base.as_os_str())
    );
}

#[test]
fn daemon_limits_lookup_errors_without_hiding_removal_errors() {
    let h = Harness::new();
    let base = h.home.join("log-base");
    let env = base_env(&base);
    let (root, _) = campaign(&h);
    h.ok(&env, &["tmp", &root.to_string(), "--mkdir"]);
    for id in 900000..900300 {
        fs::create_dir(base.join(id.to_string())).unwrap();
    }
    let (unsafe_root, _) = campaign(&h);
    let outside = h.home.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), "outside").unwrap();
    symlink(&outside, base.join(unsafe_root.to_string())).unwrap();
    h.ok(&env, &["daemon", "--once"]);
    let log = fs::read_to_string(h.home.join(".local/state/taskr/daemon.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("does not exist"))
            .count(),
        1,
        "{log}"
    );
    assert!(
        log.lines()
            .any(|line| line.contains(&format!("tmp sweep {unsafe_root}:"))),
        "{log}"
    );
    assert_eq!(
        fs::read_to_string(outside.join("sentinel")).unwrap(),
        "outside"
    );
}
