//! HTTP hub. Browser routes and peer push are retired by the migration scope.
mod child;
mod documents;
mod events;
mod hidden;
mod host;
mod http;
mod protocol;
pub(crate) mod usage;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use protocol::{RpcReply, RpcRequest};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::{IpAddr, SocketAddr, TcpListener},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use taskr_core::{
    compact_json,
    db::{self, OptionalExtension},
    store,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Mutex, watch},
    task::JoinSet,
};

pub(crate) use child::dispatch as child_dispatch;
pub(crate) use child::document_set_input;

#[derive(Clone)]
pub(crate) struct HubIdentity {
    pub node_id: String,
    pub machine: String,
    pub login: String,
}

pub(crate) struct HubConfig {
    pub listeners: Vec<TcpListener>,
    pub db_path: PathBuf,
    pub clock: fn() -> time::OffsetDateTime,
    pub identity: Option<HubIdentity>,
    pub hosts: BTreeSet<String>,
    pub home: PathBuf,
    pub log: Arc<crate::daemon::Log>,
    pub tailscale_bin: PathBuf,
}

struct Hub {
    cfg: HubConfig,
    whois: Mutex<BTreeMap<IpAddr, (time::OffsetDateTime, Option<HubIdentity>)>>,
    usage: usage::Usage,
    events: Arc<events::Events>,
    shutdown: watch::Receiver<bool>,
    stored_requests: StdMutex<JoinSet<()>>,
    reapers: Arc<StdMutex<JoinSet<()>>>,
}

pub(crate) async fn serve(
    mut cfg: HubConfig,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let listeners = std::mem::take(&mut cfg.listeners);
    let (stop, stopped) = watch::channel(false);
    let events = events::Events::shared(&cfg.db_path);
    let hub = Arc::new(Hub {
        cfg,
        whois: Mutex::default(),
        usage: usage::Usage::default(),
        events,
        shutdown: stopped,
        stored_requests: StdMutex::default(),
        reapers: Arc::default(),
    });
    let mut servers = JoinSet::new();
    for listener in listeners {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let stopped = hub.shutdown.clone();
        let router = Router::new().fallback(handle).with_state(hub.clone());
        servers.spawn(http::listen(listener, router, stopped, hub.cfg.log.clone()));
    }
    let mut minute = tokio::time::interval(Duration::from_secs(60));
    minute.tick().await;
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = minute.tick() => if let Err(e)=flush(hub.clone()).await {
                crate::daemon::rpc_log(&hub.cfg.home,&format!("dashboard usage flush failed: {e}"));
            },
            result = servers.join_next(), if !servers.is_empty() => {
                if let Some(result) = result { result??; }
            }
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    stop.send_replace(true);
    drain(&mut servers, deadline).await;
    let mut stored_requests =
        std::mem::take(&mut *hub.stored_requests.lock().expect("stored requests"));
    drain(&mut stored_requests, deadline).await;
    let mut reapers = std::mem::take(&mut *hub.reapers.lock().expect("reapers"));
    drain(
        &mut reapers,
        tokio::time::Instant::now() + Duration::from_secs(1),
    )
    .await;
    flush(hub).await
}

async fn drain<T: 'static>(tasks: &mut JoinSet<T>, deadline: tokio::time::Instant) {
    let _ = tokio::time::timeout_at(deadline, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

async fn flush(hub: Arc<Hub>) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        let mut db = db::open_migrated(&hub.cfg.db_path).map_err(anyhow::Error::msg)?;
        hub.usage
            .flush(&mut db, (hub.cfg.clock)())
            .map_err(|e| anyhow::Error::msg(e.message))
    })
    .await?
}

fn response(status: StatusCode, value: Value) -> Response {
    (
        status,
        [("content-type", "application/json; charset=utf-8")],
        format!("{}\n", compact_json(&value).expect("JSON")),
    )
        .into_response()
}

fn http_error(status: StatusCode, error: impl AsRef<str>) -> Response {
    response(status, json!({"ok":false,"error":error.as_ref()}))
}

async fn handle(
    State(hub): State<Arc<Hub>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response {
    let mut result = handle_inner(&hub, peer, req).await;
    hub.usage
        .record(result.status().as_u16(), (hub.cfg.clock)());
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
    ] {
        result
            .headers_mut()
            .insert(name, value.parse().expect("header"));
    }
    result
}

