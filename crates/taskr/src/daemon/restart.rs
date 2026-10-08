use super::*;
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
pub(super) fn restart(_db: Option<&db::Connection>, _dir: &Path, _lock: &Path) -> Result<Value> {
    Err(store::reject(
        "daemon --restart is unsupported on macOS in this build",
    ))
}
#[cfg(not(target_os = "macos"))]
pub(super) fn restart(db: Option<&db::Connection>, dir: &Path, lock: &Path) -> Result<Value> {
    let mut out = json!({"ok":true,"restarted":false});
    let mut args = vec!["daemon".to_string()];
    let record = |pid| {
        if let Some(db) = db {
            identity::running(db, dir, pid)
        } else {
            identity::client_running(dir, pid)
        }
    };
    let socket = || {
        if let Some(db) = db {
            meta(db, "daemon_socket")
                .map(|s| s.filter(|s| !s.is_empty()).unwrap_or_else(herdr::socket))
        } else {
            Ok(herdr::socket())
        }
    };
    if held(lock).map_err(database)? {
        let pid = lock_pid(lock);
        if let Err(e) = identity::verify(db, dir, pid) {
            return Ok(failed(e, json!({"pid":lock_pid(lock)})));
        }
        let old = record(pid)?;
        args.extend(old.args);
        identity::signal(pid).map_err(|e| store::reject(format!("stopping daemon {pid}: {e}")))?;
        out["old_pid"] = json!(pid);
        out["restarted"] = json!(true);
        out["old_version"] = json!(old.version);
        if old.supervised {
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end {
                let next = lock_pid(lock);
                if held(lock).unwrap_or(false)
                    && next > 0
                    && identity::verify(db, dir, next).is_ok()
                {
                    let r = record(next)?;
                    if !r.proc_start.is_empty() && r.proc_start != old.proc_start {
                        out["new_pid"] = json!(next);
                        out["version"] = json!(r.version);
                        out["started_at"] = json!(r.started_at);
                        out["supervised"] = json!(true);
                        out["started_detached"] = json!(false);
                        out["socket"] = json!(socket().unwrap_or_default());
                        return Ok(out);
                    }
                }
                std::thread::park_timeout(Duration::from_millis(50));
            }
            out["started_detached"] = json!(true);
        }
        let end = Instant::now() + Duration::from_secs(10);
        let mut freed = false;
        while Instant::now() < end {
            if held(lock).is_ok_and(|h| !h) {
                freed = true;
                break;
            }
            std::thread::park_timeout(Duration::from_millis(50));
        }
        if !freed {
            return Ok(failed(
                store::reject(format!(
                    "daemon {pid} did not release its lock within 10s; nothing started"
                )),
                out,
            ));
        }
    }
    let sock = socket()?;
    let exe = std::env::current_exe()
        .map_err(|e| store::usage(format!("cannot find this taskr binary: {e}")))?;
    let home = store::env("HOME");
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env_clear()
        .env("HOME", &home)
        .env("HERDR_SOCKET_PATH", &sock)
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if !store::env("PATH").is_empty() {
        cmd.env("PATH", store::env("PATH"));
    }
    let mut child =
        spawn_detached(&mut cmd).map_err(|e| transport(format!("starting the daemon: {e}")))?;
    let pid = child.id() as i32;
    out["new_pid"] = json!(pid);
    out["socket"] = json!(sock);
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().map_err(database)? {
            out["lock_pid"] = json!(lock_pid(lock));
            let error = if status.success() {
                "<nil>".into()
            } else {
                format!("exit status {}", status.code().unwrap_or(-1))
            };
            return Ok(failed(
                store::reject(format!(
                    "the new daemon {pid} exited at start ({error}); `taskr daemon --status` shows who holds the lock"
                )),
                out,
            ));
        }
        let r = record(pid)?;
        if r.version != "unknown" {
            out["version"] = json!(r.version);
            out["started_at"] = json!(r.started_at);
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(out);
        }
        if Instant::now() > end {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(failed(
                store::reject(format!(
                    "the new daemon {pid} did not record itself within 10s; check daemon.log"
                )),
                out,
            ));
        }
        std::thread::park_timeout(Duration::from_millis(50));
    }
}
#[allow(unsafe_code)]
fn spawn_detached(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe; the child callback allocates nothing after fork.
    unsafe {
        cmd.pre_exec(|| rustix::process::setsid().map(drop).map_err(Into::into));
    }
    cmd.spawn()
}
