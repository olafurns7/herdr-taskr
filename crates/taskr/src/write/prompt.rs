use super::*;
use db::{OptionalExtension, params};
use std::time::{Duration, Instant};
pub(crate) struct Delivery<'a> {
    pub(crate) body: &'a str,
    pub(crate) file: &'a str,
    pub(crate) input: Option<documents::Input>,
    pub(crate) data: Value,
    pub(crate) related: Option<i64>,
    pub(crate) reopen: bool,
    pub(crate) confirm: bool,
    pub(crate) confirm_timeout: i64,
    pub(crate) receipt_timeout: i64,
    pub(crate) receipt_recipient: Option<i64>,
    pub(crate) answer: Option<(&'a str, &'a str)>,
}
fn contract(role: &str) -> String {
    if !["implementer", "researcher", "reviewer"].contains(&role) {
        return String::new();
    }
    format!(
        " Worker: run that got even if a hook did (dup = success); {}honor Progress; taskr ready TEXT --report PATH per slice; missing decision: taskr ask TEXT --blocking, stop dependent work, taskr wait --for answer, taskr ack EVENT_ID --as $TASKR_TASK; end: taskr done [TEXT] or taskr fail TEXT; write report at brief's path, reply with it and three lines; no commit, push, PR, issue-tracker write or agent start unless the brief allows; never sleep waiting: use taskr wait, and herdr pane wait-output only for a non-agent process; exit 6: stop; qd1: queued, don't resend; other failure: read ~/.agents/skills/taskr/references/recovery.md.",
        if role == "reviewer" {
            "reviewer: first load ~/.agents/skills/taskr/references/review.md; "
        } else {
            ""
        }
    )
}
pub fn answer(f: &FlagSet) -> Result<Value> {
    let ask = store::id(&f.positional[0], "ask id")?;
    if f.get_bool("confirm") && !f.get_bool("prompt") {
        return Err(store::usage("--confirm needs --prompt"));
    }
    if f.get_int("confirm-timeout") < 0 {
        return Err(store::usage("--confirm-timeout must not be negative"));
    }
    let mut db = open()?;
    let mut ans = orch::answer(
        &mut db,
        ask,
        &f.positional[1],
        if f.was_set("as") {
            Some(f.get_int("as"))
        } else {
            None
        },
    )?;
    if !f.get_bool("prompt") {
        return Ok(ans);
    }
    let asker = ans["task_id"].as_i64().expect("asker");
    let aid = ans["answer_id"].as_i64().expect("answer");
    let body = format!(
        "taskr answer to ask {}: {}",
        f.positional[0], f.positional[1]
    );
    let d = Delivery {
        body: &body,
        file: "",
        input: None,
        data: json!({"answer_id":aid}),
        related: Some(aid),
        reopen: false,
        confirm: f.get_bool("confirm"),
        confirm_timeout: f.get_int("confirm-timeout"),
        receipt_timeout: 300000,
        receipt_recipient: if f.was_set("as") {
            Some(f.get_int("as"))
        } else {
            None
        },
        answer: Some((&f.positional[0], &f.positional[1])),
    };
    match deliver(&mut db, asker, d, f.json()) {
        Ok(result) => {
            for (k, v) in result.as_object().expect("object") {
                ans[k] = v.clone();
            }
            ans["ok"] = json!(ans.get("error").is_none());
            ans["delivered"] =
                json!(ans["outcome"] == "activity_observed" || ans["outcome"] == "no_receipt");
            Ok(ans)
        }
        Err(e) => {
            ans["error"] = json!(e.message);
            ans["kind"] = json!(match e.code {
                ExitCode::Usage => "usage",
                ExitCode::Rejected => "rejected",
                ExitCode::Transport => "herdr",
                _ => "database",
            });
            ans["_exit"] = json!(e.code as u8);
            Ok(ans)
        }
    }
}
pub fn prompt(f: &FlagSet) -> Result<Value> {
    let id = store::id(&f.positional[0], "task id")?;
    let text = f.get_string("text");
    let file = f.get_string("file");
    if file.is_empty() == text.is_empty() {
        return Err(store::usage(
            "prompt needs exactly one of --file and --text",
        ));
    }
    if f.get_int("confirm-timeout") < 0 {
        return Err(store::usage("--confirm-timeout must not be negative"));
    }
    let receipt = f.get_int("receipt-timeout");
    let confirm = f.get_bool("confirm");
    if f.was_set("receipt-timeout") {
        if confirm {
            return Err(store::usage(
                "--receipt-timeout does not apply with --confirm",
            ));
        }
        if receipt < 0 {
            return Err(store::usage("--receipt-timeout must not be negative"));
        }
        if receipt > 0 && receipt < 60000 {
            return Err(store::usage(
                "--receipt-timeout must be 0 (no deadline) or at least 60000 ms",
            ));
        }
    }
    let abs = if file.is_empty() {
        String::new()
    } else {
        orch::absolute(
            &std::env::current_dir()
                .map_err(|e| store::usage(e.to_string()))?
                .to_string_lossy(),
            file,
        )
    };
    let input = if file.is_empty() {
        documents::body(text.as_bytes(), "")
    } else {
        let bytes = std::fs::read(file).map_err(|e| {
            store::usage(format!(
                "read prompt file: open {file}: {}",
                e.to_string()
                    .split(" (os error")
                    .next()
                    .unwrap_or("error")
                    .to_lowercase()
            ))
        })?;
        documents::body(&bytes, &abs)
    };
    let data = if file.is_empty() {
        json!({})
    } else {
        json!({"file":abs,"sha256":input.hash,"bytes":input.bytes})
    };
    let d = Delivery {
        body: text,
        file: &abs,
        input: Some(input),
        data,
        related: None,
        reopen: true,
        confirm,
        confirm_timeout: f.get_int("confirm-timeout"),
        receipt_timeout: receipt,
        receipt_recipient: None,
        answer: None,
    };
    deliver(&mut open()?, id, d, f.json())
}
fn deliver(db: &mut db::Connection, id: i64, d: Delivery<'_>, json_mode: bool) -> Result<Value> {
    let sock = herdr::socket();
    let confirm = d.confirm;
    let timeout = d.confirm_timeout;
    let attempt = begin(db, id, d, false, || {
        if herdr::up(&sock) {
            Ok(())
        } else {
            Err(Error {
                code: ExitCode::Transport,
                message: format!("Herdr server not reachable at {sock}; not delivering"),
            })
        }
    })?
    .expect("local attempt");
    if let Some(launch) = attempt.launch {
        inbox::observe::bind(db, id, launch);
    }
    let (outcome, detail) = herdr::run_prompt(&sock, &attempt.target, &attempt.text, json_mode);
    finish(db, attempt, outcome, detail, confirm, timeout)
}

