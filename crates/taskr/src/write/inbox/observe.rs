use super::*;
use db::{OptionalExtension, params};
mod capacity;
mod rollout;
pub use capacity::{bind, capacity};
fn get_meta(db: &db::Connection, key: &str) -> Result<Option<String>> {
    Ok(db
        .query_row("select value from meta where key=?", [key], |r| r.get(0))
        .optional()?)
}
fn set_meta(db: &db::Connection, key: &str, value: &str) -> Result<()> {
    db.execute("insert into meta(key,value) values(?,?) on conflict(key) do update set value=excluded.value",params![key,value])?;
    Ok(())
}
fn fresh(at: &str, seconds: i64) -> bool {
    store::parse_time(at).is_some_and(|at| {
        taskr_core::frozen_now().expect("clock") - at < store::millis(seconds * 1000)
    })
}
// Reads freshness first and takes the write lock only to claim; the
// transaction re-checks so two waiters never both claim.
fn claim(db: &mut db::Connection, as_id: i64, key: Option<&str>) -> Result<bool> {
    let stale = |db: &db::Connection| -> Result<bool> {
        let last: Option<String> = if let Some(key) = key {
            get_meta(db, key)?
        } else {
            db.query_row("select last_poll_at from tasks where id=?", [as_id], |r| {
                r.get(0)
            })?
        };
        Ok(!last.is_some_and(|s| fresh(&s, 15)))
    };
    if !stale(db)? {
        return Ok(false);
    }
    store::transaction(db, |tx| {
        if !stale(tx)? {
            return Ok(false);
        }
        if let Some(key) = key {
            set_meta(tx, key, &store::now())?;
        } else {
            tx.execute(
                "update tasks set last_poll_at=? where id=?",
                params![store::now(), as_id],
            )?;
        }
        Ok(true)
    })
}
#[derive(Clone)]
struct Watched {
    task: i64,
    parent: i64,
    launch: i64,
    version: i64,
    pane: String,
    status: Option<String>,
    seq: Option<i64>,
    present: bool,
}
fn watched(db: &db::Connection, as_id: Option<i64>) -> Result<Vec<Watched>> {
    watched_on(db, as_id, None)
}
fn watched_on(db: &db::Connection, as_id: Option<i64>, host: Option<&str>) -> Result<Vec<Watched>> {
    let mut stmt=db.prepare("select t.id,t.parent_id,l.id,l.observed_version,coalesce(l.pane_id,t.pane_id),l.observed_status,l.observed_seq,l.present from tasks t join launches l on l.id=t.current_launch_id where t.parent_id is not null and(?1 is null or t.parent_id=?1) and t.status not in('closed','planned') and t.role!='gate' and coalesce(l.pane_id,t.pane_id) is not null and(t.waiting_until is null or t.waiting_until<=?2) and l.machine is ?3 order by t.id")?;
    Ok(stmt
        .query_map(params![as_id, store::now(), host], |r| {
            Ok(Watched {
                task: r.get(0)?,
                parent: r.get(1)?,
                launch: r.get(2)?,
                version: r.get(3)?,
                pane: r.get(4)?,
                status: r.get(5)?,
                seq: r.get(6)?,
                present: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?)
}
pub fn maybe(db: &mut db::Connection, as_id: i64, quota: bool, left: Duration) -> Result<()> {
    let ws = watched(db, Some(as_id))?;
    if ws.is_empty() {
        return Ok(());
    }
    let sock = herdr::socket();
    if !herdr::up(&sock) {
        return Ok(());
    }
    let heartbeat = get_meta(db, "daemon_heartbeat")?.is_some_and(|s| fresh(&s, 30));
    if heartbeat {
        if !quota || !claim(db, as_id, Some(&format!("quota_poll:{as_id}")))? {
            return Ok(());
        }
    } else {
        if !claim(db, as_id, None)? {
            return Ok(());
        }
        observe_snapshot(db, &sock, &ws, left)?;
        rollout::scan(db, as_id, left)?;
        if !quota {
            return Ok(());
        }
    }
    for w in watched(db, Some(as_id))? {
        let Ok(output) = herdr::command(
            &sock,
            &["agent", "read", &w.pane, "--source", "visible"],
            left.min(Duration::from_secs(10)),
        ) else {
            continue;
        };
        if output.code != Some(0) {
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
        let hit = [
            "hit your weekly limit",
            "hit your usage limit",
            "hit your session limit",
        ]
        .iter()
        .any(|s| text.contains(s));
        let low = text
            .rsplit_once("weekly limit:")
            .and_then(|(_, s)| s.trim_start().split_once("% left"))
            .and_then(|(s, _)| s.parse::<i64>().ok())
            .filter(|n| *n <= 10);
        let (kind, pct, key, summary) = if hit {
            ("limit", 0, "limit".into(), "quota limit hit".into())
        } else if let Some(pct) = low {
            (
                "low",
                pct,
                format!("low:{pct}"),
                format!("quota {pct}% left"),
            )
        } else {
            continue;
        };
        let key = format!("quota:{}:{key}", w.launch);
        store::transaction(db, |tx| {
            let current:bool=tx.query_row("select exists(select 1 from tasks where current_launch_id=? and status!='closed') and not exists(select 1 from events where event_key=?)",params![w.launch,key],|r|r.get(0))?;
            if current {
                store::event(
                    tx,
                    store::Event {
                        task: w.task,
                        to: Some(as_id),
                        launch: Some(w.launch),
                        kind: "herdr",
                        summary: &summary,
                        data: Some(json!({"quota":kind,"percent":pct,"pane_id":w.pane})),
                        key: &key,
                        ..store::Event::default()
                    },
                )?;
            }
            Ok(())
        })?;
    }
    Ok(())
}
fn hint_phase(tx: &db::Connection, launch: i64, status: &str) -> Result<Option<&'static str>> {
    if status == "blocked" {
        return Ok(None);
    }
    let (prompt,report,ready,hooked):(i64,i64,i64,bool)=tx.query_row("select coalesce((select max(id) from events where launch_id=?1 and kind='prompt'),0),coalesce((select max(id) from events where launch_id=?1 and kind in('done','fail')),0),coalesce((select max(id) from events where launch_id=?1 and kind='ready'),0),exists(select 1 from launches where id=?1 and session_source like 'hook:%' and session_ref is not null and session_ref!='')",[launch],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    if prompt == 0 {
        return Ok(Some("startup"));
    }
    if prompt <= report {
        return Ok(Some("post_done"));
    }
    if ["idle", "unknown", "missing", "done"].contains(&status) && hooked {
        return Ok(if status == "missing" {
            None
        } else {
            Some("hooked")
        });
    }
    if ready > prompt && status != "missing" {
        return Ok(Some("post_ready"));
    }
    let (got,armed):(bool,bool)=tx.query_row("select exists(select 1 from events where kind='got' and related_event_id=?),exists(select 1 from meta where key=?)",params![prompt,format!("receipt_due:{prompt}")],|r|Ok((r.get(0)?,r.get(1)?)))?;
    Ok(
        if !got && armed && !(ready > prompt && status == "missing") {
            Some("armed")
        } else {
            None
        },
    )
}

fn observe_snapshot(
    db: &mut db::Connection,
    sock: &str,
    ws: &[Watched],
    left: Duration,
) -> Result<usize> {
    let list = herdr::command(sock, &["agent", "list"], left.min(Duration::from_secs(10)))
        .map_err(|e| Error {
            code: ExitCode::Transport,
            message: e.to_string(),
        })?;
    if list.code != Some(0) {
        return Err(Error {
            code: ExitCode::Transport,
            message: format!(
                "herdr agent list failed: exit status {}",
                list.code.unwrap_or(-1)
            ),
        });
    }
    let v: Value = serde_json::from_slice(&list.stdout).map_err(|e| Error {
        code: ExitCode::Transport,
        message: format!("herdr agent list returned malformed JSON: {e}"),
    })?;
    let agents = validate_agents(&v)?;
    apply_snapshot(db, ws, agents)
}

/// One unthrottled daemon pass; wait and daemon share CAS and hint rules.
pub(crate) fn daemon_observe(
    db: &mut db::Connection,
    sock: &str,
    timeout: Duration,
) -> Result<usize> {
    let start = Instant::now();
    let ws = watched(db, None)?;
    let leads:bool=db.query_row("select exists(select 1 from tasks where parent_id is null and status not in('closed','planned') and pane_id is not null and machine is null)",[],|r|r.get(0))?;
    if ws.is_empty() && !leads {
        return Ok(0);
    }
    // Fetch once, including when only campaign leads are present.
    let list = herdr::command(
        sock,
        &["agent", "list"],
        timeout.min(Duration::from_secs(10)),
    )
    .map_err(|e| Error {
        code: ExitCode::Transport,
        message: e.to_string(),
    })?;
    if list.code != Some(0) {
        return Err(Error {
            code: ExitCode::Transport,
            message: format!(
                "herdr agent list failed: exit status {}",
                list.code.unwrap_or(-1)
            ),
        });
    }
    let v: Value = serde_json::from_slice(&list.stdout).map_err(|e| Error {
        code: ExitCode::Transport,
        message: format!("herdr agent list returned malformed JSON: {e}"),
    })?;
    let agents = validate_agents(&v)?;
    set_meta(db, "lead_listed_at", &store::now())?;
    observe_leads(db, None, agents)?;
    let count = apply_snapshot(db, &ws, agents)?;
    rollout::scan_all(db, timeout.saturating_sub(start.elapsed()))?;
    Ok(count)
}

fn validate_agents(v: &Value) -> Result<&Vec<Value>> {
    let agents = v["result"]["agents"].as_array().ok_or_else(|| Error {
        code: ExitCode::Transport,
        message: "herdr agent list returned no result.agents".into(),
    })?;
    for a in agents {
        if a["pane_id"].as_str().unwrap_or("").is_empty()
            || a["agent_status"].as_str().unwrap_or("").is_empty()
        {
            return Err(Error {
                code: ExitCode::Transport,
                message: "herdr agent list entry without pane_id or agent_status".into(),
            });
        }
    }
    Ok(agents)
}

fn apply_snapshot(db: &mut db::Connection, ws: &[Watched], agents: &[Value]) -> Result<usize> {
    let mut count = 0;
    for w in ws {
        let a = agents.iter().find(|a| a["pane_id"] == w.pane);
        let status = a.and_then(|a| a["agent_status"].as_str());
        let seq = a.and_then(|a| a["state_change_seq"].as_i64()).unwrap_or(0);
        if let Some(s) = status {
            if w.status.as_deref() == Some(s) && w.seq.unwrap_or(0) == seq && w.present {
                continue;
            }
        } else if !w.present {
            continue;
        }
        count += store::transaction(db, |tx| {
            let n = if let Some(s) = status {
                tx.execute("update launches set observed_status=?,observed_seq=?,present=1,observed_at=?,observed_version=observed_version+1 where id=? and observed_version=? and exists(select 1 from tasks t where t.id=launches.task_id and t.status!='closed' and t.current_launch_id=launches.id)",params![s,seq,store::now(),w.launch,w.version])?
            } else {
                tx.execute("update launches set present=0,observed_at=?,observed_version=observed_version+1 where id=? and observed_version=? and exists(select 1 from tasks t where t.id=launches.task_id and t.status!='closed' and t.current_launch_id=launches.id)",params![store::now(),w.launch,w.version])?
            };
            if n != 1 || status == Some("working") {
                return Ok(n);
            }
            let status = status.unwrap_or("missing");
            let phase = hint_phase(tx, w.launch, status)?;
            if let Some(phase) = phase {
                let key = format!("hint_suppressed:{}:{status}:{phase}", &store::now()[..10]);
                tx.execute("insert into meta(key,value) values(?,'1') on conflict(key) do update set value=cast(cast(value as integer)+1 as text)",[key])?;
                return Ok(n);
            }
            let mut data = json!({"pane_id":w.pane,"present":a.is_some()});
            let summary = if let Some(a) = a {
                data["agent_status"] = json!(status);
                data["state_change_seq"] = json!(seq);
                if let Some(name) = a["name"].as_str().filter(|s| !s.is_empty()) {
                    data["agent_name"] = json!(name);
                }
                format!("agent_status {status} (seq {seq})")
            } else {
                "missing from herdr agent list".into()
            };
            store::event(
                tx,
                store::Event {
                    task: w.task,
                    to: Some(w.parent),
                    launch: Some(w.launch),
                    kind: "herdr",
                    summary: &summary,
                    data: Some(data),
                    ..store::Event::default()
                },
            )?;
            Ok(n)
        })?;
    }
    Ok(count)
}

pub(crate) fn host_snapshot(
    db: &mut db::Connection,
    host: &str,
    agents: &[Value],
) -> Result<usize> {
    let ws = watched_on(db, None, Some(host))?;
    let count = apply_snapshot(db, &ws, agents)?;
    observe_leads(db, Some(host), agents)?;
    Ok(count)
}
fn observe_leads(db: &db::Connection, host: Option<&str>, agents: &[Value]) -> Result<()> {
    let mut stmt=db.prepare("select id,pane_id,lead_status,lead_present from tasks where parent_id is null and status not in('closed','planned') and pane_id is not null and machine is ? order by id")?;
    let leads = stmt
        .query_map([host], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<bool>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, pane, status, present) in leads {
        let a = agents.iter().find(|a| a["pane_id"] == pane);
        match a {
            Some(a) if status.as_deref() != a["agent_status"].as_str() || present != Some(true) => {
                db.execute("update tasks set lead_status=?,lead_present=1,lead_observed_at=? where id=? and pane_id=? and machine is ? and parent_id is null and status not in('closed','planned')",params![a["agent_status"].as_str(),store::now(),id,pane,host])?;
            }
            None if present != Some(false) => {
                db.execute("update tasks set lead_present=0,lead_observed_at=? where id=? and pane_id=? and machine is ? and parent_id is null and status not in('closed','planned')",params![store::now(),id,pane,host])?;
            }
            _ => {}
        }
    }
    Ok(())
}
