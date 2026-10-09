//! Client-host routing, verified RPC, and the Go-compatible offline spool.
mod doc;
pub(crate) mod events;
mod hook;
mod prompt;
mod rpc;
mod spool;
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use taskr_core::{ExitCode, compact_json};

#[derive(Clone, Debug)]
struct Error {
    code: ExitCode,
    kind: &'static str,
    message: String,
    retryable: bool,
    reached: bool,
    body: Option<Value>,
}
type Result<T> = std::result::Result<T, Error>;
impl Error {
    fn usage(s: impl Into<String>) -> Self {
        Self::new(ExitCode::Usage, "usage", s)
    }
    fn rejected(s: impl Into<String>) -> Self {
        Self::new(ExitCode::Rejected, "rejected", s)
    }
    fn new(code: ExitCode, kind: &'static str, s: impl Into<String>) -> Self {
        Self {
            code,
            kind,
            message: s.into(),
            retryable: false,
            reached: false,
            body: None,
        }
    }
    fn transport(s: impl Into<String>, retryable: bool, reached: bool) -> Self {
        Self {
            retryable,
            reached,
            ..Self::new(ExitCode::Transport, "transport", s)
        }
    }
    fn io(e: std::io::Error) -> Self {
        Self::new(ExitCode::Database, "database", e.to_string())
    }
    fn emit(&self, json_mode: bool, cmd: &str) -> ExitCode {
        let mut body = self.body.clone().unwrap_or_else(|| json!({}));
        if json_mode {
            body["error"] = json!(self.message);
            body["kind"] = json!(self.kind);
            eprintln!("taskr {cmd}: {}", self.message);
            println!("{}", compact_json(&body).unwrap());
        } else {
            body["err"] = json!(self.message);
            body["k"] = json!(self.kind);
            println!("x1 {} {}", self.code as u8, compact_json(&body).unwrap());
        }
        self.code
    }
}
fn command(args: &[String]) -> (&str, &[String]) {
    let args = if args.first().is_some_and(|s| s == "--json") {
        &args[1..]
    } else {
        args
    };
    args.split_first()
        .map_or(("", &[]), |(cmd, args)| (cmd, args))
}
fn flag<'a>(args: &'a [String], name: &str) -> Option<(&'a str, usize, bool)> {
    for (i, arg) in args
        .iter()
        .enumerate()
        .take_while(|(_, a)| a.as_str() != "--")
    {
        if !arg.starts_with('-') {
            continue;
        }
        let (n, v) = arg
            .trim_start_matches('-')
            .split_once('=')
            .map_or((arg.trim_start_matches('-'), None), |(n, v)| (n, Some(v)));
        if n == name {
            return Some(if let Some(v) = v {
                (v, i, true)
            } else {
                (
                    args.get(i + 1).map(String::as_str).unwrap_or(""),
                    i + 1,
                    false,
                )
            });
        }
    }
    None
}
fn flag_true(args: &[String], name: &str) -> bool {
    args.iter()
        .take_while(|s| s.as_str() != "--")
        .filter(|s| s.starts_with('-'))
        .any(|s| {
            let s = s.trim_start_matches('-');
            s == name || s == format!("{name}=true") || s == format!("{name}=1")
        })
}
fn help(args: &[String]) -> bool {
    args.iter()
        .take_while(|s| s.as_str() != "--")
        .filter(|s| s.starts_with('-'))
        .any(|s| {
            let (n, v) = s
                .trim_start_matches('-')
                .split_once('=')
                .map_or((s.trim_start_matches('-'), None), |(n, v)| (n, Some(v)));
            matches!(n, "h" | "help") && v.is_none_or(|v| v == "true")
        })
}
fn watch_requested(args: &[String]) -> bool {
    args.iter()
        .take_while(|s| s.as_str() != "--")
        .filter(|s| s.starts_with('-'))
        .any(|s| {
            let (n, v) = s
                .trim_start_matches('-')
                .split_once('=')
                .map_or((s.trim_start_matches('-'), None), |(n, v)| (n, Some(v)));
            n == "watch" && v.is_none_or(|v| taskr_core::goflag::parse_bool(v) == Some(true))
        })
}
pub(crate) fn spoolable(argv: &[String]) -> bool {
    matches!(
        command(argv).0,
        "got" | "ready" | "done" | "fail" | "decide" | "next" | "note" | "close" | "_hook"
    )
}
fn valid_key(s: &str) -> bool {
    (8..=128).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}
