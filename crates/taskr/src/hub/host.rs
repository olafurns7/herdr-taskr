//! Capability-gated client uplink. Snapshots are scoped to the admitted host.
use super::*;
use taskr_core::{goflag::FlagSet, store::Result};

pub(super) fn call(args: &[String], json_mode: bool) -> Result<Value> {
    let mut flags = FlagSet::new("_host", json_mode);
    flags
        .string("agents", "", "the host's agent list")
        .string("removed", "[]", "removed pane IDs")
        .string("epoch", "", "hub epoch")
        .int("base", 0, "acknowledged snapshot generation");
    flags.parse(args, 1, 1).map_err(store::usage)?;
    let host = store::caller_machine().unwrap_or_default();
    let mut db = super::child::open()?;
    // RPC children are separate processes. Serialize snapshot bases across
    // them while reusing the observation helper's existing CAS transactions.
    use std::os::unix::fs::OpenOptionsExt;
    let path = std::path::Path::new(&store::env("HOME")).join(".local/state/taskr/hostd.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| store::Error {
            code: taskr_core::ExitCode::Database,
            message: e.to_string(),
        })?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).map_err(|e| {
        store::Error {
            code: taskr_core::ExitCode::Database,
            message: e.to_string(),
        }
    })?;
    apply(&mut db, &host, &flags)
}
fn get(db: &db::Connection, key: &str) -> Result<Option<String>> {
    Ok(db
        .query_row("select value from meta where key=?", [key], |r| r.get(0))
        .optional()?)
}
fn put(db: &db::Connection, key: &str, value: &str) -> Result<()> {
    db.execute("insert into meta(key,value) values(?,?) on conflict(key) do update set value=excluded.value", db::params![key,value])?;
    Ok(())
}
fn inputs(db: &db::Connection, host: &str) -> Result<Value> {
    // A new launch or a released waiter needs the cached pane state applied
    // even if Herdr itself has not changed since the last observation.
    let mut query = db.prepare("select t.id,t.current_launch_id,coalesce(l.pane_id,t.pane_id),t.status,case when t.waiting_until>?2 then t.waiting_until else null end from tasks t left join launches l on l.id=t.current_launch_id where t.machine is ?1 or l.machine is ?1 order by t.id")?;
    let rows = query
        .query_map(db::params![host, store::now()], |r| {
            Ok(json!([
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?
            ]))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(json!(rows))
}

fn apply(db: &mut db::Connection, host: &str, flags: &FlagSet) -> Result<Value> {
    if host.is_empty() {
        return Err(store::usage("_host is only for a client host over RPC"));
    }
    let kind = flags.positional[0].as_str();
    if !matches!(kind, "observe" | "delta" | "heartbeat") {
        return Err(store::usage(format!(
            "_host: unknown call {}",
            taskr_core::goflag::quote(kind)
        )));
    }
    // Set only by the admitted hub parent after env_clear; shared with SSE.
    let epoch = store::env("TASKR_HOSTD_EPOCH");
    let key = format!("hostd_snapshot:{host}");
    let previous: Value = get(db, &key)?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let mut generation = previous["generation"].as_i64().unwrap_or(0);
    if kind != "observe"
        && (epoch.is_empty()
            || flags.get_string("epoch") != epoch
            || previous["epoch"] != epoch
            || flags.get_int("base") != generation)
    {
        return Err(store::reject(
            "host uplink needs a full observe (epoch or snapshot changed)",
        ));
    }
    let mut reply = if kind == "heartbeat" {
        if previous["inputs"] != inputs(db, host)? {
            return Err(store::reject(
                "host uplink needs a full observe (launch inputs changed)",
            ));
        }
        crate::daemon::heartbeat_host(db, host)?
    } else {
        let agents: Value = serde_json::from_str(flags.get_string("agents"))
            .map_err(|_| store::usage("_host observe: --agents must be a JSON array"))?;
        let agents = if kind == "delta" {
            let changed = crate::daemon::validate_host_agents(&agents)?;
            let removed: Vec<String> = serde_json::from_str(flags.get_string("removed"))
                .map_err(|_| store::usage("_host delta: --removed must be an array of pane IDs"))?;
            let mut snapshot = BTreeMap::new();
            for agent in previous["agents"]
                .as_array()
                .ok_or_else(|| store::reject("host uplink needs a full observe"))?
            {
                snapshot.insert(
                    agent["pane_id"].as_str().unwrap_or_default().to_string(),
                    agent.clone(),
                );
            }
            for pane in removed {
                snapshot.remove(&pane);
            }
            for agent in changed {
                snapshot.insert(
                    agent["pane_id"].as_str().unwrap_or_default().to_string(),
                    agent.clone(),
                );
            }
            json!(snapshot.into_values().collect::<Vec<_>>())
        } else {
            agents
        };
        generation = generation
            .checked_add(1)
            .ok_or_else(|| store::reject("host snapshot generation exhausted"))?;
        let snapshot =
            compact_json(&json!({"epoch":epoch,"generation":generation,"agents":agents,"inputs":inputs(db, host)?}))
                .expect("snapshot");
        if snapshot.len() > super::protocol::BODY_MAX {
            return Err(store::usage(
                "host snapshot is too large; send a full observe",
            ));
        }
        let reply = crate::daemon::observe_host(db, host, &agents)?;
        put(db, &key, &snapshot)?;
        reply
    };
    if !epoch.is_empty() {
        reply["hostd"] = json!({"version":1,"epoch":epoch,"generation":generation,"heartbeat_ms":10000,"stale_ms":30000});
    }
    Ok(reply)
}
