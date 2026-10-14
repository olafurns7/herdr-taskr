//! `taskr delivery`: durable push delivery attempts and their receipts. States are
//! timestamps (attempted, submitted, received), never the inbox cursor: nothing here
//! reads offer/wait or writes acked_event_id/pending_event_id, and no event is written.
use super::*;

/// The writer an attempt binds: a worker's current launch, or a root's adopt generation.
#[derive(Clone, Copy, PartialEq)]
pub enum Binding {
    Launch(i64),
    Session(i64),
}

/// A root's generation: its latest adopt event id, 0 before any adopt.
pub fn generation(db: &Connection, root: i64) -> Result<i64> {
    Ok(db.query_row(
        "select coalesce(max(id),0) from events where task_id=? and kind='adopt'",
        [root],
        |r| r.get(0),
    )?)
}

/// A worker with TASKR_TASK and its current TASKR_LAUNCH; an env-only identity
/// without a launch is not one.
pub(super) fn worker(db: &Connection) -> Result<(Task, i64)> {
    if env("TASKR_LAUNCH").is_empty() {
        if env("TASKR_TASK").is_empty() {
            return Err(usage(
                "delivery needs TASKR_TASK and TASKR_LAUNCH (a worker) or --as ROOT --session GEN (a root)",
            ));
        }
        return Err(reject(
            "delivery needs TASKR_LAUNCH with TASKR_TASK; a root uses --as ROOT --session GEN",
        ));
    }
    let t = worker::resolve(db, 0)?;
    let lid = t.launch.expect("resolve checked TASKR_LAUNCH");
    Ok((t, lid))
}

/// The caller of a mutation. A root's GEN is pinned by the caller and must be the
/// root's current generation; it is never refreshed from the ledger.
pub(super) fn identify(
    db: &Connection,
    as_id: i64,
    session: Option<i64>,
) -> Result<(Task, Binding)> {
    if as_id == 0 {
        let (t, lid) = worker(db)?;
        return Ok((t, Binding::Launch(lid)));
    }
    let t = worker::resolve(db, as_id)?;
    let session_gen = session.ok_or_else(|| usage("--as ROOT needs --session GEN"))?;
    let current = generation(db, t.id)?;
    if session_gen != current {
        return Err(reject(format!(
            "session generation {session_gen} is not root {}'s current generation {current}",
            t.id
        )));
    }
    Ok((t, Binding::Session(session_gen)))
}

const COLUMNS: &str =
    "id,event_id,task_id,launch_id,session_gen,machine,adapter,created_at,submitted_at,received_at";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let submitted: Option<String> = r.get(8)?;
    let received: Option<String> = r.get(9)?;
    let state = if received.is_some() {
        "received"
    } else if submitted.is_some() {
        "submitted"
    } else {
        "attempted"
    };
    Ok(json!({
        "attempt_id": r.get::<_, i64>(0)?,
        "event_id": r.get::<_, i64>(1)?,
        "task_id": r.get::<_, i64>(2)?,
        "launch_id": r.get::<_, Option<i64>>(3)?,
        "session_gen": r.get::<_, Option<i64>>(4)?,
        "machine": r.get::<_, Option<String>>(5)?,
        "adapter": r.get::<_, String>(6)?,
        "created_at": r.get::<_, String>(7)?,
        "submitted_at": submitted,
        "received_at": received,
        "state": state,
    }))
}
fn get(db: &Connection, id: i64) -> Result<Value> {
    let mut v = db.query_row(
        &format!("select {COLUMNS} from delivery_attempts where id=?"),
        [id],
        row,
    )?;
    v["ok"] = json!(true);
    Ok(v)
}

