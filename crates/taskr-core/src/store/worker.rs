use super::*;

pub fn resolve(db: &Connection, as_id: i64) -> Result<Task> {
    let ts = env("TASKR_TASK");
    let ls = env("TASKR_LAUNCH");
    if as_id != 0 {
        if !ts.is_empty() && ts != as_id.to_string() {
            return Err(reject(format!(
                "--as {as_id} names a different task than TASKR_TASK={ts}"
            )));
        }
        if !ls.is_empty() {
            return Err(reject(
                "--as is for a root orchestrator, which has no launch; TASKR_LAUNCH is set",
            ));
        }
        let t = open_task(db, as_id)?;
        if t.status == "planned" {
            return Err(reject(format!("task {as_id} is planned, not launched")));
        }
        if let Some(p) = t.parent {
            return Err(reject(format!(
                "--as is only for a root orchestrator: task {as_id} has parent {p}"
            )));
        }
        if let Some(l) = t.launch {
            return Err(reject(format!(
                "--as is only for a root orchestrator: task {as_id} has launch {l}; it writes with TASKR_TASK and TASKR_LAUNCH"
            )));
        }
        check_host(db, as_id)?;
        return Ok(t);
    }
    if ts.is_empty() {
        return Err(usage("TASKR_TASK is not set"));
    }
    let tid = id(&ts, "TASKR_TASK")?;
    let t = open_task(db, tid)?;
    if t.status == "planned" {
        return Err(reject(format!(
            "task {tid} is planned, not launched; its orchestrator runs `taskr launch {tid}` first"
        )));
    }
    if let Some(pa) = planned_ancestor(db, tid)? {
        return Err(reject(format!(
            "task {tid} is under planned task {pa}, which has no inbox yet"
        )));
    }
    if ls.is_empty() {
        if let Some(l) = t.launch {
            return Err(reject(format!(
                "TASKR_LAUNCH is not set but task {tid} has launch {l}"
            )));
        }
        check_host(db, tid)?;
        return Ok(t);
    }
    let lid = ls
        .parse::<i64>()
        .map_err(|_| usage(format!("TASKR_LAUNCH must be an integer, got {ls:?}")))?;
    if t.launch != Some(lid) {
        return Err(reject(format!(
            "launch {lid} is not the current launch of task {tid}"
        )));
    }
    let machine = db.query_row("select machine from launches where id=?", [lid], |r| {
        r.get::<_, Option<String>>(0)
    })?;
    let caller = caller_machine();
    if machine != caller {
        return Err(reject(format!(
            "launch is on {}, caller is {}",
            machine_name(machine.as_deref()),
            machine_name(caller.as_deref())
        )));
    }
    Ok(t)
}
pub fn identity(db: &Connection, lid: i64) -> Result<Value> {
    let provider: String = db.query_row(
        "select coalesce(provider,'') from launches where id=?",
        [lid],
        |r| r.get(0),
    )?;
    let codex = env("CODEX_HOME");
    let claude = env("CLAUDE_CONFIG_DIR");
    let home = match provider.as_str() {
        "codex" => codex,
        "claude" => claude,
        _ => {
            if codex.is_empty() {
                claude
            } else {
                codex
            }
        }
    };
    let thread = if provider == "codex" {
        env("CODEX_THREAD_ID")
    } else {
        String::new()
    };
    let pane = env("HERDR_PANE_ID");
    let account = std::path::Path::new(&home)
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "native"))
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|p| p.to_str())
        .unwrap_or("");
    db.execute("update launches set native_home=coalesce(?,native_home),account=coalesce(?,account),session_ref=case when session_source like 'hook:%' then session_ref else coalesce(?,session_ref) end,session_kind=case when session_source like 'hook:%' then session_kind else coalesce(?,session_kind) end,session_source=case when session_source like 'hook:%' then session_source else coalesce(?,session_source) end,pane_id=coalesce(pane_id,?) where id=?",params![null(&home),null(account),null(&thread),if thread.is_empty(){None}else{Some("thread_id")},if thread.is_empty(){None}else{Some("env:CODEX_THREAD_ID")},null(&pane),lid])?;
    let mut data = json!({});
    for (k, v) in [
        ("native_home", home.as_str()),
        ("account", account),
        ("session_ref", thread.as_str()),
        ("herdr_pane_id", pane.as_str()),
    ] {
        if !v.is_empty() {
            data[k] = json!(v);
        }
    }
    Ok(data)
}
pub fn start(db: &mut Connection) -> Result<Value> {
    transaction(db, |tx| {
        let t = resolve(tx, 0)?;
        let lid = t
            .launch
            .ok_or_else(|| reject(format!("task {} has no launch to start", t.id)))?;
        let data = identity(tx, lid)?;
        let eid = event(
            tx,
            Event {
                task: t.id,
                launch: Some(lid),
                kind: "start",
                data: Some(data.clone()),
                ..Event::default()
            },
        )?;
        let mut out =
            json!({"ok":true,"kind":"start","event_id":eid,"task_id":t.id,"launch_id":lid});
        for (k, v) in data.as_object().expect("object") {
            out[k] = v.clone();
        }
        Ok(out)
    })
}
#[derive(Default)]
pub struct Write<'a> {
    pub kind: &'a str,
    pub summary: &'a str,
    pub key: &'a str,
    pub data: Option<Value>,
    pub status: &'a str,
    pub owner: bool,
    pub as_id: i64,
}
pub fn write(db: &mut Connection, w: Write<'_>) -> Result<Value> {
    for p in [
        "got:",
        "no_receipt:",
        "late_receipt:",
        "quota:",
        "capacity:",
    ] {
        if w.key.starts_with(p) {
            return Err(usage(format!(
                "--key prefix {p:?} is reserved for events taskr writes itself"
            )));
        }
    }
    let report = if matches!(w.kind, "ready" | "done" | "fail") {
        let t = resolve(db, w.as_id)?;
        documents::prepare_report(
            db,
            t.id,
            w.data
                .as_ref()
                .and_then(|d| d["report"].as_str())
                .unwrap_or(""),
            w.kind == "ready",
        )
    } else {
        None
    };
    let mut warn_owner_items = false;
    let out = transaction(db, |tx| {
        if w.kind == "note" && w.owner && (!env("TASKR_TASK").is_empty() || w.as_id == 0) {
            return Err(reject("--owner: only a root orchestrator's own note"));
        }
        let t = resolve(tx, w.as_id)?;
        if !w.key.is_empty() {
            let old=tx.query_row("select id,task_id,kind,coalesce(json_extract(data,'$.owner'),0) from events where event_key=?",[w.key],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,bool>(3)?))).optional()?;
            if let Some((eid, tid, kind, owner)) = old {
                if tid != t.id {
                    return Err(reject(format!(
                        "event key {:?} belongs to task {tid}",
                        w.key
                    )));
                }
                if kind != w.kind {
                    return Err(reject(format!(
                        "event key {:?} is a {kind} event, not {}",
                        w.key, w.kind
                    )));
                }
                if kind == "note" && owner != w.owner {
                    return Err(reject(format!(
                        "event key {:?} is a note {} --owner",
                        w.key,
                        if owner { "with" } else { "without" }
                    )));
                }
                let mut out = json!({"ok":true,"kind":w.kind,"event_id":eid,"task_id":tid,"status":t.status,"duplicate":true});
                if w.kind == "ask" {
                    out["ask_id"] = json!(eid);
                }
                return Ok(out);
            }
        }
        if w.kind == "note" && w.owner && plan::owner_has_items(w.summary) {
            let has_ask: bool = tx.query_row("with recursive tree(id) as (select ? union all select t.id from tasks t join tree on t.parent_id=tree.id) select exists(select 1 from tree join tasks t on t.id=tree.id join events e on e.task_id=t.id where t.status!='closed' and e.kind='ask' and e.answered_by is null and json_extract(e.data,'$.owner')=1)",[t.id],|r|r.get(0))?;
            warn_owner_items = !has_ask;
        }
        let to = if matches!(w.kind, "note" | "start") {
            None
        } else if w.owner {
            let r = root(tx, t.id)?;
            if r == t.id { None } else { Some(r) }
        } else {
            t.parent
        };
        recipient(tx, to)?;
        let eid = event(
            tx,
            Event {
                task: t.id,
                to,
                launch: t.launch,
                kind: w.kind,
                summary: w.summary,
                data: w.data.clone(),
                key: w.key,
                ..Event::default()
            },
        )?;
        let status = if w.status.is_empty() {
            t.status.as_str()
        } else {
            tx.execute(
                "update tasks set status=?,updated_at=? where id=?",
                params![w.status, now(), t.id],
            )?;
            w.status
        };
        let mut out =
            json!({"ok":true,"kind":w.kind,"event_id":eid,"task_id":t.id,"status":status});
        if let Some(to) = to {
            out["recipient_task_id"] = json!(to);
        }
        if w.kind == "ask" {
            out["ask_id"] = json!(eid);
        }
        documents::capture_report(tx, t.id, eid, report.as_ref())?;
        Ok(out)
    })?;
    if warn_owner_items {
        eprintln!("owner actions must be asks: taskr ask --owner ...");
    }
    Ok(out)
}
pub fn got(db: &mut Connection, attempt: i64) -> Result<Value> {
    transaction(db, |tx| got_tx(tx, attempt, None))
}

