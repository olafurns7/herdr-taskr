use super::*;
pub fn ack(db: &mut Connection, as_id: i64, eid: i64, already: bool) -> Result<Value> {
    transaction(db, |tx| {
        let t = task(tx, as_id)?;
        check_host(tx, as_id)?;
        if t.status == "planned" {
            return Err(reject(format!(
                "task {as_id} is planned and has no inbox; launch it first"
            )));
        }
        let mut out = json!({"ok":true,"as":as_id,"acked_event_id":eid});
        if t.pending == Some(eid) {
            tx.execute(
                "update tasks set acked_event_id=?,pending_event_id=null where id=?",
                params![eid, as_id],
            )?;
            return Ok(out);
        }
        let to = tx
            .query_row(
                "select recipient_task_id from events where id=?",
                [eid],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        if already && eid <= t.acked && to == Some(as_id) {
            out["already"] = json!(true);
            out["acked_event_id"] = json!(t.acked);
            return Ok(out);
        }
        if let Some(p) = t.pending {
            return Err(reject(format!(
                "event {eid} is not the pending event {p} of task {as_id}"
            )));
        }
        Err(reject(format!(
            "task {as_id} has no pending event; run wait first"
        )))
    })
}
pub fn load_event(db: &Connection, eid: i64) -> Result<Value> {
    let mut out=db.query_row("select e.id,e.task_id,t.name,e.kind,e.created_at,e.recipient_task_id,e.launch_id,e.summary,e.data,e.related_event_id,e.answered_by,e.event_key from events e join tasks t on t.id=e.task_id where e.id=?",[eid],|r|{
        let mut v=json!({"id":r.get::<_,i64>(0)?,"task_id":r.get::<_,i64>(1)?,"record":"event","task_name":r.get::<_,String>(2)?,"kind":r.get::<_,String>(3)?,"created_at":r.get::<_,String>(4)?});
        for (idx,key) in [(5,"recipient_task_id"),(6,"launch_id"),(9,"related_event_id"),(10,"answered_by")]{if let Some(n)=r.get::<_,Option<i64>>(idx)?{v[key]=json!(n);}}
        for (idx,key) in [(7,"summary"),(11,"event_key")]{if let Some(s)=r.get::<_,Option<String>>(idx)?{v[key]=json!(s);}}
        if let Some(s)=r.get::<_,Option<String>>(8)? && let Ok(d)=serde_json::from_str::<Value>(&s){v["data"]=crate::event_data(d);}
        Ok(v)
    })?;
    if out["data"].is_null() {
        out.as_object_mut().expect("object").remove("data");
    }
    Ok(out)
}
pub fn bypass(ev: &Value) -> bool {
    ev["kind"] == "herdr"
        && (ev["data"]["reason"] == "model_capacity"
            || ev["data"]["reason"] == "stall"
            || ev["data"].get("quota").is_some())
}
fn coalesce(
    tx: &Connection,
    as_id: i64,
    ev: &Value,
    pending: Option<i64>,
    filtered: bool,
) -> Result<bool> {
    let kind = ev["kind"].as_str().unwrap_or("");
    let eid = ev["id"].as_i64().expect("event id");
    if !filtered && !(kind == "prompt_outcome" && ev["summary"] == "late_receipt") {
        return Ok(false);
    }
    if bypass(ev) {
        return Ok(false);
    }
    if kind == "herdr" && filtered {
        if ev["data"].get("reason").is_some() || ev["data"].get("quota").is_some() {
            return Ok(false);
        }
        let t = task(tx, ev["task_id"].as_i64().expect("task"))?;
        let lid = ev["launch_id"].as_i64();
        if t.status == "closed" || lid.is_none() || t.launch != lid {
            return Ok(true);
        }
        return Ok(tx.query_row("select exists(select 1 from events where recipient_task_id=? and id>? and launch_id=? and ((kind='herdr' and json_type(data,'$.reason') is null and json_type(data,'$.quota') is null) or kind in('ready','done','fail','ask')))",params![as_id,eid,lid],|r|r.get(0))?);
    }
    if kind != "prompt_outcome" {
        return Ok(false);
    }
    if ev["summary"] == "no_receipt" {
        if pending == Some(eid) {
            return Ok(false);
        }
        let got: bool = tx.query_row(
            "select exists(select 1 from events where kind='got' and related_event_id=?)",
            [ev["related_event_id"].as_i64()],
            |r| r.get(0),
        )?;
        if !got {
            return Ok(false);
        }
        tx.execute(
            "insert into meta(key,value) values(?,'1') on conflict(key) do nothing",
            [format!("no_receipt_coalesced:{eid}")],
        )?;
        return Ok(true);
    }
    if ev["summary"] == "late_receipt"
        && let Some(nr) = ev["data"]["no_receipt_event_id"].as_f64()
    {
        let n = tx.execute(
            "delete from meta where key=?",
            [format!("no_receipt_coalesced:{}", nr as i64)],
        )?;
        return Ok(filtered && n > 0);
    }
    Ok(false)
}
pub fn offer(
    db: &mut Connection,
    as_id: i64,
    filtered: bool,
) -> Result<(Option<Value>, bool, bool)> {
    transaction(db, |tx| {
        let t = task(tx, as_id)?;
        check_host(tx, as_id)?;
        let eid = tx
            .query_row(
                "select id from events where recipient_task_id=? and id>? order by id limit 1",
                params![as_id, t.acked],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        let Some(eid) = eid else {
            return Ok((None, false, false));
        };
        let ev = load_event(tx, eid)?;
        let skip = coalesce(tx, as_id, &ev, t.pending, filtered)?;
        if skip {
            tx.execute(
                "update tasks set acked_event_id=?,pending_event_id=null where id=?",
                params![eid, as_id],
            )?;
            return Ok((None, false, true));
        }
        let replay = t.pending == Some(eid);
        if !replay {
            tx.execute(
                "update tasks set pending_event_id=? where id=?",
                params![eid, as_id],
            )?;
        }
        Ok((Some(ev), replay, false))
    })
}
pub fn counts(db: &Connection, as_id: i64) -> Result<(i64, i64)> {
    let owed=db.query_row("select count(*) from tasks t where parent_id=? and current_launch_id is not null and status!='closed' and coalesce((select max(id) from events where launch_id=t.current_launch_id and kind='prompt'),0)>coalesce((select max(id) from events where launch_id=t.current_launch_id and kind in('done','fail')),0)",[as_id],|r|r.get(0))?;
    let due=db.query_row("select count(*) from meta where key>='receipt_due:' and key<'receipt_due;' and json_valid(value) and json_extract(value,'$.recipient')=?",[as_id],|r|r.get(0))?;
    Ok((owed, due))
}
/// Like Go's expireReceiptsNow: the write lock only when a deadline is pending.
pub fn expire(db: &mut Connection) -> Result<()> {
    let pending: bool = db.query_row(
        "select exists(select 1 from meta where key>='receipt_due:' and key<'receipt_due;')",
        [],
        |r| r.get(0),
    )?;
    if !pending {
        return Ok(());
    }
    transaction(db, |tx| {
        let mut stmt = tx.prepare(
            "select key,value from meta where key>='receipt_due:' and key<'receipt_due;'",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (key, raw) in rows {
            let attempt = key.trim_start_matches("receipt_due:").parse::<i64>();
            let data = serde_json::from_str::<Value>(&raw);
            let (attempt, d) = match (attempt, data) {
                (Ok(a), Ok(d)) => (a, d),
                _ => {
                    tx.execute("delete from meta where key=?", [key])?;
                    continue;
                }
            };
            if d["due_at"].as_str().is_some_and(|s| s > now().as_str()) {
                continue;
            }
            tx.execute("delete from meta where key=?", [key])?;
            let Some(tid) = d["task"].as_i64() else {
                continue;
            };
            let Ok(t) = task(tx, tid) else {
                continue;
            };
            let launch = d["launch"].as_i64();
            if t.status == "closed" || launch.is_none() || t.launch != launch {
                continue;
            }
            let old=tx.query_row("select created_at,exists(select 1 from events where event_key in(?,?)) from events where id=?",params![format!("got:{attempt}"),format!("no_receipt:{attempt}"),attempt],|r|Ok((r.get::<_,String>(0)?,r.get::<_,bool>(1)?))).optional()?;
            if let Some((at, false)) = old {
                let window = parse_time(d["due_at"].as_str().unwrap_or(""))
                    .zip(parse_time(&at))
                    .map_or(0, |(a, b)| (a - b).whole_milliseconds() as i64);
                event(
                    tx,
                    Event {
                        task: tid,
                        to: d["recipient"].as_i64(),
                        launch,
                        kind: "prompt_outcome",
                        summary: "no_receipt",
                        data: Some(json!({"outcome":"no_receipt","window_ms":window,"async":true})),
                        related: Some(attempt),
                        key: &format!("no_receipt:{attempt}"),
                    },
                )?;
            }
        }
        Ok(())
    })
}
