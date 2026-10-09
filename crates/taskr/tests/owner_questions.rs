//! `ask --question` / `--dialog`: structured owner asks through the CLI, glance, asks and a client.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use serde_json::{Value, json};
use support::*;

const QUESTION: &str = r#"{"header":"Auth","question":"Which auth method?","multiSelect":false,"options":[{"label":"OAuth","description":"Use the IdP","recommended":true,"preview":"mock"},{"label":"Keys"}]}"#;

fn stored() -> Value {
    json!({"header":"Auth","question":"Which auth method?","multiSelect":false,
        "options":[{"label":"OAuth","description":"Use the IdP","recommended":true},{"label":"Keys"}]})
}

fn ask_row(h: &Harness, id: i64) -> (String, Value) {
    h.conn()
        .query_row("select summary,data from events where id=?", [id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map(|(s, d)| (s, serde_json::from_str(&d).unwrap()))
        .unwrap()
}

fn need(h: &Harness, ask: i64) -> Value {
    let glance = h.ok(&[], &["glance"]);
    glance["needs_you"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["ask_id"] == ask)
        .cloned()
        .unwrap_or_else(|| panic!("ask {ask} not in {glance}"))
}

fn asks_json(h: &Harness, ask: i64) -> Value {
    let out = h.output(&[], &["--json", "asks", "--open"], b"");
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|r| r["id"] == ask)
        .unwrap()
}

#[test]
fn structured_ask_round_trips_and_plain_ask_is_unchanged() {
    let h = Harness::new();
    let (_, _, lane) = h.lane("claude", "implementer");
    let plain = h.ok(&lane, &["ask", "Merge now?", "--owner"])["event_id"]
        .as_i64()
        .unwrap();
    let structured = h.ok(
        &lane,
        &["ask", "--owner", "--blocking", "--question", QUESTION],
    )["event_id"]
        .as_i64()
        .unwrap();
    let context = h.ok(
        &lane,
        &[
            "ask",
            "PR #12 needs login.",
            "--owner",
            "--question",
            QUESTION,
            "--dialog",
        ],
    )["event_id"]
        .as_i64()
        .unwrap();

    // Plain asks keep their exact data, glance item and asks row.
    assert_eq!(
        ask_row(&h, plain),
        ("Merge now?".into(), json!({"blocking":false,"owner":true}))
    );
    let n = need(&h, plain);
    assert!(
        n.get("question").is_none() && n.get("dialog").is_none(),
        "{n}"
    );
    assert_eq!(
        asks_json(&h, plain)["data"],
        json!({"blocking":false,"owner":true})
    );

    // The summary is derived from the question; TEXT, when given, is context before it.
    let (summary, data) = ask_row(&h, structured);
    assert_eq!(summary, "Auth: Which auth method? (A) OAuth; (B) Keys");
    assert_eq!(
        data,
        json!({"blocking":true,"owner":true,"question":stored()})
    );
    let n = need(&h, structured);
    assert_eq!(n["question"], stored());
    assert_eq!(n["text"], summary);
    assert!(n.get("dialog").is_none(), "{n}");
    assert_eq!(asks_json(&h, structured)["data"]["question"], stored());

    let (summary, data) = ask_row(&h, context);
    assert_eq!(
        summary,
        "PR #12 needs login. Auth: Which auth method? (A) OAuth; (B) Keys"
    );
    assert_eq!(
        data,
        json!({"blocking":false,"owner":true,"question":stored(),"dialog":true})
    );
    let n = need(&h, context);
    assert_eq!(
        (n["dialog"].clone(), n["question"].clone()),
        (json!(true), stored())
    );
    let row = asks_json(&h, context);
    assert_eq!(row["data"]["dialog"], json!(true));
    assert_eq!(row["summary"], summary);

    // --dialog alone on a plain owner ask.
    let d = h.ok(&lane, &["ask", "Ship?", "--owner", "--dialog"])["event_id"]
        .as_i64()
        .unwrap();
    assert_eq!(need(&h, d)["dialog"], json!(true));
    assert!(need(&h, d).get("question").is_none());
}

#[test]
fn usage_errors() {
    let h = Harness::new();
    let (_, _, lane) = h.lane("claude", "implementer");
    for (args, why) in [
        (
            &["ask", "--question", QUESTION][..],
            "--question needs --owner",
        ),
        (&["ask", "x", "--dialog"], "--dialog needs --owner"),
        (
            &["ask", "--owner"],
            "ask: expected 1 to 1 positional arguments, got 0",
        ),
        (
            &["ask", "a", "b", "--owner"],
            "ask: expected 0 to 1 positional arguments, got 2",
        ),
        (
            &["ask", "--owner", "--question", "{"],
            "--question: not JSON",
        ),
        (
            &[
                "ask",
                "--owner",
                "--question",
                r#"{"question":"q","options":[{"label":"a"}]}"#,
            ],
            "1 options, want 2-4",
        ),
    ] {
        let out = h.output(&lane, args, b"");
        let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {text}");
        assert!(text.contains(why), "{args:?}: {text}");
    }
    assert_eq!(
        h.count(
            "select count(*) from events where kind='ask' and task_id>?",
            0
        ),
        0
    );
}

#[cfg(feature = "contract")]
#[test]
fn client_ask_stores_the_same_data_on_the_hub() {
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
    let root = h.ok(&[], &["new", "q-root", "--role", "orchestrator"])["task_id"]
        .as_i64()
        .unwrap();
    h.conn()
        .execute("update tasks set machine='host-a' where id=?", [root])
        .unwrap();
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
    let r = root.to_string();
    let out = h.ok(
        &client,
        &[
            "ask",
            "--owner",
            "--question",
            QUESTION,
            "--dialog",
            "--blocking",
            "--as",
            &r,
        ],
    );
    let id = out["event_id"].as_i64().unwrap();
    assert_eq!(
        ask_row(&h, id),
        (
            "Auth: Which auth method? (A) OAuth; (B) Keys".into(),
            json!({"blocking":true,"owner":true,"question":stored(),"dialog":true})
        )
    );
    // The hub's validator answers the client: exit 2, nothing stored.
    let bad = h.output(
        &client,
        &["ask", "--owner", "--question", "[]", "--as", &r],
        b"",
    );
    assert_eq!(bad.status.code(), Some(2));
    assert_eq!(
        h.count(
            "select count(*) from events where kind='ask' and task_id=?",
            root
        ),
        1
    );

    // The byte limit is enforced on bytes, not characters: 4096 fits either
    // way, 4097 ASCII and 4098 Unicode bytes are rejected, and rejected calls
    // store no event.
    for (kind, n, expected) in [
        ("ascii", 4096usize, 0),
        ("ascii", 4097, 2),
        ("unicode", 4096, 0),
        ("unicode", 4098, 2),
    ] {
        let base = json!({"question":"", "options":[{"label":"A"},{"label":"B"}]}).to_string();
        let fill = n - base.len();
        let body = if kind == "unicode" {
            "é".repeat(fill / 2) + &"x".repeat(fill % 2)
        } else {
            "x".repeat(fill)
        };
        let raw = json!({"question":body,"options":[{"label":"A"},{"label":"B"}]}).to_string();
        assert_eq!(raw.len(), n);
        let before = h.count(
            "select count(*) from events where kind='ask' and task_id=?",
            root,
        );
        let out = h.output(
            &client,
            &["ask", "--owner", "--question", &raw, "--as", &r],
            b"",
        );
        assert_eq!(out.status.code(), Some(expected), "{kind} {n}: {out:?}");
        assert_eq!(
            h.count(
                "select count(*) from events where kind='ask' and task_id=?",
                root
            ),
            before + i64::from(expected == 0)
        );
    }

    // A 500-character multibyte description is within the description limit.
    let raw = json!({"question":"q","options":[{"label":"A","description":"é".repeat(500)},{"label":"B"}]}).to_string();
    let out = h.output(
        &client,
        &["ask", "--owner", "--question", &raw, "--as", &r],
        b"",
    );
    assert!(out.status.success(), "{out:?}");
}
