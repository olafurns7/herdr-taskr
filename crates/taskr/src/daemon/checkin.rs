//! Opt-in hub sweep; prompt claims survive delivery failures and daemon restarts.
use super::*;

pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| store::env("TASKR_CHECKIN") == "1")
}

struct Root {
    id: i64,
    name: String,
    pane: String,
    machine: Option<String>,
    created: String,
    acked: i64,
}

fn anchor(
    db: &db::Connection,
    root: &Root,
    now: time::OffsetDateTime,
) -> Result<Option<(&'static str, i64, String)>> {
    let tree = "with recursive tree(id) as (select ? union all select t.id from tasks t join tree on t.parent_id=tree.id) ";
    let signal = db.query_row(
        &format!("select max(id),min(created_at) from ({LEAD_IDLE_SIGNALS}) where recipient_task_id=? and id>?"),
        params![root.id, root.acked], |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<String>>(1)?)),
    )?;
    if let (Some(id), Some(at)) = signal
        && old(&at, now, 30)
    {
        return Ok(Some(("R1", id, at)));
    }
    let busy: bool = db.query_row(&format!("{tree}select exists(select 1 from tree join tasks t on t.id=tree.id where t.id!=? and t.status not in('closed','planned','done','failed')) or exists(select 1 from tree join tasks t on t.id=tree.id join events e on e.task_id=t.id where t.status!='closed' and e.kind='ask' and e.answered_by is null and json_extract(e.data,'$.owner')=1)"), params![root.id, root.id], |r| r.get(0))?;
    if busy {
        return Ok(None);
    }
    let latest = db.query_row(&format!("{tree}select e.id,e.created_at from tree join events e on e.task_id=tree.id where (e.task_id!=? or e.kind='prompt') and not (e.kind='prompt' and json_extract(e.data,'$.nudge') is not null) and not (e.kind='prompt_outcome' and exists(select 1 from events p where p.id=e.related_event_id and json_extract(p.data,'$.nudge') is not null)) and not {noise} order by e.id desc limit 1", noise = super::github::PR_NOISE), params![root.id, root.id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).optional()?;
    let (id, at) = latest.unwrap_or((0, root.created.clone()));
    Ok(old(&at, now, 90).then_some(("R2", id, at)))
}

fn old(at: &str, now: time::OffsetDateTime, minutes: i64) -> bool {
    store::parse_time(at).is_some_and(|at| now - at >= time::Duration::minutes(minutes))
}

