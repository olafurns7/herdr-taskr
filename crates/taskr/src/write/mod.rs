//! Local write command routing and Go-compatible output.
use serde_json::{Value, json};
use taskr_core::{
    ExitCode, compact_json, db,
    goflag::FlagSet,
    store::{self, Error, Result, documents, orch, worker},
};

mod handover;
#[path = "../herdr.rs"]
pub(crate) mod herdr;
mod hook;
mod inbox;
pub(crate) use inbox::observe::{daemon_observe, host_snapshot};
mod prompt;
mod question;
pub(crate) use prompt::{Attempt, Delivery, begin, finish};

pub fn flags(cmd: &str, json: bool) -> FlagSet {
    let mut f = FlagSet::new(cmd, json);
    match cmd {
        "next" => {
            f.bool("clear", false, "clear the lane's next step");
        }
        "decide" => {
            f.int("as", 0, "the root orchestrator's own task id").int(
                "revoke",
                0,
                "retire this decision (or answered owner ask) event",
            );
        }
        "adopt" => {
            f.string(
                "workspace",
                "",
                "Herdr workspace id (default: HERDR_WORKSPACE_ID)",
            )
            .string("tab", "", "Herdr tab id (default: HERDR_TAB_ID)")
            .string("pane", "", "Herdr pane id (default: HERDR_PANE_ID)");
        }
        "doc rm" => {
            f.bool("purge",false,"delete all versions and unshared blobs; the write-ahead log and earlier backups can still hold the text");
        }
        "doc backfill" => {
            f.int("tree", 0, "only this task and descendants").bool(
                "dry-run",
                false,
                "count without writing",
            );
        }
        "new" => {
            f.int("parent", 0, "parent task id")
                .string(
                    "role",
                    "",
                    "orchestrator | sub-orchestrator | implementer | reviewer | researcher | gate",
                )
                .string("workspace", "", "Herdr workspace id")
                .string("tab", "", "Herdr tab id")
                .string("pane", "", "Herdr pane id")
                .string("cwd", "", "task working directory (default: current)")
                .string("brief", "", "brief path")
                .string("report", "", "report path")
                .bool(
                    "planned",
                    false,
                    "a planned lane: no launch until `taskr launch`",
                )
                .string("machine", "", "the task's host (default: the caller's)");
        }
        "launch" => {
            f.string("provider", "", "claude | codex | ...")
                .string("model", "", "model id")
                .string("effort", "", "effort level")
                .string(
                    "workspace",
                    "",
                    "move the task to this Herdr workspace (default: keep)",
                )
                .string("tab", "", "move the task to this Herdr tab (default: keep)")
                .string(
                    "pane",
                    "",
                    "move the task to this pane (default: the task's pane)",
                )
                .string("agent", "", "Herdr agent name (default: the task name)")
                .string(
                    "herdr-scope",
                    "",
                    "Herdr server socket and session the pane id belongs to",
                )
                .string("machine", "", "the launch's host (default: the task's)");
        }
        "note" => {
            f.string("key", "", "idempotency key")
                .bool("owner", false, "for the owner; root orchestrators only")
                .int("as", 0, "a root orchestrator's own task id");
        }
        "ready" => {
            f.string("report", "", "report path")
                .string("key", "", "idempotency key")
                .strings("kv", "KEY=VALUE, repeatable");
        }
        "ask" => {
            f.bool(
                "blocking",
                false,
                "the worker cannot continue without an answer",
            )
            .bool(
                "owner",
                false,
                "a question for the owner; routed to the root",
            )
            .string(
                "question",
                "",
                "one AskUserQuestion-shaped `JSON` object (with --owner); TEXT becomes optional context",
            )
            .bool("dialog", false, "an owner ask relayed from the hub's question dialog")
            .string("key", "", "idempotency key")
            .int("as", 0, "a root orchestrator's own task id");
        }
        "done" | "fail" => {
            f.string("key", "", "idempotency key");
        }
        "close" => {
            f.string("outcome","","accepted (work taken as delivered) | reworked (taken after a fix round) | rejected (not taken) | abandoned (stopped before a result)");
        }
        "answer" => {
            f.int("as", 0, "the task that received the ask")
                .bool("prompt", false, "also prompt the asker with the answer")
                .bool(
                    "withdraw",
                    false,
                    "withdraw a stale ask (not an owner answer); --as the asker's root or a hub root",
                )
                .bool(
                    "confirm",
                    false,
                    "with --prompt: wait for the asker's taskr got receipt",
                )
                .int(
                    "confirm-timeout",
                    60000,
                    "milliseconds to wait for the receipt",
                );
        }
        "ack" => {
            f.int("as", 0, "task id whose inbox event to ack");
        }
        "wait" => {
            f.int(
                "as",
                0,
                "task id whose inbox to read (an orchestrator, or a worker waiting on its own ask)",
            )
            .int("timeout", 540000, "milliseconds")
            .bool(
                "scan-quota",
                false,
                "also scan children's visible panes for quota lines",
            )
            .int("ack", 0, "ack this pending handled event, then wait")
            .strings(
                "for",
                "only return these event kinds (comma-separated; repeatable)",
            )
            .strings(
                "from",
                "only return events from this task name or id (repeatable)",
            );
        }
        "doc set" => {
            f.string("file", "", "document file, read on any host")
                .string("name", "", "plan name, [a-z0-9][a-z0-9._-]{0,63}");
        }
        "prompt" => {
            f.string("file","","prompt file; the agent is told to read it").string("text","","literal prompt text").bool("confirm",false,"after observed activity, wait for the worker's taskr got receipt").int("confirm-timeout",60000,"milliseconds to wait for the receipt").int("receipt-timeout",300000,"without --confirm: milliseconds until a missing receipt alarms the parent; 0 disarms");
        }
        "handover" => {
            f.int("as", 0, "the root orchestrator's own task id")
                .string("note", "", "a note for the successor")
                .string("out", "", "also write the Markdown to this file");
        }
        _ => {}
    }
    f
}
fn usage_line(cmd: &str) -> String {
    crate::cli::usage_line(cmd)
}

