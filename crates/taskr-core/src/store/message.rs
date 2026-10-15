//! Root-to-root peer text uses the existing inbox and delivery receipts.
use super::*;

/// Choose once, on the ledger server, before any pane prerequisite. A push is terminal.
pub fn route(
    db: &mut Connection,
    target: i64,
    sender: i64,
    text: &str,
    file: &str,
    push: bool,
    pane: bool,
) -> Result<Option<Value>> {
    if push && pane {
        return Err(usage("--push and --pane are mutually exclusive"));
    }
    transaction(db, |tx| {
        let to = open_task(tx, target)?;
        if to.parent.is_some() {
            if sender != 0 {
                return Err(reject(
                    "--as needs a root target; worker prompts use the pane protocol",
                ));
            }
            if push {
                return Err(reject("--push needs a root target"));
            }
            return Ok(None);
        }
        if sender == target {
            return Err(reject("a root cannot message itself"));
        }
        let generation = consumer::push_generation(tx, &to)?;
        if pane || sender == 0 || generation.is_none() {
            if push {
                if sender == 0 {
                    return Err(reject(
                        "root messages need --as SENDER_ROOT (a worker cannot send them)",
                    ));
                }
                return Err(reject(format!("root {target} is not push-live")));
            }
            return Ok(None);
        }
        let from = worker::resolve(tx, sender)?;
        if from.launch.is_some() {
            return Err(reject("the sender root must have no launch"));
        }
        if !file.is_empty()
            || text.is_empty()
            || text.len() > 4096
            || text.chars().any(|c| c.is_control() && c != '\n')
        {
            return Err(usage(
                "messages need --text: 1 to 4096 UTF-8 bytes, no control characters except newline; --file is not supported",
            ));
        }
        let generation = generation.expect("live generation");
        let e = event(
            tx,
            Event {
                task: from.id,
                to: Some(target),
                kind: "message",
                summary: text,
                data: Some(json!({"text":text,"gen":generation})),
                ..Event::default()
            },
        )?;
        Ok(Some(
            json!({"path":"push","event":e,"target":target,"gen":generation,"received":false}),
        ))
    })
}

/// Sender-scoped read; receipt and handling remain separate, including across adopts.
pub fn status(db: &Connection, event: i64, sender: i64) -> Result<Value> {
    if sender <= 0 {
        return Err(usage("message status needs --as SENDER_ROOT"));
    }
    let from = worker::resolve(db, sender)?;
    let target: Option<i64> = db
        .query_row(
            "select recipient_task_id from events where id=? and kind='message' and task_id=?",
            params![event, from.id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let target =
        target.ok_or_else(|| reject(format!("message {event} was not sent by root {sender}")))?;
    let acked = task(db, target)?.acked >= event;
    let attempts: i64 = db.query_row(
        "select count(*) from delivery_attempts where event_id=?",
        [event],
        |r| r.get(0),
    )?;
    let received: Option<(String, i64)> = db.query_row(
        "select received_at,session_gen from delivery_attempts where event_id=? and task_id=? and received_at is not null order by id desc limit 1",
        params![event, target], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?;
    let mut out = json!({"event":event,"target":target,"state":if acked {"acked"} else if received.is_some() {"received"} else {"pending"},"attempts":attempts});
    if let Some((at, generation)) = received {
        out["received_at"] = json!(at);
        out["gen"] = json!(generation);
    }
    Ok(out)
}

/// Called by the adapter at its redelivery cap; the server verifies and deduplicates.
pub fn unreceived(
    db: &mut Connection,
    as_id: i64,
    session: Option<i64>,
    event_id: i64,
    fence: Option<i64>,
) -> Result<Value> {
    transaction(db, |tx| {
        let (to, _) = delivery::identify(tx, as_id, session)?;
        let f = fence.map(|fence| consumer::Fence {
            fence,
            as_id,
            session,
        });
        consumer::guard(tx, to.id, f.as_ref())?;
        let sender: Option<i64> = tx
            .query_row(
                "select task_id from events where id=? and kind='message' and recipient_task_id=?",
                params![event_id, to.id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(sender) = sender else {
            return Ok(json!({"alarmed":false}));
        };
        let (count, received): (i64, bool) = tx.query_row("select count(*),coalesce(max(received_at is not null),0) from delivery_attempts where event_id=? and task_id=? and session_gen is ?", params![event_id,to.id,session], |r|Ok((r.get(0)?,r.get(1)?)))?;
        if count < 3 || received || to.acked >= event_id {
            return Ok(json!({"alarmed":false}));
        }
        if let Some(e) = tx
            .query_row("select id from events where kind='herdr' and related_event_id=? and task_id=? and recipient_task_id=? and json_extract(data,'$.reason')='message_unreceived'", params![event_id,sender,sender], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
        {
            return Ok(json!({"alarmed":true,"event":e}));
        }
        let e = event(
            tx,
            Event {
                task: sender,
                to: Some(sender),
                kind: "herdr",
                summary: &format!(
                    "message {event_id} to root {} has no receipt after 3 pushes; inspect its pane and consumer lease before handoff or resume",
                    to.id
                ),
                data: Some(
                    json!({"reason":"message_unreceived","event":event_id,"target":to.id,"gen":session}),
                ),
                related: Some(event_id),
                ..Event::default()
            },
        )?;
        Ok(json!({"alarmed":true,"event":e}))
    })
}
