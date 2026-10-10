use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};
use taskr_core::{Document, compact_json};

pub(super) const BODY_MAX: usize = 2 << 20;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RpcRequest {
    #[serde(deserialize_with = "null_default")]
    pub argv: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    pub cwd: String,
    #[serde(deserialize_with = "null_default")]
    pub env: BTreeMap<String, String>,
    pub request_key: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(deserialize_with = "null_default")]
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<Document>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub queued_at: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub queued_age_ms: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct RpcReply {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload: Option<Vec<DocWant>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DocWant {
    pub task: i64,
    pub kind: String,
    pub name: String,
    pub path: String,
    pub event_id: Option<i64>,
}

pub(super) fn command(argv: &[String]) -> (&str, &[String]) {
    let argv = if argv.first().is_some_and(|s| s == "--json") {
        &argv[1..]
    } else {
        argv
    };
    argv.split_first().map_or(("", &[]), |(s, rest)| (s, rest))
}

fn wants_help(args: &[String]) -> bool {
    args.iter().take_while(|s| s.as_str() != "--").any(|s| {
        let (name, value) = s
            .trim_start_matches('-')
            .split_once('=')
            .map_or((s.trim_start_matches('-'), None), |(k, v)| (k, Some(v)));
        s.starts_with('-') && matches!(name, "h" | "help") && value.is_none_or(|v| v == "true")
    })
}

pub(super) fn stored(argv: &[String]) -> bool {
    let (name, args) = command(argv);
    if name == "_hook" {
        return true;
    }
    crate::cli::known(name)
        && !matches!(
            name,
            "daemon"
                | "wait"
                | "status"
                | "asks"
                | "log"
                | "notes"
                | "search"
                | "version"
                | "help"
                | "glance"
                | "campaign"
                | "slotr"
                | "tmp"
        )
        && !(name == "doc"
            && args
                .first()
                .is_some_and(|s| matches!(s.as_str(), "get" | "ls" | "backfill")))
        && !(name == "after"
            && args.iter().take_while(|s| s.as_str() != "--").any(|s| {
                s.starts_with('-') && matches!(s.trim_start_matches('-'), "list" | "list=true")
            }))
        && !wants_help(args)
}

pub(super) fn flag<'a>(args: &'a [String], wanted: &str) -> Option<&'a str> {
    for (i, s) in args
        .iter()
        .enumerate()
        .take_while(|(_, s)| s.as_str() != "--")
    {
        if !s.starts_with('-') {
            continue;
        }
        let (name, value) = s
            .trim_start_matches('-')
            .split_once('=')
            .map_or((s.trim_start_matches('-'), None), |(k, v)| (k, Some(v)));
        if name == wanted {
            return Some(value.unwrap_or_else(|| args.get(i + 1).map_or("", String::as_str)));
        }
    }
    None
}

pub(super) fn check_args(argv: &[String]) -> Result<(), String> {
    let (name, args) = command(argv);
    let mut seen = std::collections::BTreeSet::new();
    let mut iter = args.iter();
    while let Some(s) = iter.next() {
        if s == "--" {
            break;
        }
        if !s.starts_with('-') || s == "-" {
            continue;
        }
        let (key, value) = s
            .trim_start_matches('-')
            .split_once('=')
            .map_or((s.trim_start_matches('-'), None), |(k, v)| (k, Some(v)));
        if !matches!(
            key,
            "cwd"
                | "brief"
                | "report"
                | "file"
                | "out"
                | "machine"
                | "role"
                | "planned"
                | "timeout"
                | "name"
                | "confirm"
                | "confirm-timeout"
                | "prompt"
                | "receipt-timeout"
        ) {
            continue;
        }
        if !seen.insert(key) {
            return Err(format!("repeated RPC flag --{key}"));
        }
        if value.is_none() && !matches!(key, "planned" | "confirm" | "prompt") {
            iter.next();
        }
    }
    match name {
        "daemon" => return Err("daemon runs on its own host; it is not available over RPC".into()),
        "spool" => {
            return Err(
                "spool commands run on their own host; they are not available over RPC".into(),
            );
        }
        _ => {}
    }
    let paths: &[&str] = match name {
        "new" => &["cwd", "brief", "report"],
        "ready" => &["report"],
        "prompt" => &["file"],
        _ => &[],
    };
    for key in paths {
        if let Some(value) = flag(args, key)
            && !value.is_empty()
            && !Path::new(value).is_absolute()
        {
            return Err(format!(
                "--{key} must be an absolute path over RPC, got {}",
                taskr_core::goflag::quote(value)
            ));
        }
    }
    if name == "tmp"
        && args
            .iter()
            .take_while(|s| s.as_str() != "--")
            .any(|s| s.starts_with('-') && s.trim_start_matches('-').starts_with("mkdir"))
    {
        return Err("tmp --mkdir creates the dir on the caller's host; the client does it".into());
    }
    if name == "close"
        && args
            .iter()
            .take_while(|s| s.as_str() != "--")
            .any(|s| s.starts_with('-') && s.trim_start_matches('-').starts_with("clean-tmp"))
    {
        return Err(
            "close --clean-tmp removes dirs on the caller's host; the client does it".into(),
        );
    }
    if name == "handover" && flag(args, "out").is_some() {
        return Err("handover --out writes on the caller's host; the client writes it".into());
    }
    Ok(())
}

