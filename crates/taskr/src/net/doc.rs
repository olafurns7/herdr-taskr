use super::rpc::{Client, request};
use super::{Error, Result, flag, new_key};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, time::Duration};

pub fn capture(want: &Value) -> Value {
    let path = want["path"].as_str().unwrap_or("");
    let mut p = json!({"task":want["task"],"kind":want["kind"],"name":want["name"].as_str().unwrap_or(""),"path":path,"event_id":want.get("event_id").unwrap_or(&Value::Null)});
    p["reason"] = json!("missing");
    if let Ok(meta) = std::fs::metadata(path) {
        if !meta.is_file() {
            return p;
        }
        p["bytes"] = json!(meta.len());
        if meta.len() > 1 << 20 {
            p["reason"] = json!("too_large");
            return p;
        }
        if let Ok(f) = File::open(path) {
            let mut b = Vec::new();
            if f.take((1 << 20) + 1).read_to_end(&mut b).is_ok() {
                p["bytes"] = json!(b.len());
                if b.len() > 1 << 20 {
                    p["reason"] = json!("too_large");
                    return p;
                }
                p["sha256"] = json!(format!("{:x}", Sha256::digest(&b)));
                if std::str::from_utf8(&b).is_err() || b.contains(&0) {
                    p["reason"] = json!("binary");
                } else {
                    p.as_object_mut().unwrap().remove("reason");
                    p["body"] = json!(STANDARD.encode(b));
                }
            }
        }
    }
    p
}
pub fn set_payload(args: &[String], json_mode: bool) -> Result<Option<Value>> {
    if args.first().map(String::as_str) != Some("set") {
        return Ok(None);
    }
    let mut f = taskr_core::goflag::FlagSet::new("doc set", json_mode);
    f.string("file", "", "").string("name", "", "");
    if f.parse(&args[1..], 2, 2).is_err() {
        return Ok(None);
    }
    let task = f.positional[0].parse::<i64>().unwrap_or(0);
    let kind = &f.positional[1];
    let name = f.get_string("name");
    if task <= 0
        || !matches!(kind.as_str(), "goal" | "plan")
        || (kind == "goal" && f.was_set("name"))
        || f.get_string("file").is_empty()
        || (f.was_set("name") && !valid_name(name))
    {
        return Ok(None);
    }
    let p = capture(&json!({"task":task,"kind":kind,"name":name,"path":f.get_string("file")}));
    if let Some(reason) = p["reason"].as_str() {
        let path = f.get_string("file");
        return Err(Error::usage(if reason == "missing" {
            format!("document file is missing or unreadable: {path}")
        } else {
            format!("document file: {reason} ({path})")
        }));
    }
    Ok(Some(p))
}
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .enumerate()
            .all(|(i, c)| c.is_ascii_alphanumeric() || i > 0 && matches!(c, b'_' | b'.' | b'-'))
}
pub fn ready_payload(args: &[String], env: &Value) -> Option<Value> {
    let (path, _, _) = flag(args, "report")?;
    let task = env["TASKR_TASK"].as_str()?.parse::<i64>().ok()?;
    if path.is_empty() || task <= 0 {
        return None;
    }
    Some(capture(
        &json!({"task":task,"kind":"report","name":"","path":path}),
    ))
}
pub fn uploads(
    cl: &Client,
    rep: &Value,
    cwd: &str,
    env: &Value,
    saved: Option<&Value>,
) -> Result<()> {
    for want in rep["upload"].as_array().into_iter().flatten() {
        let mut p = if let Some(s) = saved.filter(|s| {
            ["task", "kind", "name", "path"]
                .into_iter()
                .all(|k| s[k] == want[k])
        }) {
            s.clone()
        } else {
            capture(want)
        };
        p["event_id"] = want.get("event_id").cloned().unwrap_or(Value::Null);
        let argv = ["--json", "_doc", "put"].map(String::from);
        let r = request(&argv, cwd, &new_key()?, env.clone(), Some(p));
        let put = cl.call(&r, Duration::from_secs(5), false)?;
        if put["exit"] != 0 {
            return Err(Error::rejected(format!(
                "document upload refused: {}",
                super::reply_error(&put)
            )));
        }
    }
    Ok(())
}

pub fn backfill(
    cl: &Client,
    args: &[String],
    json_mode: bool,
    cwd: &str,
    env: &Value,
) -> Result<super::ExitCode> {
    let mut f = taskr_core::goflag::FlagSet::new("doc backfill", json_mode);
    f.int("tree", 0, "only this task and descendants").bool(
        "dry-run",
        false,
        "count without writing",
    );
    f.parse(args, 0, 0).map_err(Error::usage)?;
    if f.get_int("tree") < 0 {
        return Err(Error::usage("--tree must be a positive task id"));
    }
    let mut counts =
        json!({"captured":0,"too_large":0,"binary":0,"missing":0,"client":0,"unchanged":0});
    let mut offset = 0;
    loop {
        let mut argv = vec![
            "--json".into(),
            "_doc".into(),
            "wanted".into(),
            "--offset".into(),
            format!("{offset}"),
        ];
        if f.get_int("tree") != 0 {
            argv.extend(["--tree".into(), format!("{}", f.get_int("tree"))]);
        }
        let req = request(&argv, cwd, &new_key()?, env.clone(), None);
        let rep = super::call_retry(cl, &req, f.json(), "taskr doc backfill")?;
        if rep["exit"] != 0 {
            return Err(Error::new(
                super::exit(rep["exit"].as_i64().unwrap()),
                "rejected",
                rep["stdout"]
                    .as_str()
                    .unwrap()
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or(""),
            ));
        }
        let page: Value = serde_json::from_str(
            rep["stdout"]
                .as_str()
                .unwrap()
                .trim()
                .lines()
                .last()
                .unwrap_or(""),
        )
        .map_err(|_| {
            Error::new(
                super::ExitCode::Database,
                "database",
                "invalid _doc wanted reply",
            )
        })?;
        counts["unchanged"] =
            json!(counts["unchanged"].as_i64().unwrap() + page["unchanged"].as_i64().unwrap_or(0));
        for want in page["documents"].as_array().into_iter().flatten() {
            let mut p = capture(want);
            p["backfill"] = json!(true);
            if f.get_bool("dry-run") {
                p["dry_run"] = json!(true);
            }
            let argv = ["--json", "_doc", "put"].map(String::from);
            let put = cl.call(
                &request(&argv, cwd, &new_key()?, env.clone(), Some(p)),
                Duration::from_secs(5),
                false,
            )?;
            if put["exit"] != 0 {
                return Err(Error::new(
                    super::exit(put["exit"].as_i64().unwrap()),
                    "rejected",
                    super::reply_error(&put),
                ));
            }
            let v: Value = serde_json::from_str(
                put["stdout"]
                    .as_str()
                    .unwrap()
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or(""),
            )
            .map_err(|_| {
                Error::new(
                    super::ExitCode::Database,
                    "database",
                    "invalid _doc put reply",
                )
            })?;
            let key = v["count"].as_str().unwrap_or("");
            let n = counts.get(key).and_then(Value::as_i64).ok_or_else(|| {
                Error::new(
                    super::ExitCode::Database,
                    "database",
                    "invalid _doc put count",
                )
            })?;
            counts[key] = json!(n + 1);
        }
        if page["more"] != true {
            break;
        }
        offset = page["offset"].as_i64().unwrap_or(0);
    }
    crate::cli::emit(f.json(), &counts, false);
    Ok(super::ExitCode::Ok)
}