async fn handle_inner(hub: &Arc<Hub>, peer: SocketAddr, req: Request<Body>) -> Response {
    let ip = peer.ip().to_canonical();
    let loopback = if fixture() {
        ip == IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    } else {
        ip.is_loopback()
    };
    let identity = if loopback {
        None
    } else {
        if hub.cfg.identity.is_none() {
            return http_error(StatusCode::FORBIDDEN, "this dashboard serves loopback only");
        }
        match admit(hub, ip).await {
            Some(id) => Some(id),
            None => return http_error(StatusCode::FORBIDDEN, "this tailnet node is not admitted"),
        }
    };
    let headers = req.headers().clone();
    let header = |key: &str| headers.get(key).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !hub.cfg.hosts.contains(&header("host").to_lowercase()) {
        return http_error(StatusCode::MISDIRECTED_REQUEST, "unexpected Host header");
    }
    if req.uri().path() == "/api/events" {
        if loopback {
            let host = header("host");
            let local_host = host.rsplit_once(':').map(|(h, _)| h);
            if !matches!(local_host, Some("127.0.0.1" | "[::1]" | "localhost")) {
                return http_error(
                    StatusCode::MISDIRECTED_REQUEST,
                    "local events need a loopback Host",
                );
            }
        } else {
            let (Some(identity), Some(server)) = (identity.as_ref(), hub.cfg.identity.as_ref())
            else {
                return http_error(StatusCode::FORBIDDEN, "events need a tailnet identity");
            };
            if identity.node_id == server.node_id
                || identity.machine.is_empty()
                || identity.machine == store::local_machine()
            {
                return http_error(StatusCode::FORBIDDEN, "this node is the server");
            }
        }
        return events::handle(hub, req, ip, loopback).await;
    }
    if req.uri().path() != "/api/rpc" {
        return (StatusCode::NOT_FOUND, "404 page not found\n").into_response();
    }
    if req.method() != axum::http::Method::POST {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [("allow", "POST")],
            "Method Not Allowed\n",
        )
            .into_response();
    }
    let (Some(identity), Some(server)) = (identity, hub.cfg.identity.as_ref()) else {
        return http_error(StatusCode::FORBIDDEN, "rpc needs a tailnet identity");
    };
    if identity.node_id == server.node_id {
        return http_error(StatusCode::FORBIDDEN, "this node is the server");
    }
    if identity.machine.is_empty() || identity.machine == store::local_machine() {
        return http_error(
            StatusCode::FORBIDDEN,
            "the node's name is the server's name",
        );
    }
    if !header("origin").is_empty() || !header("sec-fetch-site").is_empty() {
        return http_error(StatusCode::FORBIDDEN, "browser requests are refused");
    }
    if !header("content-type")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return http_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content type must be application/json",
        );
    }
    if header("x-taskr-rpc") != "1" {
        return http_error(StatusCode::BAD_REQUEST, "X-Taskr-RPC: 1 is required");
    }
    let deadline = req.extensions().get::<http::ReadDeadline>().map_or_else(
        || tokio::time::Instant::now() + Duration::from_secs(10),
        |v| v.0,
    );
    let bytes = match tokio::time::timeout_at(
        deadline,
        to_bytes(req.into_body(), protocol::BODY_MAX),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return http_error(StatusCode::PAYLOAD_TOO_LARGE, "request too large"),
        Err(_) => return http_error(StatusCode::BAD_REQUEST, "bad request: read timeout"),
    };
    let req: RpcRequest = match protocol::decode(&bytes) {
        Ok(req) => req,
        Err(e) => return http_error(StatusCode::BAD_REQUEST, format!("bad request: {e}")),
    };
    if req.argv.is_empty() {
        return http_error(StatusCode::BAD_REQUEST, "argv is empty");
    }
    if !std::path::Path::new(&req.cwd).is_absolute() {
        return http_error(StatusCode::BAD_REQUEST, "cwd must be an absolute path");
    }
    if !(8..=128).contains(&req.request_key.len())
        || !req
            .request_key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return http_error(
            StatusCode::BAD_REQUEST,
            "request_key must match [A-Za-z0-9_-]{8,128}",
        );
    }
    let log_machine = identity.machine.clone();
    let log_key = req.request_key.clone();
    let name = protocol::command(&req.argv).0;
    let log_name =
        if crate::cli::known(name) || matches!(name, "_host" | "_hook" | "_prompt" | "_doc") {
            name
        } else {
            "unknown"
        }
        .to_string();
    let rep = if protocol::stored(&req.argv) {
        // Stored writes survive a disconnected caller; only shutdown/budget cancels them.
        let (send, reply) = tokio::sync::oneshot::channel();
        {
            let hub_task = hub.clone();
            let mut tasks = hub.stored_requests.lock().expect("stored requests");
            while tasks.try_join_next().is_some() {}
            tasks.spawn(async move {
                let _ = send.send(stored(hub_task, identity.machine, req).await);
            });
        }
        match reply.await {
            Ok(reply) => reply,
            Err(_) => RpcReply {
                exit: 4,
                stderr: "taskr: internal error; inspect `taskr log` before a retry\n".into(),
                ..RpcReply::default()
            },
        }
    } else {
        run(hub, &identity.machine, &req).await.into_reply()
    };
    if !matches!(log_name.as_str(), "glance" | "campaign" | "slotr") || rep.exit != 0 {
        crate::daemon::rpc_log(
            &hub.cfg.home,
            &format!(
                "rpc: machine={log_machine} cmd={log_name} key={log_key} exit={}",
                rep.exit
            ),
        );
    }
    // Go's struct order is part of the wire contract.
    (
        StatusCode::OK,
        [("content-type", "application/json; charset=utf-8")],
        format!(
            "{}\n",
            taskr_core::escape_json(&serde_json::to_string(&rep).expect("reply"))
        ),
    )
        .into_response()
}