pub(crate) struct Attempt {
    pub(crate) id: i64,
    pub(crate) task_id: i64,
    pub(crate) launch: Option<i64>,
    pub(crate) target: String,
    pub(crate) agent: String,
    pub(crate) text: String,
}
pub(crate) fn begin(
    db: &mut db::Connection,
    id: i64,
    mut d: Delivery<'_>,
    here: bool,
    ready: impl FnOnce() -> Result<()>,
) -> Result<Option<Attempt>> {
    if let Some(input) = d.input.as_mut()
        && !input.path.is_empty()
        && let Some(caller) = store::caller_machine()
    {
        input.host = caller;
        input.reason = "client".into();
        input.body.clear();
        input.format.clear();
    }
    if d.data.get("sha256").is_none() {
        let body = documents::body(d.body.as_bytes(), "");
        d.data["sha256"] = json!(body.hash);
        d.data["bytes"] = json!(d.body.len());
    }
    let attempt = store::transaction(db, |tx| {
        let t = store::open_task(tx, id)?;
        if t.status == "planned" {
            return Err(store::reject(format!(
                "task {id} is planned; run `taskr launch {id}` before prompting it"
            )));
        }
        if let Some(pa) = store::planned_ancestor(tx, id)? {
            return Err(store::reject(format!(
                "task {id} is under planned task {pa}; launch task {pa} first"
            )));
        }
        let (lpane, host) = if let Some(lid) = t.launch {
            tx.query_row(
                "select coalesce(pane_id,''),machine from launches where id=?",
                [lid],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )?
        } else {
            (String::new(), t.machine.clone())
        };
        if here && host.is_none() {
            return Ok(None);
        }
        if let Some(host) = host.as_ref()
            && (!here || store::caller_machine().as_deref() != Some(host.as_str()))
        {
            let message = if !here && store::caller_machine().as_deref() == Some(host.as_str()) {
                format!(
                    "lane {} is on {host}, this caller's host; send it with `taskr prompt {id}` there",
                    t.name
                )
            } else {
                format!(
                    "lane {} is on {host}; prompt it from that host (start a sub-orchestrator there)",
                    t.name
                )
            };
            return Err(Error {
                code: ExitCode::Transport,
                message,
            });
        }
        let agent: String = tx.query_row(
            "select coalesce(agent_name,'') from tasks where id=?",
            [id],
            |r| r.get(0),
        )?;
        let target = if !lpane.is_empty() {
            lpane
        } else if !t.pane.is_empty() {
            t.pane
        } else {
            agent.clone()
        };
        if target.is_empty() {
            return Err(store::usage(format!(
                "task {id} has no pane or agent name to prompt"
            )));
        }
        ready()?;
        d.data["target"] = json!(target);
        let eid = store::event(
            tx,
            store::Event {
                task: id,
                launch: t.launch,
                kind: "prompt",
                data: Some(d.data.clone()),
                related: d.related,
                ..store::Event::default()
            },
        )?;
        if d.reopen {
            tx.execute(
                "update tasks set status='open',updated_at=? where id=?",
                params![store::now(), id],
            )?;
        }
        if !d.confirm
            && store::millis(d.receipt_timeout) > store::millis(0)
            && t.parent.is_some()
            && let Some(to) = d.receipt_recipient.or(t.parent)
        {
            let created: String =
                tx.query_row("select created_at from events where id=?", [eid], |r| {
                    r.get(0)
                })?;
            let due = store::stamp(
                store::parse_time(&created).expect("event timestamp")
                    + store::millis(d.receipt_timeout),
            );
            // receiptDue is a Go struct: preserve its field order in the stored value.
            let value = format!(
                "{{\"due_at\":{},\"recipient\":{to},\"task\":{id},\"launch\":{}}}",
                compact_json(&json!(due))?,
                t.launch.map_or("null".into(), |n| n.to_string())
            );
            tx.execute("insert into meta(key,value) values(?,?) on conflict(key) do update set value=excluded.value",params![format!("receipt_due:{eid}"),value])?;
        }
        if let Some(input) = &d.input {
            documents::capture(tx, || {
                let (kind, name) = if input.path.is_empty() {
                    ("prompt", String::new())
                } else {
                    let brief: String = tx.query_row(
                        "select coalesce(brief_path,'') from tasks where id=?",
                        [id],
                        |r| r.get(0),
                    )?;
                    if brief == input.path {
                        ("brief", String::new())
                    } else {
                        (
                            "prompt",
                            std::path::Path::new(&input.path)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned(),
                        )
                    }
                };
                documents::store(tx, id, kind, &name, input, Some(eid), 0)?;
                Ok(())
            })?;
        }
        Ok(Some((eid, t.launch, target, t.role, agent)))
    })?;
    let Some((attempt, launch, target, role, agent)) = attempt else {
        return Ok(None);
    };
    let text = if let Some((ask, text)) = d.answer {
        format!("First taskr got {attempt}. ask {ask}: {text}")
    } else if d.file.is_empty() {
        format!("First taskr got {attempt}. {}", d.body)
    } else {
        format!(
            "First taskr got {attempt}; read {}; execute exactly.{}",
            d.file,
            contract(&role)
        )
    };
    Ok(Some(Attempt {
        id: attempt,
        task_id: id,
        launch,
        target,
        agent,
        text,
    }))
}

