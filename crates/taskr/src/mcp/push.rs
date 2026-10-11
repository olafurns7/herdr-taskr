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
    sync::atomic::{AtomicBool, Ordering},
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

    /// One bounded `codex queue` on this thread: killed with its process group at
    /// `deadline` or on `cancel`. Both output pipes are read without blocking inside the
    /// same deadline, and the operation closes them when it ends, so nothing outlives
    /// it. The parent stays unreaped (WNOWAIT) until then, so its group id cannot be
    /// reused before the group kill.
    pub(super) fn submit(&self, message: &str, deadline: Duration, cancel: &AtomicBool) -> Done {
        let start = Instant::now();
        if cancel.load(Ordering::SeqCst) {
            return Done::of(How::Spawn("stopped before the start".into()));
        }
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
            Err(e) => return Done::of(How::Spawn(e.to_string())),
        };
        let mut pipes = [
            Pipe::new("stdout", child.stdout.take().map(|p| Box::new(p) as _)),
            Pipe::new("stderr", child.stderr.take().map(|p| Box::new(p) as _)),
        ];
        let pid = rustix::process::Pid::from_child(&child);
        let tick = Duration::from_millis(10);
        let mut parent = None;
        let mut how = None;
        loop {
            pipes.iter_mut().for_each(Pipe::drain);
            let open = pipes.iter().any(|p| p.fd.is_some());
            if parent.is_none() {
                parent = exited(pid);
            }
            if let (Some(p), false) = (&parent, open) {
                how = Some(p.clone());
                break;
            }
            let cancelled = cancel.load(Ordering::SeqCst);
            if cancelled || start.elapsed() >= deadline {
                taskr_core::store::terminate_group(child.id());
                if parent.is_none() {
                    // It may have left its group; the wait below must not block.
                    let _ = child.kill();
                }
                // The group is gone: its pipe ends close at once unless a process
                // outside it holds them.
                let grace = Instant::now() + Duration::from_millis(200);
                while pipes.iter().any(|p| p.fd.is_some()) && Instant::now() < grace {
                    thread::sleep(tick);
                    pipes.iter_mut().for_each(Pipe::drain);
                }
                break;
            }
            thread::sleep(tick);
        }
        let _ = child.wait();
        let stranded = pipes.iter().any(|p| p.fd.is_some());
        let out = pipes.into_iter().filter_map(Pipe::text).collect();
        Done {
            // An ending with output still outstanding is unconfirmed, whatever the parent
            // did; its status stays a diagnostic.
            how: how.unwrap_or(if cancel.load(Ordering::SeqCst) {
                How::Cancelled
            } else {
                How::Timeout
            }),
            parent,
            out,
            stranded,
        }
    }
}

/// The parent's ending, if it has one, leaving it waitable.
fn exited(pid: rustix::process::Pid) -> Option<How> {
    use rustix::process::{WaitId, WaitIdOptions, waitid};
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    let status = waitid(WaitId::Pid(pid), options).ok()??;
    Some(status.exit_status().map_or(How::Signal, How::Exit))
}

/// One output pipe, nonblocking: the first KEEP bytes kept, the rest counted.
struct Pipe {
    name: &'static str,
    fd: Option<Box<dyn Read + Send>>,
    kept: Vec<u8>,
    total: usize,
}
impl Pipe {
    fn new(name: &'static str, fd: Option<Box<dyn ReadFd>>) -> Self {
        let fd = fd.and_then(|fd| {
            let flags = rustix::fs::fcntl_getfl(fd.as_fd()).ok()?;
            rustix::fs::fcntl_setfl(fd.as_fd(), flags | rustix::fs::OFlags::NONBLOCK).ok()?;
            Some(fd as Box<dyn Read + Send>)
        });
        Pipe {
            name,
            fd,
            kept: vec![],
            total: 0,
        }
    }
    /// Read what is there now, at most 64 KiB per call so a stream that never runs dry
    /// still returns to the caller's deadline and stop checks; EOF or an error closes it.
    fn drain(&mut self) {
        let Some(fd) = &mut self.fd else { return };
        let mut buf = [0u8; 8192];
        for _ in 0..8 {
            match fd.read(&mut buf) {
                Ok(0) => return self.fd = None,
                Ok(n) => {
                    self.total += n;
                    let room = KEEP.saturating_sub(self.kept.len()).min(n);
                    self.kept.extend_from_slice(&buf[..room]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return self.fd = None,
            }
        }
    }
    fn text(self) -> Option<String> {
        if self.total == 0 {
            return None;
        }
        let text = String::from_utf8_lossy(&self.kept).trim_end().to_string();
        let cut = if self.total > self.kept.len() {
            format!(
                " ({} bytes, the first {} kept)",
                self.total,
                self.kept.len()
            )
        } else {
            String::new()
        };
        Some(format!("{} {text:?}{cut}", self.name))
    }
}
trait ReadFd: Read + Send + std::os::fd::AsFd {}
impl<T: Read + Send + std::os::fd::AsFd> ReadFd for T {}

#[derive(Clone)]
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
    /// The parent's own ending when the operation ended otherwise (a diagnostic).
    parent: Option<How>,
    out: Vec<String>,
    /// A process outside the killed group still holds the output: cleanup is
    /// incomplete.
    pub(super) stranded: bool,
}
impl Done {
    fn of(how: How) -> Self {
        Done {
            how,
            parent: None,
            out: vec![],
            stranded: false,
        }
    }
    /// Exit 0 with all output closed: a submission, never a receipt. Every other
    /// ending after the spawn is unconfirmed (the message may have been queued); only a
    /// spawn failure proves no queue process ran.
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
        match &self.parent {
            Some(How::Exit(n)) if !matches!(self.how, How::Exit(_)) => {
                write!(f, " (it had exited {n} with its output still open)")?
            }
            Some(How::Signal) if !matches!(self.how, How::Signal) => {
                write!(f, " (it had died by a signal with its output still open)")?
            }
            _ => {}
        }
        if self.stranded {
            write!(f, "; a process outside its group still holds its output")?;
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
    fn a_drain_that_never_runs_dry_returns_within_its_budget() {
        struct Endless;
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(b'x');
                Ok(buf.len())
            }
        }
        let mut p = Pipe {
            name: "stdout",
            fd: Some(Box::new(Endless)),
            kept: vec![],
            total: 0,
        };
        for round in 1..=3 {
            p.drain();
            assert!(p.fd.is_some(), "an exhausted budget keeps the pipe open");
            assert_eq!(p.total, round * (64 << 10));
            assert_eq!(p.kept.len(), KEEP);
        }
    }
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
