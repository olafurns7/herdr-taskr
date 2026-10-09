//! `taskr after`: cross-tree subscriptions. A matching event fires one `after` event
//! into the waiter's inbox inside the transaction that inserted it.
use super::*;

pub const TASK_KINDS: &[&str] = &["done", "ready", "closed", "fail"];
pub const PR_SUBS: &[&str] = &[
    "checks_green",
    "checks_failed",
    "dirty",
    "behind",
    "blocked",
    "thread_opened",
    "threads_clear",
    "merged",
    "closed",
];

/// The target and kind an event can fire: a task's done/ready/closed/fail, or a
/// `pr` event's sub. `after` itself never matches.
fn source(e: &Event<'_>) -> Option<(String, String)> {
    match e.kind {
        "done" | "ready" | "closed" | "fail" => Some((e.task.to_string(), e.kind.into())),
        "pr" => {
            let d = e.data.as_ref()?;
            Some((
                format!("pr:{}", d["pr"].as_str()?),
                d["sub"].as_str()?.into(),
            ))
        }
        _ => None,
    }
}
pub(super) fn fire(db: &Connection, eid: i64, e: &Event<'_>) -> Result<()> {
    let Some((target, on)) = source(e) else {
        return Ok(());
    };
    let mut stmt = db.prepare("select s.id,s.waiter_task_id,s.kinds,s.keep from subscriptions s join tasks t on t.id=s.waiter_task_id where s.target=? and s.fired_at is null and t.status!='closed' order by s.id")?;
    let subs = stmt
        .query_map([&target], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let label = if target.starts_with("pr:") {
        target.clone()
    } else {
        format!("task {target}")
    };
    for (id, waiter, kinds, keep) in subs {
        if !kinds.split(',').any(|k| k == on) {
            continue;
        }
        if !keep {
            db.execute(
                "update subscriptions set fired_at=? where id=?",
                params![now(), id],
            )?;
        }
        event(
            db,
            Event {
                task: e.task,
                to: Some(waiter),
                kind: "after",
                summary: &format!("{label} {on}"),
                data: Some(json!({"target":target,"on":on,"source_event_id":eid})),
                ..Event::default()
            },
        )?;
    }
    Ok(())
}
/// Subscribe root `as_id` to `target` (a task id, or `pr:owner/repo#N`), already
/// canonical; `kinds` is a validated comma list.
pub fn add(
    db: &mut Connection,
    as_id: i64,
    target: &str,
    kinds: &str,
    keep: bool,
) -> Result<Value> {
    transaction(db, |tx| {
        worker::resolve(tx, as_id)?;
        if let Ok(id) = target.parse::<i64>() {
            open_task(tx, id)?;
        }
        tx.execute(
            "insert into subscriptions(waiter_task_id,target,kinds,keep,created_at) values(?,?,?,?,?)",
            params![as_id, target, kinds, keep, now()],
        )?;
        Ok(
            json!({"ok":true,"subscription_id":tx.last_insert_rowid(),"target":target,"on":kinds,"keep":keep}),
        )
    })
}
/// A read: no write transaction.
pub fn list(db: &Connection, as_id: i64) -> Result<Value> {
    worker::resolve(db, as_id)?;
    let mut stmt = db.prepare("select id,target,kinds,keep,created_at,fired_at from subscriptions where waiter_task_id=? order by id")?;
    let rows = stmt
        .query_map([as_id], |r| {
            Ok(json!({"id":r.get::<_,i64>(0)?,"target":r.get::<_,String>(1)?,"on":r.get::<_,String>(2)?,"keep":r.get::<_,bool>(3)?,"created_at":r.get::<_,String>(4)?,"fired_at":r.get::<_,Option<String>>(5)?}))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(json!({"subscriptions":rows}))
}
pub fn cancel(db: &mut Connection, id: i64) -> Result<Value> {
    transaction(db, |tx| {
        if tx.execute("delete from subscriptions where id=?", [id])? == 0 {
            return Err(reject(format!("subscription {id} does not exist")));
        }
        Ok(json!({"ok":true,"subscription_id":id,"cancelled":true}))
    })
}
/// `close` cancels the closing task's own subscriptions.
pub(super) fn cancel_waiter(db: &Connection, waiter: i64) -> Result<()> {
    db.execute("delete from subscriptions where waiter_task_id=?", [waiter])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Roots 1 and 4 wait on lane 3 of root 2.
    fn fixture() -> Connection {
        let mut db = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut db).unwrap();
        for (id, parent) in [(1, None), (2, None), (3, Some(2)), (4, None)] {
            db.execute("insert into tasks(id,parent_id,name,role,created_at,updated_at) values(?,?,?,'orchestrator',?,?)", params![id, parent, format!("t{id}"), now(), now()]).unwrap();
        }
        db
    }
    fn sub(db: &Connection, waiter: i64, target: &str, kinds: &str, keep: bool) {
        db.execute("insert into subscriptions(waiter_task_id,target,kinds,keep,created_at) values(?,?,?,?,?)", params![waiter, target, kinds, keep, now()]).unwrap();
    }
    fn emit(db: &Connection, task: i64, kind: &str, data: Option<Value>) -> i64 {
        event(
            db,
            Event {
                task,
                kind,
                data,
                ..Event::default()
            },
        )
        .unwrap()
    }
    /// Delivered `after` events as (recipient, target, on, source event), oldest first.
    fn fired(db: &Connection) -> Vec<(i64, String, String, i64)> {
        let mut stmt = db.prepare("select recipient_task_id,json_extract(data,'$.target'),json_extract(data,'$.on'),json_extract(data,'$.source_event_id') from events where kind='after' and recipient_task_id is not null order by id").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }
    fn row(to: i64, target: &str, on: &str, source: i64) -> (i64, String, String, i64) {
        (to, target.into(), on.into(), source)
    }
    #[test]
    fn one_shot_and_keep_fire_for_task_targets() {
        let db = fixture();
        sub(&db, 1, "3", "done,closed", false);
        sub(&db, 4, "3", "ready", true);
        let r1 = emit(&db, 3, "ready", None);
        let r2 = emit(&db, 3, "ready", None);
        let d = emit(&db, 3, "done", None);
        emit(&db, 3, "closed", None);
        emit(&db, 2, "done", None);
        emit(&db, 3, "note", None);
        assert_eq!(
            fired(&db),
            [
                row(4, "3", "ready", r1),
                row(4, "3", "ready", r2),
                row(1, "3", "done", d)
            ]
        );
        let open: Vec<(i64, bool)> = db
            .prepare("select waiter_task_id,fired_at is null from subscriptions order by id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            open,
            [(1, false), (4, true)],
            "one-shot fired, keep still open"
        );
        let (task, summary, launch): (i64, String, Option<i64>) = db.query_row("select task_id,summary,launch_id from events where kind='after' and recipient_task_id=1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((task, summary.as_str(), launch), (3, "task 3 done", None));
    }
    #[test]
    fn pr_targets_fire_on_their_sub() {
        let db = fixture();
        let t = "pr:demo-org/demo#7";
        sub(&db, 1, t, "merged", false);
        sub(&db, 4, t, "checks_green,merged", true);
        let pr = |sub: &str| Some(json!({"pr":"demo-org/demo#7","sub":sub,"head":"abc"}));
        let g = emit(&db, 3, "pr", pr("checks_green"));
        emit(
            &db,
            3,
            "pr",
            Some(json!({"pr":"demo-org/demo#8","sub":"merged"})),
        );
        let m = emit(&db, 3, "pr", pr("merged"));
        emit(&db, 3, "pr", pr("merged"));
        assert_eq!(
            fired(&db),
            [
                row(4, t, "checks_green", g),
                row(1, t, "merged", m),
                row(4, t, "merged", m),
                row(4, t, "merged", m + 3)
            ]
        );
        let summary: String = db
            .query_row(
                "select summary from events where kind='after' and recipient_task_id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(summary, "pr:demo-org/demo#7 merged");
    }
    #[test]
    fn an_after_event_never_matches() {
        let db = fixture();
        sub(&db, 1, "3", "done", true);
        sub(&db, 4, "3", "done", true);
        // Each `after` carries task 3; neither fires a subscription on task 3 again.
        emit(&db, 3, "done", None);
        assert_eq!(fired(&db).len(), 2);
        emit(&db, 3, "after", Some(json!({"target":"3","on":"done"})));
        assert_eq!(fired(&db).len(), 2);
        let total: i64 = db
            .query_row("select count(*) from events where kind='after'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(total, 3);
    }
    #[test]
    fn closed_waiters_are_skipped_and_close_cancels() {
        let mut db = fixture();
        sub(&db, 1, "3", "done", true);
        sub(&db, 4, "3", "done", true);
        sub(&db, 1, "pr:demo-org/demo#7", "merged", false);
        db.execute("update tasks set status='closed' where id=4", [])
            .unwrap();
        let d = emit(&db, 3, "done", None);
        assert_eq!(fired(&db), [row(1, "3", "done", d)]);
        orch::close(&mut db, 1, "").unwrap();
        let left = |db: &Connection, w: i64| -> i64 {
            db.query_row(
                "select count(*) from subscriptions where waiter_task_id=?",
                [w],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!((left(&db, 1), left(&db, 4)), (0, 1));
        emit(&db, 3, "done", None);
        assert_eq!(fired(&db).len(), 1);
    }
}
