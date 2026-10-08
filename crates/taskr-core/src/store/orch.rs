use super::*;
use crate::goflag::FlagSet;

pub fn absolute(base: &str, path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let path = std::path::Path::new(path);
    let combined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::path::Path::new(base).join(path)
    };
    let mut clean = std::path::PathBuf::new();
    for c in combined.components() {
        match c {
            std::path::Component::ParentDir => {
                clean.pop();
            }
            std::path::Component::CurDir => {}
            _ => clean.push(c.as_os_str()),
        }
    }
    clean.to_string_lossy().into_owned()
}
pub fn valid_name(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 32
        || !value.as_bytes()[0].is_ascii_lowercase()
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(usage(format!("{field} must match [a-z][a-z0-9_-]{{0,31}}")));
    }
    Ok(())
}
pub fn validate_location(fs: &FlagSet) -> Result<()> {
    for name in ["workspace", "tab", "pane"] {
        if fs.was_set(name) {
            let v = fs.get_string(name);
            if v.is_empty()
                || v == "null"
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b":_-".contains(&b))
            {
                return Err(usage(format!(
                    "--{name} must be a non-empty, non-whitespace ID matching [0-9A-Za-z:_-]+ and not `null`"
                )));
            }
        }
    }
    if fs.was_set("workspace") {
        for name in ["tab", "pane"] {
            if fs.was_set(name)
                && let Some((prefix, _)) = fs.get_string(name).split_once(':')
                && prefix != fs.get_string("workspace")
            {
                return Err(usage(format!(
                    "--{name} must use --workspace prefix {:?} before the first colon (ID shape: [0-9A-Za-z:_-]+)",
                    fs.get_string("workspace")
                )));
            }
        }
    }
    Ok(())
}
pub fn machine(
    db: &Connection,
    value: &str,
    given: bool,
    default: Option<String>,
) -> Result<Option<String>> {
    if !given {
        return Ok(default);
    }
    let local = local_machine();
    if value == local {
        return Ok(None);
    }
    if !value.is_empty() && caller_machine().as_deref() == Some(value) {
        return Ok(Some(value.into()));
    }
    let mut stmt = db.prepare(
        "select substr(key,18),value from meta where key like 'daemon_heartbeat:%' order by key",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let fresh: Vec<_> = rows
        .into_iter()
        .filter(|(_, at)| {
            parse_time(at)
                .is_some_and(|t| frozen_now().expect("clock") - t < time::Duration::seconds(30))
        })
        .map(|(m, _)| m)
        .collect();
    if !value.is_empty() && fresh.iter().any(|m| m == value) {
        return Ok(Some(value.into()));
    }
    let mut labels = vec![local];
    for label in caller_machine().into_iter().chain(fresh) {
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    let labels = labels.join(", ");
    Err(usage(format!(
        "--machine {value:?} is not this host, the server, or a host with a fresh daemon; hosts: {labels}"
    )))
}
pub fn new(db: &mut Connection, fs: &FlagSet) -> Result<Value> {
    let cwd = caller_cwd()?;
    let dir = absolute(
        &cwd,
        if fs.get_string("cwd").is_empty() {
            &cwd
        } else {
            fs.get_string("cwd")
        },
    );
    let brief = absolute(&dir, fs.get_string("brief"));
    let report = absolute(&dir, fs.get_string("report"));
    let input = if brief.is_empty() {
        None
    } else {
        Some(documents::file(
            &brief,
            &caller_machine().unwrap_or_default(),
        ))
    };
    let out = transaction(db, |tx| {
        let host = machine(
            tx,
            fs.get_string("machine"),
            fs.was_set("machine"),
            caller_machine(),
        )?;
        if !fs.get_bool("planned") && fs.get_string("role") != "gate" {
            if host.is_none() && !std::path::Path::new(&dir).is_dir() {
                return Err(usage(format!(
                    "--cwd must be an existing directory, got {dir:?}"
                )));
            }
            if caller_machine().is_none()
                && !brief.is_empty()
                && !std::path::Path::new(&brief).is_file()
            {
                return Err(usage(format!(
                    "--brief must be an existing file, got {brief:?}"
                )));
            }
        }
        let parent = fs.get_int("parent");
        if parent != 0 {
            let p = task(tx, parent)?;
            if p.status == "closed" {
                return Err(reject(format!("parent task {parent} is closed")));
            }
        }
        let status = if fs.get_bool("planned") {
            "planned"
        } else {
            "open"
        };
        let at = now();
        tx.execute("insert into tasks(parent_id,name,role,status,workspace_id,tab_id,pane_id,cwd,brief_path,report_path,machine,created_at,updated_at) values(?,?,?,?,?,?,?,?,?,?,?,?,?)",params![if parent==0{None}else{Some(parent)},fs.positional[0],fs.get_string("role"),status,null(fs.get_string("workspace")),null(fs.get_string("tab")),null(fs.get_string("pane")),dir,null(&brief),null(&report),host,at,at])?;
        let id = tx.last_insert_rowid();
        documents::capture_brief(tx, id, input.as_ref())?;
        Ok(json!({"ok":true,"task_id":id,"name":fs.positional[0],"status":status}))
    })?;
    if fs.get_int("parent") == 0 && brief.is_empty() {
        let id = &out["task_id"];
        eprintln!(
            "taskr: no goal recorded for root {id}; run `taskr doc set {id} goal --file PATH`"
        );
    }
    if fs.get_int("parent") == 0
        && fs.get_string("role") == "orchestrator"
        && fs.get_string("pane").is_empty()
    {
        let id = &out["task_id"];
        eprintln!("taskr: lead has no pane; run `taskr adopt {id} --pane <id>` as your first act");
    }
    Ok(out)
}
pub fn launch(db: &mut Connection, id: i64, fs: &FlagSet) -> Result<Value> {
    transaction(db, |tx| {
        let t = open_task(tx, id)?;
        if t.parent.is_none() {
            return Err(reject(format!(
                "task {id} is a root task; launch records a lane, so give it a task created with --parent"
            )));
        }
        if let Some(pa) = planned_ancestor(tx, id)? {
            return Err(reject(format!(
                "task {id} is under planned task {pa}; launch task {pa} first"
            )));
        }
        let (ow, ot): (String, String) = tx.query_row(
            "select coalesce(workspace_id,''),coalesce(tab_id,'') from tasks where id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let host = machine(
            tx,
            fs.get_string("machine"),
            fs.was_set("machine"),
            t.machine,
        )?;
        let ws = if fs.get_string("workspace").is_empty() {
            ow.as_str()
        } else {
            fs.get_string("workspace")
        };
        let tab = if fs.get_string("tab").is_empty() {
            ot.as_str()
        } else {
            fs.get_string("tab")
        };
        let pane = if fs.get_string("pane").is_empty() {
            t.pane.as_str()
        } else {
            fs.get_string("pane")
        };
        let agent = if fs.get_string("agent").is_empty() {
            t.name.as_str()
        } else {
            fs.get_string("agent")
        };
        let at = now();
        tx.execute("insert into launches(task_id,provider,model,effort,herdr_scope,workspace_id,tab_id,pane_id,machine,recorded_at) values(?,?,?,?,?,?,?,?,?,?)",params![id,fs.get_string("provider"),fs.get_string("model"),fs.get_string("effort"),null(fs.get_string("herdr-scope")),null(ws),null(tab),null(pane),host,at])?;
        let lid = tx.last_insert_rowid();
        delete_receipts(tx, id)?;
        tx.execute("update tasks set current_launch_id=?,agent_name=?,workspace_id=?,tab_id=?,pane_id=?,machine=?,updated_at=?,status=case status when 'planned' then 'open' else status end where id=?",params![lid,agent,null(ws),null(tab),null(pane),host,at,id])?;
        if ["workspace", "tab", "pane"]
            .iter()
            .any(|k| !fs.get_string(k).is_empty())
        {
            event(
                tx,
                Event {
                    task: id,
                    launch: Some(lid),
                    kind: "launch",
                    summary: &format!(
                        "launched at pane {}",
                        if pane.is_empty() { "none" } else { pane }
                    ),
                    data: Some(
                        json!({"launch_id":lid,"old":location(&ow,&ot,&t.pane),"new":location(ws,tab,pane)}),
                    ),
                    ..Event::default()
                },
            )?;
        }
        let mut out = json!({"ok":true,"task_id":id,"launch_id":lid,"agent_name":agent});
        if t.status == "planned" {
            out["status"] = json!("open");
            out["was_planned"] = json!(true);
        }
        if let Some(old) = t.launch {
            out["replaced_launch_id"] = json!(old);
        }
        for (k, v) in [("pane_id", pane), ("workspace_id", ws), ("tab_id", tab)] {
            if !v.is_empty() {
                out[k] = json!(v);
            }
        }
        Ok(out)
    })
}
pub fn close(db: &mut Connection, id: i64, outcome: &str) -> Result<Value> {
    let report = documents::prepare_report(db, id, "", false);
    let out = transaction(db, |tx| {
        let t = task(tx, id)?;
        let mut out = json!({"ok":true,"task_id":id,"status":"closed"});
        if t.status == "closed" {
            out["already"] = json!(true);
            return Ok(out);
        }
        let at = now();
        tx.execute(
            "update tasks set status='closed',agent_name=null,closed_at=?,updated_at=? where id=?",
            params![at, at, id],
        )?;
        let mut stmt=tx.prepare("with recursive sub(id) as(select id from tasks where id=? union all select t.id from tasks t join sub on t.parent_id=sub.id) select e.id from sub join tasks t on t.id=sub.id join events e on e.task_id=t.id where(t.id=? or (? and t.status!='closed')) and e.kind='ask' and e.answered_by is null and json_extract(e.data,'$.owner')=1 order by e.id")?;
        let orphaned = stmt
            .query_map(params![id, id, t.parent.is_none()], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        delete_receipts(tx, id)?;
        let mut data = json!({"from_status":t.status});
        if !outcome.is_empty() {
            data["outcome"] = json!(outcome);
        }
        let eid = event(
            tx,
            Event {
                task: id,
                kind: "closed",
                data: Some(data),
                ..Event::default()
            },
        )?;
        out["event_id"] = json!(eid);
        documents::capture_report(tx, id, eid, report.as_ref())?;
        if !orphaned.is_empty() {
            out["orphaned_owner_asks"] = json!(orphaned);
        }
        Ok(out)
    })?;
    if let Some(asks) = out["orphaned_owner_asks"].as_array() {
        eprintln!(
            "taskr: closing {id} orphans owner ask(s) {}; asks hides them unless --all",
            asks.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(out)
}
pub fn answer(db: &mut Connection, ask: i64, text: &str, as_id: Option<i64>) -> Result<Value> {
    transaction(db, |tx| {
        if let Some(id) = as_id {
            check_host(tx, id)?;
        }
        let (tid,to,kind,answered,raw)=tx.query_row("select task_id,coalesce(recipient_task_id,task_id),kind,answered_by,coalesce(data,'{}') from events where id=?",[ask],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,String>(4)?))).optional()?.ok_or_else(||reject(format!("event {ask} is not an ask")))?;
        if kind != "ask" {
            return Err(reject(format!("event {ask} is not an ask")));
        }
        if let Some(id) = as_id
            && to != id
        {
            return Err(reject(format!(
                "--as must match ask recipient task {to}, got {id}"
            )));
        }
        if let Some(eid) = answered {
            return Err(reject(format!(
                "ask {ask} is already answered by event {eid}"
            )));
        }
        let t = open_task(tx, tid)?;
        let data: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
        let eid = event(
            tx,
            Event {
                task: tid,
                to: Some(tid),
                kind: "answer",
                summary: text,
                data: Some(json!({"owner":data["owner"]==true})),
                related: Some(ask),
                ..Event::default()
            },
        )?;
        if tx.execute(
            "update events set answered_by=? where id=? and answered_by is null",
            params![eid, ask],
        )? != 1
        {
            return Err(reject(format!("ask {ask} was answered concurrently")));
        }
        Ok(
            json!({"ok":true,"ask_id":ask,"answer_id":eid,"task_id":tid,"delivered":false,"asker_waiting":t.waiting.is_some_and(|w|w>now())}),
        )
    })
}
pub fn set(db: &mut Connection, id: i64, pairs: &[(String, String)]) -> Result<Value> {
    transaction(db, |tx| {
        let t = open_task(tx, id)?;
        if pairs.iter().any(|(k, _)| k == "glance.state") {
            if t.parent.is_some() || t.launch.is_some() {
                return Err(reject("glance.state is for a root orchestrator only"));
            }
            check_host(tx, id)?;
        }
        let mut stmt=tx.prepare("select json_extract(data,'$.key'),json_extract(data,'$.value') from events where id in(select max(id) from events where task_id=? and kind='ref' group by json_extract(data,'$.key')) order by 1")?;
        let mut have = std::collections::BTreeMap::<String, String>::new();
        for row in stmt.query_map([id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (k, v) = row?;
            if !v.is_empty() {
                have.insert(k, v);
            }
        }
        let mut changed = Vec::new();
        for (k, v) in pairs {
            if have.get(k).map_or("", String::as_str) == v
                && !(k == "glance.state" && v == "parked")
            {
                continue;
            }
            changed.push((k, v));
            if v.is_empty() {
                have.remove(k);
            } else {
                have.insert(k.clone(), v.clone());
            }
        }
        if have.len() > 20 {
            return Err(reject(format!(
                "task {id} would have {} references; at most 20 (delete one with KEY=)",
                have.len()
            )));
        }
        let mut ids = Vec::new();
        for (k, v) in changed {
            ids.push(event(
                tx,
                Event {
                    task: id,
                    kind: "ref",
                    summary: &format!("{k}={v}"),
                    data: Some(json!({"key":k,"value":v})),
                    ..Event::default()
                },
            )?);
        }
        Ok(json!({"ok":true,"task_id":id,"event_ids":ids,"refs":have}))
    })
}