#[cfg(feature = "contract")]
fn fixture() -> bool {
    std::env::var("TASKR_CONTRACT_TAILNET").is_ok_and(|s| s == "1")
        && std::env::var("TASKR_CONTRACT_ORACLE").is_ok_and(|s| s == "1")
}
#[cfg(not(feature = "contract"))]
fn fixture() -> bool {
    false
}

async fn admit(hub: &Hub, ip: IpAddr) -> Option<HubIdentity> {
    {
        let cache = hub.whois.lock().await;
        if let Some((at, result)) = cache.get(&ip)
            && (hub.cfg.clock)() - *at
                < if fixture() {
                    time::Duration::ZERO
                } else {
                    time::Duration::seconds(60)
                }
        {
            return result.clone();
        }
    }
    let result = async {
        let mut child = Command::new(&hub.cfg.tailscale_bin)
            .args(["whois", "--json", &ip.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()?
            .take((4 << 20) + 1)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        if bytes.len() > 4 << 20 || !child.wait().await.ok()?.success() {
            return None;
        }
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        let node = value.get("Node")?;
        let node_id = node["StableID"].as_str().filter(|s| !s.is_empty())?;
        let name = node["Name"].as_str().filter(|s| !s.is_empty())?;
        if node
            .get("Tags")
            .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|v| !v.is_empty()))
        {
            return None;
        }
        let login = value["UserProfile"]["LoginName"].as_str()?;
        if login != hub.cfg.identity.as_ref()?.login {
            return None;
        }
        Some(HubIdentity {
            node_id: clip(node_id, 64),
            machine: clip(
                name.trim_end_matches('.')
                    .split('.')
                    .next()?
                    .to_lowercase()
                    .as_str(),
                63,
            ),
            login: login.into(),
        })
    };
    let result = tokio::time::timeout(Duration::from_secs(3), result)
        .await
        .ok()
        .flatten();
    let mut cache = hub.whois.lock().await;
    // Match Go's independently capped positive/negative caches.
    if cache
        .values()
        .filter(|(_, v)| v.is_some() == result.is_some())
        .count()
        >= 1024
    {
        cache.retain(|_, (_, v)| v.is_some() != result.is_some());
    }
    cache.insert(ip, ((hub.cfg.clock)(), result.clone()));
    result
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    } else {
        s.into()
    }
}