fn new_key() -> Result<String> {
    let mut b = [0; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(Error::io)?;
    Ok(b.iter().map(|v| format!("{v:02x}")).collect())
}
fn state_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Err(Error::usage("HOME is not set"));
    }
    Ok(Path::new(&home).join(".local/state/taskr"))
}
fn now() -> time::OffsetDateTime {
    if cfg!(feature = "contract") {
        taskr_core::frozen_now().unwrap()
    } else {
        time::OffsetDateTime::now_utc()
    }
}
fn timestamp() -> String {
    taskr_core::frozen_now()
        .unwrap()
        .replace_nanosecond(0)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}
fn env() -> Value {
    let mut env = json!({});
    for k in [
        "TASKR_TASK",
        "TASKR_LAUNCH",
        "TASKR_FORMAT",
        "HERDR_PANE_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "CODEX_HOME",
        "CODEX_THREAD_ID",
        "CLAUDE_CONFIG_DIR",
    ] {
        if let Ok(v) = std::env::var(k)
            && !v.is_empty()
        {
            env[k] = json!(v);
        }
    }
    env
}
fn repeated(args: &[String]) -> Option<&str> {
    let mut seen = std::collections::HashSet::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            break;
        }
        if a.starts_with('-') && a != "-" {
            let (n, inline) = a
                .trim_start_matches('-')
                .split_once('=')
                .map_or((a.trim_start_matches('-'), false), |(n, _)| (n, true));
            if [
                "cwd",
                "brief",
                "report",
                "file",
                "out",
                "machine",
                "role",
                "planned",
                "timeout",
                "name",
                "confirm",
                "confirm-timeout",
                "prompt",
                "receipt-timeout",
            ]
            .contains(&n)
            {
                if !seen.insert(n) {
                    return Some(n);
                }
                if !inline && !["planned", "confirm", "prompt"].contains(&n) && i + 1 < args.len() {
                    i += 1;
                }
            }
        }
        i += 1;
    }
    None
}
fn absolute(base: &Path, s: &str) -> PathBuf {
    let path = Path::new(s);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}
