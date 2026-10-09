//! `glance --brief` through the CLI: usage errors, and a client prints the hub's prose unchanged.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use support::*;

fn brief_with_ask(h: &Harness) -> i64 {
    let (_, _, lane) = h.lane("claude", "implementer");
    h.ok(&lane, &["ask", "Merge now?", "--owner"]);
    1
}

#[test]
fn brief_usage_errors() {
    let h = Harness::new();
    let root = brief_with_ask(&h);
    for args in [
        &["glance", "--brief", "--watch"][..],
        &["--json", "glance", "--brief"],
        &["glance", "--since", "30m"],
        &["glance", "--brief", "--since", "soon"],
        &["glance", "--brief", "--since", "-5m"],
    ] {
        let out = h.output(&[], args, b"");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
    let out = h.output(&[], &["glance", "--brief", "--since", "0"], b"");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "{text}");
    assert!(
        text.starts_with("taskr · ") && text.contains(" · cursor="),
        "{text}"
    );
    assert!(
        text.contains(&format!("#{root} ")) && text.contains(" o1 "),
        "{text}"
    );
}

#[cfg(feature = "contract")]
#[test]
fn client_prints_hub_brief_unchanged() {
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
    let frozen = (
        "TASKR_FROZEN_NOW".to_string(),
        "2026-10-07T17:30:00Z".to_string(),
    );
    let contract = [
        ("TASKR_CONTRACT_TAILNET".to_string(), "1".to_string()),
        ("TASKR_CONTRACT_ORACLE".to_string(), "1".to_string()),
        frozen.clone(),
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
    brief_with_ask(&h);
    let local = h.output(&[frozen], &["glance", "--brief"], b"");
    let forwarded = h.output(&client, &["glance", "--brief"], b"");
    assert!(local.status.success() && forwarded.status.success());
    let text = String::from_utf8(forwarded.stdout.clone()).unwrap();
    assert!(
        text.starts_with("taskr · ") && text.contains(" o1 "),
        "{text}"
    );
    assert_eq!(forwarded.stdout, local.stdout);
    let usage = h.output(&client, &["glance", "--brief", "--json"], b"");
    assert_eq!(usage.status.code(), Some(2));
}
