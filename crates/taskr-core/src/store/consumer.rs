//! `taskr consumer`: an opt-in lease that fences inbox consumption. Only `take` creates
//! or bumps a lease; nothing here writes acked_event_id or pending_event_id. A lease is
//! live while it has a holder, expires_at is in the future and its binding (a worker's
//! launch, a root's adopt generation) is still current on an open task. With no live
//! lease, plain offer/ack behave exactly as before.
use super::delivery::{Binding, generation, identify, worker as env_worker};
use super::*;

/// A fenced caller: the fence it holds and the identity it pinned. `as_id` 0 is the
/// worker from TASKR_TASK/TASKR_LAUNCH; a root names `--as ROOT --session GEN`.
pub struct Fence {
    pub fence: i64,
    pub as_id: i64,
    pub session: Option<i64>,
}

struct Lease {
    fence: i64,
    holder: Option<String>,
    binding: Option<Binding>,
    machine: Option<String>,
    acquired_at: String,
    expires_at: String,
}

fn load(db: &Connection, task_id: i64) -> Result<Option<Lease>> {
    Ok(db
        .query_row(
            "select fence,holder,launch_id,session_gen,machine,acquired_at,expires_at from consumer_leases where task_id=?",
            [task_id],
            |r| {
                let binding = match (r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?) {
                    (Some(l), _) => Some(Binding::Launch(l)),
                    (_, Some(g)) => Some(Binding::Session(g)),
                    _ => None,
                };
                Ok(Lease {
                    fence: r.get(0)?,
                    holder: r.get(1)?,
                    binding,
                    machine: r.get(4)?,
                    acquired_at: r.get(5)?,
                    expires_at: r.get(6)?,
                })
            },
        )
        .optional()?)
}

/// Held, unexpired (expires_at is exclusive) and bound to the task's current launch or
/// generation on an open task.
fn live(db: &Connection, t: &Task, l: &Lease) -> Result<bool> {
    if l.holder.is_none() || l.expires_at <= now() || t.status == "closed" {
        return Ok(false);
    }
    Ok(match l.binding {
        Some(Binding::Launch(lid)) => t.launch == Some(lid),
        Some(Binding::Session(g)) => generation(db, t.id)? == g,
        None => false,
    })
}

/// Read target liveness without impersonating its consumer.
pub fn push_generation(db: &Connection, t: &Task) -> Result<Option<i64>> {
    if t.parent.is_some() || t.launch.is_some() || t.status != "open" {
        return Ok(None);
    }
    if let Some(l) = load(db, t.id)?
        && live(db, t, &l)?
        && l.holder
            .as_deref()
            .is_some_and(|h| h.starts_with("mcp-channel-") || h.starts_with("codex-queue-"))
        && let Some(Binding::Session(g)) = l.binding
    {
        return Ok(Some(g));
    }
    Ok(None)
}

fn held_message(task_id: i64, l: &Lease) -> String {
    format!(
        "task {task_id}'s inbox is held by consumer {} with fence {} until {}; the holder hands it back with `taskr consumer release --fence {}`, or take it over with `taskr consumer take --force`",
        l.holder.as_deref().unwrap_or(""),
        l.fence,
        l.expires_at,
        l.fence
    )
}

/// The common guard of every inbox consumption (offer, ack) and of `delivery new`, run
/// inside the caller's transaction before it reads or changes anything. Plain callers
/// pass only while no lease is live. A fenced caller is identified again here (its
/// pinned launch or generation, never refreshed) and must hold the live fence.
pub fn guard(db: &Connection, task_id: i64, fence: Option<&Fence>) -> Result<()> {
    let Some(f) = fence else {
        let t = task(db, task_id)?;
        if let Some(l) = load(db, task_id)?
            && live(db, &t, &l)?
        {
            return Err(reject(held_message(task_id, &l)));
        }
        return Ok(());
    };
    let (t, binding) = identify(db, f.as_id, f.session)?;
    if t.id != task_id {
        return Err(reject(format!(
            "the fenced caller is task {}, not task {task_id}",
            t.id
        )));
    }
    holding(db, &t, binding, f.fence).map(|_| ())
}

