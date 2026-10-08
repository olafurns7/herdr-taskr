use super::*;
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::{io::Write, os::unix::fs::OpenOptionsExt};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Record {
    pub pid: i32,
    #[serde(default)]
    pub executable: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub start_time: String,
    pub uid: Option<u32>,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub started_at: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stay: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub supervised: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub herdr_missing: bool,
}
fn is_false(b: &bool) -> bool {
    !b
}
impl Record {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        serde_json::from_slice(&fs::read(path)?).map_err(std::io::Error::other)
    }
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let temp = path.with_file_name(format!(
            ".{}-{}",
            path.file_name().unwrap().to_string_lossy(),
            std::process::id()
        ));
        let result = (|| {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            let mut bytes = serde_json::to_vec(self)?;
            bytes.push(b'\n');
            f.write_all(&bytes)?;
            f.sync_all()?;
            drop(f);
            fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}
#[derive(Debug)]
pub(super) struct Proc {
    pub exe: String,
    pub argv: Vec<String>,
    pub start: String,
    pub uid: u32,
}
#[cfg(target_os = "linux")]
pub(super) fn proc_identity(pid: i32) -> std::io::Result<Proc> {
    let dir = PathBuf::from(format!("/proc/{pid}"));
    let exe = fs::read_link(dir.join("exe"))?
        .to_string_lossy()
        .trim_end_matches(" (deleted)")
        .to_string();
    let cmd = fs::read(dir.join("cmdline"))?;
    let argv = cmd
        .strip_suffix(&[0])
        .unwrap_or(&cmd)
        .split(|b| *b == 0)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();
    let stat = fs::read_to_string(dir.join("stat"))?;
    let start = stat
        .rsplit_once(')')
        .and_then(|(_, s)| s.split_whitespace().nth(19))
        .ok_or_else(|| std::io::Error::other("unreadable /proc stat"))?
        .to_string();
    Ok(Proc {
        exe,
        argv,
        start,
        uid: fs::metadata(dir)?.uid(),
    })
}
#[cfg(target_os = "macos")]
pub(super) fn proc_identity(pid: i32) -> std::io::Result<Proc> {
    super::macos::proc_identity(pid)
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn proc_identity(_pid: i32) -> std::io::Result<Proc> {
    Err(std::io::Error::other(
        "daemon process identity is unsupported on this platform",
    ))
}
pub(super) fn alive(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}
pub(super) fn signal(pid: i32) -> std::io::Result<()> {
    let pid =
        rustix::process::Pid::from_raw(pid).ok_or_else(|| std::io::Error::other("invalid pid"))?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).map_err(Into::into)
}
pub(super) fn self_record(stay: bool, missing: bool) -> Record {
    let pid = std::process::id() as i32;
    let proc = proc_identity(pid).ok();
    let executable = proc
        .as_ref()
        .map(|p| PathBuf::from(&p.exe))
        .or_else(|| std::env::current_exe().ok())
        .and_then(|p| fs::canonicalize(p).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    Record {
        pid,
        executable,
        argv: proc
            .as_ref()
            .map_or_else(|| std::env::args().collect(), |p| p.argv.clone()),
        start_time: proc.as_ref().map(|p| p.start.clone()).unwrap_or_default(),
        uid: Some(
            proc.as_ref()
                .map_or_else(|| rustix::process::getuid().as_raw(), |p| p.uid),
        ),
        version: VERSION.into(),
        started_at: store::now(),
        stay,
        supervised: !store::env("INVOCATION_ID").is_empty()
            && store::env("SYSTEMD_EXEC_PID") == pid.to_string(),
        herdr_missing: missing,
    }
}
#[derive(Default)]
pub(super) struct Running {
    pub version: String,
    pub started_at: String,
    pub args: Vec<String>,
    pub stay: bool,
    pub supervised: bool,
    pub herdr_missing: bool,
    pub proc_start: String,
}
pub(super) fn running(db: &db::Connection, dir: &Path, pid: i32) -> Result<Running> {
    let mut r = Running {
        version: "unknown".into(),
        ..Running::default()
    };
    if meta(db, "daemon_pid")?.as_deref() != Some(&pid.to_string()) {
        return Ok(r);
    }
    if let Some(v) = meta(db, "daemon_version")? {
        r.version = v
    }
    r.started_at = meta(db, "daemon_started_at")?.unwrap_or_default();
    if let Ok(id) = Record::read(&dir.join("daemon.json"))
        && id.pid == pid
        && id.argv.len() >= 2
    {
        r.args = id.argv[2..].to_vec();
        r.stay = id.stay;
        r.supervised = id.supervised;
        r.proc_start = id.start_time;
        r.herdr_missing = id.herdr_missing;
    }
    Ok(r)
}
pub(super) fn client_running(dir: &Path, pid: i32) -> Result<Running> {
    let r = match Record::read(&dir.join("client-daemon.json")) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Running {
                version: "unknown".into(),
                ..Running::default()
            });
        }
        Err(e) => return Err(database(e)),
    };
    if r.pid != pid || r.version.is_empty() || r.argv.len() < 2 {
        return Ok(Running {
            version: "unknown".into(),
            ..Running::default()
        });
    }
    Ok(Running {
        version: r.version,
        started_at: r.started_at,
        args: r.argv[2..].to_vec(),
        ..Running::default()
    })
}
fn verify_os(
    pid: i32,
    exe: &str,
    start: &str,
    argv: Option<&[String]>,
    uid: Option<u32>,
) -> Result<i32> {
    let id = proc_identity(pid).map_err(|e| {
        store::reject(format!(
            "cannot read the identity of pid {pid} ({e}); not signalled"
        ))
    })?;
    let cur = fs::canonicalize(&id.exe)
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let reason = if cur.as_deref() != Some(exe) {
        "does not run the recorded daemon executable"
    } else if id.argv.get(1).map(String::as_str) != Some("daemon") {
        "is not running `taskr daemon`"
    } else if argv.is_some_and(|v| v != id.argv) {
        "has a different argv than the recorded client daemon"
    } else if id.start != start {
        "started at a different time than the recorded daemon (a reused pid)"
    } else if uid.is_some_and(|uid| uid != id.uid) {
        "belongs to a different user than the recorded daemon"
    } else if id.uid != rustix::process::getuid().as_raw() {
        "belongs to another user"
    } else {
        return Ok(pid);
    };
    Err(store::reject(format!("pid {pid} {reason}; not signalled")))
}
fn valid_pid(pid: i32) -> Result<()> {
    if pid <= 0 {
        return Err(store::reject(
            "the daemon lock is held but records no pid; nothing signalled",
        ));
    }
    if pid == std::process::id() as i32 {
        return Err(store::reject(format!(
            "the daemon lock is held by this process {pid}; nothing signalled"
        )));
    }
    Ok(())
}
fn verify_record(rec: &Record, pid: i32) -> Result<i32> {
    valid_pid(pid)?;
    if rec.pid != pid {
        return Err(store::reject(format!(
            "the daemon lock names pid {pid} but the identity record names daemon pid {}; not signalled",
            rec.pid
        )));
    }
    if rec.executable.is_empty()
        || rec.start_time.is_empty()
        || rec.argv.len() < 2
        || rec.uid.is_none()
    {
        return Err(store::reject(format!(
            "the client daemon identity record is incomplete for daemon {pid}; not signalled"
        )));
    }
    verify_os(
        pid,
        &rec.executable,
        &rec.start_time,
        Some(&rec.argv),
        rec.uid,
    )
}
pub(super) fn verify(db: Option<&db::Connection>, dir: &Path, pid: i32) -> Result<i32> {
    let Some(db) = db else {
        let rec = Record::read(&dir.join("client-daemon.json")).map_err(|e| {
            store::reject(format!(
                "cannot read the client daemon identity record ({e}); not signalled"
            ))
        })?;
        return verify_record(&rec, pid);
    };
    valid_pid(pid)?;
    let meta_pid = meta(db, "daemon_pid")?
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);
    if meta_pid != pid {
        return Err(store::reject(format!(
            "the daemon lock names pid {pid} but the ledger records daemon pid {meta_pid} (a daemon before v0.6 records none); not signalled: stop it by hand, then run daemon --restart"
        )));
    }
    let exe = meta(db, "daemon_exe")?.unwrap_or_default();
    let start = meta(db, "daemon_proc_start")?.unwrap_or_default();
    if exe.is_empty() || start.is_empty() {
        return Err(store::reject(format!(
            "the ledger holds no executable or start time for daemon {pid}; not signalled"
        )));
    }
    verify_os(pid, &exe, &start, None, None)?;
    let path = dir.join("daemon.json");
    match Record::read(&path) {
        Ok(rec) => verify_record(&rec, pid).map_err(|e| {
            store::reject(format!(
                "local daemon identity record {}: {}",
                path.display(),
                e.message
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(pid),
        Err(_) => Err(store::reject(format!(
            "cannot read the local daemon identity record {}; not signalled",
            path.display()
        ))),
    }
}
