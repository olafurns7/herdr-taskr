//! `taskr push codex`: a local sidecar beside one Codex session. It runs the channel
//! delivery loop (lease, fenced offer, attempts, bounded redelivery, reconciliation)
//! with the queue sink: each push is `<codex> queue --thread T --message M`, argv only,
//! stdin null, `CODEX_HOME` set to the bound native home, output bounded, at most one in
//! flight, killed with its process group at the deadline. Exit 0 is a submission, never
//! a receipt. The thread and home are pinned at start: a worker's from its current
//! launch row (registered by `taskr start` inside the Codex session), a root's from the
//! typed operator flags. Launch metadata is trusted-host registration, not
//! authentication of a native session. The process itself never opens the ledger or
//! calls the hub; its ledger steps are children that route as usual.
use super::{Who, channel, identity, run};
use serde_json::Value;
use std::{
    io::Read,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use taskr_core::{ExitCode, goflag::FlagSet};

/// Bytes of each queue output stream kept for the log; the rest is drained unread.
const KEEP: usize = 2048;

pub(super) fn dispatch(json_mode: bool, keyed: bool, args: &[String]) -> ExitCode {
    let mut f = FlagSet::new("push", json_mode);
    f.int(
        "as",
        0,
        "a root orchestrator's own task id (a worker uses TASKR_TASK and TASKR_LAUNCH)",
    )
    .int(
        "session",
        0,
        "with --as: the root's generation, the event id `taskr adopt` printed (0 before any adopt)",
    )
    .string(
        "codex-thread",
        "",
        "a root only: the Codex thread id (a UUID) to queue into; a worker's comes from its launch",
    )
    .string(
        "codex-home",
        "",
        "a root only: that thread's Codex native home, an absolute existing directory; a worker's comes from its launch",
    )
    .string(
        "codex-bin",
        "codex",
        "the real Codex CLI (a path, or a name on PATH), resolved once at start",
    );
    let usage =
        |f: &FlagSet, message: &str| crate::cli::error(f.json(), "push", message, ExitCode::Usage);
    if let Err(e) = f.parse(args, 0, 1) {
        return usage(&f, &e);
    }
    let once = [
        "as",
        "session",
        "codex-thread",
        "codex-home",
        "codex-bin",
        "help",
    ];
    if let Some(k) = crate::write::repeated(args, &once) {
        return usage(&f, &format!("push: --{k} given more than once"));
    }
    if f.help() {
        print!("{}", f.usage(&crate::cli::usage_line("push")));
        return ExitCode::Ok;
    }
    if keyed {
        return usage(&f, "push is a local process; --request-key does not apply");
    }
    if f.positional.first().map(String::as_str) != Some("codex") {
        return usage(&f, "push needs a client: push codex");
    }
    let who = match identity(&f, "push") {
        Ok(who) => who,
        Err(message) => return usage(&f, &message),
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => return usage(&f, &format!("current directory: {e}")),
    };
    let bound = match who {
        Who::Worker { task, launch } => {
            if f.was_set("codex-thread") || f.was_set("codex-home") {
                return usage(
                    &f,
                    "a worker's Codex thread and home come from its launch; --codex-thread and --codex-home are for a root",
                );
            }
            match registered(&cwd, task, launch) {
                Ok(b) => b,
                Err((code, message)) => {
                    return crate::cli::error(f.json(), "push", &message, code);
                }
            }
        }
        Who::Root { .. } => {
            let (thread, home) = (f.get_string("codex-thread"), f.get_string("codex-home"));
            if !uuid(thread) {
                return usage(
                    &f,
                    "--as ROOT needs --codex-thread UUID: the Codex thread id, 8-4-4-4-12 hex digits",
                );
            }
            if !Path::new(home).is_absolute() || !Path::new(home).is_dir() {
                return usage(
                    &f,
                    "--as ROOT needs --codex-home PATH: the thread's native home, an absolute existing directory",
                );
            }
            Binding {
                thread: thread.into(),
                home: home.into(),
            }
        }
    };
    if !bound.home.is_dir() {
        let message = format!("native home {} is not a directory", bound.home.display());
        return crate::cli::error(f.json(), "push", &message, ExitCode::Rejected);
    }
    let bin = match resolve(f.get_string("codex-bin"), &cwd) {
        Ok(bin) => bin,
        Err(message) => return usage(&f, &message),
    };
    eprintln!(
        "taskr push codex: thread {}, CODEX_HOME {}, codex {}",
        bound.thread,
        bound.home.display(),
        bin.display()
    );
    if channel::serve(who, cwd, Queue { who, bin, bound }) {
        return crate::cli::error(
            f.json(),
            "push",
            "stopped for good: no more pushes (the reason is on stderr)",
            ExitCode::Rejected,
        );
    }
    ExitCode::Ok
}

#[derive(Clone, PartialEq)]
struct Binding {
    thread: String,
    home: PathBuf,
}

/// The queue sink: the pinned executable and binding.
#[derive(Clone)]
pub(super) struct Queue {
    who: Who,
    bin: PathBuf,
    bound: Binding,
}

pub(super) enum Rebind {
    /// The read failed or the home is missing: submit nothing until it reads again.
    Unreadable(String),
    /// Not the pinned binding any more: stop for good, never re-target.
    Changed(String),
}

impl Queue {
    /// A worker's launch row re-read (a root's binding is the operator's), and the home.
    pub(super) fn check(&self, cwd: &Path) -> Result<(), Rebind> {
        if let Who::Worker { task, launch } = self.who {
            let row =
                launch_row(cwd, task).map_err(|failed| Rebind::Unreadable(failed.to_string()))?;
            match binding_of(row.as_ref(), task, launch) {
                Err(why) => return Err(Rebind::Changed(why)),
                Ok(b) if b != self.bound => {
                    return Err(Rebind::Changed(format!(
                        "launch {launch} now has thread {} and home {}",
                        b.thread,
                        b.home.display()
                    )));
                }
                Ok(_) => {}
            }
        }
        if !self.bound.home.is_dir() {
            return Err(Rebind::Unreadable(format!(
                "native home {} is not a directory",
                self.bound.home.display()
            )));
        }
        Ok(())
    }

    /// One bounded `codex queue`: killed with its process group at `deadline` or on
    /// `cancel`. Draining the output is inside the same deadline; a pipe held open by a
    /// process outside the group is abandoned with its reader.
    pub(super) fn submit(&self, message: &str, deadline: Duration, cancel: &AtomicBool) -> Done {
        let start = Instant::now();
        let spawned = Command::new(&self.bin)
            .args([
                "queue",
                "--thread",
                &self.bound.thread,
                "--message",
                message,
            ])
            .env("CODEX_HOME", &self.bound.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                return Done {
                    how: How::Spawn(e.to_string()),
                    out: vec![],
                };
            }
        };
        let (send, read) = mpsc::channel();
        let pipes: [(&str, Option<Box<dyn Read + Send>>); 2] = [
            ("stdout", child.stdout.take().map(|p| Box::new(p) as _)),
            ("stderr", child.stderr.take().map(|p| Box::new(p) as _)),
        ];
        for (name, pipe) in pipes {
            let (Some(mut pipe), send) = (pipe, send.clone()) else {
                continue;
            };
            thread::spawn(move || {
                let (mut kept, mut total, mut buf) = (Vec::new(), 0, [0u8; 8192]);
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    total += n;
                    let room = KEEP.saturating_sub(kept.len()).min(n);
                    kept.extend_from_slice(&buf[..room]);
                }
                if total > 0 {
                    let text = String::from_utf8_lossy(&kept).trim_end().to_string();
                    let cut = if total > kept.len() {
                        format!(" ({total} bytes, the first {} kept)", kept.len())
                    } else {
                        String::new()
                    };
                    let _ = send.send(format!("{name} {text:?}{cut}"));
                }
            });
        }
        drop(send);
        let mut out = vec![];
        let mut how = None;
        loop {
            if how.is_none()
                && let Ok(Some(status)) = child.try_wait()
            {
                how = Some(status.code().map_or(How::Signal, How::Exit));
            }
            let tick = Duration::from_millis(10);
            if how.is_some() {
                match read.recv_timeout(tick) {
                    Ok(text) => out.push(text),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            } else {
                thread::sleep(tick);
            }
            let cancelled = cancel.load(Ordering::SeqCst);
            if cancelled || start.elapsed() >= deadline {
                taskr_core::store::terminate_group(child.id());
                if how.is_none() {
                    let _ = child.kill();
                    let _ = child.wait();
                    how = Some(if cancelled {
                        How::Cancelled
                    } else {
                        How::Timeout
                    });
                }
                // The group is gone; what its pipes still hold arrives at once.
                while let Ok(text) = read.recv_timeout(Duration::from_millis(200)) {
                    out.push(text);
                }
                break;
            }
        }
        Done {
            how: how.expect("settled"),
            out,
        }
    }
}

