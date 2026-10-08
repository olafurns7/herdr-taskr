//! Local ledger mutations. SQL and event ordering follow the Go ledger contract.
use crate::{ExitCode, compact_json, frozen_now};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::OnceLock};

pub mod documents;
pub mod handover;
pub mod hook;
pub mod inbox;
pub mod orch;
pub mod plan;
pub mod worker;

#[derive(Debug)]
pub struct Error {
    pub code: ExitCode,
    pub message: String,
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self {
            code: ExitCode::Database,
            message: crate::db::error_text(&e),
        }
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self {
            code: ExitCode::Database,
            message: e.to_string(),
        }
    }
}
pub fn usage(message: impl Into<String>) -> Error {
    Error {
        code: ExitCode::Usage,
        message: message.into(),
    }
}
pub fn reject(message: impl Into<String>) -> Error {
    Error {
        code: ExitCode::Rejected,
        message: message.into(),
    }
}
pub fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}
pub fn millis(ms: i64) -> time::Duration {
    time::Duration::nanoseconds(ms.wrapping_mul(1_000_000))
}
pub fn duration_millis(ms: i64) -> i64 {
    millis(ms).whole_milliseconds() as i64
}
pub fn real_now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}
pub fn now() -> String {
    stamp(frozen_now().expect("validated frozen clock"))
}
pub fn stamp(t: time::OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}
pub fn parse_time(s: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}
pub fn null(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}
pub fn id(s: &str, field: &str) -> Result<i64> {
    s.parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| usage(format!("{field} must be a positive integer, got {s:?}")))
}
pub fn transaction<T>(db: &mut Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let result = f(&tx)?;
    tx.commit()?;
    Ok(result)
}
#[derive(Debug)]
pub struct Task {
    pub id: i64,
    pub parent: Option<i64>,
    pub name: String,
    pub role: String,
    pub status: String,
    pub launch: Option<i64>,
    pub pane: String,
    pub cwd: String,
    pub machine: Option<String>,
    pub acked: i64,
    pub pending: Option<i64>,
    pub waiting: Option<String>,
}
pub fn task(db: &Connection, id: i64) -> Result<Task> {
    db.query_row("select id,parent_id,name,role,status,current_launch_id,coalesce(pane_id,''),coalesce(cwd,''),machine,acked_event_id,pending_event_id,waiting_until from tasks where id=?", [id], |r| Ok(Task { id:r.get(0)?, parent:r.get(1)?, name:r.get(2)?,role:r.get(3)?,status:r.get(4)?,launch:r.get(5)?,pane:r.get(6)?,cwd:r.get(7)?,machine:r.get(8)?,acked:r.get(9)?,pending:r.get(10)?,waiting:r.get(11)? })).optional()?.ok_or_else(|| reject(format!("task {id} does not exist")))
}
pub fn open_task(db: &Connection, id: i64) -> Result<Task> {
    let t = task(db, id)?;
    if t.status == "closed" {
        return Err(reject(format!("task {id} is closed")));
    }
    Ok(t)
}
pub fn root(db: &Connection, mut id: i64) -> Result<i64> {
    for _ in 0..1000 {
        match task(db, id)?.parent {
            Some(p) => id = p,
            None => return Ok(id),
        }
    }
    Err(Error {
        code: ExitCode::Database,
        message: format!("parent chain of task {id} is too deep"),
    })
}
pub fn planned_ancestor(db: &Connection, mut id: i64) -> Result<Option<i64>> {
    for _ in 0..1000 {
        match task(db, id)?.parent {
            None => return Ok(None),
            Some(p) => {
                if task(db, p)?.status == "planned" {
                    return Ok(Some(p));
                }
                id = p;
            }
        }
    }
    Err(Error {
        code: ExitCode::Database,
        message: format!("parent chain of task {id} is too deep"),
    })
}
pub fn recipient(db: &Connection, id: Option<i64>) -> Result<()> {
    if let Some(id) = id
        && task(db, id)?.status == "planned"
    {
        return Err(reject(format!(
            "task {id} is planned and has no inbox; launch it first"
        )));
    }
    Ok(())
}
#[derive(Default)]
pub struct Event<'a> {
    pub task: i64,
    pub to: Option<i64>,
    pub launch: Option<i64>,
    pub kind: &'a str,
    pub summary: &'a str,
    pub data: Option<Value>,
    pub related: Option<i64>,
    pub key: &'a str,
}
pub fn event(db: &Connection, e: Event<'_>) -> Result<i64> {
    let data = e.data.as_ref().map(compact_json).transpose()?;
    db.execute("insert into events(task_id,recipient_task_id,launch_id,kind,summary,data,related_event_id,event_key,created_at) values(?,?,?,?,?,?,?,?,?)", params![e.task,e.to,e.launch,e.kind,null(e.summary),data,e.related,null(e.key),now()])?;
    Ok(db.last_insert_rowid())
}
pub fn local_machine() -> String {
    let h = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        });
    let Some(h) = h.filter(|s| !s.trim().is_empty()) else {
        return "this machine".into();
    };
    let h = h.trim().trim_end_matches('.').to_lowercase();
    let short = h.split('.').next().unwrap_or("");
    if short.chars().count() > 63 {
        format!("{}…", short.chars().take(62).collect::<String>())
    } else {
        short.into()
    }
}

/// Set once by explicit hub-child dispatch, never inferred from inherited env.
pub struct RpcContext {
    pub caller: String,
    pub cwd: String,
    pub doc_upload: bool,
    pub upload_file: PathBuf,
}
static RPC_CONTEXT: OnceLock<RpcContext> = OnceLock::new();

pub fn init_rpc_context(context: RpcContext) -> bool {
    !context.caller.is_empty() && RPC_CONTEXT.set(context).is_ok()
}
pub fn rpc_context() -> Option<&'static RpcContext> {
    RPC_CONTEXT.get()
}
pub fn caller_machine() -> Option<String> {
    rpc_context().map(|context| context.caller.clone())
}
pub fn machine_name(host: Option<&str>) -> String {
    host.map_or_else(local_machine, str::to_owned)
}
pub fn caller_cwd() -> Result<String> {
    if let Some(context) = rpc_context() {
        return Ok(context.cwd.clone());
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| usage(e.to_string()))
}
pub fn check_host(db: &Connection, id: i64) -> Result<()> {
    let host = task(db, id)?.machine;
    let caller = caller_machine();
    if host != caller {
        return Err(reject(format!(
            "task {id} is on {}, caller is {}",
            machine_name(host.as_deref()),
            machine_name(caller.as_deref())
        )));
    }
    Ok(())
}
pub fn delete_receipts(db: &Connection, id: i64) -> Result<()> {
    let mut stmt = db.prepare(
        "select key,value from meta where key >= 'receipt_due:' and key < 'receipt_due;'",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (key, raw) in rows {
        if serde_json::from_str::<Value>(&raw)
            .ok()
            .is_none_or(|v| v["task"] == id)
        {
            db.execute("delete from meta where key=?", [key])?;
        }
    }
    Ok(())
}
pub fn location(ws: &str, tab: &str, pane: &str) -> Value {
    let mut v = json!({});
    for (k, s) in [("workspace_id", ws), ("tab_id", tab), ("pane_id", pane)] {
        if !s.is_empty() {
            v[k] = json!(s);
        }
    }
    v
}

pub fn terminate_group(pid: u32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn go_duration_milliseconds_wrap() {
        assert_eq!(super::duration_millis(60_000), 60_000);
        assert_eq!(super::duration_millis(i64::MAX), -1);
        assert_eq!(super::duration_millis(i64::MIN), 0);
    }
}
