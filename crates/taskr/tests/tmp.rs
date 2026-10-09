//! `taskr tmp`: lane and root paths, --mkdir's 0700 dirs and refusals, an unknown task, the
//! compact launch line, and a client that makes the dir on its own host.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
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
    let v = h.ok(&[], &["tmp", &lane.to_string()]);
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
    for _ in 0..2 {
        assert_eq!(
            run(&h, &env, &["tmp", &lane.to_string(), "--mkdir"]),
            (0, format!("{}\n", want.display()))
        );
        for p in [&base, &base.join(root.to_string()), &want] {
            assert_eq!(mode(p), 0o700, "{}", p.display());
        }
    }
    // A file left in the lane dir survives a second --mkdir.
    fs::write(want.join("keep"), "x").unwrap();
    assert_eq!(run(&h, &env, &["tmp", &lane.to_string(), "--mkdir"]).0, 0);
    assert!(want.join("keep").exists());
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
    // The base is a symlink, even to a good dir.
    let link = h.home.join("link");
    symlink(&real, &link).unwrap();
    refused(&h, &link, lane, "is a symlink");
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
}