fn paths(cmd: &str, args: &mut Vec<String>, cwd: &Path) -> Result<Option<PathBuf>> {
    let mut base = cwd.to_path_buf();
    let flags: &[&str] = match cmd {
        "new" => &["cwd", "brief", "report"],
        "ready" => &["report"],
        "prompt" => &["file"],
        "doc" if args.first().is_some_and(|s| s == "set") => &["file"],
        _ => &[],
    };
    for f in flags {
        if let Some((v, at, inline)) = flag(args, f)
            && !v.is_empty()
            && at < args.len()
        {
            let p = absolute(
                if cmd == "new" && *f != "cwd" {
                    &base
                } else {
                    cwd
                },
                v,
            );
            if *f == "cwd" {
                base = p.clone();
            }
            let p = p.to_string_lossy();
            args[at] = if inline {
                format!("{}={p}", args[at].split('=').next().unwrap())
            } else {
                p.into()
            };
        }
    }
    if cmd != "handover" {
        return Ok(None);
    }
    if let Some((v, at, inline)) = flag(args, "out") {
        if at >= args.len() {
            return Err(Error::usage("--out needs a value"));
        }
        if v.starts_with('-') {
            return Err(Error::usage(format!(
                "--out must be a path that does not start with `-`, got {v:?}"
            )));
        }
        let p = if v.is_empty() {
            None
        } else {
            Some(absolute(cwd, v))
        };
        if inline {
            args.remove(at);
        } else {
            args.drain(at - 1..=at);
        }
        if let Some(p) = &p
            && !p.parent().unwrap().is_dir()
        {
            return Err(Error::usage(format!(
                "--out: directory {} does not exist",
                p.parent().unwrap().display()
            )));
        }
        return Ok(p);
    }
    Ok(None)
}
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|s| {
            if !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_=.,/:@%+".contains(&b))
            {
                s.clone()
            } else {
                format!("'{}'", s.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
pub(crate) fn reply_error(rep: &Value) -> String {
    let line = rep["stdout"]
        .as_str()
        .unwrap_or("")
        .trim()
        .lines()
        .last()
        .unwrap_or("");
    let line = if line.starts_with("x1 ") {
        line.splitn(3, ' ').nth(2).unwrap_or(line)
    } else {
        line
    };
    if let Ok(v) = serde_json::from_str::<Value>(line) {
        for k in ["error", "err"] {
            if let Some(s) = v[k].as_str()
                && !s.is_empty()
            {
                return s.into();
            }
        }
    }
    let stderr = rep["stderr"].as_str().unwrap_or("").trim();
    if stderr.is_empty() {
        format!("server exited {}", rep["exit"])
    } else {
        stderr.into()
    }
}

/// Return `None` only for commands Go executes locally on a client host.
pub fn route(mut json_mode: bool, raw_args: &[String]) -> Option<ExitCode> {
    if std::env::var("TASKR_DB").is_ok_and(|s| !s.is_empty()) {
        let (cmd, args) = command(raw_args);
        if cmd == "spool"
            && let Ok(dir) = state_dir()
        {
            return Some(spool::dispatch(
                json_mode || raw_args.first().is_some_and(|s| s == "--json"),
                args,
                &dir,
                None,
            ));
        }
        return None;
    }
    let dir = match state_dir() {
        Ok(d) => d,
        Err(_) => return None,
    };
    let raw = match std::fs::read_to_string(dir.join("server.url")) {
        Ok(s) => Ok(s.lines().next().unwrap_or("").trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return if command(raw_args).0 == "spool" {
                Some(spool::dispatch(json_mode, command(raw_args).1, &dir, None))
            } else {
                None
            };
        }
        Err(e) => Err(e),
    };
    let mut args = raw_args;
    let mut lead = Vec::new();
    let mut key = String::new();
    while let Some(a) = args.first() {
        if a == "--json" {
            json_mode = true;
            lead.push(a.clone());
            args = &args[1..];
        } else if a == "--request-key" {
            if args.len() < 2 {
                return Some(Error::usage("--request-key needs a value").emit(json_mode, ""));
            }
            key = args[1].clone();
            args = &args[2..];
        } else if let Some(v) = a.strip_prefix("--request-key=") {
            key = v.into();
            args = &args[1..];
        } else {
            break;
        }
    }
    let (cmd, cargs) = command(args);
    if cmd == "version" && !help(cargs) {
        let mut f = taskr_core::goflag::FlagSet::new("version", json_mode);
        if f.parse(cargs, 0, 0).is_err() {
            return Some(Error::usage("version: takes no arguments").emit(f.json(), "version"));
        }
        let version = option_env!("TASKR_VERSION").unwrap_or("dev");
        let server = raw
            .as_ref()
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| format!(",\"server\":{}", compact_json(&json!(s)).unwrap()))
            .unwrap_or_default();
        println!(
            "{}{{\"version\":{},\"ok\":true{server}}}",
            if f.json() { "" } else { "j1 " },
            compact_json(&json!(version)).unwrap()
        );
        return Some(ExitCode::Ok);
    }
    if cmd == "spool" {
        return Some(spool::dispatch(
            json_mode,
            cargs,
            &dir,
            raw.as_ref().ok().map(String::as_str),
        ));
    }
    if !crate::cli::known(cmd) || matches!(cmd, "help" | "version") || help(cargs) {
        if raw_args
            .iter()
            .take(raw_args.len().saturating_sub(args.len()))
            .any(|s| s.starts_with("--request-key"))
        {
            return Some(
                crate::read::dispatch(json_mode, args)
                    .or_else(|| crate::write::dispatch(json_mode, args))
                    .unwrap_or_else(|| crate::cli::unknown(json_mode, cmd)),
            );
        }
        return None;
    }
    if cmd == "hook" {
        return Some(hook::run(
            cargs,
            &dir,
            raw.as_ref().ok().map(String::as_str).unwrap_or(""),
        ));
    }
    if cmd == "daemon" {
        return Some(match raw.as_ref() {
            Ok(raw) => crate::daemon::client_dispatch(json_mode, cargs, raw),
            Err(e) => Error::usage(format!("read server.url: {e}")).emit(json_mode, cmd),
        });
    }
    if cmd == "glance" && watch_requested(cargs) {
        return None;
    }
    let run = || -> Result<ExitCode> {
        if let Some(f) = repeated(cargs) {
            return Err(Error::usage(format!("repeated RPC flag --{f}")));
        }
        if cmd == "note" {
            let mut f = crate::write::flags("note", json_mode);
            if let Err(e) = f.parse(cargs, 1, 1) {
                return Err(Error::usage(e));
            }
            if f.get_int("as") < 0 {
                return Err(Error::usage("--as must be a positive task id"));
            }
            if f.get_bool("owner")
                && (std::env::var("TASKR_TASK").is_ok_and(|s| !s.is_empty())
                    || f.get_int("as") == 0)
            {
                return Err(Error::rejected(
                    "--owner: only a root orchestrator's own note",
                ));
            }
        }
        let raw = raw
            .as_ref()
            .map_err(|e| Error::usage(format!("server.url: {e}")))?;
        let key = if key.is_empty() {
            new_key()?
        } else {
            key.clone()
        };
        if !valid_key(&key) {
            return Err(Error::usage(
                "--request-key must match [A-Za-z0-9_-]{8,128}",
            ));
        }
        let cwd =
            std::env::current_dir().map_err(|e| Error::usage(format!("current directory: {e}")))?;
        let cwd_text = cwd.to_string_lossy();
        let mut cargs = cargs.to_vec();
        let out = paths(cmd, &mut cargs, &cwd)?;
        let argv: Vec<_> = lead
            .iter()
            .cloned()
            .chain(std::iter::once(cmd.to_string()))
            .chain(cargs.clone())
            .collect();
        let retry = format!(
            "taskr --request-key {key} {}",
            shell_join(
                &lead
                    .iter()
                    .cloned()
                    .chain(args.iter().cloned())
                    .collect::<Vec<_>>()
            )
        );
        let env = env();
        let doc = if cmd == "doc" {
            doc::set_payload(&cargs, json_mode)?
        } else {
            None
        };
        let request = rpc::request(&argv, &cwd_text, &key, env.clone(), doc);
        let saved = if cmd == "ready" {
            doc::ready_payload(&cargs, &env)
        } else {
            None
        };
        let queue = |reason: &str| -> Result<ExitCode> {
            let n = spool::queue(&dir, &request, saved.as_ref(), false)
                .map_err(|e| spool_failure(e, &retry))?
                .unwrap();
            queued(json_mode || flag_true(&cargs, "json"), &key, n, reason);
            Ok(ExitCode::Ok)
        };
        if spoolable(&argv)
            && let Some(n) = spool::queue(&dir, &request, saved.as_ref(), true).map_err(|e| {
                if spool::waiting(&dir) || e.kind == "transport" {
                    spool_failure(e, &retry)
                } else {
                    e
                }
            })?
        {
            queued(
                json_mode || flag_true(&cargs, "json"),
                &key,
                n,
                "earlier records wait in the spool",
            );
            return Ok(ExitCode::Ok);
        }
        let cl = match rpc::Client::new(raw) {
            Ok(cl) => cl,
            Err(e) if spoolable(&argv) && e.kind == "transport" => {
                return queue("server unreachable");
            }
            Err(e) => return Err(transport_failure(e, &retry)),
        };
        if cmd == "new"
            && flag(&cargs, "role").is_none_or(|(r, _, _)| r != "gate")
            && !flag_true(&cargs, "planned")
        {
            if flag(&cargs, "machine").is_none_or(|(m, _, _)| m == cl.short) {
                let p = flag(&cargs, "cwd")
                    .filter(|(p, _, _)| !p.is_empty())
                    .map_or(cwd_text.as_ref(), |(p, _, _)| p);
                if !Path::new(p).is_dir() {
                    return Err(Error::usage(format!("directory {p} does not exist")));
                }
            }
            if let Some((p, _, _)) = flag(&cargs, "brief")
                && !p.is_empty()
                && !Path::new(p).is_file()
            {
                return Err(Error::usage(format!("file {p} does not exist")));
            }
        }
        if cmd == "prompt"
            && let Some(code) = prompt::relay(&cl, &lead, &cargs, &cwd_text, &env, json_mode)?
        {
            return Ok(code);
        }
        if cmd == "doc" && cargs.first().is_some_and(|s| s == "backfill") {
            return doc::backfill(&cl, &cargs[1..], json_mode, &cwd_text, &env);
        }
        let rep = match call_retry(&cl, &request, json_mode, &retry) {
            Ok(rep) => rep,
            Err(e) if spoolable(&argv) && e.retryable => return queue("server unreachable"),
            Err(mut e) => {
                if e.kind == "transport" {
                    eprintln!("taskr: server unreachable; retry with: {retry}");
                    e.message = format!("server unreachable ({}); retry with: {retry}", e.message);
                }
                return Err(e);
            }
        };
        print!("{}", rep["stdout"].as_str().unwrap());
        eprint!("{}", rep["stderr"].as_str().unwrap());
        let code = exit(rep["exit"].as_i64().unwrap());
        if code == ExitCode::Ok {
            if let Some(out) = out {
                if let Err(e) = std::fs::write(&out, rep["stdout"].as_str().unwrap()) {
                    eprintln!("taskr handover: the handover is recorded, but --out failed: {e}");
                    return Ok(ExitCode::Usage);
                }
                eprintln!("taskr handover: wrote {}", out.display());
            }
            let _ = doc::uploads(&cl, &rep, &cwd_text, &env, None);
        }
        Ok(code)
    };
    Some(match run() {
        Ok(c) => c,
        Err(e) => e.emit(json_mode, cmd),
    })
}
fn exit(n: i64) -> ExitCode {
    match n {
        0 => ExitCode::Ok,
        1 => ExitCode::Watch,
        2 => ExitCode::Usage,
        3 => ExitCode::Timeout,
        4 => ExitCode::Database,
        5 => ExitCode::Transport,
        6 => ExitCode::Rejected,
        _ => ExitCode::NotImplemented,
    }
}
fn queued(json_mode: bool, key: &str, n: usize, reason: &str) {
    if json_mode {
        println!(
            "{}",
            compact_json(&json!({"queued":true,"request_key":key})).unwrap()
        );
    } else {
        println!("qd1 {key}");
    }
    eprintln!("taskr: {reason}; queued ({n} waiting)");
}
fn call_retry(cl: &rpc::Client, request: &Value, json_mode: bool, retry: &str) -> Result<Value> {
    let mut req = request.clone();
    let argv: Vec<String> = serde_json::from_value(req["argv"].clone()).unwrap();
    let (cmd, args) = command(&argv);
    let budget = rpc::budget(&argv);
    let window = if cmd == "wait" {
        budget
    } else if spoolable(&argv) {
        Duration::from_secs(3)
    } else {
        budget.max(Duration::from_secs(60))
    };
    #[cfg(feature = "contract")]
    let window = if cmd != "wait" {
        std::env::var("TASKR_CONTRACT_RETRY_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(window)
    } else {
        window
    };
    let deadline = Instant::now() + window;
    let announced_deadline = now() + time::Duration::try_from(window).unwrap();
    let mut reached = false;
    let mut announced = false;
    let mut backoff = Duration::from_secs(1);
    let mut last_error: Option<Error> = None;
    let mut last_reply: Option<Value> = None;
    let mut attempted = false;
    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut signals = Vec::new();
    if cmd != "wait" {
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            signals
                .push(signal_hook::flag::register(signal, interrupted.clone()).map_err(Error::io)?);
        }
    }
    let mut run = || -> Result<Value> {
        loop {
            if attempted && Instant::now() >= deadline {
                if cmd == "wait" && reached {
                    return Ok(wait_timeout(json_mode, args));
                }
                if let Some(e) = last_error.take() {
                    return Err(e);
                }
                if let Some(rep) = last_reply.take() {
                    return Ok(rep);
                }
            }
            if interrupted.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::transport("context canceled", false, reached));
            }
            let time = if spoolable(&argv) {
                deadline.saturating_duration_since(Instant::now())
            } else if cmd == "wait" {
                deadline.saturating_duration_since(Instant::now()) + Duration::from_secs(10)
            } else {
                (budget + Duration::from_secs(10))
                    .max(deadline.saturating_duration_since(Instant::now()))
            }
            .max(Duration::from_millis(1));
            let result = if cmd == "wait" {
                cl.call(&req, time, true)
            } else {
                let (send, recv) = std::sync::mpsc::channel();
                let client = cl.clone();
                let body = req.clone();
                std::thread::spawn(move || {
                    let _ = send.send(client.call(&body, time, true));
                });
                loop {
                    if interrupted.load(std::sync::atomic::Ordering::Relaxed) {
                        return Err(Error::transport("context canceled", false, reached));
                    }
                    match recv.recv_timeout(Duration::from_millis(10)) {
                        Ok(result) => break result,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(_) => {
                            return Err(Error::transport("RPC worker stopped", false, reached));
                        }
                    }
                }
            };
            attempted = true;
            let mut reason = "server unreachable";
            match result {
                Ok(rep)
                    if !(cmd != "wait"
                        && rep["exit"] == 5
                        && rep["stdout"]
                            .as_str()
                            .unwrap_or("")
                            .contains("outcome unknown (still running")) =>
                {
                    return Ok(rep);
                }
                Ok(rep) => {
                    last_reply = Some(rep.clone());
                    reason = "request still running";
                    if Instant::now() >= deadline {
                        if let Some(e) = last_error.take() {
                            return Err(e);
                        }
                        return Ok(rep);
                    }
                }
                Err(e) => {
                    reached |= e.reached;
                    if !e.retryable {
                        return Err(e);
                    }
                    last_error = Some(e.clone());
                    if Instant::now() >= deadline {
                        if cmd == "wait" && reached {
                            return Ok(wait_timeout(json_mode, args));
                        }
                        return Err(e);
                    }
                }
            }
            if !announced && !spoolable(&argv) {
                let end = announced_deadline
                    .replace_nanosecond(0)
                    .unwrap()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap();
                eprintln!(
                    "taskr: {reason}; retrying until {end}{}",
                    if cmd == "wait" {
                        String::new()
                    } else {
                        format!("; if interrupted, retry with: {retry}")
                    }
                );
                announced = true;
            }
            let until = (Instant::now() + backoff).min(deadline);
            while Instant::now() < until {
                if interrupted.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(Error::transport("context canceled", false, reached));
                }
                std::thread::sleep(
                    Duration::from_millis(10).min(until.saturating_duration_since(Instant::now())),
                );
            }
            backoff = (backoff * 2).min(Duration::from_secs(10));
            if cmd == "wait" {
                req["request_key"] = json!(new_key()?);
                let mut argv = argv.clone();
                let (_, args) = command(&argv);
                let offset = argv.len() - args.len();
                let ms = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .to_string();
                if let Some((_, at, inline)) = flag(args, "timeout") {
                    if at < args.len() {
                        argv[offset + at] = if inline {
                            format!("--timeout={ms}")
                        } else {
                            ms
                        };
                    }
                } else {
                    let at = argv.iter().position(|s| s == "--").unwrap_or(argv.len());
                    argv.splice(at..at, ["--timeout".into(), ms]);
                }
                req["argv"] = json!(argv);
            }
        }
    };
    let result = run();
    for s in signals {
        signal_hook::low_level::unregister(s);
    }
    result
}

