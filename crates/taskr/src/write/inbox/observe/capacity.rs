use super::*;
#[derive(Clone, PartialEq)]
struct Child {
    task: i64,
    parent: i64,
    launch: i64,
    pane: String,
    task_pane: String,
    name: String,
    model: String,
    session_kind: String,
    session_ref: String,
    attempt: i64,
}
fn children(db: &db::Connection, parent: i64) -> Result<Vec<Child>> {
    let mut stmt=db.prepare("select t.id,t.parent_id,l.id,coalesce(l.pane_id,t.pane_id),coalesce(t.pane_id,''),t.agent_name,coalesce(l.model,''),coalesce(l.session_kind,''),coalesce(l.session_ref,''),coalesce((select max(e.id) from events e where e.task_id=t.id and e.launch_id=l.id and e.kind='prompt'),0) from tasks t join launches l on l.id=t.current_launch_id where t.parent_id=? and t.status='open' and t.role!='gate' and l.provider='codex' and coalesce(l.pane_id,t.pane_id,'')!='' and coalesce(t.agent_name,'')!='' and(t.waiting_until is null or t.waiting_until<=?) and l.machine is null order by t.id")?;
    Ok(stmt
        .query_map(params![parent, store::now()], |r| {
            Ok(Child {
                task: r.get(0)?,
                parent: r.get(1)?,
                launch: r.get(2)?,
                pane: r.get(3)?,
                task_pane: r.get(4)?,
                name: r.get(5)?,
                model: r.get(6)?,
                session_kind: r.get(7)?,
                session_ref: r.get(8)?,
                attempt: r.get(9)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?)
}
fn binding(a: &Value, w: &Child, sock: &str) -> Option<Value> {
    let s = &a["agent_session"];
    let sk = s["kind"].as_str().unwrap_or("");
    let sr = s["value"].as_str().unwrap_or("");
    let term = a["terminal_id"].as_str().unwrap_or("");
    if a["agent"] != "codex"
        || s["agent"] != "codex"
        || a["name"] != w.name
        || a["pane_id"] != w.pane
        || !w.task_pane.is_empty() && w.task_pane != w.pane
        || term.is_empty()
        || sr.is_empty()
        || !["id", "thread_id"].contains(&sk)
    {
        return None;
    }
    if (!w.session_ref.is_empty() || !w.session_kind.is_empty())
        && (!["id", "thread_id"].contains(&w.session_kind.as_str()) || w.session_ref != sr)
    {
        return None;
    }
    Some(
        json!({"terminal":term,"session_hash":documents::body(sr.as_bytes(),"").hash,"socket_hash":documents::body(sock.as_bytes(),"").hash}),
    )
}
fn get(sock: &str, pane: &str, left: Duration) -> Option<Value> {
    herdr::request(sock, "agent.get", json!({"target":pane}), left)
        .ok()
        .map(|v| v["agent"].clone())
}
fn state(db: &db::Connection, launch: i64) -> Result<Option<Value>> {
    get_meta(db, &format!("capacity:{launch}"))?
        .map(|raw| {
            serde_json::from_str(&raw).map_err(|_| Error {
                code: ExitCode::Database,
                message: "invalid capacity ledger record".into(),
            })
        })
        .transpose()
}
fn save(db: &db::Connection, launch: i64, v: &Value) -> Result<()> {
    // Go structs encode in declaration order; this is also stored ledger data.
    let b = &v["binding"];
    let text = format!(
        "{{\"binding\":{{\"terminal\":{},\"session_hash\":{},\"socket_hash\":{}}},\"episode\":{},\"active\":{},\"revision\":{}}}",
        compact_json(&b["terminal"])?,
        compact_json(&b["session_hash"])?,
        compact_json(&b["socket_hash"])?,
        v["episode"],
        v["active"],
        v["revision"]
    );
    set_meta(db, &format!("capacity:{launch}"), &text)
}
pub fn bind(db: &mut db::Connection, task: i64, launch: i64) {
    let Ok(t) = store::task(db, task) else {
        return;
    };
    let Some(parent) = t.parent else {
        return;
    };
    let Ok(ws) = children(db, parent) else {
        return;
    };
    let Some(w) = ws.iter().find(|w| w.task == task && w.launch == launch) else {
        return;
    };
    let sock = herdr::socket();
    let Some(a) = get(&sock, &w.pane, Duration::from_millis(500)) else {
        return;
    };
    let Some(binding) = binding(&a, w, &sock) else {
        return;
    };
    let _ = store::transaction(db, |tx| {
        if !children(tx, parent)?.contains(w) || state(tx, launch)?.is_some() {
            return Ok(());
        }
        save(
            tx,
            launch,
            &json!({"binding":binding,"episode":0,"active":false,"revision":a["revision"].as_i64().unwrap_or(0)}),
        )
    });
}
pub fn capacity(db: &mut db::Connection, as_id: i64, left: Duration) -> Result<()> {
    if left.is_zero() {
        return Ok(());
    }
    let ws = children(db, as_id)?;
    if ws.is_empty() || !claim(db, as_id, Some(&format!("capacity_poll:{as_id}")))? {
        return Ok(());
    }
    let sock = herdr::socket();
    let start = Instant::now();
    let budget = left.min(Duration::from_secs(2));
    let key = format!("capacity_cursor:{as_id}");
    let cursor = get_meta(db, &key)?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let index = ws.iter().position(|w| w.task > cursor).unwrap_or(0);
    for i in 0..ws.len() {
        let left = budget.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        let w = &ws[(index + i) % ws.len()];
        observe(db, &sock, w, left)?;
        set_meta(db, &key, &w.task.to_string())?;
    }
    Ok(())
}
fn observe(db: &mut db::Connection, sock: &str, w: &Child, left: Duration) -> Result<()> {
    let old = state(db, w.launch)?;
    let start = Instant::now();
    let Some(before) = get(sock, &w.pane, left) else {
        return Ok(());
    };
    let Some(binding) = binding(&before, w, sock) else {
        return Ok(());
    };
    if old.as_ref().is_some_and(|s| s["binding"] != binding) {
        return Ok(());
    }
    let Ok(v) = herdr::request(
        sock,
        "agent.read",
        json!({"target":w.pane,"source":"detection","format":"text","strip_ansi":true}),
        left.saturating_sub(start.elapsed()),
    ) else {
        return Ok(());
    };
    let r = &v["read"];
    if r["pane_id"] != w.pane || r["source"] != "detection" || r["format"] != "text" {
        return Ok(());
    }
    let Some(mut revision) = r["revision"].as_i64().filter(|n| *n >= 0) else {
        return Ok(());
    };
    if revision == 0 {
        revision = before["revision"].as_i64().unwrap_or(0);
    }
    if revision < 0 {
        return Ok(());
    }
    let (signal, attempt) = tail(r["text"].as_str().unwrap_or(""));
    if signal == 0 {
        return Ok(());
    }
    if old.is_none() && w.session_ref.is_empty() && (w.attempt == 0 || attempt != w.attempt) {
        return Ok(());
    }
    let Some(after) = get(sock, &w.pane, left.saturating_sub(start.elapsed())) else {
        return Ok(());
    };
    if self::binding(&after, w, sock) != Some(binding.clone()) {
        return Ok(());
    }
    store::transaction(db, |tx| {
        if !children(tx, w.parent)?.contains(w) {
            return Ok(());
        }
        let latest = state(tx, w.launch)?;
        if latest.as_ref().is_some_and(|s| {
            s["binding"] != binding || revision <= s["revision"].as_i64().unwrap_or(0)
        }) {
            return Ok(());
        }
        if latest.is_none() && old.is_some() {
            return Ok(());
        }
        let mut latest =
            latest.unwrap_or(json!({"binding":binding,"episode":0,"active":false,"revision":0}));
        latest["revision"] = json!(revision);
        if signal == 2 {
            latest["active"] = json!(false);
        } else if latest["active"] != true {
            let episode = latest["episode"].as_i64().unwrap_or(0) + 1;
            latest["episode"] = json!(episode);
            latest["active"] = json!(true);
            store::event(
                tx,
                store::Event {
                    task: w.task,
                    to: Some(w.parent),
                    launch: Some(w.launch),
                    kind: "herdr",
                    summary: "Codex capacity warning observed; inspect helper before retry",
                    data: Some(
                        json!({"reason":"model_capacity","provider":"codex","model":w.model,"model_source":"launch","pane_id":w.pane,"agent_name":w.name,"episode":episode,"source":"detection","action":"inspect_before_retry"}),
                    ),
                    key: &format!("capacity:{}:{episode}", w.launch),
                    ..store::Event::default()
                },
            )?;
        }
        save(tx, w.launch, &latest)
    })
}
fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || c == '\x1b' && chars.next() == Some('\\') {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else {
            out.push(c);
        }
    }
    out.replace("\r\n", "\n")
}
fn fold(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
// ponytail: terminal-cell heuristic; replace with structured provider capacity events when available.
fn tail(text: &str) -> (u8, i64) {
    let text = strip_ansi(text);
    let rows: Vec<_> = text.lines().collect();
    let mut end = rows.len();
    while end > 0 && rows[end - 1].trim().is_empty() {
        end -= 1;
    }
    if end > 0 && rows[end - 1].trim_start().starts_with("? for shortcuts") {
        end -= 1;
        while end > 0 && rows[end - 1].trim().is_empty() {
            end -= 1;
        }
        let status = |s: &str| {
            s.starts_with("  ")
                && !s.trim().is_empty()
                && !s.trim_start().starts_with(' ')
                && s.contains(" · ")
        };
        if end > 0 && status(rows[end - 1]) {
            let status_end = end;
            end -= 1;
            while end > 0 && rows[end - 1].trim().is_empty() {
                end -= 1;
            }
            if end > 0 && status(rows[end - 1]) {
                return (0, 0);
            }
            if end == 0 || !rows[end - 1].trim().starts_with('›') {
                end = status_end;
            }
        }
        if end > 0 && rows[end - 1].trim().starts_with('›') {
            let c = rows[end - 1].trim();
            if ![
                "›",
                "› Summarize recent commits",
                "› Write tests for @filename",
                "› Explain this codebase",
                "› Implement {feature}",
                "› Find and fix a bug in @filename",
                "› Improve documentation in @filename",
                "› Ask Codex to do anything",
            ]
            .contains(&c)
            {
                return (0, 0);
            }
            end -= 1;
        }
    } else if end > 0 && rows[end - 1].trim() == "›" {
        end -= 1;
    }
    let (mut signal, mut attempt) = (0, 0);
    let mut fence = "";
    let mut i = 0;
    while i < end {
        let row = rows[i];
        let trimmed = row.trim();
        i += 1;
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let prefix = &trimmed[..3];
            if fence == prefix {
                fence = "";
            } else if fence.is_empty() {
                fence = prefix;
            }
            if signal == 1 {
                signal = 0;
            }
            continue;
        }
        if !fence.is_empty() {
            continue;
        }
        if let Some(cell) = row.strip_prefix("› ") {
            let mut cell = cell.to_string();
            while i < end && rows[i].starts_with("  ") && !rows[i].trim().is_empty() {
                cell.push(' ');
                cell.push_str(rows[i].trim());
                i += 1;
            }
            let cell = fold(&cell);
            attempt = cell
                .strip_prefix("First taskr got ")
                .and_then(|s| {
                    let n = s.find(|c: char| !c.is_ascii_digit())?;
                    let rest = &s[n..];
                    if n == 0
                        || s.starts_with('0')
                        || !rest.starts_with(['.', ';'])
                        || rest.len() > 1 && !rest[1..].starts_with(' ')
                    {
                        return None;
                    }
                    s[..n].parse::<i64>().ok()
                })
                .unwrap_or(0);
            if signal == 1 {
                signal = 2;
            }
            continue;
        }
        if trimmed.starts_with("• Working (") && trimmed.ends_with(" • esc to interrupt)") {
            continue;
        }
        if let Some(cell) = row.strip_prefix("■ ") {
            let mut cell = cell.to_string();
            while i < end {
                let next = rows[i].trim();
                if next.is_empty() || next.starts_with(['›', '•', '■', '└']) {
                    break;
                }
                cell.push(' ');
                cell.push_str(next);
                i += 1;
            }
            signal =
                if fold(&cell) == "Selected model is at capacity. Please try a different model." {
                    1
                } else {
                    0
                };
            continue;
        }
        if row.starts_with("• ") {
            if signal == 1 {
                signal = 2;
            }
            continue;
        }
        if signal == 1 {
            signal = 0;
        }
    }
    (signal, attempt)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_warning_tail() {
        assert_eq!(
            tail(
                "› First taskr got 42. Do it\n■ Selected model is at capacity. Please try a different model.\n›"
            ),
            (1, 42)
        );
        assert_eq!(
            tail("```\n■ Selected model is at capacity. Please try a different model.\n```"),
            (0, 0)
        );
        assert_eq!(
            tail("■ Selected model is at capacity. Please try a different model.\n• Resumed"),
            (2, 0)
        );
    }
}
