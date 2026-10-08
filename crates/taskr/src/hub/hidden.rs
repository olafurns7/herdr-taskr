use crate::write::{Attempt, Delivery};
use serde_json::{Value, json};
use taskr_core::{
    ExitCode,
    db::OptionalExtension,
    goflag::FlagSet,
    store::{self, documents},
};

pub(super) fn hook(args: &[String], req: &super::protocol::RpcRequest) -> store::Result<()> {
    if args.len() < 3 {
        return Ok(());
    }
    let event = &args[0];
    if ![
        "SessionStart",
        "UserPromptSubmit",
        "Stop",
        "StopFailure",
        "session.created",
        "chat.message",
        "session.idle",
        "session.error",
        "session_start",
        "input",
        "agent_settled",
    ]
    .contains(&event.as_str())
    {
        return Ok(());
    }
    let mut record = store::hook::Record {
        event: event.clone(),
        ..Default::default()
    };
    for pair in args[1..].as_chunks::<2>().0 {
        match pair[0].as_str() {
            "--session" => record.session = pair[1].clone(),
            "--transcript" => record.transcript = pair[1].clone(),
            "--attempt" => record.attempt = pair[1].parse().unwrap_or(0),
            "--error" => record.error = store::hook::error_code(&json!(pair[1])),
            _ => return Ok(()),
        }
    }
    if record.session.is_empty()
        || !record.transcript.is_empty() && !std::path::Path::new(&record.transcript).is_absolute()
        || ["UserPromptSubmit", "chat.message", "input"].contains(&event.as_str())
            && record.attempt <= 0
    {
        return Ok(());
    }
    if [
        "Stop",
        "StopFailure",
        "session.idle",
        "session.error",
        "agent_settled",
    ]
    .contains(&event.as_str())
        && req.queued_age_ms > 600000
    {
        println!("expired");
        return Ok(());
    }
    if let Ok(mut db) = super::child::open() {
        let _ = db.busy_timeout(std::time::Duration::from_millis(500));
        let _ = store::hook::apply(&mut db, &record);
    }
    Ok(())
}