fn spool_failure(e: Error, retry: &str) -> Error {
    let msg = if e.message.contains("spool full") {
        eprintln!("taskr: spool full");
        "server unreachable; spool full".to_string()
    } else {
        format!("server unreachable; spool write failed: {}", e.message)
    };
    eprintln!("taskr: server unreachable; retry with: {retry}");
    Error::transport(
        format!("server unreachable ({msg}); retry with: {retry}"),
        false,
        false,
    )
}

/// A watch renders on its invoking host, but a client's snapshot comes from the hub.
pub(crate) fn glance_watch_client()
-> Option<std::result::Result<GlanceWatchClient, (ExitCode, String)>> {
    if std::env::var("TASKR_DB").is_ok_and(|s| !s.is_empty()) {
        return None;
    }
    let dir = state_dir().ok()?;
    let raw = match std::fs::read_to_string(dir.join("server.url")) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(Err((ExitCode::Usage, format!("server.url: {e}")))),
    };
    let run = || -> Result<GlanceWatchClient> {
        Ok(GlanceWatchClient {
            client: rpc::Client::new(raw.lines().next().unwrap_or("").trim())?,
            cwd: std::env::current_dir().map_err(Error::io)?,
        })
    };
    Some(run().map_err(|e| (e.code, e.message)))
}
pub(crate) struct GlanceWatchClient {
    client: rpc::Client,
    cwd: PathBuf,
}
impl GlanceWatchClient {
    pub(crate) fn snapshot(
        &self,
        every: Duration,
    ) -> std::result::Result<Value, (ExitCode, String)> {
        let run = || -> Result<Value> {
            let argv = ["--json", "glance"].map(String::from);
            let rep = self
                .client
                .call(
                    &rpc::request(
                        &argv,
                        &self.cwd.to_string_lossy(),
                        &new_key()?,
                        Value::Null,
                        None,
                    ),
                    every.min(Duration::from_secs(10)),
                    true,
                )
                .map_err(|e| Error::new(ExitCode::Watch, "watch", e.message))?;
            if rep["exit"] != 0 {
                return Err(Error::new(
                    ExitCode::Watch,
                    "watch",
                    format!("glance: {}", rep["stdout"].as_str().unwrap_or("").trim()),
                ));
            }
            serde_json::from_str(rep["stdout"].as_str().unwrap_or(""))
                .map_err(|e| Error::new(ExitCode::Watch, "watch", e.to_string()))
        };
        run().map_err(|e| (e.code, e.message))
    }
}