pub(super) fn sweep(db: &mut db::Connection, sock: &str, log: &Log) -> Result<()> {
    let now = taskr_core::frozen_now().expect("clock");
    let stamp = store::stamp(now);
    let started = Instant::now();
    let mut stmt = db.prepare("select t.id,t.name,t.pane_id,t.machine,t.created_at,t.acked_event_id from tasks t where t.parent_id is null and t.status not in('closed','planned') and t.pane_id is not null and t.lead_status in('idle','done') and t.lead_present=1 and t.lead_observed_at is not null and coalesce(t.waiting_until,'')<=? and coalesce((select json_extract(e.data,'$.value') from events e where e.task_id=t.id and e.kind='ref' and json_extract(e.data,'$.key')='glance.state' order by e.id desc limit 1),'')!='parked' order by t.id")?;
    let roots = stmt
        .query_map([&stamp], |r| {
            Ok(Root {
                id: r.get(0)?,
                name: r.get(1)?,
                pane: r.get(2)?,
                machine: r.get(3)?,
                created: r.get(4)?,
                acked: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    let mut nudged = false;
    for root in roots {
        // Leave headroom for the guarded command's socket and pipe deadlines.
        if started.elapsed() >= Duration::from_secs(1) {
            break;
        }
        if let Some(host) = &root.machine {
            let fresh = meta(db, &format!("daemon_heartbeat:{host}"))?
                .filter(|at| at.len() == 24 && at.as_bytes()[19] == b'.' && at.ends_with('Z'))
                .and_then(|at| store::parse_time(&at))
                .is_some_and(|at| now - at < time::Duration::seconds(30));
            if !fresh {
                continue;
            }
        }
        let last_r1: Option<(i64, i64, String)> = db.query_row(
            "select id,json_extract(data,'$.anchor'),created_at from events where task_id=? and kind='prompt' and json_extract(data,'$.nudge')='R1' order by id desc limit 1",
            [root.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).optional()?;
        let overdue_r1 = last_r1
            .as_ref()
            .filter(|(_, anchor, at)| root.acked < *anchor && old(at, now, 60));
        let candidate = if let Some((_, anchor, at)) = overdue_r1 {
            Some(("R1", *anchor, at.clone()))
        } else {
            anchor(db, &root, now)?
        };
        let Some((rule, anchor, at)) = candidate else {
            continue;
        };
        let key = format!("nudge:{rule}:{}:{anchor}", root.id);
        let sent: Option<(i64, i64, String)> =
            if rule == "R1" {
                last_r1
            } else {
                db.query_row(
                "select id,json_extract(data,'$.anchor'),created_at from events where event_key=?",
                [&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ).optional()?
            };
        let replied = if rule == "R2" {
            match &sent {
                Some((id, _, _)) => db.query_row(
                    "select exists(select 1 from events where task_id=? and kind in('note','next') and id>?)",
                    params![root.id, id], |r| r.get::<_, bool>(0),
                )?,
                None => false,
            }
        } else {
            false
        };
        let overdue = sent.as_ref().is_some_and(|(_, sent_anchor, at)| {
            old(at, now, 60) && (rule != "R1" || root.acked < *sent_anchor)
        });
        if root.machine.is_some() || (overdue && !replied) {
            if root.machine.is_some() && db.query_row(
                "select exists(select 1 from meta where key glob 'escalated:R[12]:'||?||':*' and value>?)",
                params![root.id, store::stamp(now - time::Duration::minutes(60))],
                |r| r.get::<_, bool>(0),
            )? {
                continue;
            }
            let claim = format!("escalated:{rule}:{}:{anchor}", root.id);
            if db.execute(
                "insert into meta(key,value) values(?,?) on conflict(key) do nothing",
                params![claim, stamp],
            )? == 1
            {
                let at = if rule == "R1" {
                    db.query_row(
                        &format!("select min(created_at) from ({LEAD_IDLE_SIGNALS}) where recipient_task_id=? and id>?"),
                        params![root.id, root.acked], |r| r.get::<_, Option<String>>(0),
                    )?.unwrap_or(at)
                } else {
                    at
                };
                let age = (now - store::parse_time(&at).expect("validated anchor")).whole_minutes();
                let body = format!("{}: {rule} {age}m", root.name);
                if let Err(e) = tokens::run(
                    sock,
                    &[
                        "notification",
                        "show",
                        "taskr: stale",
                        "--body",
                        &body,
                        "--sound",
                        "request",
                    ],
                    Duration::from_secs(1),
                ) {
                    log.line(&e.message);
                }
            }
            continue;
        }
        if nudged
            || sent
                .as_ref()
                .is_some_and(|(_, sent_anchor, _)| *sent_anchor == anchor)
            || root.pane.is_empty()
        {
            continue;
        }
        let text = if rule == "R1" {
            "taskr check-in (automatic, not the owner): process your lane results"
        } else {
            "taskr check-in (automatic, not the owner): say why with a root note, park, or close"
        };
        let attempt = store::transaction(db, |tx| {
            if tx.query_row(
                "select exists(select 1 from events where event_key=?)",
                [&key],
                |r| r.get::<_, bool>(0),
            )? {
                return Ok(None);
            }
            let last: Option<String> = tx.query_row("select created_at from events where task_id=? and kind='prompt' and json_extract(data,'$.nudge') is not null order by id desc limit 1", [root.id], |r| r.get(0)).optional()?;
            if last.is_some_and(|at| !old(&at, now, 60)) {
                return Ok(None);
            }
            Ok(Some(store::event(
                tx,
                store::Event {
                    task: root.id,
                    kind: "prompt",
                    summary: text,
                    data: Some(json!({"nudge":rule,"anchor":anchor})),
                    key: &key,
                    ..Default::default()
                },
            )?))
        })?;
        let Some(attempt) = attempt else { continue };
        nudged = true;
        let result = tokens::run(
            sock,
            &["agent", "prompt", &root.pane, text],
            Duration::from_secs(1),
        );
        let outcome = if result.is_ok() {
            "delivered"
        } else {
            "delivery_unknown"
        };
        store::event(
            db,
            store::Event {
                task: root.id,
                kind: "prompt_outcome",
                summary: outcome,
                data: Some(json!({"outcome":outcome})),
                related: Some(attempt),
                ..Default::default()
            },
        )?;
        if let Err(e) = result {
            log.line(&e.message);
        }
    }
    Ok(())
}
