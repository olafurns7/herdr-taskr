//! `taskr mcp` (a stdio MCP server bound at start) and `taskr inbox` (a raw,
//! non-consuming inbox read). Each server is its own process fed newline-delimited
//! JSON-RPC; every ledger assertion reads the reopened scratch database.
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use serde_json::{Value, json};
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
fn id(v: &Value, key: &str) -> String {
    v[key]
        .as_i64()
        .unwrap_or_else(|| panic!("{key}: {v}"))
        .to_string()
}
fn parent(h: &Harness, task: i64) -> String {
    h.count("select parent_id from tasks where id=?", task)
        .to_string()
}
/// Tasks with their cursors and markers, events, attempts, leases, meta and documents:
/// what a read or a refused call must leave untouched.
fn snapshot(h: &Harness) -> String {
    [
        "select group_concat(id||':'||status||':'||ifnull(current_launch_id,'-')||':'||acked_event_id||':'||ifnull(pending_event_id,'-')||':'||ifnull(waiting_until,'-')||':'||updated_at,' ') from (select * from tasks order by id)",
        "select count(*)||'/'||ifnull(max(id),0) from events",
        "select group_concat(id||'|'||ifnull(submitted_at,'-')||'|'||ifnull(received_at,'-'),' ') from (select * from delivery_attempts order by id)",
        "select group_concat(task_id||'|'||fence||'|'||ifnull(holder,'-')||'|'||expires_at,' ') from (select * from consumer_leases order by task_id)",
        "select group_concat(key||'='||value,' ') from (select * from meta order by key)",
        "select count(*)||'' from documents",
    ]
    .map(|sql| text(h, sql))
    .join(" # ")
}
fn rejected(h: &Harness, env: &[(String, String)], args: &[&str], want: i32) -> String {
    let before = snapshot(h);
    let (code, out, err) = run(h, env, args);
    assert_eq!(code, Some(want), "{args:?}: {out}{err}");
    assert_eq!(snapshot(h), before, "{args:?} wrote");
    out
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
            (
                k.clone(),
                if k == "TASKR_LAUNCH" {
                    l.clone()
                } else {
                    v.clone()
                },
            )
        })
        .collect();
    (l, env)
}
/// The event id of a compact `e1` wait line.
fn waited(out: &str) -> String {
    out.lines()
        .find(|l| l.starts_with("e1\t"))
        .unwrap_or_else(|| panic!("no e1 line: {out}"))
        .split('\t')
        .nth(1)
        .unwrap()
        .to_string()
}
fn insert_event(
    h: &Harness,
    task: &str,
    to: &str,
    kind: &str,
    summary: &str,
    data: &str,
) -> String {
    let c = h.conn();
    c.execute(
        "insert into events(task_id,recipient_task_id,kind,summary,data,created_at) values(?,?,?,?,?,'2026-01-01T00:00:00.000Z')",
        taskr_core::db::params![task, to, kind, summary, data],
    )
    .unwrap();
    c.last_insert_rowid().to_string()
}

const INIT: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"synthetic-client","version":"0"}}}"#;
const READY: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

/// Feed `input` to `taskr mcp ARGS` and close stdin: the exit, every stdout line (each
/// must be one JSON value), and stderr.
fn serve(
    h: &Harness,
    env: &[(String, String)],
    args: &[&str],
    input: &[u8],
) -> (Option<i32>, Vec<Value>, String) {
    let mut argv = vec!["mcp"];
    argv.extend(args);
    let out = h.output(env, &argv, input);
    let lines = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("stdout {l:?}: {e}")))
        .collect();
    (
        out.status.code(),
        lines,
        String::from_utf8_lossy(&out.stderr).into(),
    )
}
/// Initialize, then send each (method, params) as request id 1.., then EOF: the replies.
fn session(
    h: &Harness,
    env: &[(String, String)],
    args: &[&str],
    calls: &[(&str, Value)],
) -> Vec<Value> {
    let mut input = format!("{INIT}\n{READY}\n");
    for (i, (method, params)) in calls.iter().enumerate() {
        input += &json!({"jsonrpc":"2.0","id":i+1,"method":method,"params":params}).to_string();
        input.push('\n');
    }
    let (code, lines, err) = serve(h, env, args, input.as_bytes());
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(lines.len(), calls.len() + 1, "{lines:?}");
    assert!(
        lines[0]["result"]["protocolVersion"] == "2025-06-18",
        "{}",
        lines[0]
    );
    for (i, l) in lines[1..].iter().enumerate() {
        assert_eq!(l["id"], i + 1, "{l}");
    }
    lines[1..].to_vec()
}
fn tool(name: &str, arguments: Value) -> (&'static str, Value) {
    ("tools/call", json!({"name":name,"arguments":arguments}))
}
/// One tool call's CallToolResult.
fn call(h: &Harness, env: &[(String, String)], args: &[&str], name: &str, a: Value) -> Value {
    let r = session(h, env, args, &[tool(name, a)]).remove(0);
    assert!(r.get("result").is_some(), "{name}: {r}");
    r["result"].clone()
}
fn outcome(r: &Value) -> &str {
    r["structuredContent"]["outcome"].as_str().unwrap()
}
fn code(r: &Value) -> i64 {
    r["error"]["code"].as_i64().unwrap_or_else(|| panic!("{r}"))
}
fn root_args(r: &str, generation: &str) -> Vec<String> {
    ["--as", r, "--session", generation]
        .map(String::from)
        .to_vec()
}
fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}
/// The worker write tools, each with valid arguments.
fn worker_writes(
    attempt: &str,
    event: &str,
    fence: &str,
    report: &str,
) -> Vec<(&'static str, Value)> {
    let n = |s: &str| s.parse::<i64>().unwrap();
    vec![
        ("taskr_note", json!({"text":"stale note"})),
        ("taskr_ask", json!({"text":"stale ask","blocking":true})),
        (
            "taskr_ready",
            json!({"summary":"stale ready","report_path":report}),
        ),
        ("taskr_done", json!({"summary":"stale done"})),
        ("taskr_fail", json!({"summary":"stale fail"})),
        ("taskr_received", json!({"attempt":n(attempt)})),
        ("taskr_ack", json!({"event":n(event),"fence":n(fence)})),
    ]
}

