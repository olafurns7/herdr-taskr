//! `taskr after`: subscribe, list, cancel, usage errors, a filtered wait that wakes
//! on `af`, and the same through a client host.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use support::*;

fn code(h: &Harness, env: &[(String, String)], args: &[&str]) -> Option<i32> {
    h.output(env, args, b"").status.code()
}
fn stdout(h: &Harness, env: &[(String, String)], args: &[&str]) -> String {
    let out = h.output(env, args, b"");
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn subscribe_list_cancel_and_a_filtered_wait_wakes() {
    let h = Harness::new();
    let (lane, _, env) = h.lane("claude", "implementer");
    let waiter = h.ok(&[], &["new", "hub", "--role", "orchestrator"])["task_id"]
        .as_i64()
        .unwrap();
    let (ws, ls) = (waiter.to_string(), lane.to_string());
    let (w, l) = (ws.as_str(), ls.as_str());
    let v = h.ok(&[], &["after", l, "--as", w]);
    assert!(
        v["target"] == l && v["on"] == "done,closed" && v["keep"] == false,
        "{v}"
    );
    let keep = h.ok(
        &[],
        &["after", l, "--as", w, "--on", "ready,ready", "--keep"],
    );
    assert!(keep["on"] == "ready" && keep["keep"] == true, "{keep}");
    let text = stdout(&h, &[], &["after", l, "--as", w, "--on", "fail"]);
    assert!(
        text.starts_with("af1 ") && text.lines().count() == 1,
        "{text}"
    );
    let pr = h.ok(&[], &["after", "pr:demo-org/demo#7", "--as", w]);
    assert!(
        pr["target"] == "pr:demo-org/demo#7" && pr["on"] == "merged",
        "{pr}"
    );
    // pr:N needs watch.json's default repo.
    assert_eq!(code(&h, &[], &["after", "pr:7", "--as", w]), Some(2));
    let state = h.home.join(".local/state/taskr");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("watch.json"),
        r#"{"github":{"enabled":true,"default_repo":"demo-org/demo"}}"#,
    )
    .unwrap();
    let short = h.ok(
        &[],
        &["after", "pr:#7", "--as", w, "--on", "checks_green,merged"],
    );
    assert_eq!(short["target"], "pr:demo-org/demo#7");
    for (args, want) in [
        (vec!["after", l], 2),
        (vec!["after", "--as", w], 2),
        (vec!["after", "deploy:trip-web", "--as", w], 2),
        (vec!["after", "0", "--as", w], 2),
        (vec!["after", l, "--as", w, "--on", "merged"], 2),
        (vec!["after", "pr:7", "--as", w, "--on", "done"], 2),
        (vec!["after", "--list", "--as", w, "--keep"], 2),
        (vec!["after", "--cancel", "0"], 2),
        (vec!["after", "--cancel", "1", "--as", w], 2),
        (vec!["after", "999999", "--as", w], 6),
        (vec!["after", w, "--as", l], 6),
    ] {
        assert_eq!(code(&h, &[], &args), Some(want), "{args:?}");
    }
    // --list: every subscription of the root, fired or not.
    let list = h.ok(&[], &["after", "--list", "--as", w])["subscriptions"].clone();
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 5, "{list}");
    assert_eq!(rows[0]["target"], l);
    assert!(rows.iter().all(|r| r["fired_at"].is_null()), "{list}");
    let text = stdout(&h, &[], &["after", "--list", "--as", w]);
    assert!(
        text.lines().all(|l| l.starts_with("j1 {")) && text.lines().count() == 5,
        "{text}"
    );
    // --cancel removes one; a second cancel is rejected.
    let id = rows[2]["id"].to_string();
    assert_eq!(
        stdout(&h, &[], &["after", "--cancel", &id]),
        format!("af1 {id} cancelled\n")
    );
    assert_eq!(code(&h, &[], &["after", "--cancel", &id]), Some(6));
    // The lane is done: a wait filtered to other kinds and tasks still wakes on af.
    h.ok(&env, &["done", "finished"]);
    let text = stdout(
        &h,
        &[],
        &[
            "wait",
            "--as",
            w,
            "--for",
            "ask",
            "--from",
            w,
            "--timeout",
            "0",
        ],
    );
    let fields: Vec<&str> = text.trim_end().split('\t').collect();
    assert_eq!(fields[0], "e1", "{text}");
    assert_eq!((fields[2], fields[3], fields[4]), (l, "-", "af"), "{text}");
    assert_eq!(fields[7], format!("\"task {l} done\""), "{text}");
    assert!(
        fields[8].contains(&format!("\"target\":\"{l}\"")) && fields[8].contains("\"on\":\"done\""),
        "{text}"
    );
    let list = h.ok(&[], &["after", "--list", "--as", w])["subscriptions"].clone();
    assert!(
        !list[0]["fired_at"].is_null() && list[1]["fired_at"].is_null(),
        "{list}"
    );
    // close cancels the closing root's subscriptions.
    h.ok(&[], &["close", w]);
    assert_eq!(
        h.count(
            "select count(*) from subscriptions where waiter_task_id=?",
            waiter
        ),
        0
    );
}

#[cfg(feature = "contract")]
#[test]
fn a_client_subscribes_lists_and_waits_through_the_hub() {
    use std::io::BufRead;
    struct Hub(std::process::Child);
    impl Drop for Hub {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let h = Harness::new();
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
    std::io::BufReader::new(hub.0.stdout.take().unwrap())
        .read_line(&mut url)
        .unwrap();
    assert!(url.starts_with("http://[::1]:"), "{url:?}");
    std::fs::create_dir_all(h.home.join("client/.local/state/taskr")).unwrap();
    std::fs::write(h.home.join("client/.local/state/taskr/server.url"), &url).unwrap();
    let mut client = contract.to_vec();
    client.extend([
        (
            "HOME".into(),
            h.home.join("client").to_str().unwrap().into(),
        ),
        ("TASKR_DB".into(), "".into()),
        ("NET_ID".into(), "host-a".into()),
    ]);
    let (lane, _, _) = h.lane("claude", "implementer");
    let root = h.ok(&client, &["new", "remote", "--role", "orchestrator"])["task_id"]
        .as_i64()
        .unwrap();
    let (rs, ls) = (root.to_string(), lane.to_string());
    let (r, l) = (rs.as_str(), ls.as_str());
    let requests = || h.count("select count(*) from requests where length(key)>?", 0);
    let before = requests();
    let v = h.ok(&client, &["after", l, "--as", r]);
    assert_eq!(v["target"], l, "{v}");
    assert_eq!(requests(), before + 1, "after is a stored write");
    let list = h.ok(&client, &["after", "--list", "--as", r]);
    assert_eq!(list["subscriptions"][0]["target"], l, "{list}");
    assert_eq!(requests(), before + 1, "after --list is a read");
    h.ok(&[], &["close", l]);
    let text = stdout(
        &h,
        &client,
        &["wait", "--as", r, "--for", "ask", "--timeout", "0"],
    );
    assert!(
        text.starts_with("e1\t") && text.contains("\taf\t"),
        "{text}"
    );
    assert!(text.contains(&format!("\"task {l} closed\"")), "{text}");
}