pub fn got_tx(tx: &Connection, attempt: i64, hook_identity: Option<Value>) -> Result<Value> {
    let t = resolve(tx, 0)?;
    let lid = t
        .launch
        .ok_or_else(|| reject(format!("task {} has no launch to receive prompts", t.id)))?;
    let (tid, kind, launch) = tx
        .query_row(
            "select task_id,kind,launch_id from events where id=?",
            [attempt],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| reject(format!("event {attempt} does not exist")))?;
    if kind != "prompt" {
        return Err(reject(format!(
            "event {attempt} is a {kind} event, not a prompt attempt"
        )));
    }
    if tid != t.id {
        return Err(reject(format!(
            "prompt attempt {attempt} is for task {tid}, not task {}",
            t.id
        )));
    }
    if launch != Some(lid) {
        return Err(reject(format!(
            "prompt attempt {attempt} is not for the current launch {lid} of task {}",
            t.id
        )));
    }
    let key = format!("got:{attempt}");
    let old=tx.query_row("select id,task_id,kind,launch_id,related_event_id,coalesce(data,'{}') from events where event_key=?",[&key],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,Option<i64>>(4)?,r.get::<_,String>(5)?))).optional()?;
    if let Some((eid, tid, kind, launch, related, raw)) = old {
        if kind != "got" || tid != t.id || launch != Some(lid) || related != Some(attempt) {
            return Err(reject(format!(
                "key collision: event key {key:?} is already event {eid} ({kind} on task {tid}), not this receipt"
            )));
        }
        let data: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
        return Ok(
            json!({"ok":true,"kind":"got","attempt_id":attempt,"event_id":eid,"round":data["round"].as_i64().unwrap_or(0),"duplicate":true}),
        );
    }
    let round: i64 = tx.query_row(
        "select count(*) from events where kind='prompt' and task_id=? and launch_id=?",
        params![t.id, lid],
        |r| r.get(0),
    )?;
    let mut data = json!({"round":round});
    let home: Option<String> =
        tx.query_row("select native_home from launches where id=?", [lid], |r| {
            r.get(0)
        })?;
    if home.is_none() {
        let ident = if let Some(identity) = hook_identity {
            identity
        } else {
            identity(tx, lid)?
        };
        if !ident.as_object().expect("object").is_empty() {
            data["identity"] = ident;
        }
    }
    recipient(tx, t.parent)?;
    let eid = event(
        tx,
        Event {
            task: t.id,
            to: t.parent,
            launch: Some(lid),
            kind: "got",
            summary: &format!("prompt attempt {attempt} received"),
            data: Some(data),
            related: Some(attempt),
            key: &key,
        },
    )?;
    tx.execute(
        "delete from meta where key=?",
        [format!("receipt_due:{attempt}")],
    )?;
    let nr=tx.query_row("select id,recipient_task_id,created_at from events where task_id=? and kind='prompt_outcome' and summary='no_receipt' and related_event_id=? order by id limit 1",params![t.id,attempt],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,Option<i64>>(1)?,r.get::<_,String>(2)?))).optional()?;
    if let Some((nr, to, at)) = nr {
        let got_at: String =
            tx.query_row("select created_at from events where id=?", [eid], |r| {
                r.get(0)
            })?;
        let delay = parse_time(&got_at)
            .zip(parse_time(&at))
            .map_or(0, |(a, b)| (a - b).whole_milliseconds() as i64);
        event(
            tx,
            Event {
                task: t.id,
                to: to.or(t.parent),
                launch: Some(lid),
                kind: "prompt_outcome",
                summary: "late_receipt",
                data: Some(
                    json!({"outcome":"late_receipt","got_event_id":eid,"no_receipt_event_id":nr,"delay_ms":delay}),
                ),
                related: Some(attempt),
                key: &format!("late_receipt:{attempt}"),
            },
        )?;
    }
    Ok(json!({"ok":true,"kind":"got","attempt_id":attempt,"event_id":eid,"round":round}))
}
