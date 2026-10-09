#![cfg(feature = "contract")]
#[allow(dead_code)]
#[path = "hook/support.rs"]
mod support;
use serde_json::{Value, json};
use std::{fs, time::Duration};
use support::*;
mod rpc {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        process::{Child, Stdio},
    };
    use taskr_core::db::params;
    struct Hub(Child);
    impl Drop for Hub {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn client_env(h: &Harness, env: &[(String, String)]) -> Env {
        let mut env = env.to_vec();
        env.extend([
            (
                "HOME".into(),
                h.home.join("client").to_str().unwrap().into(),
            ),
            ("TASKR_DB".into(), "".into()),
            ("NET_ID".into(), "host-a".into()),
            ("TASKR_CONTRACT_TAILNET".into(), "1".into()),
            ("TASKR_CONTRACT_ORACLE".into(), "1".into()),
        ]);
        env
    }
    fn read_request(stream: &mut TcpStream) -> (Vec<u8>, Value) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut headers = Vec::new();
        let mut len = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(!line.is_empty());
            headers.extend_from_slice(line.as_bytes());
            if line == "\r\n" {
                break;
            }
            if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = n.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        let value = serde_json::from_slice(&body).unwrap();
        headers.extend(body);
        (headers, value)
    }
    fn start_hub(h: &Harness) -> (Hub, u16) {
        let env = vec![
            ("NET_ID".into(), "hub".into()),
            ("TASKR_CONTRACT_TAILNET".into(), "1".into()),
            ("TASKR_CONTRACT_ORACLE".into(), "1".into()),
        ];
        let mut hub = Hub(h
            .command(&env)
            .arg("--contract-hub")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap());
        let (send, recv) = std::sync::mpsc::channel();
        let stdout = hub.0.stdout.take().unwrap();
        std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            send.send(line).unwrap();
        });
        let url = recv.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(url.starts_with("http://[::1]:"));
        let port = url.trim().rsplit_once(':').unwrap().1.parse().unwrap();
        (hub, port)
    }
    fn post(port: u16, req: &Value) -> Value {
        let body = req.to_string();
        let mut stream = TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(40)))
            .unwrap();
        write!(stream, "POST /api/rpc HTTP/1.1\r\nHost: [::1]:{port}\r\nX-Taskr-RPC: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap()
    }
    #[test]
    fn busy_child_exit_is_not_stored_and_same_key_runs_again() {
        let h = Harness::new();
        h.script("tailscale", include_str!("hook/tailscale.sh"));
        let root = h.ok(&[], &["new", "busy-root", "--role", "orchestrator"])["task_id"]
            .as_i64()
            .unwrap();
        h.conn()
            .execute("update tasks set machine='host-a' where id=?", [root])
            .unwrap();
        let (_hub, port) = start_hub(&h);
        let req = json!({"argv":["--json","note","busy-child","--as",root.to_string()],"cwd":h.home,"request_key":"busy-child-request"});
        let first_req = req.clone();
        let (send, recv) = std::sync::mpsc::channel();
        let first = std::thread::spawn(move || send.send(post(port, &first_req)).unwrap());
        let blocker = h.conn();
        blocker.busy_timeout(Duration::ZERO).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let claimed: bool = blocker.query_row("select exists(select 1 from requests where key='busy-child-request' and state='running')", [], |r| r.get(0)).unwrap();
            if claimed && blocker.execute_batch("begin immediate").is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "request was never claimed"
            );
            std::thread::yield_now();
        }
        assert_eq!(h.count("select count(*) from events where kind='note' and summary='busy-child' and task_id=?", root), 0);
        // Hold beyond both former 5 s waits; result storage must survive the child busy exit.
        assert!(matches!(
            recv.recv_timeout(Duration::from_secs(12)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        blocker.execute_batch("rollback").unwrap();
        let busy = recv.recv_timeout(Duration::from_secs(15)).unwrap();
        first.join().unwrap();
        assert_eq!(busy["exit"], 4, "{busy}");
        assert!(
            busy["stdout"]
                .as_str()
                .unwrap()
                .contains("database is locked")
        );
        assert_eq!(
            h.conn()
                .query_row(
                    "select count(*) from requests where key=?",
                    [req["request_key"].as_str().unwrap()],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        let delivered = post(port, &req);
        assert_eq!(delivered["exit"], 0, "{delivered}");
        assert_eq!(post(port, &req), delivered);
        assert_eq!(h.count("select count(*) from events where kind='note' and summary='busy-child' and task_id=?", root), 1);
    }
    #[test]
    fn busy_prompt_same_key_replays_without_second_delivery() {
        let h = Harness::new();
        h.script("tailscale", include_str!("hook/tailscale.sh"));
        // Pause the fake delivery so the blocker can hold the outcome transaction.
        h.script("herdr", "#!/bin/sh\ncase \"$1 $2\" in\n'agent prompt') echo delivered >> \"$HOME/deliveries\"; sleep 2; echo '{\"result\":{}}' ;;\n'agent list') cat \"$HOME/agents.json\" ;;\n'workspace list') echo '{\"result\":{\"workspaces\":[]}}' ;;\n'pane list') echo '{\"result\":{\"panes\":[]}}' ;;\n'agent read') exit 1 ;;\n*) echo '{\"result\":{}}' ;;\nesac\n");
        let (worker, _launch, _env) = h.lane("claude", "implementer");
        let (_hub, port) = start_hub(&h);
        let req = json!({"argv":["prompt",worker.to_string(),"--text","work","--receipt-timeout","0"],"cwd":h.home,"request_key":"busy-prompt-request"});
        let first_req = req.clone();
        let (send, recv) = std::sync::mpsc::channel();
        let first = std::thread::spawn(move || send.send(post(port, &first_req)).unwrap());
        let blocker = h.conn();
        blocker.busy_timeout(Duration::ZERO).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let begun: bool = blocker
                .query_row(
                    "select exists(select 1 from events where kind='prompt' and task_id=?)",
                    [worker],
                    |r| r.get(0),
                )
                .unwrap();
            if begun && blocker.execute_batch("begin immediate").is_ok() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "prompt never began");
            std::thread::yield_now();
        }
        assert!(matches!(
            recv.recv_timeout(Duration::from_secs(14)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        blocker.execute_batch("rollback").unwrap();
        let busy = recv.recv_timeout(Duration::from_secs(30)).unwrap();
        first.join().unwrap();
        assert_eq!(busy["exit"], 4, "{busy}");
        assert!(
            busy["stdout"]
                .as_str()
                .unwrap()
                .contains("database is locked")
        );
        let deliveries = || {
            fs::read_to_string(h.home.join("deliveries"))
                .unwrap_or_default()
                .lines()
                .count()
        };
        let prompts = || {
            h.count(
                "select count(*) from events where kind='prompt' and task_id=?",
                worker,
            )
        };
        assert_eq!(deliveries(), 1);
        assert_eq!(prompts(), 1);
        assert_eq!(h.conn().query_row("select count(*) from requests where key='busy-prompt-request' and state='done' and exit=4", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        assert_eq!(post(port, &req), busy);
        assert_eq!(
            deliveries(),
            1,
            "the same request key delivered the prompt twice"
        );
        assert_eq!(prompts(), 1);
    }
    #[test]
    fn spool_retries_busy_reply_then_delivers_and_refuses_other_database_error() {
        let h = Harness::new();
        h.script("tailscale", include_str!("hook/tailscale.sh"));
        let state = h.home.join("client/.local/state/taskr");
        fs::create_dir_all(&state).unwrap();
        // A closed scratch listener forces both foreground records into the spool.
        let listener = TcpListener::bind("[::1]:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        fs::write(state.join("server.url"), format!("http://[::1]:{port}\n")).unwrap();
        let client = client_env(&h, &[]);
        for text in ["busy-first", "other-error"] {
            assert_eq!(h.ok(&client, &["note", text, "--as", "1"])["queued"], true);
        }
        let listener = TcpListener::bind("[::1]:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        fs::write(state.join("server.url"), format!("http://[::1]:{port}\n")).unwrap();
        let proxy = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in [
                json!({"exit":4,"stdout":"x1 4 {\"err\":\"database is locked (5) (SQLITE_BUSY)\",\"k\":\"database\"}\n","stderr":""}),
                json!({"exit":0,"stdout":"{\"ok\":true}\n","stderr":""}),
                json!({"exit":4,"stdout":"{\"error\":\"disk I/O error (10)\",\"kind\":\"database\"}\n","stderr":""}),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream).1);
                let body = reply.to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        h.ok(&client, &["spool", "send"]);
        let held = h.ok(&client, &["spool", "ls"]);
        assert_eq!(held["queued"].as_array().unwrap().len(), 2);
        assert!(held["refused"].as_array().unwrap().is_empty());
        let queue = state.join("spool/queue");
        for entry in fs::read_dir(queue).unwrap() {
            let record: Value =
                serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).unwrap();
            assert!(record.get("stuck_since").is_none());
        }
        assert!(
            fs::read_to_string(state.join("daemon.log"))
                .unwrap()
                .contains("hub database is locked")
        );
        h.ok(&client, &["spool", "send"]);
        let delivered = h.ok(&client, &["spool", "ls"]);
        assert!(delivered["queued"].as_array().unwrap().is_empty());
        assert_eq!(delivered["refused"].as_array().unwrap().len(), 1);
        let requests = proxy.join().unwrap();
        assert_eq!(requests[0]["request_key"], requests[1]["request_key"]);
        assert_ne!(requests[1]["request_key"], requests[2]["request_key"]);
    }
    #[test]
    fn client_rpc_normalized_fields_and_host_check() {
        let h = Harness::new();
        h.script("tailscale", include_str!("hook/tailscale.sh"));
        let (_hub, port) = start_hub(&h);
        let listener = TcpListener::bind("[::1]:0").unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        fs::create_dir_all(h.home.join("client/.local/state/taskr")).unwrap();
        fs::write(
            h.home.join("client/.local/state/taskr/server.url"),
            format!("http://[::1]:{proxy_port}\n"),
        )
        .unwrap();
        let (send, requests) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        listener.set_nonblocking(true).unwrap();
        let proxy = std::thread::spawn(move || {
            while matches!(
                stop_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ) {
                let Ok((mut client, _)) = listener.accept() else {
                    std::thread::park_timeout(Duration::from_millis(1));
                    continue;
                };
                client.set_nonblocking(false).unwrap();
                let (raw, value) = read_request(&mut client);
                send.send(value).unwrap();
                let mut upstream =
                    TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
                upstream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let raw = String::from_utf8(raw).unwrap().replace(
                    &format!("Host: [::1]:{proxy_port}\r\n"),
                    &format!("Host: [::1]:{port}\r\n"),
                );
                upstream.write_all(raw.as_bytes()).unwrap();
                let mut reply = Vec::new();
                upstream.read_to_end(&mut reply).unwrap();
                client.write_all(&reply).unwrap();
            }
        });
        let client = client_env(&h, &[]);
        let top = h.ok(&client, &["new", "top", "--role", "orchestrator"])["task_id"]
            .as_i64()
            .unwrap();
        let w = h.ok(
            &client,
            &[
                "new",
                "worker",
                "--role",
                "sub-orchestrator",
                "--parent",
                &top.to_string(),
                "--cwd",
                h.home.to_str().unwrap(),
                "--pane",
                "w9:p1",
            ],
        )["task_id"]
            .as_i64()
            .unwrap();
        let l = h.ok(
            &client,
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
        )["launch_id"]
            .as_i64()
            .unwrap();
        let mut env = client.clone();
        env.extend([
            ("TASKR_TASK".into(), w.to_string()),
            ("TASKR_LAUNCH".into(), l.to_string()),
            ("HERDR_ENV".into(), "1".into()),
            ("HERDR_PANE_ID".into(), "w9:p1".into()),
        ]);
        h.hook(
            &env,
            "claude",
            "SessionStart",
            fixture("claude-session-start.json"),
        );
        let db = h.conn();
        let binding: (Option<String>, Option<String>, Option<String>) = db
            .query_row(
                "select session_ref,session_source,machine from launches where id=?",
                [l],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            binding.0.as_deref(),
            Some("claude-session"),
            "{binding:?}; requests: {:?}",
            requests.try_iter().collect::<Vec<_>>()
        );
        let at = taskr_core::store::now();
        db.execute("insert into events(task_id,launch_id,kind,summary,created_at) values(?,?,'prompt','stored prompt',?)",params![w,l,at]).unwrap();
        let a = db.last_insert_rowid();
        let secret = "LOCAL_PROMPT_MUST_NOT_CROSS_RPC";
        let mut payload = prompt_fixture("claude-user-prompt-submit.json", a);
        payload["prompt"] = json!(format!("First taskr got {a}. {secret}"));
        h.hook(&env, "claude", "UserPromptSubmit", payload);
        assert_eq!(h.got(a), 1);
        h.hook(&env, "claude", "Stop", fixture("claude-stop.json"));
        assert_eq!(h.stalls(a), 0);
        let mut error = fixture("claude-stop-failure.json");
        error["error"] = json!("API Error: 529 overloaded");
        h.hook(&env, "claude", "StopFailure", error);
        assert_eq!(h.stalls(a), 1);
        assert_eq!(h.stall(a)["error"], "unknown");
        // Toggle the hub's authenticated caller; client self-identity is irrelevant.
        db.execute("insert into events(task_id,launch_id,kind,summary,created_at) values(?,?,'prompt','probe prompt',?)",params![w,l,taskr_core::store::now()]).unwrap();
        let probe = db.last_insert_rowid();
        fs::write(h.home.join("whois-host"), "host-b").unwrap();
        h.hook(
            &env,
            "claude",
            "UserPromptSubmit",
            prompt_fixture("claude-user-prompt-submit.json", probe),
        );
        assert_eq!(h.got(probe), 0, "wrong host recorded a receipt");
        let bound: String = db
            .query_row("select session_ref from launches where id=?", [l], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(bound, "claude-session");
        stop_tx.send(()).unwrap();
        proxy.join().unwrap();
        let records: Vec<Value> = requests.try_iter().collect();
        let hooks: Vec<_> = records.iter().filter(|v| v["argv"][1] == "_hook").collect();
        assert_eq!(hooks.len(), 5);
        let prompt = hooks
            .iter()
            .find(|v| v["argv"][2] == "UserPromptSubmit")
            .unwrap();
        assert_eq!(
            prompt["argv"],
            json!([
                "--json",
                "_hook",
                "UserPromptSubmit",
                "--session",
                "claude-session",
                "--transcript",
                "/tmp/taskr-hooks/claude.jsonl",
                "--attempt",
                a.to_string()
            ])
        );
        assert_eq!(
            prompt["env"],
            json!({"TASKR_TASK":w.to_string(),"TASKR_LAUNCH":l.to_string(),"HERDR_PANE_ID":"w9:p1"})
        );
        assert!(hooks.iter().all(|v| !v.to_string().contains(secret)));
        let mut stmt = db
            .prepare("select coalesce(summary,'')||coalesce(data,'') from events")
            .unwrap();
        for row in stmt.query_map([], |r| r.get::<_, String>(0)).unwrap() {
            assert!(!row.unwrap().contains(secret));
        }
    }
}