pub(super) fn budget(argv: &[String]) -> std::time::Duration {
    let (name, args) = command(argv);
    let ms = |name, default| {
        flag(args, name)
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|n| *n >= 0)
            .unwrap_or(default)
    };
    let truth = |name: &str| {
        args.iter().take_while(|s| s.as_str() != "--").any(|s| {
            s.starts_with('-')
                && matches!(
                    s.trim_start_matches('-').strip_prefix(name),
                    Some("" | "=true" | "=1")
                )
        })
    };
    let millis = if wants_help(args) {
        30000
    } else {
        match name {
            "wait" => ms("timeout", 540000),
            "prompt" | "_prompt" => 30000_i64.wrapping_add(if truth("confirm") {
                ms("confirm-timeout", 60000)
            } else {
                0
            }),
            "answer" if truth("prompt") => 30000_i64.wrapping_add(if truth("confirm") {
                ms("confirm-timeout", 60000)
            } else {
                0
            }),
            _ => 30000,
        }
    };
    std::time::Duration::from_nanos(millis.wrapping_mul(1_000_000).max(0) as u64)
}

pub(super) fn error(req: &RpcRequest, exit: i32, kind: &str, message: &str) -> RpcReply {
    let json = req.env.get("TASKR_FORMAT").is_some_and(|s| s == "json")
        || req.argv.first().is_some_and(|s| s == "--json");
    let (cmd, _) = command(&req.argv);
    let value = if json {
        serde_json::json!({"error":message,"kind":kind})
    } else {
        serde_json::json!({"err":message,"k":kind})
    };
    RpcReply {
        exit,
        stdout: format!(
            "{}{}\n",
            if json {
                String::new()
            } else {
                format!("x1 {exit} ")
            },
            compact_json(&value).expect("JSON")
        ),
        stderr: if json {
            format!("taskr {cmd}: {message}\n")
        } else {
            String::new()
        },
        upload: None,
    }
}