fn transport_failure(mut e: Error, retry: &str) -> Error {
    if e.kind == "transport" {
        eprintln!("taskr: server unreachable; retry with: {retry}");
        e.message = format!("server unreachable ({}); retry with: {retry}", e.message);
    }
    e
}

fn wait_timeout(json_mode: bool, args: &[String]) -> Value {
    let as_id = flag(args, "as")
        .map(|(v, _, _)| v.to_string())
        .unwrap_or_else(|| std::env::var("TASKR_TASK").unwrap_or_default())
        .parse::<i64>()
        .unwrap_or(0);
    let json_mode = json_mode || flag_true(args, "json");
    json!({"exit":if json_mode {3}else{0},"stdout":if json_mode {format!("{}\n",compact_json(&json!({"timeout":true,"as":as_id,"unreachable":true})).unwrap())} else {"x1 3 timeout unreachable\n".into()},"stderr":""})
}

pub(crate) struct HubDiscovery {
    pub identity: crate::hub::HubIdentity,
    pub addresses: Vec<std::net::SocketAddr>,
    pub hosts: std::collections::BTreeSet<String>,
    pub tailscale_bin: PathBuf,
    pub url: String,
}
/// The Go tailscaleSelf/enableHub startup contract; the daemon binds addresses.
pub(crate) fn hub_discovery(port: u16) -> std::result::Result<HubDiscovery, String> {
    let run = || -> Result<HubDiscovery> {
        let (ip, me) = rpc::self_identity()?;
        let mut addresses = vec![std::net::SocketAddr::new(ip, port)];
        if let Some(ip6) = me.ip6 {
            addresses.push(std::net::SocketAddr::new(ip6.into(), port));
        }
        let mut hosts: std::collections::BTreeSet<String> =
            addresses.iter().map(ToString::to_string).collect();
        for name in [&me.short, &me.dns, &format!("{}.", me.dns)] {
            hosts.insert(format!("{name}:{port}"));
            if port == 80 {
                hosts.insert(name.clone());
            }
        }
        if port == 80 {
            hosts.insert(ip.to_string());
            if let Some(ip6) = me.ip6 {
                hosts.insert(format!("[{ip6}]"));
            }
        }
        Ok(HubDiscovery {
            url: format!("http://{}:{port}/", me.dns),
            identity: crate::hub::HubIdentity {
                node_id: me.node_id,
                machine: me.short,
                login: me.login,
            },
            addresses,
            hosts,
            tailscale_bin: rpc::tailscale_bin()?,
        })
    };
    run().map_err(|e| e.message)
}

