use super::*;
use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
};
pub fn scan(db: &mut db::Connection, parent: i64, left: Duration) -> Result<()> {
    scan_on(db, Some(parent), left)
}
pub(super) fn scan_all(db: &mut db::Connection, left: Duration) -> Result<()> {
    scan_on(db, None, left)
}
fn scan_on(db: &mut db::Connection, parent: Option<i64>, left: Duration) -> Result<()> {
    let mut stmt=db.prepare("select t.id,t.parent_id,l.id,l.session_ref,l.transcript_path from tasks t join launches l on l.id=t.current_launch_id where t.parent_id is not null and(?1 is null or t.parent_id=?1) and t.status='open' and t.role!='gate' and(t.waiting_until is null or t.waiting_until<=?2) and l.machine is null and l.provider='codex' and l.session_source like 'hook:%' and l.session_ref is not null and l.transcript_path is not null order by l.id")?;
    let rows = stmt
        .query_map(params![parent, store::now()], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    let start = Instant::now();
    for (task, _parent, launch, session, path) in rows {
        if start.elapsed() >= left {
            break;
        }
        if !valid_path(&session, &path) {
            continue;
        }
        let (prompt,report):(i64,i64)=db.query_row("select coalesce((select max(id) from events where launch_id=?1 and kind='prompt'),0),coalesce((select max(id) from events where launch_id=?1 and kind in('done','fail')),0)",[launch],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if prompt == 0 || prompt <= report {
            continue;
        }
        let (stalled,at):(bool,String)=db.query_row("select exists(select 1 from events where event_key=?),(select created_at from events where id=?)",params![format!("stall:{prompt}"),prompt],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if stalled {
            continue;
        }
        let Some(error) = read_error(&path, &at) else {
            continue;
        };
        if start.elapsed() >= left {
            break;
        }
        store::transaction(db, |tx| {
            let current:bool=tx.query_row("select exists(select 1 from tasks t join launches l on l.id=t.current_launch_id where t.id=? and t.status='open' and l.id=? and l.session_source like 'hook:%' and l.session_ref=? and l.transcript_path=?)",params![task,launch,session,path],|r|r.get(0))?;
            if current {
                let t = store::task(tx, task)?;
                store::hook::stall(tx, &t, launch, &error)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}
fn valid_path(session: &str, path: &str) -> bool {
    let clean = orch::absolute("/", path);
    let p = Path::new(&clean);
    p.is_absolute()
        && !session.is_empty()
        && path.ends_with(".jsonl")
        && p.parent()
            .is_some_and(|p| p.components().any(|c| c.as_os_str() == "sessions"))
        && p.file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("rollout-") && s.ends_with(&format!("-{session}.jsonl")))
}
fn read_error(path: &str, since: &str) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let offset = size.saturating_sub(1 << 20);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(1 << 20).read_to_end(&mut bytes).ok()?;
    let bytes = if offset > 0 {
        &bytes[bytes.iter().position(|b| *b == b'\n')? + 1..]
    } else {
        &bytes
    };
    let mut last = None;
    let mut user = None;
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        let Ok(row) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let payload = &row["payload"];
        if row["type"] == "event_msg" && payload["type"] == "task_complete" {
            let since = store::parse_time(since)?;
            let recent = if let Some(at) = row["timestamp"].as_str().filter(|s| !s.is_empty()) {
                store::parse_time(at).is_some_and(|at| at >= since)
            } else {
                payload["completed_at"]
                    .as_f64()
                    .is_some_and(|n| n as i64 >= since.unix_timestamp())
            };
            if recent {
                let raw = &payload["error"];
                let mut code =
                    store::hook::error_code(if raw.is_object() { raw } else { &Value::Null });
                if !raw.is_null() && code.is_empty() {
                    code = "unknown".into();
                }
                last = Some((i, code, !raw.is_null()));
            }
        } else if row["type"] == "response_item"
            && (row["role"] == "user"
                || row["role"].as_str().unwrap_or("").is_empty() && payload["role"] == "user")
        {
            user = Some(i);
        }
    }
    let (i, code, bad) = last?;
    if !bad || user.is_some_and(|u| u > i) {
        None
    } else {
        Some(code)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rollout_path_is_bound() {
        assert!(valid_path("s", "/tmp/sessions/rollout-x-s.jsonl"));
        assert!(!valid_path("s", "/tmp/rollout-x-s.jsonl"));
        assert!(!valid_path("other", "/tmp/sessions/rollout-x-s.jsonl"));
    }
}