#[test]
fn framing_and_lifecycle_answer_every_request_and_keep_serving() {
    let h = Harness::new();
    let (_, _, env) = h.lane("claude", "implementer");
    let before = snapshot(&h);
    let mut input: Vec<u8> = Vec::new();
    let mut line = |bytes: &[u8]| {
        input.extend_from_slice(bytes);
        input.push(b'\n');
    };
    let ping = |id: Value| json!({"jsonrpc":"2.0","id":id,"method":"ping"}).to_string();
    line(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    line(ping(json!(2)).as_bytes());
    line(b"");
    line(b"   ");
    line(b"{not json");
    line(b"\xff\xfe{}");
    let mut big = br#"{"jsonrpc":"2.0","id":99,"method":"ping","params":{"pad":""#.to_vec();
    big.extend(vec![b'x'; 1 << 20]);
    big.extend(br#""}}"#);
    line(&big);
    line(format!("[{}]", ping(json!(3))).as_bytes());
    line(b"5");
    line(br#"{"id":4,"method":"ping"}"#);
    line(br#"{"jsonrpc":"1.0","id":5,"method":"ping"}"#);
    line(br#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#);
    line(br#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#);
    line(br#"{"jsonrpc":"2.0","id":{"a":1},"method":"ping"}"#);
    line(br#"{"jsonrpc":"2.0","id":6,"method":7}"#);
    line(br#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
    line(br#"{"jsonrpc":"2.0","id":"s-8","method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"synthetic-client","version":"0"}}}"#);
    line(br#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#);
    line(INIT.replace("\"id\":0", "\"id\":10").as_bytes());
    line(br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":9}}"#);
    line(br#"{"jsonrpc":"2.0","method":"notifications/synthetic"}"#);
    // A tool call sent as a notification runs nothing and gets no reply.
    line(br#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"taskr_note","arguments":{"text":"never"}}}"#);
    line(READY.as_bytes());
    line(br#"{"jsonrpc":"2.0","id":11,"method":"no/such"}"#);
    line(br#"{"jsonrpc":"2.0","id":12,"method":"tools/list"}"#);
    line(br#"{"jsonrpc":"2.0","id":13,"method":"resources/list","params":{}}"#);
    line(br#"{"jsonrpc":"2.0","id":14,"method":"tools/call","params":"x"}"#);
    line(br#"{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"nope"}}"#);
    line(br#"{"jsonrpc":"2.0","id":16,"method":"tools/call","params":{"name":"taskr_inbox","arguments":[1]}}"#);
    line(
        br#"{"jsonrpc":"2.0","id":17,"method":"resources/read","params":{"uri":"taskr://other"}}"#,
    );
    line(br#"{"jsonrpc":"2.0","id":18,"method":"tools/call","params":{"name":"taskr_inbox","arguments":{}}}"#);
    line(ping(json!("last")).as_bytes());
    line(b"{\"jsonrpc\":\"2.0\",\"id\":19,\"method\":\"ping\"}\r");
    // A non-string cursor is invalid params; the server keeps serving after each.
    line(br#"{"jsonrpc":"2.0","id":20,"method":"tools/list","params":{"cursor":42}}"#);
    line(ping(json!(21)).as_bytes());
    line(br#"{"jsonrpc":"2.0","id":22,"method":"resources/list","params":{"cursor":[]}}"#);
    line(ping(json!(23)).as_bytes());
    line(br#"{"jsonrpc":"2.0","id":24,"method":"tools/list","params":{"cursor":"opaque"}}"#);
    let (code_, lines, err) = serve(&h, &env, &[], &input);
    assert_eq!(code_, Some(0), "EOF exits 0: {err}");
    let want: &[(Value, Option<i64>)] = &[
        (json!(1), Some(-32600)),
        (json!(2), None),
        (Value::Null, Some(-32700)),
        (Value::Null, Some(-32700)),
        (Value::Null, Some(-32700)),
        (Value::Null, Some(-32600)),
        (Value::Null, Some(-32600)),
        (json!(4), Some(-32600)),
        (json!(5), Some(-32600)),
        (Value::Null, Some(-32600)),
        (Value::Null, Some(-32600)),
        (Value::Null, Some(-32600)),
        (json!(6), Some(-32600)),
        (json!(7), Some(-32602)),
        (json!("s-8"), None),
        (json!(9), Some(-32600)),
        (json!(10), Some(-32600)),
        (json!(11), Some(-32601)),
        (json!(12), None),
        (json!(13), None),
        (json!(14), Some(-32602)),
        (json!(15), Some(-32602)),
        (json!(16), Some(-32602)),
        (json!(17), Some(-32602)),
        (json!(18), None),
        (json!("last"), None),
        (json!(19), None),
        (json!(20), Some(-32602)),
        (json!(21), None),
        (json!(22), Some(-32602)),
        (json!(23), None),
        (json!(24), None),
    ];
    assert_eq!(lines.len(), want.len(), "{lines:#?}");
    for (l, (id, c)) in lines.iter().zip(want) {
        assert!(l["jsonrpc"] == "2.0" && l["id"] == *id, "{l} want id {id}");
        match c {
            Some(c) => assert_eq!(l["error"]["code"], *c, "{l}"),
            None => assert!(l.get("result").is_some() && l.get("error").is_none(), "{l}"),
        }
    }
    assert_eq!(lines[1]["result"], json!({}));
    let init = &lines[14]["result"];
    assert_eq!(init["protocolVersion"], "2025-06-18", "only 2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "taskr");
    assert_eq!(
        init["capabilities"],
        json!({"tools":{"listChanged":false},"resources":{"subscribe":false,"listChanged":false}})
    );
    let instructions = init["instructions"].as_str().unwrap();
    for needle in [
        "not an ack",
        "inspect and reconcile (taskr log, consumer show, delivery ls) before retrying",
        "identical call with its retry_handle",
        "never force a new key",
    ] {
        assert!(instructions.contains(needle), "{needle}");
    }
    let tools = lines[18]["result"]["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "taskr_task",
            "taskr_inbox",
            "taskr_received",
            "taskr_ack",
            "taskr_note",
            "taskr_ask",
            "taskr_ready",
            "taskr_done",
            "taskr_fail"
        ]
    );
    for t in tools {
        assert_eq!(t["inputSchema"]["additionalProperties"], false, "{t}");
    }
    let uris: Vec<_> = lines[19]["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert_eq!(uris, ["taskr://self/task", "taskr://self/inbox"]);
    assert_eq!(lines[24]["result"]["isError"], false);
    assert_eq!(lines[31]["result"]["tools"], lines[18]["result"]["tools"]);
    assert_eq!(snapshot(&h), before, "nothing ran a write");
}

#[test]
fn startup_identity_is_fixed_and_bad_combinations_exit_2() {
    let h = Harness::new();
    let (w, l, env) = h.lane("claude", "implementer");
    let (ws, ls, r) = (w.to_string(), l.to_string(), parent(&h, w));
    let only = |pairs: &[(&str, &str)]| -> Env {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let before = snapshot(&h);
    for (env, args) in [
        (only(&[]), vec!["mcp"]),
        (env.clone(), vec!["mcp", "--session", "0"]),
        (env.clone(), vec!["mcp", "--as", &r]),
        (env.clone(), vec!["mcp", "--as", "0"]),
        (only(&[]), vec!["mcp", "--as", &r]),
        (only(&[]), vec!["mcp", "--session", "0"]),
        (only(&[]), vec!["mcp", "--as", &r, "--session", "-1"]),
        (only(&[("TASKR_LAUNCH", &ls)]), vec!["mcp"]),
        (
            only(&[("TASKR_TASK", "x"), ("TASKR_LAUNCH", &ls)]),
            vec!["mcp"],
        ),
        (
            only(&[("TASKR_TASK", &ws), ("TASKR_LAUNCH", "x")]),
            vec!["mcp"],
        ),
        (
            only(&[("TASKR_TASK", &ws)]),
            vec!["mcp", "--as", &r, "--session", "0"],
        ),
        (
            only(&[]),
            vec!["mcp", "--as", &r, "--session", "0", "extra"],
        ),
        (
            only(&[]),
            vec!["mcp", "--as", &r, "--as", &r, "--session", "0"],
        ),
        (
            only(&[]),
            vec![
                "--request-key",
                "mcp-start-0001",
                "mcp",
                "--as",
                &r,
                "--session",
                "0",
            ],
        ),
    ] {
        let out = h.output(&env, &args, format!("{INIT}\n").as_bytes());
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {stdout}");
        assert!(
            stdout.starts_with("x1 2 ") && stdout.lines().count() == 1,
            "{args:?} served: {stdout}"
        );
    }
    assert_eq!(snapshot(&h), before);
    // The worker's own id and a root's own TASKR_TASK are accepted.
    session(&h, &env, &["--as", &ws], &[("ping", json!({}))]);
    session(
        &h,
        &only(&[("TASKR_TASK", &r)]),
        &strs(&root_args(&r, "0")),
        &[("ping", json!({}))],
    );
    for args in [vec!["mcp", "--help"], vec!["help", "mcp"]] {
        let out = stdout(&h, &[], &args);
        assert!(out.contains("mcp --as ID --session GEN"), "{out}");
    }

    // A client host: starting and listing runs no RPC and opens no ledger.
    let hub = std::net::TcpListener::bind("[::1]:0").unwrap();
    hub.set_nonblocking(true).unwrap();
    let state = h.home.join("client/.local/state/taskr");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("server.url"),
        format!("http://[::1]:{}\n", hub.local_addr().unwrap().port()),
    )
    .unwrap();
    let client = only(&[
        ("HOME", h.home.join("client").to_str().unwrap()),
        ("TASKR_DB", ""),
        ("TASKR_TASK", "7"),
        ("TASKR_LAUNCH", "9"),
    ]);
    session(
        &h,
        &client,
        &[],
        &[
            ("tools/list", json!({})),
            ("resources/list", json!({})),
            ("ping", json!({})),
        ],
    );
    assert_eq!(
        hub.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "the server itself made an RPC"
    );
    let files: Vec<_> = std::fs::read_dir(&state)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(files, ["server.url"], "the server opened a ledger or spool");
}

#[test]
fn tools_take_no_authority_and_invalid_params_run_nothing() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let r = parent(&h, w);
    let report = h.home.join("report.md");
    let report = report.to_str().unwrap();
    let valid = [
        ("taskr_task", json!({})),
        ("taskr_inbox", json!({})),
        ("taskr_received", json!({"attempt":1})),
        ("taskr_ack", json!({"event":1,"fence":1})),
        ("taskr_note", json!({"text":"x"})),
        ("taskr_ask", json!({"text":"x"})),
        ("taskr_ready", json!({"summary":"x","report_path":report})),
        ("taskr_done", json!({"summary":"x"})),
        ("taskr_fail", json!({"summary":"x"})),
    ];
    let authority = [
        "as",
        "task",
        "session",
        "recipient",
        "launch",
        "owner",
        "request_key",
    ];
    let mut calls = vec![];
    for (name, args) in &valid {
        for k in authority {
            let mut a = args.clone();
            a[k] = json!(1);
            calls.push(tool(name, a.clone()));
            a[k] = json!(r);
            calls.push(tool(name, a));
        }
    }
    for bad in [
        json!({"event":1}),
        json!({"event":1,"fence":0}),
        json!({"event":"1","fence":1}),
        json!({"event":9_223_372_036_854_775_808_u64,"fence":1}),
        json!({"event":1.0,"fence":1}),
        json!({"event":-1,"fence":1}),
        json!({"fence":1}),
    ] {
        calls.push(tool("taskr_ack", bad));
    }
    for (name, bad) in [
        ("taskr_note", json!({})),
        ("taskr_note", json!({"text":5})),
        ("taskr_note", json!({"text":"nul\u{0}byte"})),
        (
            "taskr_note",
            json!({"text":"x","retry_handle":"00112233445566778899aabbccddeeff"}),
        ),
        (
            "taskr_ready",
            json!({"summary":"x","report_path":"relative/report.md"}),
        ),
        ("taskr_ready", json!({"summary":"x","report_path":7})),
        ("taskr_ask", json!({"text":"x","blocking":"yes"})),
        ("taskr_received", json!({"attempt":0})),
        ("taskr_inbox", json!({"limit":0})),
        ("taskr_inbox", json!({"limit":201})),
        ("taskr_task", json!({"limit":1})),
    ] {
        calls.push(tool(name, bad));
    }
    calls.push(("resources/read", json!({"uri":"taskr://self/task","as":r})));
    let before = snapshot(&h);
    for reply in session(&h, &env, &[], &calls) {
        assert_eq!(code(&reply), -32602, "{reply}");
    }
    assert_eq!(snapshot(&h), before, "an invalid call ran a child");

    // A root server has only the reads and the generation-checked writes.
    let root = root_args(&r, "0");
    let list = session(&h, &[], &strs(&root), &[("tools/list", json!({}))]);
    let names: Vec<_> = list[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        ["taskr_task", "taskr_inbox", "taskr_received", "taskr_ack"]
    );
    let mut calls: Vec<_> = valid[4..].iter().map(|(n, a)| tool(n, a.clone())).collect();
    calls.push(tool("taskr_ack", json!({"event":1})));
    calls.push(tool("taskr_received", json!({"attempt":1,"session":0})));
    calls.push(tool("taskr_ack", json!({"event":1,"fence":1,"as":w})));
    for reply in session(&h, &[], &strs(&root), &calls) {
        assert_eq!(code(&reply), -32602, "{reply}");
    }
    assert_eq!(snapshot(&h), before);
}

#[test]
fn literal_text_never_becomes_a_flag_or_recipient() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let (ws, r) = (w.to_string(), parent(&h, w));
    let report = h.home.join("report.md");
    std::fs::write(&report, "synthetic report\n").unwrap();
    let texts = [
        "--owner",
        "--help",
        "--json=false",
        "--",
        "-h",
        "a b\nc  d\t'\"$x",
    ];
    let mut calls: Vec<_> = texts
        .iter()
        .map(|t| tool("taskr_note", json!({"text":t})))
        .collect();
    calls.push(tool("taskr_ask", json!({"text":"--owner","blocking":true})));
    calls.push(tool("taskr_ask", json!({"text":"--blocking"})));
    calls.push(tool(
        "taskr_ready",
        json!({"summary":"--help","report_path":report}),
    ));
    calls.push(tool("taskr_done", json!({"summary":"--"})));
    let replies = session(&h, &env, &[], &calls);
    for reply in &replies {
        let r = &reply["result"];
        assert!(
            outcome(r) == "committed" && r["isError"] == false,
            "{reply}"
        );
        assert!(
            r["structuredContent"].get("retry_handle").is_none(),
            "local mode: {r}"
        );
        let child: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(child["event_id"].is_i64(), "{r}");
    }
    let rows = |sql: &str| -> Vec<String> {
        let c = h.conn();
        let mut s = c.prepare(sql).unwrap();
        s.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        rows(&format!(
            "select summary from events where task_id={ws} and kind='note' order by id"
        )),
        texts
    );
    assert_eq!(
        rows(&format!(
            "select summary||'|'||recipient_task_id||'|'||json_extract(data,'$.blocking')||'|'||json_extract(data,'$.owner') from events where task_id={ws} and kind='ask' order by id"
        )),
        [format!("--owner|{r}|1|0"), format!("--blocking|{r}|0|0")]
    );
    assert_eq!(
        rows(&format!(
            "select summary from events where task_id={ws} and kind in('ready','done') order by id"
        )),
        ["--help", "--"]
    );
    assert_eq!(
        text(
            &h,
            "select count(*)||'' from events where json_extract(data,'$.owner')=1"
        ),
        "0"
    );
    assert_eq!(
        text(&h, &format!("select status from tasks where id={ws}")),
        "done"
    );
    // The ready report is captured as the CLI captures it.
    assert_eq!(
        text(
            &h,
            "select b.body from documents d join doc_blobs b on b.sha256=d.sha256 where d.kind='report'"
        ),
        "synthetic report\n"
    );
}

#[test]
fn reads_never_consume_and_inbox_is_a_raw_snapshot() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let (ws, r) = (w.to_string(), parent(&h, w));
    let q1 = id(&h.ok(&env, &["ask", "first?"]), "ask_id");
    let p1 = insert_event(
        &h,
        &ws,
        &r,
        "pr",
        "PR synthetic/repo#31 checks green",
        r#"{"pr":"synthetic/repo#31","sub":"checks_green"}"#,
    );
    let p2 = insert_event(
        &h,
        &ws,
        &r,
        "pr",
        "PR synthetic/repo#31 merged",
        r#"{"pr":"synthetic/repo#31","sub":"merged"}"#,
    );
    let nr = insert_event(
        &h,
        &ws,
        &r,
        "prompt_outcome",
        "no_receipt",
        r#"{"outcome":"no_receipt","window_ms":1}"#,
    );
    let root = root_args(&r, "0");
    let reads = || {
        session(
            &h,
            &[],
            &strs(&root),
            &[
                ("resources/read", json!({"uri":"taskr://self/task"})),
                ("resources/read", json!({"uri":"taskr://self/inbox"})),
                tool("taskr_task", json!({})),
                tool("taskr_inbox", json!({})),
                tool("taskr_inbox", json!({"limit":2})),
            ],
        )
    };
    let copy = |name: &str| {
        let path = h.home.join(name);
        h.conn()
            .execute("vacuum into ?", [path.to_str().unwrap()])
            .unwrap();
        vec![("TASKR_DB".to_string(), path.to_str().unwrap().to_string())]
    };

    let twin = copy("twin-a.db");
    let before = snapshot(&h);
    for _ in 0..3 {
        let replies = reads();
        let inbox: Value = serde_json::from_str(
            replies[1]["result"]["contents"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let ids: Vec<String> = inbox["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| id(e, "id"))
            .collect();
        // Raw: the superseded PR event and the no_receipt row are listed, oldest first.
        assert_eq!(
            ids,
            [q1.clone(), p1.clone(), p2.clone(), nr.clone()],
            "{inbox}"
        );
        assert!(
            inbox["acked_event_id"] == 0
                && inbox["pending_event_id"].is_null()
                && inbox.get("more").is_none()
        );
        assert_eq!(
            replies[1]["result"]["contents"][0]["mimeType"],
            "application/json"
        );
        let cut = &replies[4]["result"]["structuredContent"];
        assert!(
            cut["events"].as_array().unwrap().len() == 2 && cut["more"] == true,
            "{cut}"
        );
        let task = &replies[2]["result"]["structuredContent"];
        let status = task["status"].as_array().unwrap();
        assert!(
            status.len() == 3
                && id(&status[0], "id") == r
                && status[1]["id"] == w
                && status[2]["record"] == "daemon"
                && id(&task["consumer"], "task_id") == r,
            "{task}"
        );
        let resource: Value = serde_json::from_str(
            replies[0]["result"]["contents"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resource["status"].as_array().unwrap().len(), 3);
        stdout(&h, &[], &["inbox", "--as", &r]);
    }
    assert_eq!(snapshot(&h), before, "reads changed the ledger");
    let wait = ["wait", "--as", &r, "--timeout", "0"];
    let next = stdout(&h, &[], &wait);
    assert_eq!(
        next,
        stdout(&h, &twin, &wait),
        "reads changed the next wait"
    );
    assert_eq!(waited(&next), q1);
    let pending: Value =
        serde_json::from_str(&stdout(&h, &[], &["--json", "inbox", "--as", &r])).unwrap();
    assert_eq!(pending["pending_event_id"].to_string(), q1);
    assert_eq!(pending["events"][0]["id"].to_string(), q1);

    // Under a live lease: reads change nothing, a plain wait is refused as before, and
    // the holder's fenced wait returns what it would have without the reads.
    let mut take = vec![
        "consumer", "take", "--holder", "mcp-test", "--ttl", "600000",
    ];
    take.extend(strs(&root));
    stdout(&h, &[], &take);
    let twin = copy("twin-b.db");
    let before = snapshot(&h);
    reads();
    stdout(&h, &[], &["inbox", "--as", &r, "--limit", "1"]);
    assert_eq!(snapshot(&h), before);
    rejected(&h, &[], &wait, 6);
    let mut fenced = vec!["wait", "--fence", "1", "--timeout", "0"];
    fenced.extend(strs(&root));
    let next = stdout(&h, &[], &fenced);
    assert_eq!(next, stdout(&h, &twin, &fenced));
    assert_eq!(waited(&next), q1, "a replay of the pending event");
    // The first raw row is not necessarily the next wait result: the superseded PR
    // event is coalesced by the wait.
    let ack = call(
        &h,
        &[],
        &strs(&root),
        "taskr_ack",
        json!({"event":q1.parse::<i64>().unwrap(),"fence":1}),
    );
    assert_eq!(outcome(&ack), "committed", "{ack}");
    let raw: Value =
        serde_json::from_str(&stdout(&h, &[], &["--json", "inbox", "--as", &r])).unwrap();
    assert_eq!(raw["events"][0]["id"].to_string(), p1);
    assert_eq!(waited(&stdout(&h, &[], &fenced)), p2);

    // The worker's own task read: one task and the daemon record.
    let mine = call(&h, &env, &[], "taskr_task", json!({}));
    let status = mine["structuredContent"]["status"].as_array().unwrap();
    assert!(
        status.len() == 2 && status[0]["id"] == w && status[1]["record"] == "daemon",
        "{mine}"
    );
    // Either child failing fails the whole read; no partial envelope.
    relaunch(&h, &ws, &env);
    let replies = session(
        &h,
        &env,
        &[],
        &[
            tool("taskr_task", json!({})),
            tool("taskr_inbox", json!({})),
            ("resources/read", json!({"uri":"taskr://self/task"})),
            ("resources/read", json!({"uri":"taskr://self/inbox"})),
        ],
    );
    for reply in &replies[..2] {
        let s = &reply["result"]["structuredContent"];
        assert!(
            reply["result"]["isError"] == true
                && s["outcome"] == "rejected"
                && s.get("status").is_none(),
            "{reply}"
        );
    }
    for reply in &replies[2..] {
        assert!(
            code(reply) == -32603 && reply["error"]["data"]["outcome"] == "rejected",
            "{reply}"
        );
    }
    let broken = vec![("TASKR_DB".to_string(), h.home.to_str().unwrap().to_string())];
    let failed = call(&h, &broken, &strs(&root), "taskr_task", json!({}));
    let s = &failed["structuredContent"];
    assert!(
        failed["isError"] == true
            && s["outcome"] == "error"
            && s["exit"] == 4
            && s.get("consumer").is_none(),
        "{failed}"
    );
}

#[test]
fn bound_writes_commit_and_stale_bindings_are_rejected() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let (ws, r) = (w.to_string(), parent(&h, w));
    let report = h.home.join("ready.md");
    std::fs::write(&report, "ready report\n").unwrap();
    let report = report.to_str().unwrap().to_string();
    let worker = |env: &Env, name: &str, a: Value| call(&h, env, &[], name, a);
    let n = |s: &str| s.parse::<i64>().unwrap();

    for (name, a) in [
        ("taskr_note", json!({"text":"progress"})),
        ("taskr_ask", json!({"text":"which way?","blocking":true})),
        (
            "taskr_ready",
            json!({"summary":"slice one","report_path":report}),
        ),
    ] {
        assert_eq!(outcome(&worker(&env, name, a)), "committed");
    }
    assert_eq!(
        text(
            &h,
            "select b.body from documents d join doc_blobs b on b.sha256=d.sha256"
        ),
        "ready report\n"
    );
    // A receipt for an attempt on an event addressed to the worker.
    let q = text(
        &h,
        &format!("select max(id)||'' from events where task_id={ws} and kind='ask'"),
    );
    let a = id(&h.ok(&[], &["answer", &q, "left"]), "answer_id");
    let att = id(
        &h.ok(&env, &["delivery", "new", &a, "--adapter", "mcp-test"]),
        "attempt_id",
    );
    let got = worker(&env, "taskr_received", json!({"attempt":n(&att)}));
    assert_eq!(outcome(&got), "committed", "{got}");
    assert_eq!(got["structuredContent"]["state"], "received");
    // A fenced ack by the lease holder commits.
    stdout(
        &h,
        &env,
        &["consumer", "take", "--holder", "mcp", "--ttl", "600000"],
    );
    assert_eq!(
        waited(&stdout(
            &h,
            &env,
            &["wait", "--fence", "1", "--timeout", "0"]
        )),
        a
    );
    let acked = worker(&env, "taskr_ack", json!({"event":n(&a),"fence":1}));
    assert_eq!(outcome(&acked), "committed", "{acked}");
    assert_eq!(
        text(
            &h,
            &format!("select acked_event_id||'' from tasks where id={ws}")
        ),
        a
    );

    // Takeover: the old fence is refused and nothing moves; the new holder acks.
    let q2 = id(&h.ok(&env, &["ask", "and now?"]), "ask_id");
    let a2 = id(&h.ok(&[], &["answer", &q2, "right"]), "answer_id");
    assert_eq!(
        waited(&stdout(
            &h,
            &env,
            &["wait", "--fence", "1", "--timeout", "0"]
        )),
        a2
    );
    stdout(
        &h,
        &env,
        &[
            "consumer", "take", "--holder", "next", "--ttl", "600000", "--force",
        ],
    );
    let before = snapshot(&h);
    let stale = worker(&env, "taskr_ack", json!({"event":n(&a2),"fence":1}));
    assert_eq!(outcome(&stale), "rejected", "{stale}");
    assert_eq!(snapshot(&h), before);
    // Another host's launch: refused, nothing moves.
    let l = env
        .iter()
        .find(|(k, _)| k == "TASKR_LAUNCH")
        .unwrap()
        .1
        .clone();
    h.conn()
        .execute("update launches set machine='host-x' where id=?", [&l])
        .unwrap();
    let before = snapshot(&h);
    let replies = session(
        &h,
        &env,
        &[],
        &worker_writes(&att, &a2, "2", &report)
            .into_iter()
            .map(|(name, a)| tool(name, a))
            .collect::<Vec<_>>(),
    );
    for reply in &replies {
        assert_eq!(outcome(&reply["result"]), "rejected", "{reply}");
    }
    assert_eq!(snapshot(&h), before);
    h.conn()
        .execute("update launches set machine=null where id=?", [&l])
        .unwrap();
    assert_eq!(
        outcome(&worker(
            &env,
            "taskr_ack",
            json!({"event":n(&a2),"fence":2})
        )),
        "committed"
    );

    // After a relaunch every write of the old binding is refused and changes nothing.
    let (_, env2) = relaunch(&h, &ws, &env);
    let q3 = id(&h.ok(&env2, &["ask", "third?"]), "ask_id");
    let a3 = id(&h.ok(&[], &["answer", &q3, "up"]), "answer_id");
    let before = snapshot(&h);
    let replies = session(
        &h,
        &env,
        &[],
        &worker_writes(&att, &a3, "2", &report)
            .into_iter()
            .map(|(name, a)| tool(name, a))
            .collect::<Vec<_>>(),
    );
    for reply in &replies {
        assert_eq!(outcome(&reply["result"]), "rejected", "{reply}");
        assert_eq!(reply["result"]["isError"], true);
    }
    assert_eq!(snapshot(&h), before);
    assert_eq!(
        outcome(&worker(&env2, "taskr_note", json!({"text":"current"}))),
        "committed"
    );

    // Root: a receipt and a fenced ack under GEN 0 commit; after adopt GEN 0 is refused.
    let root = root_args(&r, "0");
    let rq = id(&h.ok(&env2, &["ask", "root?"]), "ask_id");
    let mut new = vec!["--json", "delivery", "new", &rq, "--adapter", "mcp-test"];
    new.extend(strs(&root));
    let ra: Value = serde_json::from_str(&stdout(&h, &[], &new)).unwrap();
    let ra = id(&ra, "attempt_id");
    assert_eq!(
        outcome(&call(
            &h,
            &[],
            &strs(&root),
            "taskr_received",
            json!({"attempt":n(&ra)})
        )),
        "committed"
    );
    let mut take = vec!["consumer", "take", "--holder", "root", "--ttl", "600000"];
    take.extend(strs(&root));
    stdout(&h, &[], &take);
    let mut fenced = vec!["wait", "--fence", "1", "--timeout", "0"];
    fenced.extend(strs(&root));
    let e = waited(&stdout(&h, &[], &fenced));
    assert_eq!(
        outcome(&call(
            &h,
            &[],
            &strs(&root),
            "taskr_ack",
            json!({"event":n(&e),"fence":1})
        )),
        "committed"
    );
    let e = waited(&stdout(&h, &[], &fenced));
    let (_, _, err) = run(&h, &[], &["adopt", &r, "--pane", "w1:p7"]);
    let generation = err.trim_end().rsplit_once("event ").unwrap().1.to_string();
    let before = snapshot(&h);
    let replies = session(
        &h,
        &[],
        &strs(&root),
        &[
            tool("taskr_received", json!({"attempt":n(&ra)})),
            tool("taskr_ack", json!({"event":n(&e),"fence":1})),
        ],
    );
    for reply in &replies {
        assert_eq!(outcome(&reply["result"]), "rejected", "{reply}");
    }
    assert_eq!(snapshot(&h), before);
    // The current generation without the live fence is refused too (the lease died with GEN 0).
    let fresh = root_args(&r, &generation);
    assert_eq!(
        outcome(&call(
            &h,
            &[],
            &strs(&fresh),
            "taskr_ack",
            json!({"event":n(&e),"fence":1})
        )),
        "rejected"
    );

    // A closed task refuses every write.
    h.ok(&[], &["close", &ws]);
    let before = snapshot(&h);
    for reply in session(
        &h,
        &env2,
        &[],
        &worker_writes(&att, &a3, "2", &report)
            .into_iter()
            .map(|(name, a)| tool(name, a))
            .collect::<Vec<_>>(),
    ) {
        assert_eq!(outcome(&reply["result"]), "rejected", "{reply}");
    }
    assert_eq!(snapshot(&h), before);
    // done and fail commit on a current lane.
    let (w2, _, env3) = h.lane("claude", "implementer");
    assert_eq!(
        outcome(&worker(&env3, "taskr_fail", json!({"summary":"blocked"}))),
        "committed"
    );
    assert_eq!(
        text(&h, &format!("select status from tasks where id={w2}")),
        "failed"
    );
}

#[test]
fn inbox_cli_identity_limits_flags_and_help() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let (ws, r) = (w.to_string(), parent(&h, w));
    let a = id(&h.ok(&env, &["ask", "q?"]), "ask_id");
    let a = id(&h.ok(&[], &["answer", &a, "yes"]), "answer_id");
    let mine: Value = serde_json::from_str(&stdout(&h, &env, &["--json", "inbox"])).unwrap();
    assert_eq!(mine["events"][0]["id"].to_string(), a);
    assert_eq!(mine["events"][0]["record"], "event");
    let compact = stdout(&h, &env, &["inbox"]);
    assert!(
        compact.starts_with("j1 {") && compact.lines().count() == 1,
        "{compact}"
    );
    stdout(&h, &[], &["inbox", "--as", &r, "--limit", "200"]);
    stdout(&h, &[], &["inbox", "--as", &r, "--limit=1"]);
    let before = snapshot(&h);
    for (env, args, want) in [
        (vec![], vec!["inbox"], 2),
        (vec![], vec!["inbox", "--as", &r, "--limit", "0"], 2),
        (vec![], vec!["inbox", "--as", &r, "--limit", "201"], 2),
        (
            vec![],
            vec!["inbox", "--as", &r, "--limit", "1", "--limit", "2"],
            2,
        ),
        (vec![], vec!["inbox", "--as", &r, "--as=1"], 2),
        (vec![], vec!["inbox", "--as", &r, "-h", "--help=false"], 2),
        (vec![], vec!["inbox", "--as", &r, "extra"], 2),
        (vec![], vec!["inbox", "--as", "-3"], 2),
        // A worker task is not a root; another task is not the caller.
        (vec![], vec!["inbox", "--as", &ws], 6),
        (env.clone(), vec!["inbox", "--as", &r], 6),
    ] {
        rejected(&h, &env, &args, want);
    }
    assert_eq!(snapshot(&h), before);
    relaunch(&h, &ws, &env);
    rejected(&h, &env, &["inbox"], 6);
    for args in [vec!["inbox", "--help"], vec!["help", "inbox"]] {
        let out = stdout(&h, &[], &args);
        assert!(out.contains("never offers, receipts or acks"), "{out}");
    }
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
/// Forward RPCs to the hub. Request number `cut` (0-based) is handled per `mode`:
/// "reply" forwards it and drops the reply (the hub committed), "request" drops it
/// unsent. Every forwarded request body is sent back for inspection.
#[cfg(feature = "contract")]
fn cut_proxy(
    hub_port: u16,
    cut: usize,
    mode: &'static str,
) -> (u16, std::sync::mpsc::Receiver<Value>) {
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
            if n == cut && mode == "request" {
                continue;
            }
            let mut hub =
                std::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, hub_port)).unwrap();
            hub.write_all(head.as_bytes()).unwrap();
            hub.write_all(&body).unwrap();
            let mut reply = Vec::new();
            hub.read_to_end(&mut reply).unwrap();
            let _ = send.send(serde_json::from_slice(&body).unwrap());
            if n != cut {
                let _ = client.write_all(&reply);
            }
        }
    });
    (port, seen)
}
/// One RPC straight to the hub, as the authenticated caller host-a.
#[cfg(feature = "contract")]
fn post(port: u16, req: &Value) -> Value {
    use std::io::{Read, Write};
    let body = req.to_string();
    let mut stream = std::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
    write!(stream, "POST /api/rpc HTTP/1.1\r\nHost: [::1]:{port}\r\nX-Taskr-RPC: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap()
}
/// No client retry window: a lost or refused reply is final at once.
#[cfg(feature = "contract")]
fn fast(env: &Env) -> Env {
    let mut env = env.clone();
    env.push(("TASKR_CONTRACT_RETRY_MS".into(), "0".into()));
    env
}
/// The CLI request key a server bound to (task, launch) uses for retry handle `handle`.
#[cfg(feature = "contract")]
fn worker_key(task: &str, launch: &str, handle: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest =
        Sha256::digest(format!("taskr-mcp\nworker\n{task}\n{launch}\n{handle}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("mcp-{}", &hex[..40])
}

#[cfg(feature = "contract")]
#[test]
fn client_mode_keeps_queued_unknown_and_replay_apart() {
    let h = Harness::new();
    let (_hub, url, client, server) = start_hub(&h);
    let hub_port: u16 = url.trim().rsplit_once(':').unwrap().1.parse().unwrap();
    let r = id(
        &h.ok(&client, &["new", "remote", "--role", "orchestrator"]),
        "task_id",
    );
    let lane = |name: &str| -> (String, String, Env) {
        let w = id(
            &h.ok(
                &client,
                &[
                    "new",
                    name,
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
        let mut env = client.clone();
        env.extend([
            ("TASKR_TASK".into(), w.clone()),
            ("TASKR_LAUNCH".into(), l.clone()),
        ]);
        (w, l, env)
    };
    let (w, l, worker) = lane("impl");
    let notes = |t: &str| {
        h.conn()
            .query_row(
                "select count(*) from events where kind='note' and summary=?",
                [t],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
    };
    let note = |env: &Env, t: &str, handle: Option<&str>| {
        let mut a = json!({"text":t});
        if let Some(handle) = handle {
            a["retry_handle"] = json!(handle);
        }
        call(&h, env, &[], "taskr_note", a)
    };

    // A committed write over RPC carries a retry handle; its key is bound to the launch.
    let first = note(&worker, "first", None);
    assert_eq!(outcome(&first), "committed", "{first}");
    let handle = first["structuredContent"]["retry_handle"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(handle.len(), 32);
    let key = worker_key(&w, &l, &handle);
    assert_eq!(
        h.conn()
            .query_row("select state from requests where key=?", [&key], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
        "done"
    );
    // The identical call with its handle replays the stored result: no second event.
    let again = note(&worker, "first", Some(&handle));
    assert_eq!(outcome(&again), "committed");
    assert_eq!(
        again["structuredContent"]["event_id"],
        first["structuredContent"]["event_id"]
    );
    assert_eq!(notes("first"), 1);
    // In one server, a handle cannot name another call; after a restart the hub
    // refuses its key for other arguments. Nothing is written either way.
    let replies = session(
        &h,
        &worker,
        &[],
        &[
            tool("taskr_note", json!({"text":"first"})),
            tool(
                "taskr_note",
                json!({"text":"other","retry_handle":"0123456789abcdef0123456789abcdef"}),
            ),
        ],
    );
    let issued = replies[0]["result"]["structuredContent"]["retry_handle"]
        .as_str()
        .unwrap()
        .to_string();
    let replies2 = session(
        &h,
        &worker,
        &[],
        &[tool(
            "taskr_note",
            json!({"text":"other","retry_handle":handle}),
        )],
    );
    assert_eq!(outcome(&replies[1]["result"]), "committed");
    assert_eq!(
        outcome(&replies2[0]["result"]),
        "rejected",
        "{}",
        replies2[0]
    );
    let mut inside = format!("{INIT}\n{READY}\n");
    for (i, t) in ["first", "changed"].iter().enumerate() {
        inside += &json!({"jsonrpc":"2.0","id":i+1,"method":"tools/call","params":{"name":"taskr_note","arguments":{"text":t,"retry_handle":issued}}}).to_string();
        inside.push('\n');
    }
    let (_, lines, _) = serve(&h, &worker, &[], inside.as_bytes());
    assert_eq!(outcome(&lines[1]["result"]), "committed", "{}", lines[1]);
    assert_eq!(
        code(&lines[2]),
        -32602,
        "a handle names one call: {}",
        lines[2]
    );
    assert_eq!(notes("other"), 1);
    assert_eq!(notes("changed"), 0);

    // The same text and handle on another task is a fresh write under its own key.
    let (w2, l2, worker2) = lane("impl-two");
    let other = note(&worker2, "first", Some(&handle));
    assert_eq!(outcome(&other), "committed");
    assert_ne!(
        other["structuredContent"]["event_id"],
        first["structuredContent"]["event_id"]
    );
    assert_eq!(notes("first"), 3);
    assert_ne!(worker_key(&w2, &l2, &handle), key);

    // After a relaunch: the old binding's identical call is a historical replay (never
    // proof the binding is live); a fresh old-binding call is refused; the new launch
    // with the same handle is a fresh write under a new key.
    let relaunched = id(
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
    let mut current = worker.clone();
    current.retain(|(k, _)| k != "TASKR_LAUNCH");
    current.push(("TASKR_LAUNCH".into(), relaunched.clone()));
    let historical = note(&worker, "first", Some(&handle));
    assert_eq!(outcome(&historical), "committed");
    assert_eq!(
        historical["structuredContent"]["event_id"],
        first["structuredContent"]["event_id"]
    );
    assert_eq!(outcome(&note(&worker, "stale fresh", None)), "rejected");
    assert_eq!(notes("stale fresh"), 0);
    let renewed = note(&current, "first", Some(&handle));
    assert_eq!(outcome(&renewed), "committed");
    assert_eq!(notes("first"), 4);

    // Unreachable hub, spoolable write: queued with its key, not committed, not retried.
    let dead = std::net::TcpListener::bind("[::1]:0").unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);
    std::fs::write(&server, format!("http://[::1]:{dead_port}\n")).unwrap();
    let queued = note(&current, "queued note", None);
    let s = &queued["structuredContent"];
    let qh = s["retry_handle"].as_str().unwrap();
    assert!(
        outcome(&queued) == "queued"
            && queued["isError"] == false
            && s["request_key"] == worker_key(&w, &relaunched, qh),
        "{queued}"
    );
    assert_eq!(notes("queued note"), 0);
    // A queued ready keeps the report captured when queued, even if the file changes.
    let report = h.home.join("queued.md");
    std::fs::write(&report, "captured at queue time\n").unwrap();
    let ready = call(
        &h,
        &current,
        &[],
        "taskr_ready",
        json!({"summary":"queued ready","report_path":report}),
    );
    assert_eq!(outcome(&ready), "queued", "{ready}");
    std::fs::write(&report, "changed after queueing\n").unwrap();
    std::fs::write(&server, &url).unwrap();
    stdout(&h, &client, &["spool", "send"]);
    assert_eq!(notes("queued note"), 1);
    let body = |event: &str| {
        text(
            &h,
            &format!(
                "select b.body from documents d join doc_blobs b on b.sha256=d.sha256 join events e on e.id=d.event_id where e.summary='{event}'"
            ),
        )
    };
    assert_eq!(body("queued ready"), "captured at queue time\n");

    // A stored write whose reply is lost: transport (unconfirmed, the hub committed),
    // with the CLI's own retry line; the identical call with the handle replays once.
    let (proxy, seen) = cut_proxy(hub_port, 0, "reply");
    std::fs::write(&server, format!("http://[::1]:{proxy}\n")).unwrap();
    let lost = call(
        &h,
        &fast(&current),
        &[],
        "taskr_ask",
        json!({"text":"lost reply?"}),
    );
    let s = &lost["structuredContent"];
    let lh = s["retry_handle"].as_str().unwrap().to_string();
    let lk = worker_key(&w, &relaunched, &lh);
    assert!(
        outcome(&lost) == "transport"
            && lost["isError"] == true
            && s["error"].as_str().unwrap().contains(&format!(
                "retry with: taskr --request-key {lk} --json ask -- 'lost reply?'"
            ))
            && s["stderr"]
                .as_str()
                .unwrap()
                .contains(&format!("retry with: taskr --request-key {lk}")),
        "{lost}"
    );
    let asks = || {
        h.count(
            "select count(*) from events where kind='ask' and summary='lost reply?' and ?",
            1,
        )
    };
    assert_eq!(asks(), 1, "the hub committed before the reply was lost");
    let retried = call(
        &h,
        &current,
        &[],
        "taskr_ask",
        json!({"text":"lost reply?","retry_handle":lh}),
    );
    assert_eq!(outcome(&retried), "committed", "{retried}");
    assert_eq!(asks(), 1);
    let sent: Vec<Value> = seen.try_iter().collect();
    assert!(
        sent.len() == 2 && sent.iter().all(|s| s["request_key"] == lk.as_str()),
        "{sent:?}"
    );
    std::fs::write(&server, &url).unwrap();

    // Stored but still running on the hub: unknown, no retry line invented, nothing new.
    let rh = "00112233445566778899aabbccddeeff";
    let rk = worker_key(&w, &relaunched, rh);
    let argv: Vec<String> = ["--json", "ask", "--", "still running?"]
        .map(String::from)
        .to_vec();
    h.conn()
        .execute(
            "insert into requests(key,machine,argv_sha,state,created_at) values(?,'host-a',?,'running',?)",
            [rk, taskr_core::request_hash(Some(&argv), None).unwrap(), taskr_core::store::now()],
        )
        .unwrap();
    let before = snapshot(&h);
    let running = call(
        &h,
        &fast(&current),
        &[],
        "taskr_ask",
        json!({"text":"still running?","retry_handle":rh}),
    );
    let s = &running["structuredContent"];
    assert!(
        outcome(&running) == "unknown"
            && s["exit"] == 5
            && s["retry_handle"] == rh
            && s["error"]
                .as_str()
                .unwrap()
                .contains("outcome unknown (still running")
            && !s["error"].as_str().unwrap().contains("retry with"),
        "{running}"
    );
    assert_eq!(snapshot(&h), before);

    // The hub refuses another host's call: rejected, nothing written.
    std::fs::write(h.home.join("whois-host"), "host-b").unwrap();
    let before = snapshot(&h);
    assert_eq!(outcome(&note(&current, "wrong host", None)), "rejected");
    assert_eq!(snapshot(&h), before);
    std::fs::remove_file(h.home.join("whois-host")).unwrap();

    // ready over RPC: committed and captured; missing, binary and too-large reports and a
    // failed upload still commit the ledger write (capture is best effort).
    let captured = h.home.join("rpc.md");
    std::fs::write(&captured, "rpc report\n").unwrap();
    std::fs::write(h.home.join("binary.md"), b"a\0b").unwrap();
    std::fs::write(h.home.join("large.md"), vec![b'a'; (1 << 20) + 1]).unwrap();
    for (summary, file) in [
        ("rpc ready", "rpc.md"),
        ("missing ready", "missing.md"),
        ("binary ready", "binary.md"),
        ("large ready", "large.md"),
    ] {
        let path = h.home.join(file);
        let r = call(
            &h,
            &current,
            &[],
            "taskr_ready",
            json!({"summary":summary,"report_path":path}),
        );
        assert_eq!(outcome(&r), "committed", "{r}");
    }
    assert_eq!(body("rpc ready"), "rpc report\n");
    let doc = |event: &str| {
        text(
            &h,
            &format!(
                "select (select d.captured||'|'||ifnull(d.reason,'-') from documents d join events e on e.id=d.event_id where e.summary='{event}' order by d.id desc limit 1)"
            ),
        )
    };
    // The hub records a `client` placeholder; the client's upload adds the captured
    // version (nothing for a missing file).
    assert_eq!(doc("rpc ready"), "1|-");
    assert_eq!(doc("missing ready"), "0|client");
    assert_eq!(doc("binary ready"), "0|binary");
    assert_eq!(doc("large ready"), "0|too_large");
    let (proxy, _) = cut_proxy(hub_port, 1, "request");
    std::fs::write(&server, format!("http://[::1]:{proxy}\n")).unwrap();
    let unsent = call(
        &h,
        &current,
        &[],
        "taskr_ready",
        json!({"summary":"upload lost","report_path":captured}),
    );
    assert_eq!(outcome(&unsent), "committed", "{unsent}");
    assert_eq!(doc("upload lost"), "", "committed does not prove capture");
    std::fs::write(&server, &url).unwrap();

    // inbox over RPC: a fresh read (a reused key sees new data, nothing stored), never
    // queued; repeated flags and mcp itself are refused by the hub.
    let read = [
        "--request-key",
        "mcp-inbox-read-01",
        "--json",
        "inbox",
        "--as",
        &r,
    ];
    let before = stdout(&h, &client, &read);
    h.ok(&current, &["ask", "fresh?"]);
    assert_ne!(stdout(&h, &client, &read), before);
    assert_eq!(
        h.count(
            "select count(*) from requests where key='mcp-inbox-read-01' and ?",
            1
        ),
        0
    );
    let tool_read = call(&h, &current, &[], "taskr_inbox", json!({}));
    assert_eq!(outcome(&tool_read), "committed", "{tool_read}");
    for (argv, want) in [
        (
            json!(["--json", "inbox", "--as", r, "--limit", "1", "--limit", "2"]),
            "repeated RPC flag --limit",
        ),
        (
            json!(["--json", "inbox", "--as", r, "-h", "--help=false"]),
            "repeated RPC flag --help",
        ),
        (
            json!(["mcp", "--as", r, "--session", "0"]),
            "not available over RPC",
        ),
    ] {
        let rep = post(
            hub_port,
            &json!({"argv":argv,"request_key":"mcp-check-args-01","cwd":h.home,"env":{}}),
        );
        assert!(
            rep["exit"] == 2 && rep["stdout"].as_str().unwrap().contains(want),
            "{rep}"
        );
    }
    std::fs::write(&server, format!("http://[::1]:{dead_port}\n")).unwrap();
    let (code_, out, _) = run(&h, &fast(&client), &["inbox", "--as", &r]);
    assert!(code_ == Some(5) && !out.starts_with("qd1"), "{out}");
    std::fs::write(&server, &url).unwrap();
}

#[cfg(feature = "contract")]
#[test]
fn an_expired_fence_is_refused_without_a_cursor_change() {
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let ws = w.to_string();
    let at = |s: &str| -> Env {
        let mut e = env.clone();
        e.push(("TASKR_FROZEN_NOW".into(), format!("2026-01-01T00:00:0{s}Z")));
        e
    };
    let q = id(&h.ok(&env, &["ask", "q?"]), "ask_id");
    let a = id(&h.ok(&[], &["answer", &q, "a"]), "answer_id");
    stdout(
        &h,
        &at("0.000"),
        &["consumer", "take", "--holder", "mcp", "--ttl", "1000"],
    );
    assert_eq!(
        waited(&stdout(
            &h,
            &at("0.500"),
            &["wait", "--fence", "1", "--timeout", "0"]
        )),
        a
    );
    let before = snapshot(&h);
    let late = call(
        &h,
        &at("1.000"),
        &[],
        "taskr_ack",
        json!({"event":a.parse::<i64>().unwrap(),"fence":1}),
    );
    assert_eq!(outcome(&late), "rejected", "{late}");
    assert_eq!(snapshot(&h), before);
    assert_eq!(
        text(
            &h,
            &format!("select pending_event_id||'' from tasks where id={ws}")
        ),
        a
    );
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "macOS re-execs by path (hub::self_command), so the replaced-binary guarantee is Linux-only"
)]
fn a_replaced_binary_and_stdin_eof_finish_the_running_call() {
    use std::io::{BufRead, Write};
    let h = Harness::new();
    let (w, _, env) = h.lane("claude", "implementer");
    let copy = h.home.join("bin/taskr-copy");
    std::fs::copy(env!("CARGO_BIN_EXE_taskr"), &copy).unwrap();
    let base = h.command(&env);
    let mut cmd = std::process::Command::new(&copy);
    cmd.env_clear().current_dir(&h.home);
    for (k, v) in base.get_envs() {
        if let Some(v) = v {
            cmd.env(k, v);
        }
    }
    cmd.arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // A sibling test's fork can briefly hold the fresh copy open for writing.
    let mut tries = 0;
    let mut child = loop {
        match cmd.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 100 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            spawned => break spawned.unwrap(),
        }
    };
    let mut stdin = child.stdin.take().unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    writeln!(stdin, "{INIT}\n{READY}").unwrap();
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert!(line.contains("2025-06-18"), "{line}");
    // Replace the installed file: the server keeps running its own build.
    let fake = h.home.join("bin/taskr-fake");
    std::fs::write(&fake, "#!/bin/sh\necho not-taskr\n").unwrap();
    std::fs::rename(&fake, &copy).unwrap();
    // The call and EOF in one write: the server answers it, then exits 0.
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"taskr_note","arguments":{"text":"after replace"}}})
    )
    .unwrap();
    drop(stdin);
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut out, &mut rest).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    let reply: Value = serde_json::from_str(rest.trim()).unwrap();
    assert_eq!(outcome(&reply["result"]), "committed", "{reply}");
    assert_eq!(
        h.count(
            "select count(*) from events where kind='note' and summary='after replace' and task_id=?",
            w
        ),
        1
    );
}
