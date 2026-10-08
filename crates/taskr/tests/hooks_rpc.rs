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
    #[test]
    fn client_rpc_normalized_fields_and_host_check() {
        let h = Harness::new();
        h.script("tailscale", include_str!("hook/tailscale.sh"));
        let hub_env = vec![
            ("NET_ID".into(), "hub".into()),
            ("TASKR_CONTRACT_TAILNET".into(), "1".into()),
            ("TASKR_CONTRACT_ORACLE".into(), "1".into()),
        ];
        let mut hub = Hub(h
            .command(&hub_env)
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
        let port: u16 = url.trim().rsplit_once(':').unwrap().1.parse().unwrap();
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