pub(crate) fn finish(
    db: &mut db::Connection,
    a: Attempt,
    outcome: &str,
    detail: Value,
    confirm: bool,
    timeout: i64,
) -> Result<Value> {
    let Attempt {
        id: attempt,
        task_id: id,
        launch,
        target,
        ..
    } = a;
    let mut out = json!({"task_id":id,"attempt_id":attempt,"target":target,"outcome":outcome});
    let mut data = json!({"outcome":outcome});
    for (k, v) in detail.as_object().expect("object") {
        out[k] = v.clone();
        data[k] = v.clone();
    }
    store::transaction(db, |tx| {
        let eid = store::event(
            tx,
            store::Event {
                task: id,
                launch,
                kind: "prompt_outcome",
                summary: outcome,
                data: Some(data),
                related: Some(attempt),
                ..store::Event::default()
            },
        )?;
        out["outcome_event_id"] = json!(eid);
        if outcome == "rejected" || confirm {
            tx.execute(
                "delete from meta where key=?",
                [format!("receipt_due:{attempt}")],
            )?;
        } else if outcome == "activity_observed" {
            let raw = tx
                .query_row(
                    "select value from meta where key=?",
                    [format!("receipt_due:{attempt}")],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            if let Some(raw) = raw
                && let Ok(v) = serde_json::from_str::<Value>(&raw)
            {
                let created: String =
                    tx.query_row("select created_at from events where id=?", [attempt], |r| {
                        r.get(0)
                    })?;
                if let Some(ms) = store::parse_time(v["due_at"].as_str().unwrap_or(""))
                    .zip(store::parse_time(&created))
                    .map(|(a, b)| (a - b).whole_milliseconds() as i64)
                {
                    out["receipt_due_ms"] = json!(ms);
                }
            }
        }
        Ok(())
    })?;
    if outcome != "activity_observed" {
        out["ok"] = json!(false);
        out["error"] = json!(format!(
            "prompt attempt {attempt}: {outcome}; inspect the agent before any resend"
        ));
        out["kind"] = json!("herdr");
        out["_exit"] = json!(5);
        return Ok(out);
    }
    if !confirm {
        out["ok"] = json!(true);
        return Ok(out);
    }
    let frozen = taskr_core::frozen_now().expect("clock");
    let budget = Duration::from_millis(
        (frozen + store::millis(timeout) - store::real_now())
            .whole_milliseconds()
            .max(0) as u64,
    );
    let start = Instant::now();
    loop {
        let got=db.query_row("select id,coalesce(data,'{}') from events where kind='got' and related_event_id=? order by id limit 1",[attempt],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?))).optional()?;
        if let Some((eid, raw)) = got {
            let v: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
            out["ok"] = json!(true);
            out["receipt"] = json!(true);
            out["receipt_event_id"] = json!(eid);
            out["round"] = json!(v["round"].as_i64().unwrap_or(0));
            return Ok(out);
        }
        let left = budget.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
    out["ok"] = json!(false);
    out["receipt"] = json!(false);
    out["outcome"] = json!("no_receipt");
    out["delivery_outcome_event_id"] = out["outcome_event_id"].clone();
    let eid = store::transaction(db, |tx| {
        store::event(
            tx,
            store::Event {
                task: id,
                launch,
                kind: "prompt_outcome",
                summary: "no_receipt",
                data: Some(
                    json!({"outcome":"no_receipt","confirm_timeout_ms":store::duration_millis(timeout)}),
                ),
                related: Some(attempt),
                ..store::Event::default()
            },
        )
    })?;
    out["outcome_event_id"] = json!(eid);
    out["error"] = json!(format!(
        "prompt attempt {attempt}: activity observed but no taskr got receipt within {} ms; inspect the agent before any resend",
        store::duration_millis(timeout)
    ));
    out["kind"] = json!("no_receipt");
    out["_exit"] = json!(5);
    Ok(out)
}
