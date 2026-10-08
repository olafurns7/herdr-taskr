use super::{ExitCode, env, new_key, rpc, spool};
use std::{
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

pub fn run(args: &[String], dir: &Path, raw: &str) -> ExitCode {
    if std::env::var("TASKR_LAUNCH").is_ok_and(|s| !s.is_empty())
        && std::env::var("HERDR_ENV").is_ok_and(|s| s == "1")
        && args.len() == 2
        && taskr_core::store::hook::supported(&args[0], &args[1])
    {
        let started = Instant::now();
        let args = args.to_vec();
        let dir = dir.to_path_buf();
        let raw = raw.to_string();
        let (send, recv) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            apply(&args, &dir, &raw, started);
            let _ = send.send(());
        });
        let _ = recv.recv_timeout(Duration::from_millis(500));
    }
    ExitCode::Ok
}
fn queue_until(
    dir: &Path,
    req: &serde_json::Value,
    only_waiting: bool,
    deadline: Instant,
) -> super::Result<Option<usize>> {
    loop {
        match spool::queue_mode(dir, req, None, only_waiting, true) {
            Err(e) if e.message == "spool lock is held" && Instant::now() < deadline => {
                std::thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                )
            }
            result => return result,
        }
    }
}
fn apply(args: &[String], dir: &Path, raw: &str, started: Instant) {
    let mut b = Vec::new();
    if std::io::stdin()
        .take((8 << 20) + 1)
        .read_to_end(&mut b)
        .is_err()
    {
        return;
    }
    let Some(record) = taskr_core::store::hook::parse(&args[0], &args[1], &b) else {
        return;
    };
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let Ok(key) = new_key() else { return };
    let mut argv = vec![
        "--json".into(),
        "_hook".into(),
        record.event,
        "--session".into(),
        record.session,
    ];
    if !record.transcript.is_empty() {
        argv.extend(["--transcript".into(), record.transcript]);
    }
    if record.attempt > 0 {
        argv.extend(["--attempt".into(), record.attempt.to_string()]);
    }
    if !record.error.is_empty() {
        argv.extend(["--error".into(), record.error]);
    }
    let mut env = env();
    env.as_object_mut()
        .unwrap()
        .retain(|k, _| ["TASKR_TASK", "TASKR_LAUNCH", "HERDR_PANE_ID"].contains(&k.as_str()));
    let req = rpc::request(&argv, &cwd.to_string_lossy(), &key, env, None);
    let process = started + Duration::from_millis(500);
    match queue_until(dir, &req, true, started + Duration::from_millis(150)) {
        Ok(Some(_)) => return,
        Err(e) if e.message == "spool lock is held" => {
            if spool::waiting(dir) {
                let _ = queue_until(dir, &req, false, process);
                return;
            }
        }
        Err(_) => return,
        Ok(None) => {}
    }
    let send_deadline = started + Duration::from_millis(350);
    let (send, recv) = std::sync::mpsc::channel();
    let raw = raw.to_string();
    let body = req.clone();
    std::thread::spawn(move || {
        let result = rpc::Client::new(&raw).and_then(|cl| {
            cl.call(
                &body,
                send_deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
                true,
            )
        });
        let _ = send.send(result);
    });
    let queue = match recv.recv_timeout(send_deadline.saturating_duration_since(Instant::now())) {
        Ok(Err(e)) => {
            e.kind == "transport"
                || [401, 403, 408, 429]
                    .into_iter()
                    .any(|s| e.message.starts_with(&format!("server answered {s}:")))
        }
        Err(_) => true,
        _ => false,
    };
    if queue {
        let _ = queue_until(dir, &req, false, process);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use taskr_core::store::hook::parse;

    #[test]
    fn pi_error_codes() {
        for (name, message, want) in [
            (
                "code",
                json!({"stopReason":"error","diagnostics":[{"type":"provider_transport_failure","error":{"name":"Error","code":"ECONNRESET"}}]}),
                "ECONNRESET",
            ),
            (
                "numeric code",
                json!({"stopReason":"error","diagnostics":[{"type":"old","error":{"code":1}},{"type":"provider_transport_failure","error":{"code":529}}]}),
                "529",
            ),
            (
                "type",
                json!({"stopReason":"error","diagnostics":[{"type":"bedrock_response_failure","details":{}}]}),
                "bedrock_response_failure",
            ),
            (
                "uncoded",
                json!({"stopReason":"error","errorMessage":"API Error: 529 overloaded"}),
                "unknown",
            ),
            (
                "aborted",
                json!({"stopReason":"aborted","diagnostics":[{"type":"provider_transport_failure"}]}),
                "",
            ),
            ("stop", json!({"stopReason":"stop"}), ""),
        ] {
            let payload = serde_json::to_vec(
                &json!({"type":"agent_settled","sessionId":"pi-session","message":message}),
            )
            .unwrap();
            let record = parse("pi", "agent_settled", &payload).unwrap();
            assert_eq!(record.session, "pi-session", "{name}");
            assert_eq!(record.error, want, "{name}");
        }
        assert!(
            parse(
                "pi",
                "session_start",
                br#"{"type":"session_start","sessionId":"pi-session","sessionFile":"pi.jsonl"}"#
            )
            .is_none()
        );
    }
}