/// Verified client-relay observation followed by ordered spool delivery.
pub(crate) fn daemon_host(
    raw: &str,
    kind: &str,
    agents: &Value,
    removed: &Value,
    epoch: Option<&str>,
    generation: i64,
    cwd: &Path,
) -> std::result::Result<Value, String> {
    let run = || -> Result<Value> {
        let cl = rpc::Client::new(raw)?;
        let mut argv = vec!["--json".into(), "_host".into(), kind.into()];
        if kind != "heartbeat" {
            argv.extend(["--agents".into(), compact_json(agents).unwrap()]);
        }
        if kind != "observe" {
            argv.extend([
                "--epoch".into(),
                epoch.unwrap_or_default().into(),
                "--base".into(),
                generation.to_string(),
            ]);
        }
        if kind == "delta" {
            argv.extend(["--removed".into(), compact_json(removed).unwrap()]);
        }
        let rep = cl.call(
            &rpc::request(
                &argv,
                &cwd.to_string_lossy(),
                &new_key()?,
                Value::Null,
                None,
            ),
            rpc::budget(&argv) + Duration::from_secs(10),
            true,
        )?;
        let mut result: Value = serde_json::from_str(
            rep["stdout"]
                .as_str()
                .unwrap_or("")
                .trim()
                .lines()
                .last()
                .unwrap_or(""),
        )
        .map_err(|e| Error::usage(format!("server observe reply: {e}")))?;
        if rep["exit"] != 0 {
            return Err(Error::rejected(format!(
                "server: {}",
                result["error"].as_str().unwrap_or("")
            )));
        }
        let dir = state_dir()?;
        if let Err(e) = spool::send(&dir, raw) {
            spool::log(&dir, &format!("spool send failed: {}", e.message));
        }
        result["spool"] = spool::summary(&dir);
        Ok(result)
    };
    run().map_err(|e| e.message)
}
/// Run after owner sounds and workspace/owner tokens, matching Go relay order.
pub(crate) fn daemon_notify_spool() -> std::result::Result<(), String> {
    state_dir()
        .and_then(|dir| spool::notify(&dir, &crate::write::herdr::socket()))
        .map_err(|e| e.message)
}
/// The read-only spool portion of client daemon --status; no directories created.
pub(crate) fn spool_summary() -> Value {
    state_dir().map_or(
        json!({"queued":0,"refused":0,"bad":0,"stuck":false}),
        |dir| spool::summary(&dir),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rpc_flags_keys_and_retry_quoting() {
        let args = ["--cwd", "--machine", "/tmp", "--", "--cwd", "/else"].map(String::from);
        assert_eq!(repeated(&args), None);
        assert_eq!(
            repeated(&["--cwd=/one", "-cwd=/two"].map(String::from)),
            Some("cwd")
        );
        assert!(valid_key("request_key-0123"));
        for bad in ["short", "../../queue.json", "contains space", "λ123456789"] {
            assert!(!valid_key(bad));
        }
        assert_eq!(
            shell_join(&["note", "a'b", "", "Þ 😀"].map(String::from)),
            "note 'a'\\''b' '' 'Þ 😀'"
        );
        assert!(!help(&["--", "--help"].map(String::from)));
        let mut args = ["--cwd", "sub", "--brief", "../b.md"]
            .map(String::from)
            .to_vec();
        paths("new", &mut args, Path::new("/tmp/client")).unwrap();
        assert_eq!(
            args,
            ["--cwd", "/tmp/client/sub", "--brief", "/tmp/client/b.md"]
        );
    }
}
