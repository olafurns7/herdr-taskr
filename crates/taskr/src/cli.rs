use serde_json::{Value, json};
use taskr_core::{ExitCode, compact_json};
pub const USAGE: &str = include_str!("read/usage.txt");
pub fn usage_line(name: &str) -> String {
    for line in USAGE.lines() {
        let usage = if !line.starts_with([' ', '\t']) {
            line.split_once(':').map_or(line, |(_, rest)| rest)
        } else {
            line
        };
        if usage
            .split('|')
            .any(|segment| segment.split_whitespace().next() == Some(name))
        {
            return line.into();
        }
    }
    format!("usage: taskr {name} [args]")
}
pub fn error(json_mode: bool, cmd: &str, message: &str, code: ExitCode) -> ExitCode {
    let kind = match code {
        ExitCode::Watch => "watch",
        ExitCode::Usage => "usage",
        ExitCode::Rejected => "rejected",
        ExitCode::Database => "database",
        ExitCode::Transport => "herdr",
        ExitCode::NotImplemented => "not_implemented",
        _ => "error",
    };
    if json_mode {
        eprintln!("taskr {cmd}: {message}");
        println!(
            "{}",
            compact_json(&json!({"error":message,"kind":kind})).unwrap()
        );
    } else {
        println!(
            "x1 {} {}",
            code as u8,
            compact_json(&json!({"err":message,"k":kind})).unwrap()
        );
    }
    code
}
pub fn read_line(v: &Value) -> String {
    let aliases = [
        ("id", "i"),
        ("task_id", "t"),
        ("task_name", "n"),
        ("name", "n"),
        ("kind", "k"),
        ("created_at", "at"),
        ("updated_at", "at"),
        ("recipient_task_id", "to"),
        ("launch_id", "l"),
        ("current_launch_id", "l"),
        ("related_event_id", "rel"),
        ("answered_by", "ans"),
        ("summary", "s"),
        ("data", "d"),
        ("event_key", "key"),
        ("record", "rec"),
        ("status", "s"),
        ("workspace_id", "w"),
        ("tab_id", "tab"),
        ("pane_id", "pane"),
        ("report_path", "rp"),
        ("parent_id", "par"),
        ("acked_event_id", "ack"),
        ("pending_event_id", "pend"),
        ("last_receipt", "got"),
        ("open_asks", "a"),
        ("blocking_asks", "b"),
        ("round", "r"),
        ("waiting", "wait"),
        ("observed", "h"),
        ("next", "nx"),
    ];
    let mut out = serde_json::Map::new();
    for (k, v) in v.as_object().unwrap() {
        let key = aliases
            .iter()
            .find(|(a, _)| a == k)
            .map_or(k.as_str(), |(_, b)| b);
        let mut v = v.clone();
        if k == "observed"
            && let Some(obs) = v.as_object_mut()
        {
            if let Some(s) = obs.remove("agent_status") {
                obs.insert("s".into(), s);
            }
            if let Some(s) = obs.remove("state_change_seq") {
                obs.insert("seq".into(), s);
            }
        }
        out.insert(key.into(), v);
    }
    format!("j1 {}\n", compact_json(&Value::Object(out)).unwrap())
}
pub fn emit(json_mode: bool, value: &Value, aliases: bool) {
    if !json_mode && aliases {
        print!("{}", read_line(value));
    } else {
        println!(
            "{}{}",
            if json_mode { "" } else { "j1 " },
            compact_json(value).unwrap()
        );
    }
}
pub fn trailer(json_mode: bool, value: Value) {
    if !json_mode {
        println!("m1 {}", compact_json(&value).unwrap());
    }
}

pub const COMMANDS: &[&str] = &[
    "version", "new", "launch", "close", "start", "got", "note", "ready", "ask", "done", "fail",
    "prompt", "answer", "hook", "wait", "ack", "next", "decide", "set", "handover", "adopt",
    "status", "asks", "log", "notes", "glance", "campaign", "search", "daemon", "doc", "spool",
    "help", "slotr", "after", "tmp",
];
pub fn known(name: &str) -> bool {
    COMMANDS.contains(&name)
}
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<_> = b.chars().collect();
    let mut prev: Vec<_> = (0..=b.len()).collect();
    for (i, x) in a.chars().enumerate() {
        let mut row = vec![i + 1];
        for (j, y) in b.iter().enumerate() {
            row.push(
                (prev[j + 1] + 1)
                    .min(row[j] + 1)
                    .min(prev[j] + usize::from(x != *y)),
            );
        }
        prev = row;
    }
    prev[b.len()]
}
pub fn unknown(json_mode: bool, name: &str) -> ExitCode {
    let suggestion = match name {
        "show" => "status --tree ID / log ID",
        "inbox" => "asks --open or wait --as ID --timeout 0 (consuming)",
        _ => {
            let best = COMMANDS
                .iter()
                .map(|s| (distance(name, s), *s))
                .min()
                .unwrap();
            if best.0 <= 2 { best.1 } else { "taskr help" }
        }
    };
    let v = if json_mode {
        json!({"error":format!("unknown command {name}"),"kind":"usage","try":suggestion})
    } else {
        json!({"err":format!("unknown command {name}"),"k":"usage","try":suggestion})
    };
    println!(
        "{}{}",
        if json_mode { "" } else { "x1 2 " },
        compact_json(&v).unwrap()
    );
    ExitCode::Usage
}
pub fn ordered_trailer(json_mode: bool, value: &Value, keys: &[&str]) {
    if !json_mode {
        println!(
            "m1 {{{}}}",
            keys.iter()
                .filter_map(|k| value
                    .get(*k)
                    .map(|v| format!("\"{k}\":{}", compact_json(v).unwrap())))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
}