/// Record an attempt to deliver inbox event `event_id` to the caller. While a consumer
/// lease is live only its holder may, with `fence`; a fence with no live lease is refused.
pub fn new(
    db: &mut Connection,
    as_id: i64,
    session: Option<i64>,
    event_id: i64,
    adapter: &str,
    fence: Option<i64>,
) -> Result<Value> {
    transaction(db, |tx| {
        let (t, binding) = identify(tx, as_id, session)?;
        let fence = fence.map(|fence| consumer::Fence {
            fence,
            as_id,
            session,
        });
        consumer::guard(tx, t.id, fence.as_ref())?;
        let to = tx
            .query_row(
                "select recipient_task_id from events where id=?",
                [event_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .ok_or_else(|| reject(format!("event {event_id} does not exist")))?;
        if to != Some(t.id) {
            return Err(reject(format!(
                "event {event_id} is not addressed to task {}",
                t.id
            )));
        }
        if event_id <= t.acked {
            return Err(reject(format!(
                "event {event_id} is already handled: task {} acked through {}",
                t.id, t.acked
            )));
        }
        let (launch, session_gen) = match binding {
            Binding::Launch(l) => (Some(l), None),
            Binding::Session(g) => (None, Some(g)),
        };
        tx.execute(
            "insert into delivery_attempts(event_id,task_id,launch_id,session_gen,machine,adapter,created_at) values(?,?,?,?,?,?,?)",
            params![event_id, t.id, launch, session_gen, caller_machine(), adapter, now()],
        )?;
        get(tx, tx.last_insert_rowid())
    })
}

/// Stamp `submitted_at` or `received_at` once, after every binding check.
fn mark(
    db: &mut Connection,
    as_id: i64,
    session: Option<i64>,
    attempt: i64,
    column: &str,
) -> Result<Value> {
    transaction(db, |tx| {
        let (t, binding) = identify(tx, as_id, session)?;
        let (task, launch, session_gen, machine, done) = tx
            .query_row(
                &format!("select task_id,launch_id,session_gen,machine,{column} is not null from delivery_attempts where id=?"),
                [attempt],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, bool>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| reject(format!("delivery attempt {attempt} does not exist")))?;
        if task != t.id {
            return Err(reject(format!(
                "delivery attempt {attempt} is for task {task}, not task {}",
                t.id
            )));
        }
        match binding {
            Binding::Launch(l) if launch != Some(l) => {
                return Err(reject(format!(
                    "delivery attempt {attempt} is not bound to the current launch {l} of task {}",
                    t.id
                )));
            }
            Binding::Session(g) if session_gen != Some(g) => {
                return Err(reject(format!(
                    "delivery attempt {attempt} is not bound to session generation {g} of root {}",
                    t.id
                )));
            }
            _ => {}
        }
        let caller = caller_machine();
        if machine != caller {
            return Err(reject(format!(
                "delivery attempt {attempt} is on {}, caller is {}",
                machine_name(machine.as_deref()),
                machine_name(caller.as_deref())
            )));
        }
        if done {
            let mut v = get(tx, attempt)?;
            v["duplicate"] = json!(true);
            return Ok(v);
        }
        tx.execute(
            &format!("update delivery_attempts set {column}=? where id=?"),
            params![now(), attempt],
        )?;
        get(tx, attempt)
    })
}
pub fn submitted(
    db: &mut Connection,
    as_id: i64,
    session: Option<i64>,
    attempt: i64,
) -> Result<Value> {
    mark(db, as_id, session, attempt, "submitted_at")
}
pub fn received(
    db: &mut Connection,
    as_id: i64,
    session: Option<i64>,
    attempt: i64,
) -> Result<Value> {
    mark(db, as_id, session, attempt, "received_at")
}

/// The caller task's attempts, every binding it ever had; `open` = not received.
/// A read: no write transaction, no generation check.
pub fn list(db: &Connection, as_id: i64, open: bool) -> Result<Value> {
    let t = if as_id == 0 {
        worker(db)?.0
    } else {
        worker::resolve(db, as_id)?
    };
    let mut stmt = db.prepare(&format!(
        "select {COLUMNS} from delivery_attempts where task_id=?{} order by id",
        if open { " and received_at is null" } else { "" }
    ))?;
    let rows = stmt
        .query_map([t.id], row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(json!({"attempts":rows}))
}

/// Internal push read: constant-size state for the caller's event, binding and host.
pub fn state(
    db: &Connection,
    as_id: i64,
    session: Option<i64>,
    event: i64,
    adapter: &str,
    after: i64,
) -> Result<Value> {
    let (t, binding) = identify(db, as_id, session)?;
    let (launch, generation) = match binding {
        Binding::Launch(l) => (Some(l), None),
        Binding::Session(g) => (None, Some(g)),
    };
    let (received, watermark, attempt): (bool, i64, Option<i64>) = db.query_row(
        "select coalesce(max(received_at is not null),0), coalesce(max(id),0),
                max(case when id>? and adapter=? and submitted_at is null and received_at is null then id end)
         from delivery_attempts
         where task_id=? and event_id=? and launch_id is ? and session_gen is ? and machine is ?",
        params![after, adapter, t.id, event, launch, generation, caller_machine()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(json!({"received":received,"watermark":watermark,"attempt_id":attempt}))
}