pub(super) fn decode(bytes: &[u8]) -> Result<RpcRequest, String> {
    let text = String::from_utf8_lossy(bytes);
    let mut stream = serde_json::Deserializer::from_str(&text).into_iter::<serde_json::Value>();
    let mut value = stream
        .next()
        .ok_or_else(|| "EOF".to_string())?
        .map_err(|e| {
            if e.is_eof() {
                "unexpected EOF".into()
            } else {
                e.to_string()
            }
        })?;
    if let Some(next) = stream.next() {
        next.map_err(|e| e.to_string())?;
        return Err("trailing data after the JSON object".into());
    }
    if value.is_null() {
        value = serde_json::json!({});
    }
    if let Some(object) = value.as_object_mut() {
        let known = [
            "argv",
            "cwd",
            "env",
            "request_key",
            "capabilities",
            "document",
            "queued_at",
            "queued_age_ms",
        ];
        if let Some(key) = object.keys().find(|key| !known.contains(&key.as_str())) {
            return Err(format!(
                "json: unknown field {}",
                taskr_core::goflag::quote(key)
            ));
        }
        if let Some(doc) = object
            .get_mut("document")
            .and_then(serde_json::Value::as_object_mut)
        {
            let known = [
                "task", "kind", "name", "path", "event_id", "body", "sha256", "bytes", "reason",
                "backfill", "dry_run",
            ];
            if let Some(key) = doc.keys().find(|key| !known.contains(&key.as_str())) {
                return Err(format!(
                    "json: unknown field {}",
                    taskr_core::goflag::quote(key)
                ));
            }
            for key in ["kind", "name", "path", "sha256", "reason"] {
                if doc.get(key).is_some_and(serde_json::Value::is_null) {
                    doc.insert(key.into(), serde_json::json!(""));
                }
            }
            for key in ["task", "backfill", "dry_run"] {
                if doc.get(key).is_some_and(serde_json::Value::is_null) {
                    doc.remove(key);
                }
            }
        }
        if let Some(env) = object
            .get_mut("env")
            .and_then(serde_json::Value::as_object_mut)
        {
            for value in env.values_mut() {
                if value.is_null() {
                    *value = serde_json::json!("");
                }
            }
        }
        for key in ["argv", "capabilities"] {
            if let Some(values) = object
                .get_mut(key)
                .and_then(serde_json::Value::as_array_mut)
            {
                for value in values {
                    if value.is_null() {
                        *value = serde_json::json!("");
                    }
                }
            }
        }
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn rpc_flag_and_storage_contract() {
        let req=decode(br#"{"argv":[null,"status"],"cwd":null,"env":{"TASKR_TASK":null},"request_key":"null-request"}"#).unwrap();
        assert_eq!(req.argv, ["", "status"]);
        assert_eq!(req.cwd, "");
        assert_eq!(req.env["TASKR_TASK"], "");
        assert_eq!(
            decode(br#"{"machine":"forged"}"#).unwrap_err(),
            "json: unknown field \"machine\""
        );
        assert_eq!(
            decode(br#"{"document":{"forged":true}}"#).unwrap_err(),
            "json: unknown field \"forged\""
        );
        assert_eq!(
            decode(br#"{} {}"#).unwrap_err(),
            "trailing data after the JSON object"
        );

        assert!(stored(&argv(&["--json", "note", "once"])));
        assert!(stored(&argv(&["_hook", "claude"])));
        for args in [
            vec!["wait"],
            vec!["campaign", "1"],
            vec!["doc", "backfill"],
            vec!["note", "--help"],
            vec!["_host", "observe"],
            vec!["after", "--list", "--as", "1"],
        ] {
            assert!(!stored(&argv(&args)));
        }
        assert!(stored(&argv(&["note", "--", "--help"])));
        assert!(stored(&argv(&["after", "7", "--as", "1"])));
        assert!(stored(&argv(&["after", "--cancel", "3"])));
        assert_eq!(
            check_args(&argv(&["new", "x", "--role", "gate", "--role=gate"])),
            Err("repeated RPC flag --role".into())
        );
        assert!(
            check_args(&argv(&[
                "new", "x", "--report", "--brief", "--", "--report"
            ]))
            .is_err()
        );
        assert!(check_args(&argv(&["note", "--", "--role", "x", "--role", "y"])).is_ok());
        assert!(check_args(&argv(&["ready", "done", "--report=relative"])).is_err());
        // tmp is a read the hub answers; it never makes a caller's dir.
        assert!(!stored(&argv(&["--json", "tmp", "7"])));
        assert!(check_args(&argv(&["--json", "tmp", "7"])).is_ok());
        for clean in ["--clean-tmp", "-clean-tmp", "--clean-tmp=true"] {
            assert!(check_args(&argv(&["close", "7", clean])).is_err());
        }
        for mkdir in ["--mkdir", "-mkdir", "--mkdir=true"] {
            assert!(check_args(&argv(&["tmp", "7", mkdir])).is_err());
        }
    }
}