pub(super) fn prompt(args: &[String], json: bool) -> store::Result<Value> {
    let mut f = FlagSet::new("_prompt", json);
    f.string("text", "", "literal prompt text")
        .string(
            "file",
            "",
            "the prompt file's absolute path on the caller's host",
        )
        .string("sha256", "", "the prompt file's sha256")
        .int("bytes", -1, "the prompt file's size")
        .bool("local-herdr", false, "the caller's Herdr server accepts")
        .string(
            "outcome",
            "",
            "activity_observed, rejected or delivery_unknown",
        )
        .string("detail", "{}", "herdr's detail, as a JSON object")
        .bool("confirm", false, "wait for the worker's taskr got receipt")
        .int(
            "confirm-timeout",
            60000,
            "milliseconds to wait for the receipt",
        )
        .int(
            "receipt-timeout",
            300000,
            "the receipt window to arm; 0 disarms",
        );
    f.parse(args, 2, 2).map_err(store::usage)?;
    let receipt = f.get_int("receipt-timeout");
    if f.was_set("receipt-timeout") {
        if receipt < 0 {
            return Err(store::usage("--receipt-timeout must not be negative"));
        }
        if receipt > 0 && receipt < 60000 {
            return Err(store::usage(
                "--receipt-timeout must be 0 (no deadline) or at least 60000 ms",
            ));
        }
    }
    let id = store::id(&f.positional[1], "id")?;
    if f.get_int("confirm-timeout") < 0 {
        return Err(store::usage("--confirm-timeout must not be negative"));
    }
    let mut db = super::child::open()?;
    match f.positional[0].as_str() {
        "begin" => {
            let text = f.get_string("text");
            let file = f.get_string("file");
            if file.is_empty() == text.is_empty() {
                return Err(store::usage(
                    "prompt needs exactly one of --file and --text",
                ));
            }
            let input = if file.is_empty() {
                documents::body(text.as_bytes(), "")
            } else {
                documents::Input {
                    path: file.into(),
                    host: store::caller_machine().unwrap_or_default(),
                    hash: f.get_string("sha256").into(),
                    bytes: Some(f.get_int("bytes")),
                    reason: "client".into(),
                    ..Default::default()
                }
            };
            let data = if file.is_empty() {
                json!({})
            } else {
                json!({"file":file,"sha256":f.get_string("sha256"),"bytes":f.get_int("bytes")})
            };
            let delivery = Delivery {
                body: text,
                file,
                input: Some(input),
                data,
                related: None,
                reopen: true,
                confirm: false,
                confirm_timeout: 0,
                receipt_timeout: receipt,
                receipt_recipient: None,
                answer: None,
            };
            let attempt = crate::write::begin(&mut db, id, delivery, true, || {
                if !f.get_bool("local-herdr") {
                    return Err(store::Error {
                        code: ExitCode::Transport,
                        message: format!(
                            "Herdr server not reachable on {}; not delivering",
                            store::caller_machine().unwrap_or_default()
                        ),
                    });
                }
                if !file.is_empty() && (f.get_string("sha256").is_empty() || f.get_int("bytes") < 0)
                {
                    return Err(store::usage(format!(
                        "read prompt file: {file} is not readable on {}",
                        store::caller_machine().unwrap_or_default()
                    )));
                }
                Ok(())
            })?;
            match attempt {
                None => Ok(json!({"ok":true,"route":"server"})),
                Some(a) => Ok(
                    json!({"ok":true,"route":"here","task_id":id,"attempt_id":a.id,"target":a.target,"agent":a.agent,"text":a.text}),
                ),
            }
        }
        "outcome" => {
            let outcome = f.get_string("outcome");
            if !["activity_observed", "rejected", "delivery_unknown"].contains(&outcome) {
                return Err(store::usage(format!(
                    "--outcome must be activity_observed, rejected or delivery_unknown, got {}",
                    taskr_core::goflag::quote(outcome)
                )));
            }
            let raw: Value = serde_json::from_str(f.get_string("detail"))
                .map_err(|_| store::usage("--detail must be a JSON object"))?;
            if !raw.is_object() && !raw.is_null() {
                return Err(store::usage("--detail must be a JSON object"));
            }
            let mut detail = json!({});
            for k in ["agent_status", "herdr_error", "herdr_exit", "herdr_message"] {
                if let Some(v) = raw.get(k) {
                    detail[k] = taskr_core::event_data(v.clone());
                }
            }
            let a = store::transaction(&mut db, |tx| {
                let row=tx.query_row("select e.task_id,e.launch_id,l.machine,coalesce(e.data,'{}') from events e left join launches l on l.id=e.launch_id where e.id=? and e.kind='prompt'",[id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,Option<i64>>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,String>(3)?))).optional()?.ok_or_else(||store::reject(format!("event {id} is not a prompt attempt")))?;
                if row.1.is_none() || row.2 != store::caller_machine() {
                    return Err(store::reject(format!(
                        "prompt attempt {id} is not for a lane on {}",
                        store::caller_machine().unwrap_or_default()
                    )));
                }
                let done:bool=tx.query_row("select exists(select 1 from events where kind='prompt_outcome' and related_event_id=?)",[id],|r|r.get(0))?;
                if done {
                    return Err(store::reject(format!(
                        "prompt attempt {id} already has an outcome"
                    )));
                }
                let data: Value = serde_json::from_str(&row.3).unwrap_or(json!({}));
                Ok(Attempt {
                    id,
                    task_id: row.0,
                    launch: row.1,
                    target: data["target"].as_str().unwrap_or("").into(),
                    agent: String::new(),
                    text: String::new(),
                })
            })?;
            crate::write::finish(
                &mut db,
                a,
                outcome,
                detail,
                f.get_bool("confirm"),
                f.get_int("confirm-timeout"),
            )
        }
        _ => Err(store::usage(format!(
            "_prompt: unknown call {}",
            taskr_core::goflag::quote(&f.positional[0])
        ))),
    }
}

pub(super) fn host(args: &[String], json: bool) -> store::Result<Value> {
    let mut f = FlagSet::new("_host", json);
    f.string("agents", "", "the host's herdr agent list, as a JSON array");
    f.parse(args, 1, 1).map_err(store::usage)?;
    if f.positional[0] != "observe" {
        return Err(store::usage(format!(
            "_host: unknown call {}",
            taskr_core::goflag::quote(&f.positional[0])
        )));
    }
    let agents: Value = serde_json::from_str(f.get_string("agents"))
        .map_err(|_| store::usage("_host observe: --agents must be a JSON array"))?;
    crate::daemon::observe_host(
        &mut super::child::open()?,
        &store::caller_machine().unwrap_or_default(),
        &agents,
    )
}