pub(crate) fn error(json: bool, cmd: &str, e: Error, mut out: Value) -> ExitCode {
    let kind = match e.code {
        ExitCode::Usage => "usage",
        ExitCode::Rejected => "rejected",
        ExitCode::Transport => "herdr",
        ExitCode::NotImplemented => "not_implemented",
        _ => "database",
    };
    if !out.is_object() {
        out = json!({});
    }
    out["error"] = json!(e.message);
    if out["kind"].is_null() {
        out["kind"] = json!(kind);
    }
    if json {
        eprintln!("taskr {cmd}: {}", e.message);
        println!("{}", compact_json(&out).expect("JSON"));
    } else {
        let mut m = serde_json::Map::new();
        for (k, v) in out.as_object().expect("object") {
            let key = match k.as_str() {
                "error" => "err",
                "kind" => "k",
                "attempt_id" => "p",
                "outcome" => "o",
                "outcome_event_id" => "oe",
                "receipt_event_id" => "g",
                "receipt" => "gok",
                "round" => "r",
                "answer_id" => "a",
                "delivered" => "sent",
                "asker_waiting" => "w",
                "receipt_due_ms" => "due",
                "event_id" => "e",
                "decision_id" => "dc",
                other => other,
            };
            m.insert(key.into(), v.clone());
        }
        println!(
            "x1 {} {}",
            e.code as u8,
            compact_json(&Value::Object(m)).expect("JSON")
        );
    }
    e.code
}
pub fn emit(cmd: &str, json_mode: bool, v: &Value) {
    if json_mode {
        println!("{}", compact_json(v).expect("JSON"));
        return;
    }
    let suffix = if v["duplicate"] == true { " dup" } else { "" };
    match cmd {
        "new" => println!(
            "n1 {}{}",
            v["task_id"],
            if v["status"] == "planned" {
                " planned"
            } else {
                ""
            }
        ),
        "got" => println!("g1 {} {}{suffix}", v["event_id"], v["round"]),
        "start" | "note" | "ready" | "ask" | "done" | "fail" => println!(
            "{}1 {}{suffix}",
            match cmd {
                "start" => "s",
                "note" => "n",
                "ready" => "r",
                "ask" => "q",
                "done" => "d",
                _ => "f",
            },
            v["event_id"]
        ),
        "close" => println!(
            "c1 {}{}",
            v["task_id"],
            if v["already"] == true { " already" } else { "" }
        ),
        "ack" => println!(
            "k1 {}{}",
            v["acked_event_id"],
            if v["already"] == true { " already" } else { "" }
        ),
        "launch" => {
            let mut m = json!({});
            for k in ["launch_id", "replaced_launch_id", "status", "was_planned"] {
                if v.get(k).is_some() {
                    m[k] = v[k].clone();
                }
            }
            println!("l1 {}", compact_json(&m).expect("JSON"));
        }
        "doc set" => println!(
            "ds1 {} {}{}",
            v["doc_id"],
            v["version"],
            if v["same"] == true { " same" } else { "" }
        ),
        "doc rm" => println!("dr1 {} {}", v["doc_id"], v["removed"]),
        "doc backfill" => println!("j1 {}", compact_json(v).expect("JSON")),
        "next" => println!(
            "nx1 {}{}",
            v["event_id"],
            if v["next"].is_null() { " clear" } else { "" }
        ),
        "decide" => {
            let mut out = json!({"e":v["event_id"],"k":v["kind"]});
            if v.get("decision_id").is_some() {
                out["dc"] = v["decision_id"].clone();
            }
            if v.get("revoked").is_some() {
                out["revoked"] = v["revoked"].clone();
            }
            println!("dc1 {}", compact_json(&out).expect("JSON"));
        }
        "set" => println!("rf1 {}", compact_json(&v["event_ids"]).expect("JSON")),
        "prompt" | "answer" => {
            let mut m = json!({});
            for (k, v) in v.as_object().expect("object") {
                if ["task_id", "ask_id", "target", "ok", "agent_status"].contains(&k.as_str()) {
                    continue;
                }
                let key = match k.as_str() {
                    "attempt_id" => "p",
                    "outcome" => "o",
                    "outcome_event_id" => "oe",
                    "receipt_event_id" => "g",
                    "receipt" => "gok",
                    "round" => "r",
                    "answer_id" => "a",
                    "delivered" => "sent",
                    "asker_waiting" => "w",
                    "receipt_due_ms" => "due",
                    other => other,
                };
                m[key] = v.clone();
            }
            println!(
                "{}1 {}",
                if cmd == "prompt" { "p" } else { "a" },
                compact_json(&m).expect("JSON")
            );
        }
        "wait" => inbox::emit_wait(v),
        _ => {}
    }
}
pub fn dispatch(json_mode: bool, args: &[String]) -> Option<ExitCode> {
    let raw = args.first()?.as_str();
    if raw == "hook" {
        return Some(hook::run(&args[1..]));
    }
    let (cmd, tail) = if raw == "doc"
        && args
            .get(1)
            .is_some_and(|s| ["set", "rm", "backfill"].contains(&s.as_str()))
    {
        (
            match args[1].as_str() {
                "set" => "doc set",
                "rm" => "doc rm",
                _ => "doc backfill",
            },
            &args[2..],
        )
    } else {
        (raw, &args[1..])
    };
    if ![
        "new",
        "launch",
        "start",
        "got",
        "note",
        "ask",
        "answer",
        "ready",
        "done",
        "fail",
        "close",
        "set",
        "ack",
        "wait",
        "doc set",
        "doc rm",
        "doc backfill",
        "prompt",
        "handover",
        "adopt",
        "next",
        "decide",
    ]
    .contains(&cmd)
    {
        return None;
    }
    let mut f = flags(cmd, json_mode);
    let (min, max) = match cmd {
        "start" | "wait" | "handover" | "doc backfill" => (0, 0),
        "decide" => (0, 1),
        "next" => (1, 2),
        "done" => (0, 1),
        "answer" | "doc set" => (2, 2),
        "set" => (2, 41),
        // ask's TEXT is optional with --question: checked below, once flags are known.
        "ask" => (0, 1),
        _ => (1, 1),
    };
    let mut parsed = f.parse(tail, min, max);
    if parsed.is_ok()
        && cmd == "ask"
        && !f.help()
        && !f.was_set("question")
        && f.positional.len() != 1
    {
        parsed = Err(format!(
            "ask: expected 1 to 1 positional arguments, got {}",
            f.positional.len()
        ));
    }
    if let Err(e) = parsed {
        return Some(error(f.json(), raw, store::usage(e), json!({})));
    }
    if f.help() {
        print!("{}", f.usage(&usage_line(cmd)));
        return Some(ExitCode::Ok);
    }
    let result = run(cmd, &f);
    Some(match result {
        Ok(v) => {
            if v.get("error").is_some() {
                let mut v = v;
                let code = match v["_exit"].as_u64().unwrap_or(4) {
                    2 => ExitCode::Usage,
                    3 => ExitCode::Timeout,
                    5 => ExitCode::Transport,
                    6 => ExitCode::Rejected,
                    _ => ExitCode::Database,
                };
                v.as_object_mut().expect("object").remove("_exit");
                let message = v["error"].as_str().unwrap_or("").to_string();
                return Some(error(f.json(), raw, Error { code, message }, v));
            }
            if cmd == "handover" || cmd == "adopt" {
                return Some(ExitCode::Ok);
            }
            emit(cmd, f.json(), &v);
            if v["timeout"] == true && (f.json() || v["interrupted"] == true) {
                ExitCode::Timeout
            } else {
                ExitCode::Ok
            }
        }
        Err(e) => error(f.json(), raw, e, json!({})),
    })
}
fn open() -> Result<db::Connection> {
    let path = db::path().map_err(store::usage)?;
    let db = db::open(&path).map_err(|message| Error {
        code: ExitCode::Database,
        message,
    })?;
    if store::rpc_context().is_none() {
        db.busy_timeout(std::time::Duration::from_secs(30))?;
    }
    Ok(db)
}
fn run(cmd: &str, f: &FlagSet) -> Result<Value> {
    let p = &f.positional;
    match cmd {
        "next" => {
            let id = store::id(&p[0], "task id")?;
            let text = p.get(1).map_or("", String::as_str).trim();
            let clear = f.get_bool("clear");
            if clear && p.len() == 2 {
                return Err(store::usage("next: give TEXT or --clear, not both"));
            }
            if !clear && text.is_empty() {
                return Err(store::usage("next needs TEXT or --clear"));
            }
            store::plan::next(&mut open()?, id, text, clear)
        }
        "decide" => {
            let id = f.get_int("as");
            if id <= 0 {
                return Err(store::usage("decide needs --as ROOT_TASK_ID"));
            }
            let text = p.first().map_or("", String::as_str).trim();
            let revoke = f.get_int("revoke");
            if (revoke != 0) == (!text.is_empty()) {
                return Err(store::usage(
                    "decide needs exactly one of TEXT and --revoke EVENT_ID",
                ));
            }
            if revoke < 0 {
                return Err(store::usage("--revoke must be a positive event id"));
            }
            store::plan::decide(&mut open()?, id, text, revoke)
        }
        "doc rm" => {
            let id = store::id(&p[0], "document id")?;
            if !f.get_bool("purge") {
                return Err(store::usage(
                    "doc rm requires --purge; the write-ahead log and earlier backups can still hold the text",
                ));
            }
            documents::purge(&mut open()?, id)
        }
        "doc backfill" => {
            let tree = f.get_int("tree");
            if tree < 0 {
                return Err(store::usage("--tree must be a positive task id"));
            }
            documents::backfill(&mut open()?, tree, f.get_bool("dry-run"))
        }
        "adopt" => handover::adopt(f),
        "new" => {
            orch::valid_name(&p[0], "name")?;
            if f.get_bool("planned") && f.get_int("parent") == 0 {
                return Err(store::usage(
                    "--planned needs --parent: a plan is a lane of an orchestrator",
                ));
            }
            if ![
                "orchestrator",
                "sub-orchestrator",
                "implementer",
                "reviewer",
                "researcher",
                "gate",
            ]
            .contains(&f.get_string("role"))
            {
                return Err(store::usage(
                    "--role must be one of orchestrator, sub-orchestrator, implementer, reviewer, researcher, gate",
                ));
            }
            orch::validate_location(f)?;
            orch::new(&mut open()?, f)
        }
        "launch" => {
            let id = store::id(&p[0], "task id")?;
            if ["provider", "model", "effort"]
                .iter()
                .any(|k| f.get_string(k).is_empty())
            {
                return Err(store::usage(
                    "launch needs --provider, --model and --effort",
                ));
            }
            if f.was_set("agent") {
                orch::valid_name(f.get_string("agent"), "--agent")?;
            }
            orch::validate_location(f)?;
            orch::launch(&mut open()?, id, f)
        }
        "start" => worker::start(&mut open()?),
        "got" => {
            let id = store::id(&p[0], "attempt event id")?;
            worker::got(&mut open()?, id)
        }
        "note" | "ask" | "ready" | "done" | "fail" => {
            let as_id = if matches!(cmd, "note" | "ask") {
                f.get_int("as")
            } else {
                0
            };
            if as_id < 0 {
                return Err(store::usage("--as must be a positive task id"));
            }
            let owner = matches!(cmd, "note" | "ask") && f.get_bool("owner");
            if cmd == "note" && owner && (!store::env("TASKR_TASK").is_empty() || as_id == 0) {
                return Err(store::reject(
                    "--owner: only a root orchestrator's own note",
                ));
            }
            let mut summary = p.first().cloned().unwrap_or_default();
            let mut data = match cmd {
                "ask" => Some(json!({"blocking":f.get_bool("blocking"),"owner":owner})),
                "note" if owner => Some(json!({"owner":true})),
                "ready" => Some(json!({})),
                _ => None,
            };
            if cmd == "ask" {
                let d = data.as_mut().expect("ask data");
                if f.was_set("question") {
                    if !owner {
                        return Err(store::usage("--question needs --owner"));
                    }
                    let q = question::parse(f.get_string("question"))?;
                    summary = question::summary(&summary, &q);
                    d["question"] = q;
                }
                if f.get_bool("dialog") {
                    if !owner {
                        return Err(store::usage("--dialog needs --owner"));
                    }
                    d["dialog"] = json!(true);
                }
            }
            if cmd == "ready" {
                let d = data.as_mut().expect("ready data");
                if !f.get_string("report").is_empty() {
                    let cwd = store::caller_cwd()?;
                    d["report"] = json!(orch::absolute(&cwd, f.get_string("report")));
                }
                let mut kv = json!({});
                for s in f.get_strings("kv") {
                    let Some((k, v)) = s.split_once('=') else {
                        return Err(store::usage(format!(
                            "invalid value {s:?} for flag -kv: --kv wants KEY=VALUE, got {s:?}"
                        )));
                    };
                    if k.is_empty() {
                        return Err(store::usage(format!(
                            "invalid value {s:?} for flag -kv: --kv wants KEY=VALUE, got {s:?}"
                        )));
                    }
                    kv[k] = json!(v);
                }
                if !kv.as_object().expect("object").is_empty() {
                    d["kv"] = kv;
                }
            }
            worker::write(
                &mut open()?,
                worker::Write {
                    kind: cmd,
                    summary: &summary,
                    key: f.get_string("key"),
                    data,
                    status: match cmd {
                        "ready" => "ready",
                        "done" => "done",
                        "fail" => "failed",
                        _ => "",
                    },
                    owner,
                    as_id,
                },
            )
        }
        "close" => {
            let outcome = f.get_string("outcome");
            if f.was_set("outcome")
                && !["accepted", "reworked", "rejected", "abandoned"].contains(&outcome)
            {
                return Err(store::usage(
                    "--outcome must be one of accepted, reworked, rejected, abandoned",
                ));
            }
            let id = store::id(&p[0], "task id")?;
            orch::close(&mut open()?, id, outcome)
        }
        "answer" => prompt::answer(f),
        "ack" => {
            let id = store::id(&p[0], "event id")?;
            if f.get_int("as") <= 0 {
                return Err(store::usage("ack needs --as TASK_ID"));
            }
            inbox::ack(&mut open()?, f.get_int("as"), id, true)
        }
        "wait" => inbox::wait(f),
        "set" => {
            let id = store::id(&p[0], "task id")?;
            let mut pairs: Vec<(String, String)> = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for s in &p[1..] {
                let Some((k, v)) = s.split_once('=') else {
                    return Err(store::usage(format!(
                        "set wants KEY=VALUE (KEY= deletes), got {s:?}"
                    )));
                };
                if k.is_empty()
                    || k.len() > 32
                    || !k.as_bytes()[0].is_ascii_lowercase()
                    || !k.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b)
                    })
                {
                    return Err(store::usage(format!(
                        "set: key {k:?} must match [a-z][a-z0-9_.-]{{0,31}}"
                    )));
                }
                if !seen.insert(k) {
                    return Err(store::usage(format!("set: key {k} given twice")));
                }
                let v = v.trim();
                if v.chars().count() > 200 {
                    return Err(store::usage(format!(
                        "set: the value of {k} is longer than 200 characters"
                    )));
                }
                if v.chars().any(char::is_control) {
                    return Err(store::usage(format!(
                        "set: the value of {k} holds a control character; references are one line"
                    )));
                }
                pairs.push((k.into(), v.into()));
            }
            if let Some((_, value)) = pairs.iter().find(|(k, _)| k == "glance.state") {
                if !store::env("TASKR_TASK").is_empty() || !store::env("TASKR_LAUNCH").is_empty() {
                    return Err(store::reject(
                        "glance.state is root-only; TASKR_TASK and TASKR_LAUNCH must be unset",
                    ));
                }
                if !value.is_empty() && value != "parked" {
                    return Err(store::usage("glance.state must be parked or empty"));
                }
            }
            orch::set(&mut open()?, id, &pairs)
        }
        "doc set" => {
            let id = store::id(&p[0], "task id")?;
            let kind = p[1].as_str();
            let name = f.get_string("name");
            if !["goal", "plan"].contains(&kind) {
                return Err(store::usage("doc set accepts goal or plan"));
            }
            if kind == "goal" && f.was_set("name") {
                return Err(store::usage("goal takes no --name"));
            }
            if f.was_set("name")
                && (name.is_empty()
                    || name.len() > 64
                    || !name.as_bytes()[0].is_ascii_lowercase()
                        && !name.as_bytes()[0].is_ascii_digit()
                    || !name.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)
                    }))
            {
                return Err(store::usage("--name must match [a-z0-9][a-z0-9._-]{0,63}"));
            }
            if f.get_string("file").is_empty() {
                return Err(store::usage("doc set needs --file PATH"));
            }
            let path = orch::absolute(&store::caller_cwd()?, f.get_string("file"));
            let input = if store::caller_machine().is_some() {
                crate::hub::document_set_input(id, kind, name, &path)?
            } else {
                documents::file(&path, "")
            };
            if input.reason == "missing" {
                return Err(store::usage(format!(
                    "document file is missing or unreadable: {path}"
                )));
            }
            if !input.reason.is_empty() {
                return Err(store::usage(format!(
                    "document file: {} ({path})",
                    input.reason
                )));
            }
            documents::set(&mut open()?, id, kind, name, &input)
        }
        "prompt" => prompt::prompt(f),
        "handover" => handover::run(f),
        _ => unreachable!(),
    }
}
