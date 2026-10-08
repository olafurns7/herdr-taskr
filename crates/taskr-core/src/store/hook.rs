//! Silent local harness hook application; transport is owned by the net lane.
use super::*;
#[derive(Default, Debug)]
pub struct Record {
    pub event: String,
    pub session: String,
    pub transcript: String,
    pub error: String,
    pub attempt: i64,
}
pub fn supported(harness: &str, event: &str) -> bool {
    match harness {
        "claude" => ["SessionStart", "UserPromptSubmit", "Stop", "StopFailure"].contains(&event),
        "codex" => ["SessionStart", "UserPromptSubmit", "Stop"].contains(&event),
        "opencode" => [
            "chat.message",
            "session.created",
            "session.idle",
            "session.error",
        ]
        .contains(&event),
        "pi" => ["session_start", "input", "agent_settled"].contains(&event),
        _ => false,
    }
}
fn start(event: &str) -> bool {
    ["SessionStart", "session.created", "session_start"].contains(&event)
}
fn prompt(event: &str) -> bool {
    ["UserPromptSubmit", "chat.message", "input"].contains(&event)
}
fn stall_event(event: &str) -> bool {
    [
        "Stop",
        "StopFailure",
        "session.idle",
        "session.error",
        "agent_settled",
    ]
    .contains(&event)
}
fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn valid_code(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 96
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
pub fn error_code(v: &Value) -> String {
    if let Some(s) = v.as_str().filter(|s| valid_code(s)) {
        return s.into();
    }
    if v.is_object() {
        for k in ["codex_error_info", "code", "type", "name"] {
            let v = &v[k];
            if let Some(s) = v.as_str().filter(|s| valid_code(s)) {
                return s.into();
            }
            if let Some(m) = v.as_object().filter(|m| m.len() == 1)
                && let Some(k) = m.keys().next().filter(|k| valid_code(k))
            {
                return k.clone();
            }
        }
    }
    String::new()
}
fn pi_error(v: &Value) -> String {
    if let Some(ds) = v["diagnostics"].as_array() {
        for d in ds.iter().rev() {
            let code = &d["error"]["code"];
            if let Some(s) = code.as_str().filter(|s| valid_code(s)) {
                return s.into();
            }
            if let Some(n) = code.as_f64().filter(|n| *n == (*n as i64) as f64) {
                return (n as i64).to_string();
            }
            let ty = text(d, "type");
            if valid_code(ty) {
                return ty.into();
            }
        }
    }
    "unknown".into()
}
pub fn receipt_attempt(s: &str) -> i64 {
    let Some(s) = s.strip_prefix("First taskr got ") else {
        return 0;
    };
    let Some(n) = s.find(|c: char| !c.is_ascii_digit()) else {
        return 0;
    };
    if n == 0 || !s[n..].starts_with(['.', ';']) {
        return 0;
    }
    s[..n].parse::<i64>().ok().filter(|n| *n > 0).unwrap_or(0)
}
pub fn parse(harness: &str, event: &str, bytes: &[u8]) -> Option<Record> {
    if !supported(harness, event) || bytes.len() > 8 << 20 {
        return None;
    }
    let v: Value = serde_json::from_slice(bytes).ok()?;
    if !v.is_object() {
        return None;
    }
    let mut h = Record {
        event: event.into(),
        ..Record::default()
    };
    match harness {
        "claude" | "codex" => {
            if !text(&v, "hook_event_name").is_empty() && text(&v, "hook_event_name") != event {
                return None;
            }
            h.session = text(&v, "session_id").into();
            h.transcript = text(&v, "transcript_path").into();
            if prompt(event) {
                h.attempt = receipt_attempt(text(&v, "prompt"));
            }
            if event == "StopFailure" {
                h.error = error_code(&v["error"]);
                if h.error.is_empty() {
                    h.error = "unknown".into();
                }
            }
        }
        "opencode" => {
            if event == "chat.message" {
                let input = &v["input"];
                let output = &v["output"];
                h.session = text(input, "sessionID").into();
                if h.session.is_empty() {
                    h.session = text(&output["message"], "sessionID").into();
                }
                if let Some(parts) = output["parts"].as_array() {
                    for p in parts {
                        if p["type"] == "text" {
                            h.attempt = receipt_attempt(text(p, "text"));
                            break;
                        }
                    }
                }
            } else {
                if text(&v, "type") != event {
                    return None;
                }
                let props = &v["properties"];
                h.session = text(props, "sessionID").into();
                if h.session.is_empty() {
                    h.session = text(&props["info"], "id").into();
                }
                if event == "session.error" {
                    h.error = error_code(&props["error"]);
                    if h.error.is_empty() {
                        h.error = "unknown".into();
                    }
                }
            }
        }
        "pi" => {
            if text(&v, "type") != event {
                return None;
            }
            h.session = text(&v, "sessionId").into();
            h.transcript = text(&v, "sessionFile").into();
            if event == "input" {
                h.attempt = receipt_attempt(text(&v, "text"));
            }
            if event == "agent_settled" && v["message"]["stopReason"] == "error" {
                h.error = pi_error(&v["message"]);
            }
        }
        _ => return None,
    }
    if h.session.is_empty()
        || prompt(event) && h.attempt == 0
        || !h.transcript.is_empty() && !std::path::Path::new(&h.transcript).is_absolute()
    {
        return None;
    }
    Some(h)
}
pub fn apply(db: &mut Connection, h: &Record) -> Result<()> {
    transaction(db, |tx| {
        let t = worker::resolve(tx, 0)?;
        let Some(lid) = t.launch else {
            return Ok(());
        };
        let(provider,pane,session,source):(String,String,String,String)=tx.query_row("select coalesce(l.provider,''),coalesce(l.pane_id,t.pane_id,''),coalesce(l.session_ref,''),coalesce(l.session_source,'') from launches l join tasks t on t.id=l.task_id where l.id=?",[lid],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let hp = env("HERDR_PANE_ID");
        if hp.is_empty() || hp != pane || h.session.is_empty() {
            return Ok(());
        }
        if start(&h.event) {
            if !source.starts_with("hook:") {
                tx.execute("update launches set session_ref=?,session_kind=?,session_source=?,transcript_path=? where id=?",params![h.session,if provider=="codex"{"thread_id"}else{"id"},format!("hook:{}",h.event),null(&h.transcript),lid])?;
            }
            return Ok(());
        }
        if !source.starts_with("hook:") || session != h.session {
            return Ok(());
        }
        if prompt(&h.event) {
            let path: Option<String> = tx.query_row(
                "select transcript_path from launches where id=?",
                [lid],
                |r| r.get(0),
            )?;
            worker::got_tx(
                tx,
                h.attempt,
                Some(
                    json!({"session_ref":session,"session_source":format!("hook:{}",h.event),"transcript_path":path}),
                ),
            )?;
        } else if stall_event(&h.event) {
            stall(tx, &t, lid, &h.error)?;
        }
        Ok(())
    })
}
pub fn stall(tx: &Connection, t: &Task, launch: i64, error: &str) -> Result<()> {
    if error.is_empty() && ["orchestrator", "sub-orchestrator"].contains(&t.role.as_str()) {
        return Ok(());
    }
    let (prompt,report):(i64,i64)=tx.query_row("select coalesce((select max(id) from events where launch_id=?1 and kind='prompt'),0),coalesce((select max(id) from events where launch_id=?1 and kind in('done','fail')),0)",[launch],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if prompt == 0 || prompt <= report {
        return Ok(());
    }
    let ended:bool=tx.query_row("select exists(select 1 from events where launch_id=? and id>? and kind in('done','fail','ask'))",params![launch,prompt],|r|r.get(0))?;
    if ended {
        return Ok(());
    }
    let key = format!("stall:{prompt}");
    let exists: bool = tx.query_row(
        "select exists(select 1 from events where event_key=?)",
        [&key],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(());
    }
    let mut data = json!({"reason":"stall"});
    if !error.is_empty() {
        data["error"] = json!(error);
    }
    let last=tx.query_row("select kind from events where launch_id=? and kind in('ready','done','fail','ask') order by id desc limit 1",[launch],|r|r.get::<_,String>(0)).optional()?;
    if let Some(last) = last {
        data["last"] = json!(match last.as_str() {
            "ready" => "r",
            "done" => "d",
            "fail" => "f",
            _ => "q",
        });
    }
    event(
        tx,
        Event {
            task: t.id,
            to: t.parent,
            launch: Some(launch),
            kind: "herdr",
            summary: "worker turn stalled",
            data: Some(data),
            key: &key,
            ..Event::default()
        },
    )?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn payload_boundary() {
        assert_eq!(
            parse(
                "claude",
                "UserPromptSubmit",
                br#"{"session_id":"s","prompt":"First taskr got 7. Read it"}"#
            )
            .unwrap()
            .attempt,
            7
        );
        assert!(parse("pi","input",br#"{"type":"input","sessionId":"s","text":"First taskr got 7. X","sessionFile":"relative"}"#).is_none());
        assert!(parse("codex", "SessionStart", br#"{"session_id":"s"} {}"#).is_none());
    }
}
