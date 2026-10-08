//! Resident event bridge. All state and sockets come from the selected HOME/ledger.
mod hub;
mod identity;
mod restart;
mod status;
mod subscription;
#[cfg(test)]
mod tests;
mod tokens;
use crate::write::herdr;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use taskr_core::{
    ExitCode, compact_json,
    db::{self, OptionalExtension, params},
    goflag::FlagSet,
    store::{self, Error, Result},
};
const VERSION: &str = match option_env!("TASKR_VERSION") {
    Some(s) => s,
    None => "dev",
};
fn database(e: impl std::fmt::Display) -> Error {
    Error {
        code: ExitCode::Database,
        message: e.to_string(),
    }
}
fn transport(e: impl std::fmt::Display) -> Error {
    Error {
        code: ExitCode::Transport,
        message: e.to_string(),
    }
}
fn meta(db: &db::Connection, key: &str) -> Result<Option<String>> {
    Ok(db
        .query_row("select value from meta where key=?", [key], |r| r.get(0))
        .optional()?)
}
fn set_meta(db: &db::Connection, key: &str, value: &str) -> Result<()> {
    db.execute("insert into meta(key,value) values(?,?) on conflict(key) do update set value=excluded.value",params![key,value])?;
    Ok(())
}
fn open() -> Result<db::Connection> {
    let path = db::path().map_err(store::usage)?;
    db::open(&path).map_err(database)
}
fn state_dir() -> Result<PathBuf> {
    let home = store::env("HOME");
    if home.is_empty() {
        return Err(store::usage("HOME is not set"));
    }
    Ok(Path::new(&home).join(".local/state/taskr"))
}
fn emit(json_mode: bool, v: Value) {
    println!(
        "{}{}",
        if json_mode { "" } else { "j1 " },
        compact_json(&v).unwrap()
    );
    let _ = std::io::stdout().flush();
}
fn lock_pid(path: &Path) -> i32 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}
struct Lock(fs::File);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.set_len(0);
    }
}
fn acquire(path: &Path) -> std::io::Result<Option<Lock>> {
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(path)?;
    match rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    f.set_len(0)?;
    let mut f = f;
    writeln!(f, "{}", std::process::id())?;
    Ok(Some(Lock(f)))
}
fn held(path: &Path) -> std::io::Result<bool> {
    let f = match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(false),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Err(e) => Err(e.into()),
    }
}
struct Log {
    file: Mutex<Option<fs::File>>,
    limited: Mutex<BTreeMap<String, (Instant, usize)>>,
}
impl Log {
    fn open(path: &Path) -> Self {
        Self {
            file: Mutex::new(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o644)
                    .open(path)
                    .ok(),
            ),
            limited: Mutex::new(BTreeMap::new()),
        }
    }
    fn line(&self, text: &str) {
        if let Some(file) = self.file.lock().unwrap().as_mut() {
            if file.metadata().is_ok_and(|m| m.len() > 1 << 20) {
                let _ = file.set_len(0);
            }
            let _ = writeln!(file, "{} {text}", store::now());
        }
    }
    fn limited(&self, key: &str, interval: Duration, text: &str) {
        let mut limited = self.limited.lock().unwrap();
        if let Some((at, n)) = limited.get_mut(key)
            && at.elapsed() < interval
        {
            *n += 1;
            return;
        }
        let skipped = limited
            .insert(key.into(), (Instant::now(), 0))
            .map_or(0, |(_, n)| n);
        drop(limited);
        self.line(&if skipped > 0 {
            format!("{text} ({skipped} similar suppressed)")
        } else {
            text.into()
        });
    }
}
pub(crate) fn dispatch(json_mode: bool, args: &[String]) -> Option<ExitCode> {
    if args.first().map(String::as_str) != Some("daemon") {
        return None;
    }
    Some(execute(json_mode, &args[1..], None))
}
pub(crate) fn client_dispatch(json_mode: bool, args: &[String], raw: &str) -> ExitCode {
    execute(json_mode, args, Some(raw))
}
fn execute(json_mode: bool, args: &[String], client: Option<&str>) -> ExitCode {
    let mut f = FlagSet::new("daemon", json_mode);
    f.bool("once", false, "run one observation pass and exit")
        .bool(
            "status",
            false,
            if client.is_some() {
                "print the mode, the server and the last call"
            } else {
                "print heartbeat age, pid and socket path"
            },
        )
        .bool(
            "restart",
            false,
            if client.is_some() {
                "restart the client daemon"
            } else {
                "stop the running daemon and start this binary's, detached"
            },
        )
        .bool(
            "stay",
            false,
            if client.is_some() {
                "local ledger host only"
            } else {
                "keep the local ledger serving without Herdr; wait for the daemon lock"
            },
        );
    let parsed = f.parse(args, 0, 0);
    let json_mode = f.json();
    if f.help() {
        print!("{}", f.usage(&crate::cli::usage_line("daemon")));
        return ExitCode::Ok;
    }
    let result = parsed
        .map_err(store::usage)
        .and_then(|()| run(json_mode, &f, args, client));
    match result {
        Ok(Some(mut value)) => {
            if let Some(code) = value
                .as_object_mut()
                .and_then(|v| v.remove("_code"))
                .and_then(|v| v.as_u64())
            {
                let code = match code {
                    2 => ExitCode::Usage,
                    4 => ExitCode::Database,
                    5 => ExitCode::Transport,
                    6 => ExitCode::Rejected,
                    _ => ExitCode::NotImplemented,
                };
                if json_mode {
                    if value.get("kind").is_some() {
                        eprintln!("taskr daemon: {}", value["error"].as_str().unwrap_or(""));
                    }
                    println!("{}", compact_json(&value).unwrap());
                } else if value.get("kind").is_some() {
                    let obj = value.as_object_mut().unwrap();
                    if let Some(v) = obj.remove("error") {
                        obj.insert("err".into(), v);
                    }
                    if let Some(v) = obj.remove("kind") {
                        obj.insert("k".into(), v);
                    }
                    println!("x1 {} {}", code as u8, compact_json(&value).unwrap());
                } else {
                    emit(json_mode, value);
                }
                code
            } else {
                emit(json_mode, value);
                ExitCode::Ok
            }
        }
        Ok(None) => ExitCode::Ok,
        Err(e) => crate::cli::error(json_mode, "daemon", &e.message, e.code),
    }
}
fn run(
    json_mode: bool,
    f: &FlagSet,
    args: &[String],
    client: Option<&str>,
) -> Result<Option<Value>> {
    let once = f.get_bool("once");
    let status = f.get_bool("status");
    let restart = f.get_bool("restart");
    let stay = f.get_bool("stay");
    if client.is_some() && stay {
        return Err(store::usage(
            "daemon --stay is only available in local mode",
        ));
    }
    if [once, status, restart].iter().filter(|v| **v).count() > 1 {
        return Err(store::usage(if client.is_some() {
            "daemon in client mode: use --once, --status, --restart, or neither"
        } else {
            "daemon: --once, --status and --restart are exclusive"
        }));
    }
    if stay && (once || status || restart) {
        return Err(store::usage(
            "daemon: --stay cannot be combined with --once, --status or --restart",
        ));
    }
    let dir = state_dir()?;
    fs::create_dir_all(&dir).map_err(database)?;
    let lock_path = dir.join("daemon.lock");
    let mut db = if client.is_none() {
        Some(open()?)
    } else {
        None
    };
    if status {
        return if let Some(raw) = client {
            client_status(&dir, &lock_path, raw).map(Some)
        } else {
            status::local(db.as_ref().unwrap(), &dir, &lock_path).map(Some)
        };
    }
    if restart {
        return restart::restart(db.as_ref(), &dir, &lock_path).map(Some);
    }
    let log = Arc::new(Log::open(&dir.join("daemon.log")));
    // Match Go's runtime: give the resident daemon its available descriptor budget.
    let mut nofile = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    if nofile.current != nofile.maximum {
        nofile.current = nofile.maximum;
        if let Err(error) = rustix::process::setrlimit(rustix::process::Resource::Nofile, nofile) {
            log.line(&format!("raise RLIMIT_NOFILE: {error}"));
        }
    }
    let sock = herdr::socket();
    let mut state = State {
        tokens: tokens::Tokens::default(),
        watch: Vec::new(),
        raw: client.map(String::from),
        sock: sock.clone(),
        dir: dir.clone(),
        log: log.clone(),
        missing: false,
    };
    if once {
        let (observed, notified, error) = state.pass(db.as_mut(), false);
        if let Some(error) = error {
            if client.is_some() {
                let mut out = json!({"ok":false,"once":true,"mode":"client","watch":if state.watch.is_empty(){Value::Null}else{json!(state.watch)},"error":error.message});
                out["_code"] = json!(5);
                return Ok(Some(out));
            }
            return Ok(Some(failed(
                error,
                json!({"ok":false,"once":true,"observed":observed,"notified":notified}),
            )));
        }
        return Ok(Some(if client.is_some() {
            json!({"ok":true,"once":true,"mode":"client","watch":if state.watch.is_empty(){Value::Null}else{json!(state.watch)}})
        } else {
            json!({"ok":true,"once":true,"observed":observed,"notified":notified})
        }));
    }
    let interrupted = Arc::new(AtomicBool::new(false));
    let signals = signals(&interrupted)?;
    let lock = loop {
        if let Some(lock) =
            acquire(&lock_path).map_err(|e| database(format!("daemon lock: {e}")))?
        {
            break lock;
        }
        if !stay {
            return Ok(Some(
                json!({"ok":true,"already_running":true,"pid":lock_pid(&lock_path)}),
            ));
        }
        if interrupted.load(Ordering::SeqCst) {
            return Ok(Some(Value::Null));
        }
        std::thread::park_timeout(Duration::from_millis(500));
    };
    state.missing = stay && find_bin("herdr").is_none();
    if state.missing {
        log.line(&format!("herdr missing; PATH={}", store::env("PATH")));
    }
    let mut record = identity::self_record(stay, state.missing);
    record.argv = std::iter::once(record.executable.clone())
        .chain(std::iter::once("daemon".into()))
        .chain(args.iter().cloned())
        .collect();
    if let Ok(proc) = identity::proc_identity(record.pid)
        && proc.argv.get(1).is_some_and(|s| s == "daemon")
    {
        record.argv = proc.argv;
    }
    let identity_path = dir.join(if client.is_some() {
        "client-daemon.json"
    } else {
        "daemon.json"
    });
    let guard = ResidentGuard {
        identity_path: identity_path.clone(),
        lock,
        signals,
    };
    if let Err(e) = record.write(&identity_path) {
        if client.is_some() {
            log.line(&format!("client daemon identity write failed: {e}"));
            let _ = fs::remove_file(&identity_path);
        } else {
            return Err(database(e));
        }
    }
    if let Some(db) = db.as_mut() {
        let res = store::transaction(db, |tx| {
            for (key, value) in [
                ("daemon_pid", record.pid.to_string()),
                ("daemon_exe", record.executable.clone()),
                ("daemon_proc_start", record.start_time.clone()),
                ("daemon_socket", sock.clone()),
                ("daemon_version", VERSION.into()),
                ("daemon_started_at", record.started_at.clone()),
            ] {
                set_meta(tx, key, &value)?
            }
            Ok(())
        });
        if let Err(e) = res {
            log.line(&format!("meta write failed: {}", e.message));
        }
        db.execute(
            "delete from meta where key in('dashboard_url','hub_tailnet_url')",
            [],
        )?;
    }
    log.line(&format!(
        "start pid {} version {VERSION} socket {sock}",
        record.pid
    ));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(database)?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let mut first = if let Some(raw) = client {
        json!({"ok":true,"pid":record.pid,"mode":"client","server":raw,"socket":sock})
    } else {
        json!({"ok":true,"pid":record.pid,"socket":sock,"log":dir.join("daemon.log").to_string_lossy()})
    };
    let hub = if let Some(db) = db.as_ref() {
        hub::start(
            &rt,
            db,
            &dir,
            stay,
            stopped.clone(),
            &mut first,
            log.clone(),
        )?
    } else {
        None
    };
    emit(json_mode, first);
    let reason = resident(
        &rt,
        &mut state,
        db.as_mut(),
        stay,
        stopped.clone(),
        &interrupted,
    );
    stop.send_replace(true);
    if let Some(hub) = hub {
        let _ = rt.block_on(hub);
    }
    rt.shutdown_timeout(Duration::from_secs(5));
    log.line(&format!("exit: {reason}"));
    if let Some(db) = db.as_ref() {
        db.execute(
            "delete from meta where key in('daemon_heartbeat','dashboard_url','hub_tailnet_url')",
            [],
        )?;
    }
    drop(guard);
    Ok(None)
}
struct ResidentGuard {
    identity_path: PathBuf,
    lock: Lock,
    signals: Vec<signal_hook::SigId>,
}
impl Drop for ResidentGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.identity_path);
        for sig in self.signals.drain(..) {
            signal_hook::low_level::unregister(sig);
        }
        let _ = &self.lock;
    }
}
fn signals(flag: &Arc<AtomicBool>) -> Result<Vec<signal_hook::SigId>> {
    let mut signals = vec![signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM];
    if !hup_ignored() {
        signals.push(signal_hook::consts::SIGHUP)
    }
    signals
        .into_iter()
        .map(|sig| signal_hook::flag::register(sig, flag.clone()).map_err(database))
        .collect()
}
#[cfg(target_os = "linux")]
fn hup_ignored() -> bool {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|line| {
                line.strip_prefix("SigIgn:")
                    .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
            })
        })
        .is_some_and(|mask| mask & 1 != 0)
}
#[cfg(not(target_os = "linux"))]
fn hup_ignored() -> bool {
    true
}

