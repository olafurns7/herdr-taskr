use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};
use taskr_core::db::Connection;

pub type Env = Vec<(String, String)>;

static NEXT: AtomicU64 = AtomicU64::new(0);
pub struct Harness {
    pub home: PathBuf,
    pub db: PathBuf,
    socket: PathBuf,
}
impl Harness {
    pub fn new() -> Self {
        let home = std::env::temp_dir().join(format!(
            "taskr-hooks-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join("bin")).unwrap();
        let socket = home.join("h.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        });
        let h = Self {
            db: home.join("taskr.db"),
            home,
            socket,
        };
        h.script("herdr", "#!/bin/sh\ncase \"$1 $2\" in\n'agent list') cat \"$HOME/agents.json\" ;;\n'workspace list') echo '{\"result\":{\"workspaces\":[]}}' ;;\n'pane list') echo '{\"result\":{\"panes\":[]}}' ;;\n'agent read') exit 1 ;;\n*) echo '{\"result\":{}}' ;;\nesac\n");
        h.agents(None, 0);
        h
    }
    pub fn script(&self, name: &str, body: &str) {
        let path = self.home.join("bin").join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    pub fn command(&self, env: &[(String, String)]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_taskr"));
        c.env_clear()
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("TASKR_DB", &self.db)
            .env("TASKR_TMP_BASE", self.home.join("taskr-tmp"))
            .env("TMPDIR", &self.home)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.home.join("bin").display()),
            )
            .env("HERDR_SOCKET_PATH", &self.socket)
            .envs(env.iter().cloned());
        c
    }
    pub fn output(&self, env: &[(String, String)], args: &[&str], input: &[u8]) -> Output {
        let mut child = self
            .command(env)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Err(e) = child.stdin.take().unwrap().write_all(input) {
            assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe);
        }
        child.wait_with_output().unwrap()
    }
    pub fn ok(&self, env: &[(String, String)], args: &[&str]) -> Value {
        let mut argv = vec!["--json"];
        argv.extend_from_slice(args);
        let out = self.output(env, &argv, b"");
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    pub fn lane(&self, provider: &str, role: &str) -> (i64, i64, Env) {
        let top = self.ok(&[], &["new", "top", "--role", "orchestrator"])["task_id"]
            .as_i64()
            .unwrap();
        let worker = self.ok(
            &[],
            &[
                "new",
                "impl",
                "--role",
                role,
                "--parent",
                &top.to_string(),
                "--cwd",
                self.home.to_str().unwrap(),
                "--pane",
                "w9:p1",
            ],
        )["task_id"]
            .as_i64()
            .unwrap();
        let launch = self.ok(
            &[],
            &[
                "launch",
                &worker.to_string(),
                "--provider",
                provider,
                "--model",
                "test",
                "--effort",
                "medium",
            ],
        )["launch_id"]
            .as_i64()
            .unwrap();
        (
            worker,
            launch,
            vec![
                ("TASKR_TASK".into(), worker.to_string()),
                ("TASKR_LAUNCH".into(), launch.to_string()),
                ("HERDR_ENV".into(), "1".into()),
                ("HERDR_PANE_ID".into(), "w9:p1".into()),
            ],
        )
    }
    pub fn prompt(&self, w: i64, armed: bool) -> i64 {
        let mut args = vec![
            "prompt".into(),
            w.to_string(),
            "--text".into(),
            "work".into(),
        ];
        if !armed {
            args.extend(["--receipt-timeout".into(), "0".into()]);
        }
        self.ok(&[], &args.iter().map(String::as_str).collect::<Vec<_>>())["attempt_id"]
            .as_i64()
            .unwrap()
    }
    pub fn hook(&self, env: &[(String, String)], harness: &str, event: &str, payload: Value) {
        self.hook_bytes(env, harness, event, &serde_json::to_vec(&payload).unwrap());
    }
    pub fn hook_bytes(&self, env: &[(String, String)], harness: &str, event: &str, payload: &[u8]) {
        let out = self.output(env, &["hook", harness, event], payload);
        assert!(
            out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
            "{harness}/{event}: {out:?}"
        );
    }
    pub fn conn(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }
    pub fn count(&self, sql: &str, id: i64) -> i64 {
        self.conn().query_row(sql, [id], |r| r.get(0)).unwrap()
    }
    pub fn got(&self, a: i64) -> i64 {
        self.count(
            "select count(*) from events where kind='got' and related_event_id=?",
            a,
        )
    }
    pub fn stalls(&self, a: i64) -> i64 {
        self.count("select count(*) from events where event_key='stall:'||?", a)
    }
    pub fn stall(&self, a: i64) -> Value {
        let s: String = self
            .conn()
            .query_row(
                "select data from events where event_key='stall:'||?",
                [a],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&s).unwrap()
    }
    pub fn agents(&self, status: Option<&str>, seq: i64) {
        let agents = status.map(|s| vec![json!({"pane_id":"w9:p1","name":"fixture","agent_status":s,"state_change_seq":seq})]).unwrap_or_default();
        fs::write(
            self.home.join("agents.json"),
            json!({"result":{"agents":agents}}).to_string(),
        )
        .unwrap();
    }
    pub fn daemon(&self) {
        self.ok(&[], &["daemon", "--once"]);
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.home).unwrap();
    }
}
pub fn fixture(name: &str) -> Value {
    serde_json::from_slice(
        &fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../testdata/hooks")
                .join(name),
        )
        .unwrap(),
    )
    .unwrap()
}
pub fn prompt_fixture(name: &str, attempt: i64) -> Value {
    let mut v = fixture(name);
    let text = json!(format!("First taskr got {attempt}. Continue."));
    if v.get("input").is_some() {
        v["output"]["parts"][0]["text"] = text;
    } else if v["type"] == "input" {
        v["text"] = text;
    } else {
        v["prompt"] = text;
    }
    v
}