enum How {
    Spawn(String),
    Exit(i32),
    Signal,
    Timeout,
    Cancelled,
}
/// A finished submission, for the loop's settle and its log line.
pub(super) struct Done {
    how: How,
    out: Vec<String>,
}
impl Done {
    /// Exit 0 only: a submission, never a receipt. Every other ending after the spawn is
    /// unconfirmed (the message may have been queued); only a spawn failure proves no
    /// queue process ran.
    pub(super) fn submitted(&self) -> bool {
        matches!(self.how, How::Exit(0))
    }
}
impl std::fmt::Display for Done {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let unconfirmed = "unconfirmed, it may have been queued";
        match &self.how {
            How::Spawn(e) => write!(f, "codex queue could not start ({e}): nothing was queued")?,
            How::Exit(0) => write!(f, "codex queue exited 0: submitted, not received")?,
            How::Exit(n) => write!(f, "codex queue exited {n}: {unconfirmed}")?,
            How::Signal => write!(f, "codex queue died by a signal: {unconfirmed}")?,
            How::Timeout => write!(
                f,
                "codex queue passed its deadline and was killed: {unconfirmed}"
            )?,
            How::Cancelled => write!(f, "codex queue was killed by the stop: {unconfirmed}")?,
        }
        for text in &self.out {
            write!(f, "; {text}")?;
        }
        Ok(())
    }
}

