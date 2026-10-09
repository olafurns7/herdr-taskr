use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use taskr_core::store::inbox as ledger;
pub(crate) mod observe;
#[cfg(test)]
mod tests;
pub use ledger::ack;
/// Go's dbPollInterval: 1 s. Go's test binary polls every 50 ms, and so does
/// the contract oracle here (TASKR_CONTRACT_ORACLE=1 in a contract build).
pub(super) fn poll_interval() -> Duration {
    #[cfg(feature = "contract")]
    if std::env::var("TASKR_CONTRACT_ORACLE").is_ok_and(|s| s == "1") {
        return Duration::from_millis(50);
    }
    Duration::from_secs(1)
}
pub fn code(kind: &str) -> &str {
    match kind {
        "got" => "g",
        "ready" => "r",
        "ask" => "q",
        "answer" => "a",
        "owner_answer" => "oa",
        "done" => "d",
        "fail" => "f",
        "herdr" => "h",
        "note" => "n",
        "start" => "s",
        "prompt" => "p",
        "prompt_outcome" => "po",
        "decision" => "dc",
        "revoke" => "rv",
        "doc" => "do",
        "ref" => "rf",
        "next" => "nx",
        "handover" => "ho",
        "adopt" => "ad",
        "launch" => "l",
        "closed" => "c",
        "pr" => "pu",
        "after" => "af",
        other => other,
    }
}
pub fn emit_wait(v: &Value) {
    if v["timeout"] == true {
        if v["interrupted"] != true {
            println!("w1 {{\"owed\":{},\"due\":{}}}", v["owed"], v["due"]);
        }
        println!(
            "x1 3 timeout{}",
            if v["interrupted"] == true {
                " interrupted"
            } else {
                ""
            }
        );
        return;
    }
    let ev = &v["event"];
    let kind = ev["kind"].as_str().unwrap_or("");
    let mut data = ev.get("data").cloned().unwrap_or(json!({}));
    let summary = if kind == "got" {
        if let Some(m) = data.as_object_mut() {
            m.remove("identity");
        }
        ""
    } else {
        ev["summary"].as_str().unwrap_or("")
    };
    let null_id = |k: &str| ev[k].as_i64().map_or("-".into(), |n| n.to_string());
    println!(
        "e1\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        ev["id"],
        ev["task_id"],
        null_id("launch_id"),
        code(kind),
        if v["replay"] == true { 1 } else { 0 },
        null_id("related_event_id"),
        compact_json(&json!(summary)).expect("JSON"),
        compact_json(&data).expect("JSON")
    );
}
pub fn wait(f: &FlagSet) -> Result<Value> {
    let as_given = f.was_set("as");
    let caller = store::env("TASKR_TASK");
    let mut as_id = f.get_int("as");
    let timeout = f.get_int("timeout");
    let ack_id = f.get_int("ack");
    if !as_given && caller.is_empty() || as_given && as_id <= 0 {
        return Err(store::usage("wait needs --as TASK_ID"));
    }
    if as_given && !caller.is_empty() && store::id(&caller, "TASKR_TASK")? != as_id {
        return Err(store::reject(format!(
            "--as {as_id} names a different task than TASKR_TASK={caller}"
        )));
    }
    if timeout < 0 {
        return Err(store::usage("--timeout must not be negative"));
    }
    if f.was_set("ack") && ack_id <= 0 {
        return Err(store::usage("--ack must be a positive event id"));
    }
    let mut kinds = std::collections::BTreeSet::new();
    for s in f.get_strings("for") {
        for k in s.split(',') {
            if code(k) == k {
                return Err(store::usage(format!("unknown event kind {k:?}")));
            }
            kinds.insert(k.to_string());
        }
    }
    let mut db = open()?;
    if !as_given || !caller.is_empty() && !store::env("TASKR_LAUNCH").is_empty() {
        let t = store::transaction(&mut db, |tx| worker::resolve(tx, 0))?;
        if !as_given {
            as_id = t.id;
        }
    }
    let t = store::task(&db, as_id)?;
    if t.status == "planned" {
        return Err(store::reject(format!(
            "task {as_id} is planned and has no inbox; launch it first"
        )));
    }
    store::check_host(&db, as_id)?;
    let mut tasks = std::collections::BTreeSet::new();
    for s in f.get_strings("from") {
        if let Ok(id) = s.parse::<i64>() {
            if id <= 0 {
                return Err(store::usage("--from task id must be positive"));
            }
            store::task(&db, id)?;
            tasks.insert(id);
        } else {
            let mut stmt = db
                .prepare("select id from tasks where name=?")
                .map_err(Error::from)?;
            let ids = stmt
                .query_map([s], |r| r.get::<_, i64>(0))
                .map_err(Error::from)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::from)?;
            if ids.len() != 1 {
                return Err(store::reject(format!(
                    "--from name {s:?} matches {} tasks; use a task id",
                    ids.len()
                )));
            }
            tasks.insert(ids[0]);
        }
    }
    let filtered = !kinds.is_empty() || !tasks.is_empty();
    if ack_id != 0 {
        ack(&mut db, as_id, ack_id, store::caller_machine().is_some())?;
    }
    let interrupted = Arc::new(AtomicBool::new(false));
    let mut signals = Vec::new();
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signals.push(
            signal_hook::flag::register(signal, Arc::clone(&interrupted)).map_err(|e| Error {
                code: ExitCode::Database,
                message: e.to_string(),
            })?,
        );
    }
    let start = Instant::now();
    let frozen = taskr_core::frozen_now().expect("frozen clock");
    let deadline = store::stamp(frozen + taskr_core::store::millis(timeout));
    let budget = (frozen + store::millis(timeout) - store::real_now())
        .whole_milliseconds()
        .max(0) as u64;
    let budget = Duration::from_millis(budget);
    let mut marker = String::new();
    let mut skipped = 0;
    let result: Result<Value> = (|| {
        loop {
            if interrupted.load(Ordering::Relaxed) {
                return Ok(json!({"timeout":true,"interrupted":true,"as":as_id}));
            }
            if skipped > 0 && timeout > 0 && start.elapsed() >= budget {
                break;
            }
            if timeout > 0 {
                observe::capacity(&mut db, as_id, budget.saturating_sub(start.elapsed()))?;
            }
            let _ = ledger::expire(&mut db);
            let (ev, replay, coalesced) = ledger::offer(&mut db, as_id, filtered)?;
            if coalesced {
                skipped += 1;
                continue;
            }
            if let Some(ev) = ev {
                if ledger::bypass(&ev)
                    || (kinds.is_empty() || kinds.contains(ev["kind"].as_str().unwrap_or("")))
                        && (tasks.is_empty()
                            || tasks.contains(&ev["task_id"].as_i64().expect("task")))
                {
                    return Ok(json!({"as":as_id,"event":ev,"replay":replay}));
                }
                ack(&mut db, as_id, ev["id"].as_i64().expect("event"), false)?;
                skipped += 1;
                continue;
            }
            if start.elapsed() >= budget {
                break;
            }
            if marker.is_empty() {
                marker = deadline.clone();
                store::transaction(&mut db, |tx| {
                    store::check_host(tx, as_id)?;
                    tx.execute(
                        "update tasks set waiting_until=? where id=?",
                        db::params![marker, as_id],
                    )?;
                    Ok(())
                })?;
            }
            observe::maybe(
                &mut db,
                as_id,
                f.get_bool("scan-quota"),
                budget.saturating_sub(start.elapsed()),
            )?;
            let (ev, replay, coalesced) = ledger::offer(&mut db, as_id, filtered)?;
            if coalesced {
                skipped += 1;
                continue;
            }
            if let Some(ev) = ev {
                if ledger::bypass(&ev)
                    || (kinds.is_empty() || kinds.contains(ev["kind"].as_str().unwrap_or("")))
                        && (tasks.is_empty()
                            || tasks.contains(&ev["task_id"].as_i64().expect("task")))
                {
                    return Ok(json!({"as":as_id,"event":ev,"replay":replay}));
                }
                ack(&mut db, as_id, ev["id"].as_i64().expect("event"), false)?;
                skipped += 1;
                continue;
            }
            let left = budget.saturating_sub(start.elapsed());
            if left.is_zero() {
                break;
            }
            // Go returns on a signal at once; nap so a hub-served wait whose
            // client left clears its marker without waiting out the poll.
            let poll = Instant::now() + left.min(poll_interval());
            while !interrupted.load(Ordering::Relaxed) {
                let nap = poll.saturating_duration_since(Instant::now());
                if nap.is_zero() {
                    break;
                }
                std::thread::sleep(nap.min(Duration::from_millis(20)));
            }
        }
        let (owed, due) = ledger::counts(&db, as_id)?;
        Ok(json!({"timeout":true,"as":as_id,"owed":owed,"due":due}))
    })();
    for id in signals {
        signal_hook::low_level::unregister(id);
    }
    if skipped > 0 {
        if f.json() {
            println!(
                "{}",
                compact_json(&json!({"as":as_id,"skipped":skipped})).expect("JSON")
            );
        } else {
            println!("sk1 {skipped}");
        }
    }
    if !marker.is_empty() {
        let cleared = store::transaction(&mut db, |tx| {
            store::check_host(tx, as_id)?;
            tx.execute(
                "update tasks set waiting_until=null where id=? and waiting_until=?",
                db::params![as_id, marker],
            )?;
            Ok(())
        });
        if let Err(e) = cleared
            && result.is_ok()
        {
            let mut out = json!({"as":as_id,"stale_waiting_until":marker,"error":format!("clear waiting_until: {}",e.message),"kind":"database","_exit":4});
            if let Ok(v) = &result
                && v.get("event").is_some()
            {
                out["pending_event_id"] = v["event"]["id"].clone();
            }
            if ack_id != 0 {
                out["acked_event_id"] = json!(ack_id);
            }
            return Ok(out);
        }
    }
    if let Err(e) = result {
        if ack_id != 0 {
            return Ok(
                json!({"error":e.message,"kind":match e.code{ExitCode::Rejected=>"rejected",ExitCode::Transport=>"herdr",ExitCode::Usage=>"usage",_=>"database"},"_exit":e.code as u8,"acked_event_id":ack_id}),
            );
        }
        return Err(e);
    }
    result
}
