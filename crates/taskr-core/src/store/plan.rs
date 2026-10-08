use super::*;
pub fn next(db: &mut Connection, id: i64, text: &str, clear: bool) -> Result<Value> {
    transaction(db, |tx| {
        open_task(tx, id)?;
        let eid = event(
            tx,
            Event {
                task: id,
                kind: "next",
                summary: text,
                data: if clear {
                    Some(json!({"clear":true}))
                } else {
                    None
                },
                ..Event::default()
            },
        )?;
        Ok(
            json!({"ok":true,"task_id":id,"next":if clear{Value::Null}else{json!(text)},"event_id":eid}),
        )
    })
}
pub fn decide(db: &mut Connection, id: i64, text: &str, revoke: i64) -> Result<Value> {
    transaction(db, |tx| {
        worker::resolve(tx, id)?;
        if revoke == 0 {
            let eid = event(
                tx,
                Event {
                    task: id,
                    kind: "decision",
                    summary: text,
                    ..Event::default()
                },
            )?;
            return Ok(
                json!({"ok":true,"task_id":id,"event_id":eid,"kind":"decision","decision_id":eid}),
            );
        }
        let target=tx.query_row("with recursive sub(id) as(select ?1 union all select t.id from tasks t join sub on t.parent_id=sub.id) select e.kind,coalesce(e.summary,'') from events e where e.id=?2 and((e.kind='decision' and e.task_id=?1) or(e.kind='ask' and e.answered_by is not null and json_extract(e.data,'$.owner')=1 and e.task_id in(select id from sub))) and not exists(select 1 from events r where r.kind='revoke' and r.related_event_id=e.id and r.task_id=?1)",params![id,revoke],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?;
        let Some((kind, summary)) = target else {
            let by=tx.query_row("select id from events where kind='revoke' and related_event_id=? and task_id=?",params![revoke,id],|r|r.get::<_,i64>(0)).optional()?;
            if let Some(by) = by {
                return Err(reject(format!(
                    "event {revoke} is already revoked by event {by}"
                )));
            }
            return Err(reject(format!(
                "event {revoke} is not a decision in force under task {id}"
            )));
        };
        let summary = if summary.chars().count() > 300 {
            format!("{}…", summary.chars().take(299).collect::<String>())
        } else {
            summary
        };
        let eid = event(
            tx,
            Event {
                task: id,
                kind: "revoke",
                summary: &summary,
                data: Some(
                    json!({"revoked":revoke,"revoked_kind":if kind=="ask"{"owner_ask"}else{"decision"}}),
                ),
                related: Some(revoke),
                ..Event::default()
            },
        )?;
        Ok(json!({"ok":true,"task_id":id,"event_id":eid,"kind":"revoke","revoked":revoke}))
    })
}