fn find_bin(name: &str) -> Option<PathBuf> {
    store::env("PATH")
        .split(':')
        .map(|p| Path::new(p).join(name))
        .find(|p| {
            fs::metadata(p).is_ok_and(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && m.permissions().mode() & 0o111 != 0
            })
        })
}
struct State {
    tokens: tokens::Tokens,
    watch: Vec<String>,
    raw: Option<String>,
    sock: String,
    dir: PathBuf,
    log: Arc<Log>,
    missing: bool,
}
impl State {
    fn pass(
        &mut self,
        db: Option<&mut db::Connection>,
        connected: bool,
    ) -> (usize, usize, Option<Error>) {
        if let Some(db) = db {
            if let Err(e) = store::inbox::expire(db) {
                self.log
                    .line(&format!("receipt expiry failed: {}", e.message));
            }
            if self.missing || !herdr::up(&self.sock) {
                let text = format!(
                    "Herdr server not reachable at {}; no herdr command run",
                    self.sock
                );
                self.log
                    .limited("no-server", Duration::from_secs(60), &text);
                return (0, 0, Some(transport(text)));
            }
            let result = crate::write::daemon_observe(db, &self.sock, Duration::from_secs(10));
            let (observed, error) = match result {
                Ok(n) => (n, None),
                Err(e) => {
                    self.log.line(&format!("observe failed: {}", e.message));
                    (0, Some(e))
                }
            };
            let notified = tokens::notify(db, &self.sock, &self.log).unwrap_or_else(|e| {
                self.log
                    .line(&format!("owner asks query failed: {}", e.message));
                0
            });
            let _ = self.tokens.panes(db, &self.sock, &self.log);
            if connected {
                let _ = set_meta(db, "daemon_heartbeat", &store::now());
            }
            match tokens::wanted_workspaces(db, None) {
                Ok(want) => {
                    self.tokens.workspaces(want, &self.sock, &self.log);
                }
                Err(e) => self
                    .log
                    .line(&format!("campaign token query failed: {}", e.message)),
            }
            match tokens::wanted_owner_asks(db, None) {
                Ok(want) => {
                    self.tokens.owner_asks(want, &self.sock, &self.log);
                }
                Err(e) => self
                    .log
                    .line(&format!("owner ask token query failed: {}", e.message)),
            }
            (observed, notified, error)
        } else {
            let result = self.relay();
            let mut value = json!({"last_call_at":store::now()});
            if let Err(e) = &result {
                value["last_error"] = json!(e.message);
                self.log.limited(
                    "relay",
                    Duration::from_secs(60),
                    &format!("observe failed: {}", e.message),
                );
            }
            let _ = fs::write(
                self.dir.join("client-state.json"),
                serde_json::to_vec(&value).unwrap(),
            );
            (0, 0, result.err())
        }
    }
    fn relay(&mut self) -> Result<()> {
        if !herdr::up(&self.sock) {
            return Err(transport(format!(
                "Herdr server not reachable at {}; nothing sent",
                self.sock
            )));
        }
        let output = herdr::command(&self.sock, &["agent", "list"], Duration::from_secs(10))
            .map_err(transport)?;
        if output.code != Some(0) {
            return Err(transport(format!(
                "herdr agent list failed: exit status {}",
                output.code.unwrap_or(-1)
            )));
        }
        let value: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| transport(format!("herdr agent list returned malformed JSON: {e}")))?;
        let agents = value["result"]["agents"]
            .as_array()
            .ok_or_else(|| transport("herdr agent list returned no result.agents"))?;
        let mut by_pane = BTreeMap::new();
        for a in agents {
            if a["pane_id"].as_str().unwrap_or("").is_empty()
                || a["agent_status"].as_str().unwrap_or("").is_empty()
            {
                return Err(transport(
                    "herdr agent list entry without pane_id or agent_status",
                ));
            }
            by_pane.insert(a["pane_id"].as_str().unwrap().to_string(),json!({"name":a["name"].as_str().unwrap_or(""),"agent_status":a["agent_status"],"state_change_seq":a["state_change_seq"].as_i64().unwrap_or(0),"pane_id":a["pane_id"]}));
        }
        let result = crate::net::daemon_observe(
            self.raw.as_ref().unwrap(),
            &json!(by_pane.into_values().collect::<Vec<_>>()),
            &self.dir,
        )
        .map_err(transport)?;
        self.watch = serde_json::from_value(result["watch"].clone()).unwrap_or_default();
        let asks = result["owner_asks"].as_array().map_or_else(Vec::new, |a| {
            a.iter()
                .map(|a| {
                    (
                        a["id"].as_i64().unwrap_or(0),
                        a["summary"].as_str().unwrap_or("").into(),
                    )
                })
                .collect()
        });
        let (_, notify_error) = tokens::notify_claimed(asks, &self.sock, &self.log);
        if let Some(map) = result["workspace_tokens"].as_object() {
            let want = map
                .iter()
                .map(|(key, v)| {
                    (
                        key.clone(),
                        tokens::WorkspaceToken {
                            campaign: v["campaign"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(String::from),
                            parent: v["parent"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(String::from),
                        },
                    )
                })
                .collect();
            self.tokens.workspaces(want, &self.sock, &self.log);
        }
        if result["owner_ask_tokens"].is_object() {
            let want =
                serde_json::from_value(result["owner_ask_tokens"].clone()).map_err(transport)?;
            self.tokens.owner_asks(want, &self.sock, &self.log);
        }
        if let Err(e) = crate::net::daemon_notify_spool() {
            self.log.limited(
                "spool-notify",
                Duration::from_secs(60),
                &format!("spool notification failed: {e}"),
            );
        }
        if let Some(error) = notify_error {
            return Err(error);
        }
        Ok(())
    }
}
fn client_status(dir: &Path, lock: &Path, raw: &str) -> Result<Value> {
    let mut out = json!({"ok":true,"mode":"client","server":raw,"spool":crate::net::spool_summary(),"running":false});
    let bytes = fs::read(dir.join("client-state.json"))
        .or_else(|_| fs::read(dir.join("client-daemon.json")));
    if let Ok(bytes) = bytes
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
    {
        for key in ["last_call_at", "last_error"] {
            if let Some(s) = value[key].as_str().filter(|s| !s.is_empty()) {
                out[key] = json!(s)
            }
        }
    }
    let pid = lock_pid(lock);
    if identity::alive(pid) {
        out["running"] = json!(true);
        out["pid"] = json!(pid);
        let rec = identity::Record::read(&dir.join("client-daemon.json"))
            .ok()
            .filter(|r| r.pid == pid && !r.version.is_empty());
        out["running_version"] = json!(rec.as_ref().map_or("unknown", |r| r.version.as_str()));
        out["stale"] = json!(rec.as_ref().is_none_or(|r| r.version != VERSION));
        if let Some(rec) = rec
            && !rec.started_at.is_empty()
        {
            out["started_at"] = json!(rec.started_at)
        }
    }
    Ok(out)
}
fn watched(db: &db::Connection) -> Result<Vec<String>> {
    watched_on(db, None)
}
fn watched_on(db: &db::Connection, host: Option<&str>) -> Result<Vec<String>> {
    let mut stmt=db.prepare("select distinct coalesce(l.pane_id,t.pane_id) as p from tasks t join launches l on l.id=t.current_launch_id where t.parent_id is not null and t.status!='closed' and t.role!='gate' and coalesce(l.pane_id,t.pane_id) is not null and l.present=1 and l.machine is ? order by p")?;
    Ok(stmt
        .query_map([host], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}
fn resident(
    rt: &tokio::runtime::Runtime,
    state: &mut State,
    mut db: Option<&mut db::Connection>,
    stay: bool,
    stopped: tokio::sync::watch::Receiver<bool>,
    interrupted: &AtomicBool,
) -> &'static str {
    let connected = Arc::new(AtomicBool::new(false));
    let (send, recv) = std::sync::mpsc::sync_channel(16);
    let panes = db
        .as_ref()
        .map_or_else(Vec::new, |db| watched(db).unwrap_or_default());
    let (pane_send, pane_recv) = tokio::sync::watch::channel(panes);
    rt.spawn(subscription::subscribe(
        state.sock.clone(),
        stay,
        pane_recv,
        stopped,
        send,
        connected.clone(),
        state.log.clone(),
    ));
    let mut heartbeat = Instant::now() + Duration::from_secs(15);
    let mut fallback = Instant::now() + Duration::from_secs(60);
    let mut relay = Instant::now() + Duration::from_secs(10);
    let mut last = Instant::now() - Duration::from_secs(1);
    if state.raw.is_some() {
        state.pass(None, false);
        pane_send.send_replace(state.watch.clone());
    }
    let mut pending = None;
    loop {
        if interrupted.load(Ordering::SeqCst) {
            return "signal";
        }
        let now = Instant::now();
        if now >= heartbeat {
            heartbeat = now + Duration::from_secs(15);
            if !stay && !Path::new(&state.sock).exists() {
                return "socket removed";
            }
            if connected.load(Ordering::SeqCst)
                && let Some(db) = db.as_ref()
            {
                let _ = set_meta(db, "daemon_heartbeat", &store::now());
            }
        }
        let periodic = if state.raw.is_some() && now >= relay {
            relay = now + Duration::from_secs(10);
            state.tokens.retry_workspaces();
            true
        } else {
            false
        };
        let fallback_due = now >= fallback;
        if fallback_due {
            fallback = now + Duration::from_secs(60);
            state.tokens.fallback();
        }
        let event = recv.recv_timeout(Duration::from_millis(50));
        match event {
            Ok(subscription::Wake::Gone) => return "socket removed",
            Ok(subscription::Wake::Attached) => {
                state.tokens.reattach();
                pending = Some(Instant::now() + Duration::from_millis(100));
            }
            Ok(subscription::Wake::Dirty) => {
                pending.get_or_insert_with(|| {
                    (Instant::now() + Duration::from_millis(100)).max(
                        last + Duration::from_millis(if state.raw.is_some() { 0 } else { 500 }),
                    )
                });
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return "socket removed",
            Err(_) => {}
        }
        if !periodic && !fallback_due && !pending.is_some_and(|p| Instant::now() >= p) {
            continue;
        }
        pending = None;
        while let Ok(event) = recv.try_recv() {
            if matches!(event, subscription::Wake::Gone) {
                return "socket removed";
            }
            if matches!(event, subscription::Wake::Attached) {
                state.tokens.reattach();
            }
        }
        last = Instant::now();
        state.pass(db.as_deref_mut(), connected.load(Ordering::SeqCst));
        let panes = if let Some(db) = db.as_ref() {
            watched(db).unwrap_or_else(|_| pane_send.borrow().clone())
        } else {
            state.watch.clone()
        };
        if *pane_send.borrow() != panes {
            pane_send.send_replace(panes);
        }
    }
}
fn failed(e: Error, mut output: Value) -> Value {
    output["_code"] = json!(e.code as u8);
    output["error"] = json!(e.message);
    output["kind"] = json!(match e.code {
        ExitCode::Usage => "usage",
        ExitCode::Database => "database",
        ExitCode::Rejected => "rejected",
        _ => "herdr",
    });
    output
}

/// Hub _host observe entry; caller host has already been admitted by RPC.
pub(crate) fn observe_host(db: &mut db::Connection, host: &str, agents: &Value) -> Result<Value> {
    if host.is_empty() {
        return Err(store::usage("_host is only for a client host over RPC"));
    }
    let agents = agents
        .as_array()
        .ok_or_else(|| store::usage("_host observe: --agents must be a JSON array"))?;
    let mut by_pane = BTreeMap::new();
    for a in agents {
        if !a.is_object()
            || ["name", "agent_status", "pane_id"]
                .iter()
                .any(|key| !a[*key].is_null() && !a[*key].is_string())
            || (!a["state_change_seq"].is_null() && !a["state_change_seq"].is_i64())
        {
            return Err(store::usage("_host observe: --agents must be a JSON array"));
        }
        if a["pane_id"].as_str().unwrap_or("").is_empty()
            || a["agent_status"].as_str().unwrap_or("").is_empty()
        {
            return Err(store::usage(
                "_host observe: an agent without pane_id or agent_status",
            ));
        }
        by_pane.insert(a["pane_id"].as_str().unwrap(), a.clone());
    }
    let agents = by_pane.into_values().collect::<Vec<_>>();
    set_meta(db, &format!("daemon_heartbeat:{host}"), &store::now())?;
    let count = crate::write::host_snapshot(db, host, &agents)?;
    let panes = watched_on(db, Some(host))?;
    let mut reply = json!({"ok":true,"observed":count,"watch":panes});
    if let Ok(workspaces) = tokens::wanted_workspaces(db, Some(host)) {
        let mut tokens = BTreeMap::new();
        for (workspace, t) in workspaces {
            if let Some(campaign) = t.campaign {
                let mut v = json!({"campaign":campaign});
                if let Some(parent) = t.parent.filter(|s| !s.is_empty()) {
                    v["parent"] = json!(parent)
                }
                tokens.insert(workspace, v);
            }
        }
        reply["workspace_tokens"] = json!(tokens);
    }
    if let Ok(asks) = tokens::wanted_owner_asks(db, Some(host)) {
        reply["owner_ask_tokens"] = json!(asks);
    }
    let asks = tokens::claim(db, Some(host))?;
    reply["owner_asks"] = if asks.is_empty() {
        Value::Null
    } else {
        json!(
            asks.into_iter()
                .map(|(id, summary)| json!({"id":id,"summary":summary}))
                .collect::<Vec<_>>()
        )
    };
    Ok(reply)
}

/// RPC audit diagnostics share the resident daemon's bounded log sink.
pub(crate) fn rpc_log(home: &Path, text: &str) {
    Log::open(&home.join(".local/state/taskr/daemon.log")).line(text);
}
