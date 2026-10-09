//! Guarded Herdr access: never run its CLI without a reachable server.
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
pub fn socket() -> String {
    std::env::var("HERDR_SOCKET_PATH")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            format!(
                "{}/.config/herdr/herdr.sock",
                std::env::var("HOME").unwrap_or_default()
            )
        })
}
fn connect(sock: &str, timeout: Duration) -> std::io::Result<UnixStream> {
    let sock = sock.to_string();
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(UnixStream::connect(sock));
    });
    recv.recv_timeout(timeout)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "Herdr socket deadline"))?
}
pub fn up(sock: &str) -> bool {
    !sock.is_empty() && connect(sock, Duration::from_millis(500)).is_ok()
}
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub deadline: bool,
}
pub fn command(sock: &str, args: &[&str], timeout: Duration) -> std::io::Result<Output> {
    if !up(sock) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "Herdr server not reachable; no herdr command run",
        ));
    }
    let mut cmd = Command::new("herdr");
    cmd.args(args).env("HERDR_SOCKET_PATH", sock);
    run(cmd, timeout)
}
/// Run a child in its own process group with pipe readers and a deadline that
/// kills the group; `herdr` and the PR poller's `gh` share it.
pub fn run(mut cmd: Command, timeout: Duration) -> std::io::Result<Output> {
    let mut child = cmd
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("stdout pipe");
    let mut stderr = child.stderr.take().expect("stderr pipe");
    let (send, recv) = std::sync::mpsc::channel();
    let errsend = send.clone();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = send.send((false, bytes));
    });
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        let _ = errsend.send((true, bytes));
    });
    let start = Instant::now();
    let (status, deadline) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if start.elapsed() >= timeout {
            taskr_core::store::terminate_group(child.id());
            let _ = child.kill();
            break (child.wait()?, true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let grace = Instant::now();
    let mut pipe_timeout = false;
    for _ in 0..2 {
        match recv.recv_timeout(Duration::from_secs(2).saturating_sub(grace.elapsed())) {
            Ok((is_error, bytes)) => {
                if is_error {
                    stderr = bytes;
                } else {
                    stdout = bytes;
                }
            }
            Err(_) => {
                taskr_core::store::terminate_group(child.id());
                pipe_timeout = true;
                break;
            }
        }
    }
    if pipe_timeout && status.success() {
        return Err(std::io::Error::other(
            "exec: WaitDelay expired before I/O complete",
        ));
    }
    Ok(Output {
        code: status.code(),
        stdout,
        stderr,
        deadline,
    })
}
pub fn request(
    sock: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> std::io::Result<Value> {
    let mut conn = connect(sock, timeout.min(Duration::from_millis(500)))?;
    conn.set_read_timeout(Some(timeout.min(Duration::from_millis(500))))?;
    conn.set_write_timeout(Some(timeout.min(Duration::from_millis(500))))?;
    let id = "taskr-capacity";
    let body = taskr_core::compact_json(&json!({"id":id,"method":method,"params":params}))?;
    writeln!(conn, "{body}")?;
    let mut line = Vec::new();
    BufReader::new(conn.take(256 * 1024 + 1)).read_until(b'\n', &mut line)?;
    let reply: Value = serde_json::from_slice(&line)?;
    if line.len() > 256 * 1024
        || reply["id"] != id
        || !reply["error"].is_null()
        || reply["result"].is_null()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid capacity response",
        ));
    }
    Ok(reply["result"].clone())
}

/// Run one guarded prompt delivery, preserving Go's outcome/detail fields.
pub(crate) fn run_prompt(
    sock: &str,
    target: &str,
    text: &str,
    json_mode: bool,
) -> (&'static str, Value) {
    let mut detail = json!({});
    let outcome = match command(
        sock,
        &[
            "agent",
            "prompt",
            target,
            text,
            "--wait",
            "--until",
            "working",
            "--until",
            "blocked",
            "--timeout",
            "20000",
        ],
        Duration::from_secs(30),
    ) {
        Ok(output) => {
            if output.code == Some(0) {
                if let Ok(v) = serde_json::from_slice::<Value>(&output.stdout)
                    && let Some(s) = v["result"]["agent"]["agent_status"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                {
                    detail["agent_status"] = json!(s);
                }
                "activity_observed"
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if json_mode && !stderr.is_empty() {
                    eprintln!("herdr: {}", stderr.trim());
                }
                let parsed = serde_json::from_slice::<Value>(&output.stderr);
                let code = parsed
                    .as_ref()
                    .ok()
                    .and_then(|v| v["error"]["code"].as_str())
                    .unwrap_or("");
                if !json_mode {
                    let msg = if let Ok(v) = &parsed {
                        v["error"]["message"].as_str().unwrap_or("")
                    } else {
                        stderr.trim()
                    };
                    if !msg.is_empty() {
                        detail["herdr_message"] = json!(msg);
                    }
                }
                if output.deadline {
                    detail["herdr_error"] = json!("deadline");
                    "delivery_unknown"
                } else {
                    let exit = output.code.unwrap_or(-1);
                    detail["herdr_exit"] = json!(exit);
                    if !code.is_empty() {
                        detail["herdr_error"] = json!(code);
                    }
                    if exit == 2 || code == "agent_blocked" || code.contains("not_found") {
                        "rejected"
                    } else {
                        "delivery_unknown"
                    }
                }
            }
        }
        Err(e) => {
            detail["herdr_error"] = json!(if e.kind() == std::io::ErrorKind::NotConnected {
                "no_server"
            } else {
                "not_run"
            });
            "rejected"
        }
    };
    (outcome, detail)
}