async fn stored(hub: Arc<Hub>, machine: String, req: RpcRequest) -> RpcReply {
    let db_hub = hub.clone();
    let key = req.request_key.clone();
    let hash = taskr_core::request_hash(Some(&req.argv), req.document.as_ref()).expect("hash");
    let claim_machine = machine.clone();
    let rerun_busy = crate::net::spoolable(&req.argv);
    let claim = tokio::task::spawn_blocking(move || -> store::Result<Option<RpcReply>> {
        let mut db = db::open_migrated(&db_hub.cfg.db_path).map_err(|message| store::Error { code: taskr_core::ExitCode::Database, message })?;
        store::transaction(&mut db, |tx| {
            tx.execute("delete from requests where created_at < ?", [store::stamp((db_hub.cfg.clock)() - time::Duration::days(7))])?;
            let old = tx.query_row("select coalesce(machine,''),argv_sha,state,coalesce(exit,0),coalesce(stdout,''),coalesce(stderr,''),upload from requests where key=?", [&key], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i32>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, Option<String>>(6)?))).optional()?;
            if let Some((host, sha, state, exit, stdout, stderr, upload)) = old {
                if host != claim_machine || sha != hash { return Err(store::reject(format!("request key {key} belongs to another command"))); }
                if state != "done" { return Err(store::Error { code: taskr_core::ExitCode::Transport, message: format!("request {key}: outcome unknown (still running, or the server stopped during it); inspect `taskr log` before any resend") }); }
                if !(rerun_busy && db::busy_reply(exit.into(), &crate::net::reply_error(&json!({"exit":exit,"stdout":stdout,"stderr":stderr})))) {
                    return Ok(Some(RpcReply { exit, stdout, stderr, upload: upload.map(|s| serde_json::from_str(&s)).transpose()?.flatten() }));
                }
                tx.execute("delete from requests where key=?", [&key])?;
            }
            tx.execute("insert into requests(key,machine,argv_sha,state,created_at) values(?,?,?,'running',?)", db::params![key, claim_machine, hash, store::stamp((db_hub.cfg.clock)())])?;
            Ok(None)
        })
    }).await;
    match claim {
        Ok(Ok(Some(mut prior))) => {
            if !req.capabilities.iter().any(|s| s == "doc-upload") {
                prior.upload = None;
            }
            return prior;
        }
        Ok(Ok(None)) => {}
        Ok(Err(e)) => {
            return protocol::error(
                &req,
                e.code as i32,
                if e.code == taskr_core::ExitCode::Rejected {
                    "rejected"
                } else if e.code == taskr_core::ExitCode::Transport {
                    "herdr"
                } else {
                    "database"
                },
                &e.message,
            );
        }
        Err(e) => return protocol::error(&req, 4, "database", &e.to_string()),
    }
    let result = run(&hub, &machine, &req).await;
    if matches!(&result, Execution::Unknown(_)) {
        return result.into_reply();
    }
    let exited = matches!(&result, Execution::Exited(_));
    let rep = result.into_reply();
    let exited = exited
        && !(crate::net::spoolable(&req.argv)
            && db::busy_reply(
                rep.exit.into(),
                &crate::net::reply_error(
                    &json!({"exit":rep.exit,"stdout":rep.stdout,"stderr":rep.stderr}),
                ),
            ));
    let upload = serde_json::to_string(&rep.upload).expect("upload");
    let (exit, stdout, stderr) = (rep.exit, rep.stdout.clone(), rep.stderr.clone());
    let _ = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let db = db::open_migrated(&hub.cfg.db_path)?;
        db.busy_timeout(Duration::from_secs(30))
            .map_err(|e| db::error_text(&e))?;
        if exited {
            db.execute(
                "update requests set state='done',exit=?,stdout=?,stderr=?,upload=? where key=?",
                db::params![exit, stdout, stderr, upload, req.request_key],
            )
            .map_err(|e| e.to_string())?;
        } else {
            db.execute(
                "delete from requests where key=? and state='running'",
                [req.request_key],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await;
    rep
}

enum Execution {
    Exited(RpcReply),
    NotStarted(RpcReply),
    Unknown(RpcReply),
}
impl Execution {
    fn into_reply(self) -> RpcReply {
        match self {
            Self::Exited(reply) | Self::NotStarted(reply) | Self::Unknown(reply) => reply,
        }
    }
}

async fn run(hub: &Hub, machine: &str, req: &RpcRequest) -> Execution {
    if let Err(message) = protocol::check_args(&req.argv) {
        return Execution::NotStarted(protocol::error(req, 2, "usage", &message));
    }
    let mut spawned = false;
    let finished = {
        let result = async {
            let uploads = UploadFile::new()?;
            // ponytail: one process per request (~19/min); use in-process dispatch if RPC volume warrants it.
            let executable = std::env::current_exe()?;
            #[cfg(target_os = "linux")]
            let mut cmd = Command::new("/proc/self/exe");
            #[cfg(not(target_os = "linux"))]
            let mut cmd = Command::new(&executable);
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                cmd.as_std_mut().arg0(&executable);
            }
            cmd.arg("--hub-child")
                .args(&req.argv)
                .env_clear()
                .env("HOME", &hub.cfg.home)
                .env("TASKR_DB", &hub.cfg.db_path)
                .env("TASKR_RPC_CALLER", machine)
                .env("TASKR_HOSTD_EPOCH", hub.events.epoch())
                .env("TASKR_RPC_CWD", &req.cwd)
                .env("TASKR_RPC_UPLOAD_FILE", uploads.dir.join("uploads"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            for key in ["PATH", "HERDR_SOCKET_PATH", "TASKR_CHECKIN"] {
                if let Some(value) = std::env::var_os(key) {
                    cmd.env(key, value);
                }
            }
            for key in [
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
                if let Some(value) = req.env.get(key).filter(|s| !s.is_empty()) {
                    cmd.env(key, value);
                }
            }
            if req.capabilities.iter().any(|s| s == "doc-upload") {
                cmd.env("TASKR_RPC_DOC_UPLOAD", "1");
            }
            #[cfg(feature = "contract")]
            if let Ok(now) = std::env::var("TASKR_FROZEN_NOW") {
                cmd.env("TASKR_FROZEN_NOW", now);
            }
            let mut child = ChildGuard {
                child: Some(cmd.spawn()?),
                reapers: hub.reapers.clone(),
            };
            spawned = true;
            let process = child.child.as_mut().expect("child");
            let mut stdout = process.stdout.take().expect("stdout");
            let mut stderr = process.stderr.take().expect("stderr");
            let mut stdin = process.stdin.take().expect("stdin");
            let input = serde_json::to_vec(req)?;
            let writer = tokio::spawn(async move { stdin.write_all(&input).await });
            let mut out = Vec::new();
            let mut err = Vec::new();
            let (_, _, status) = tokio::try_join!(
                stdout.read_to_end(&mut out),
                stderr.read_to_end(&mut err),
                process.wait()
            )?;
            hub.events.check();
            writer.await??;
            let exit = status
                .code()
                .ok_or_else(|| anyhow::anyhow!("RPC child terminated without an exit code"))?;
            let upload = if exit == 0 && req.capabilities.iter().any(|s| s == "doc-upload") {
                let raw = std::fs::read_to_string(uploads.dir.join("uploads"))?;
                let wants = raw
                    .lines()
                    .map(serde_json::from_str)
                    .collect::<Result<Vec<protocol::DocWant>, _>>()?;
                if wants.is_empty() { None } else { Some(wants) }
            } else {
                None
            };
            Ok::<_, anyhow::Error>(RpcReply {
                exit,
                stdout: String::from_utf8_lossy(&out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
                upload,
            })
        };
        tokio::pin!(result);
        let mut stopped = hub.shutdown.clone();
        tokio::select! {
            result = tokio::time::timeout(protocol::budget(&req.argv) + Duration::from_secs(10), &mut result) => result.map_err(|_|"request execution deadline exceeded"),
            _ = stopped.changed() => {
                if protocol::stored(&req.argv) {
                    tokio::time::timeout(Duration::from_secs(2),&mut result).await.map_err(|_|"server stopped during request")
                }else {Err("server stopped during request")}
            },
        }
    };
    match finished {
        Ok(Ok(reply)) => Execution::Exited(reply),
        Ok(Err(error)) if !spawned => {
            Execution::NotStarted(protocol::error(req, 4, "database", &error.to_string()))
        }
        error => {
            let message = if protocol::stored(&req.argv) {
                format!(
                    "request {}: outcome unknown (still running, or the server stopped during it); inspect `taskr log` before any resend",
                    req.request_key
                )
            } else {
                match error {
                    Ok(Err(error)) => error.to_string(),
                    Err(message) => message.into(),
                    Ok(Ok(_)) => unreachable!(),
                }
            };
            Execution::Unknown(protocol::error(req, 5, "herdr", &message))
        }
    }
}

struct UploadFile {
    dir: PathBuf,
}
impl UploadFile {
    fn new() -> std::io::Result<Self> {
        use std::{
            os::unix::fs::{DirBuilderExt, OpenOptionsExt},
            sync::atomic::{AtomicU64, Ordering},
        };
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let dir = std::env::temp_dir().join(format!(
                "taskr-rpc-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    let private = Self { dir };
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(private.dir.join("uploads"))?;
                    return Ok(private);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}
impl Drop for UploadFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct ChildGuard {
    child: Option<tokio::process::Child>,
    reapers: Arc<StdMutex<JoinSet<()>>>,
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take()
            && let Some(pid) = child.id()
        {
            if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
            // Let wait's signal handler clear its exact waiting marker, then reap or kill.
            let mut reapers = self.reapers.lock().expect("reapers");
            while reapers.try_join_next().is_some() {}
            reapers.spawn(async move {
                if tokio::time::timeout(Duration::from_secs(1), child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
            });
        }
    }
}