/// The live lease the caller holds with `fence`, from the same binding and host.
fn holding(db: &Connection, t: &Task, binding: Binding, fence: i64) -> Result<Lease> {
    let l = match load(db, t.id)? {
        Some(l) if live(db, t, &l)? => l,
        _ => {
            return Err(reject(format!(
                "fence {fence} is not live: task {} has no live consumer lease",
                t.id
            )));
        }
    };
    if l.fence != fence {
        return Err(reject(format!(
            "fence {fence} is stale: task {}'s live lease has fence {}",
            t.id, l.fence
        )));
    }
    if l.binding != Some(binding) {
        return Err(reject(format!(
            "fence {fence} of task {} is bound to another launch or session",
            t.id
        )));
    }
    let caller = caller_machine();
    if l.machine != caller {
        return Err(reject(format!(
            "fence {fence} of task {} is on {}, caller is {}",
            t.id,
            machine_name(l.machine.as_deref()),
            machine_name(caller.as_deref())
        )));
    }
    Ok(l)
}

fn view(db: &Connection, t: &Task) -> Result<Value> {
    let Some(l) = load(db, t.id)? else {
        return Ok(
            json!({"task_id":t.id,"fence":0,"holder":null,"launch_id":null,"session_gen":null,"machine":null,"acquired_at":null,"expires_at":null,"live":false}),
        );
    };
    let (launch, session) = match l.binding {
        Some(Binding::Launch(x)) => (Some(x), None),
        Some(Binding::Session(g)) => (None, Some(g)),
        None => (None, None),
    };
    Ok(json!({
        "task_id": t.id,
        "fence": l.fence,
        "holder": l.holder,
        "launch_id": launch,
        "session_gen": session,
        "machine": l.machine,
        "acquired_at": l.acquired_at,
        "expires_at": l.expires_at,
        "live": live(db, t, &l)?,
    }))
}

fn deadline(ttl: i64) -> String {
    stamp(frozen_now().expect("validated frozen clock") + millis(ttl))
}

/// Take the caller's inbox lease: fence = previous fence + 1. Refused while a live lease
/// or a future wait marker (advisory, possibly stale) exists, unless `force`.
pub fn take(
    db: &mut Connection,
    as_id: i64,
    session: Option<i64>,
    holder: &str,
    ttl: i64,
    force: bool,
) -> Result<Value> {
    transaction(db, |tx| {
        let (t, binding) = identify(tx, as_id, session)?;
        let old = load(tx, t.id)?;
        if !force {
            if let Some(l) = &old
                && live(tx, &t, l)?
            {
                return Err(reject(held_message(t.id, l)));
            }
            if let Some(w) = t.waiting.as_deref().filter(|w| *w > now().as_str()) {
                return Err(reject(format!(
                    "a wait on task {}'s inbox is marked until {w} (it may be stale); stop it, or take over with `taskr consumer take --force`",
                    t.id
                )));
            }
        }
        let (launch, session_gen) = match binding {
            Binding::Launch(l) => (Some(l), None),
            Binding::Session(g) => (None, Some(g)),
        };
        tx.execute(
            "insert into consumer_leases(task_id,fence,holder,launch_id,session_gen,machine,acquired_at,expires_at) values(?1,?2,?3,?4,?5,?6,?7,?8)
             on conflict(task_id) do update set fence=?2,holder=?3,launch_id=?4,session_gen=?5,machine=?6,acquired_at=?7,expires_at=?8",
            params![
                t.id,
                old.map_or(0, |l| l.fence) + 1,
                holder,
                launch,
                session_gen,
                caller_machine(),
                now(),
                deadline(ttl)
            ],
        )?;
        view(tx, &t)
    })
}

/// Extend the live lease the caller holds with `fence`.
pub fn renew(
    db: &mut Connection,
    as_id: i64,
    session: Option<i64>,
    fence: i64,
    ttl: i64,
) -> Result<Value> {
    transaction(db, |tx| {
        let (t, binding) = identify(tx, as_id, session)?;
        holding(tx, &t, binding, fence)?;
        tx.execute(
            "update consumer_leases set expires_at=? where task_id=?",
            params![deadline(ttl), t.id],
        )?;
        view(tx, &t)
    })
}

/// Hand the lease back: the holder is cleared, the fence counter stays.
pub fn release(db: &mut Connection, as_id: i64, session: Option<i64>, fence: i64) -> Result<Value> {
    transaction(db, |tx| {
        let (t, binding) = identify(tx, as_id, session)?;
        holding(tx, &t, binding, fence)?;
        tx.execute(
            "update consumer_leases set holder=null where task_id=?",
            [t.id],
        )?;
        view(tx, &t)
    })
}

/// The caller task's lease and whether it is live. A read.
pub fn show(db: &Connection, as_id: i64) -> Result<Value> {
    let t = if as_id == 0 {
        env_worker(db)?.0
    } else {
        worker::resolve(db, as_id)?
    };
    view(db, &t)
}
