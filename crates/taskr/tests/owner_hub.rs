//! A root with `hub=on` receives other campaigns' owner asks as `owner_ask` and
//! their answers as `owner_closed`, through the CLI.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use serde_json::{Value, json};
use support::*;

fn inbox(h: &Harness, hub: &str, kind: &str) -> Vec<Value> {
    h.ok(&[], &["inbox", "--as", hub, "--limit", "200"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == kind)
        .cloned()
        .collect()
}
fn ask(h: &Harness, lane: &Env, text: &str) -> i64 {
    h.ok(lane, &["ask", text, "--owner", "--blocking"])["event_id"]
        .as_i64()
        .unwrap()
}

#[test]
fn hub_root_receives_owner_asks_and_closes() {
    let h = Harness::new();
    let (hw, _, hub_lane) = h.lane("claude", "implementer");
    let (cw, _, c_lane) = h.lane("claude", "implementer");
    let hub = h
        .conn()
        .query_row("select parent_id from tasks where id=?", [hw], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
        .to_string();
    let c = h
        .conn()
        .query_row("select parent_id from tasks where id=?", [cw], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    h.ok(&[], &["set", &hub, "hub=on"]);
    let code = |args: &[&str]| h.output(&[], args, b"").status.code();
    assert_eq!(
        code(&["set", &c.to_string(), "hub=on"]),
        Some(6),
        "second hub"
    );
    assert_eq!(
        code(&["set", &cw.to_string(), "hub=on"]),
        Some(6),
        "lane hub"
    );
    assert_eq!(code(&["set", &hub, "hub=yes"]), Some(2), "bad value");

    let a = ask(&h, &c_lane, "Ship the release?");
    let got = inbox(&h, &hub, "owner_ask");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["related_event_id"], a);
    assert_eq!(got[0]["task_id"], cw);
    assert_eq!(got[0]["summary"], "Ship the release?");
    assert_eq!(got[0]["data"], json!({"ask":a,"root":c,"blocking":true}));

    h.ok(
        &[],
        &["answer", &a.to_string(), "yes", "--as", &c.to_string()],
    );
    let closed = inbox(&h, &hub, "owner_closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["related_event_id"], a);
    assert_eq!(closed[0]["summary"], "yes");
    let answer = h
        .conn()
        .query_row("select answered_by from events where id=?", [a], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    assert_eq!(
        closed[0]["data"],
        json!({"ask":a,"answer":answer,"withdrawn":false})
    );

    // The hub's own tree: no owner_ask.
    ask(&h, &hub_lane, "Own tree?");
    assert_eq!(inbox(&h, &hub, "owner_ask").len(), 1);

    // hub= cleared: nothing new.
    h.ok(&[], &["set", &hub, "hub="]);
    ask(&h, &c_lane, "After clear?");
    assert_eq!(inbox(&h, &hub, "owner_ask").len(), 1);
    // wait --for accepts both kinds.
    let w = h.ok(
        &[],
        &[
            "wait",
            "--as",
            &hub,
            "--for",
            "owner_ask,owner_closed",
            "--timeout",
            "1000",
        ],
    );
    assert_eq!(w["event"]["kind"], "owner_ask", "{w}");
}