/// The task's current launch record from `log` (a routed JSON read), or None.
fn launch_row(cwd: &Path, task: i64) -> Result<Option<Value>, Value> {
    let argv = vec!["log".into(), task.to_string(), "--limit".into(), "1".into()];
    let rows = run(cwd, argv, None).lines()?;
    Ok(rows
        .into_iter()
        .find(|r| r["record"] == "launch" && r["task_id"] == task))
}

fn registered(cwd: &Path, task: i64, launch: i64) -> Result<Binding, (ExitCode, String)> {
    let row = launch_row(cwd, task).map_err(|failed| {
        let code = match failed["outcome"].as_str() {
            Some("transport" | "unknown") => ExitCode::Transport,
            Some("rejected") => ExitCode::Rejected,
            Some("usage") => ExitCode::Usage,
            _ => ExitCode::Database,
        };
        (code, format!("reading launch {launch}: {failed}"))
    })?;
    binding_of(row.as_ref(), task, launch).map_err(|m| (ExitCode::Rejected, m))
}

/// A worker's binding: the current launch, provider codex, a thread_id session that is a
/// UUID, and an absolute native home.
fn binding_of(row: Option<&Value>, task: i64, launch: i64) -> Result<Binding, String> {
    let Some(row) = row.filter(|r| r["id"] == launch) else {
        return Err(format!(
            "launch {launch} is not the current launch of task {task}"
        ));
    };
    let field = |k: &str| row[k].as_str().unwrap_or("");
    if field("provider") != "codex" {
        return Err(format!(
            "launch {launch} is a {:?} launch, not codex",
            field("provider")
        ));
    }
    let (thread, home) = (field("session_ref"), field("native_home"));
    if thread.is_empty() || home.is_empty() {
        return Err(format!(
            "launch {launch} has no registered Codex session (thread and native home): run `taskr start` inside that Codex session first, with this task's TASKR_TASK and TASKR_LAUNCH"
        ));
    }
    if field("session_kind") != "thread_id" || !uuid(thread) {
        return Err(format!(
            "launch {launch} session {thread:?} (kind {:?}) is not a Codex thread UUID",
            field("session_kind")
        ));
    }
    if !Path::new(home).is_absolute() {
        return Err(format!(
            "launch {launch} native home {home:?} is not an absolute path"
        ));
    }
    Ok(Binding {
        thread: thread.into(),
        home: home.into(),
    })
}

fn uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// `--codex-bin`: a path (relative to the start directory) or a name on an absolute PATH
/// entry, pinned as the canonical executable file so later PATH changes do not matter.
fn resolve(bin: &str, cwd: &Path) -> Result<PathBuf, String> {
    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let found = if bin.contains('/') {
        Some(cwd.join(bin))
    } else if bin.is_empty() {
        None
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .filter(|d| d.is_absolute())
                .map(|d| d.join(bin))
                .find(|p| executable(p))
        })
    };
    found
        .filter(|p| executable(p))
        .and_then(|p| std::fs::canonicalize(p).ok())
        .ok_or_else(|| format!("--codex-bin {bin:?}: no executable file found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_bindings_are_typed() {
        let row = |provider: &str, kind: &str, thread: &str, home: &str| serde_json::json!({"id":8,"task_id":7,"provider":provider,"session_kind":kind,"session_ref":thread,"native_home":home});
        let t = "019a0f91-fbbd-7051-83dd-74a92bc55288";
        assert!(binding_of(Some(&row("codex", "thread_id", t, "/h")), 7, 8).is_ok());
        for (bad, launch) in [
            (row("claude", "thread_id", t, "/h"), 8),
            (row("codex", "id", t, "/h"), 8),
            (row("codex", "thread_id", "env-thread", "/h"), 8),
            (row("codex", "thread_id", t, "h"), 8),
            (row("codex", "thread_id", "", "/h"), 8),
            (row("codex", "thread_id", t, ""), 8),
            (row("codex", "thread_id", t, "/h"), 9),
        ] {
            assert!(binding_of(Some(&bad), 7, launch).is_err(), "{bad}");
        }
        assert!(binding_of(None, 7, 8).is_err());
        assert!(uuid(&t.to_uppercase()) && !uuid(&t[1..]) && !uuid(&t.replace('-', "x")));
    }
}
