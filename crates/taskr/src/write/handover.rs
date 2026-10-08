use super::*;
pub fn run(f: &FlagSet) -> Result<Value> {
    let id = f.get_int("as");
    if id <= 0 {
        return Err(store::usage("handover needs --as ROOT_TASK_ID"));
    }
    let out = f.get_string("out");
    if out.starts_with('-') {
        return Err(store::usage(format!(
            "--out must be a path that does not start with `-`, got {out:?}"
        )));
    }
    let out = if out.is_empty() {
        String::new()
    } else {
        orch::absolute(
            &std::env::current_dir()
                .map_err(|e| store::usage(e.to_string()))?
                .to_string_lossy(),
            out,
        )
    };
    let note = f.get_string("note").trim();
    let mut db = open()?;
    let (rendered, eid) = store::transaction(&mut db, |tx| {
        worker::resolve(tx, id)?;
        let rendered = store::handover::render(tx, id, note, false)?;
        if !out.is_empty() {
            std::fs::write(&out, &rendered.text).map_err(|e| {
                store::usage(format!(
                    "--out: open {out}: {}",
                    e.to_string()
                        .split(" (os error")
                        .next()
                        .unwrap_or("error")
                        .to_lowercase()
                ))
            })?;
        }
        let mut input = documents::body(rendered.text.as_bytes(), &out);
        if input.reason.is_empty() {
            input.format = "md".into();
        }
        let mut data =
            json!({"sha256":input.hash,"bytes":rendered.text.len(),"counts":rendered.counts});
        if !out.is_empty() {
            data["out"] = json!(out);
        }
        let eid = store::event(
            tx,
            store::Event {
                task: id,
                kind: "handover",
                summary: note,
                data: Some(data),
                ..store::Event::default()
            },
        )?;
        documents::capture(tx, || {
            documents::store(tx, id, "handover", "", &input, Some(eid), 0)?;
            Ok(())
        })?;
        Ok((rendered.text, eid))
    })?;
    print!("{rendered}");
    if out.is_empty() {
        eprintln!("taskr handover: recorded event {eid}");
    } else {
        eprintln!("taskr handover: recorded event {eid}; wrote {out}");
    }
    Ok(json!({}))
}

pub fn adopt(f: &FlagSet) -> Result<Value> {
    orch::validate_location(f)?;
    let id = store::id(&f.positional[0], "task id")?;
    let value = |key: &str, env: &str| {
        let v = f.get_string(key);
        if v.is_empty() {
            store::env(env)
        } else {
            v.into()
        }
    };
    let ws = value("workspace", "HERDR_WORKSPACE_ID");
    let tab = value("tab", "HERDR_TAB_ID");
    let pane = value("pane", "HERDR_PANE_ID");
    for (key, v) in [("workspace", &ws), ("tab", &tab), ("pane", &pane)] {
        if !v.is_empty()
            && (v == "null"
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b":_-".contains(&b)))
        {
            return Err(store::usage(format!(
                "--{key} must be a non-empty, non-whitespace ID matching [0-9A-Za-z:_-]+ and not `null`"
            )));
        }
    }
    if pane.is_empty() {
        return Err(store::usage(
            "adopt needs --pane or HERDR_PANE_ID matching [0-9A-Za-z:_-]+",
        ));
    }
    let mut db = open()?;
    let (text, eid, old) = store::transaction(&mut db, |tx| {
        let t = store::open_task(tx, id)?;
        if let Some(parent) = t.parent {
            return Err(store::reject(format!(
                "adopt is for a root orchestrator: task {id} has parent {parent}; its parent relaunches it"
            )));
        }
        if let Some(launch) = t.launch {
            return Err(store::reject(format!(
                "adopt is for a root orchestrator: task {id} has launch {launch}"
            )));
        }
        let (ow, ot): (String, String) = tx.query_row(
            "select coalesce(workspace_id,''),coalesce(tab_id,'') from tasks where id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        tx.execute("update tasks set workspace_id=?,tab_id=?,pane_id=?,machine=?,waiting_until=null,updated_at=?,lead_status=null,lead_present=null,lead_observed_at=null where id=?",db::params![store::null(&ws),store::null(&tab),pane,store::caller_machine(),store::now(),id])?;
        let eid = store::event(
            tx,
            store::Event {
                task: id,
                kind: "adopt",
                summary: &format!("adopted at pane {pane}"),
                data: Some(
                    json!({"old":store::location(&ow,&ot,&t.pane),"new":store::location(&ws,&tab,&pane)}),
                ),
                ..store::Event::default()
            },
        )?;
        Ok((store::handover::render(tx, id, "", true)?.text, eid, t.pane))
    })?;
    print!("{text}");
    eprintln!(
        "taskr adopt: task {id} now at pane {pane} (was {}); event {eid}",
        if old.is_empty() { "none" } else { &old }
    );
    Ok(json!({}))
}
