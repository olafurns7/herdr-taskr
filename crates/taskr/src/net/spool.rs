use super::rpc::Client;
use super::{Error, Result, command, flag, reply_error, timestamp, valid_key};
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const UNKNOWN: &str =
    "outcome unknown: look for the record in the ledger and run the command again if it is missing";
fn mkdir(p: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
        .map_err(Error::io)
}
fn root(dir: &Path) -> PathBuf {
    dir.join("spool")
}
fn lock(dir: &Path, name: &str, nonblocking: bool) -> Result<Option<File>> {
    let root = root(dir);
    if nonblocking && !root.exists() {
        return Ok(None);
    }
    mkdir(&root)?;
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(root.join(name))
        .map_err(Error::io)?;
    match flock(
        &f,
        if nonblocking {
            FlockOperation::NonBlockingLockExclusive
        } else {
            FlockOperation::LockExclusive
        },
    ) {
        Ok(()) => Ok(Some(f)),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Ok(None),
        Err(e) => Err(Error::io(e.into())),
    }
}
fn sync(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(Error::io)
}
fn rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(Error::io)?;
    sync(to.parent().unwrap())?;
    if from.parent() != to.parent() {
        sync(from.parent().unwrap())?;
    }
    Ok(())
}
fn atomic(path: &Path, record: &Value) -> Result<()> {
    let dir = path.parent().unwrap();
    let tmp = dir.join(format!(".spool-{}.tmp", super::new_key()?));
    let run = || -> Result<()> {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(Error::io)?;
        writeln!(f, "{}", taskr_core::compact_json(record).unwrap()).map_err(Error::io)?;
        f.sync_all().map_err(Error::io)?;
        drop(f);
        fs::rename(&tmp, path).map_err(Error::io)?;
        sync(dir)
    };
    let result = run();
    let _ = fs::remove_file(tmp);
    result
}
fn seq(path: &Path) -> i64 {
    path.file_name()
        .unwrap()
        .to_string_lossy()
        .split('-')
        .next()
        .unwrap_or("")
        .parse()
        .unwrap_or(0)
}
fn files(folder: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(folder) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(Error::io(e)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(Error::io)?;
        if !entry.file_type().map_err(Error::io)?.is_dir()
            && entry.path().extension().is_some_and(|s| s == "json")
        {
            files.push(entry.path());
        }
    }
    files.sort_by_key(|p| (seq(p), p.clone()));
    Ok(files)
}
fn parsed_time(s: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}
fn read(path: &Path) -> Result<Value> {
    let b = fs::read(path).map_err(Error::io)?;
    let r: Value = serde_json::from_slice(&b)
        .map_err(|e| Error::new(taskr_core::ExitCode::Database, "database", e.to_string()))?;
    if r["v"] != 1 {
        return Err(Error::new(
            taskr_core::ExitCode::Database,
            "database",
            format!("unsupported spool version {}", r["v"]),
        ));
    }
    let key = r["request_key"].as_str().unwrap_or("");
    let stuck = r["stuck_since"].as_str().unwrap_or("");
    if r["seq"].as_i64().unwrap_or(0) <= 0
        || !valid_key(key)
        || !r["request"].is_object()
        || !r["request"]["argv"]
            .as_array()
            .is_some_and(|argv| argv.iter().all(Value::is_string))
        || r["request"]["request_key"] != key
        || parsed_time(r["queued_at"].as_str().unwrap_or("")).is_none()
        || (!stuck.is_empty() && parsed_time(stuck).is_none())
        || r.get("busy_since")
            .is_some_and(|v| v.as_str().and_then(parsed_time).is_none())
        || (stuck.is_empty()
            && (r["stuck_reason"].as_str().is_some_and(|s| !s.is_empty())
                || r["stuck_shown"] == true))
    {
        return Err(Error::new(
            taskr_core::ExitCode::Database,
            "database",
            "invalid spool record fields",
        ));
    }
    Ok(r)
}
fn quarantine(dir: &Path, folder: &str) -> Result<Vec<(PathBuf, Value)>> {
    let mut out = Vec::new();
    for p in files(&root(dir).join(folder))? {
        if let Ok(r) = read(&p) {
            out.push((p, r));
        } else {
            let bad = root(dir).join("bad");
            mkdir(&bad)?;
            let mut to = bad.join(p.file_name().unwrap());
            let mut suffix = 1;
            while to.symlink_metadata().is_ok() {
                to = bad.join(format!(
                    "{}-bad-{suffix}.json",
                    p.file_stem().unwrap().to_string_lossy()
                ));
                suffix += 1;
            }
            rename(&p, &to)?;
        }
    }
    out.sort_by_key(|(p, r)| (r["seq"].as_i64().unwrap(), p.clone()));
    Ok(out)
}
fn hash(req: &Value) -> Result<String> {
    let argv: Vec<String> =
        serde_json::from_value(req["argv"].clone()).map_err(|e| Error::usage(e.to_string()))?;
    let doc: Option<taskr_core::Document> = req
        .get("document")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(|e| Error::usage(e.to_string()))?;
    taskr_core::request_hash(Some(&argv), doc.as_ref()).map_err(|e| Error::usage(e.to_string()))
}
pub fn queue(
    dir: &Path,
    req: &Value,
    document: Option<&Value>,
    only_waiting: bool,
) -> Result<Option<usize>> {
    queue_mode(dir, req, document, only_waiting, false)
}
pub fn queue_mode(
    dir: &Path,
    req: &Value,
    document: Option<&Value>,
    only_waiting: bool,
    nonblocking: bool,
) -> Result<Option<usize>> {
    let queue = root(dir).join("queue");
    if only_waiting && !queue.exists() {
        return Ok(None);
    }
    if !valid_key(req["request_key"].as_str().unwrap_or(""))
        || req["argv"].as_array().is_none_or(|a| a.is_empty())
    {
        return Err(Error::usage("invalid spool request"));
    }
    if !only_waiting {
        for folder in ["queue", "refused", "bad"] {
            mkdir(&root(dir).join(folder))?;
        }
    }
    let Some(mut lock) = lock(dir, "lock", nonblocking)? else {
        return Err(Error::transport("spool lock is held", false, false));
    };
    let records = if nonblocking && only_waiting {
        let paths = files(&queue)?;
        if paths.is_empty() {
            return Ok(None);
        }
        let key = req["request_key"].as_str().unwrap();
        if paths.iter().any(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(&format!("-{key}.json"))
        }) {
            return Ok(Some(paths.len()));
        }
        paths.into_iter().map(|p| (p, Value::Null)).collect()
    } else {
        quarantine(dir, "queue")?
    };
    if only_waiting && records.is_empty() {
        return Ok(None);
    }
    for (_, r) in &records {
        if r["request_key"] == req["request_key"] {
            if hash(&r["request"])? != hash(req)? {
                return Err(Error::rejected(format!(
                    "request key {} belongs to another command",
                    req["request_key"].as_str().unwrap()
                )));
            }
            return Ok(Some(records.len()));
        }
    }
    if records.len() >= 500 {
        return Err(Error::transport(
            "server unreachable; spool full",
            false,
            false,
        ));
    }
    let size: u64 = files(&queue)?
        .iter()
        .map(|p| fs::metadata(p).map(|m| m.len()).map_err(Error::io))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .sum();
    let mut stored = String::new();
    lock.read_to_string(&mut stored).map_err(Error::io)?;
    let mut last = stored.trim().parse::<i64>().unwrap_or(0);
    for folder in ["queue", "refused", "bad"] {
        for p in files(&root(dir).join(folder))? {
            last = last.max(seq(&p));
        }
    }
    let n = last
        .checked_add(1)
        .ok_or_else(|| Error::usage("spool sequence overflow"))?;
    lock.set_len(0).map_err(Error::io)?;
    lock.seek(SeekFrom::Start(0)).map_err(Error::io)?;
    writeln!(lock, "{n}").map_err(Error::io)?;
    lock.sync_all().map_err(Error::io)?;
    let mut req = req.clone();
    req.as_object_mut().unwrap().remove("queued_at");
    let mut r = json!({"v":1,"seq":n,"request_key":req["request_key"],"request":req,"queued_at":timestamp()});
    if let Some(d) = document {
        r["document"] = d.clone();
    }
    if size + taskr_core::compact_json(&r).unwrap().len() as u64 + 1 > 20 << 20 {
        return Err(Error::transport(
            "server unreachable; spool full",
            false,
            false,
        ));
    }
    atomic(
        &queue.join(format!(
            "{n:06}-{}.json",
            r["request_key"].as_str().unwrap()
        )),
        &r,
    )?;
    Ok(Some(records.len() + 1))
}
fn head(dir: &Path) -> Result<Option<(PathBuf, Value)>> {
    let _lock = lock(dir, "lock", false)?;
    Ok(quarantine(dir, "queue")?.into_iter().next())
}
fn remove(dir: &Path, p: &Path) -> Result<()> {
    let _lock = lock(dir, "lock", false)?;
    match fs::remove_file(p) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(e)),
    };
    sync(p.parent().unwrap())
}
fn refuse(dir: &Path, p: &Path, mut r: Value, code: i64, message: &str) -> Result<()> {
    let _lock = lock(dir, "lock", false)?;
    if !p.exists() {
        return Ok(());
    }
    r["refused_at"] = json!(timestamp());
    if code != 0 {
        r["exit"] = json!(code);
    }
    if !message.is_empty() {
        r["error"] = json!(message);
    }
    atomic(p, &r)?;
    mkdir(&root(dir).join("refused"))?;
    rename(p, &root(dir).join("refused").join(p.file_name().unwrap()))
}
fn stuck(dir: &Path, p: &Path, message: &str) -> Result<Value> {
    let _lock = lock(dir, "lock", false)?;
    let mut r = read(p)?;
    if r["stuck_since"].as_str().is_none_or(|s| s.is_empty()) {
        r["stuck_since"] = json!(timestamp());
    }
    r["stuck_reason"] = json!(message);
    atomic(p, &r)?;
    Ok(r)
}
fn http_stuck(e: &Error) -> bool {
    [401, 403, 408, 429]
        .into_iter()
        .any(|s| e.message.starts_with(&format!("server answered {s}:")))
}
fn busy(dir: &Path, p: &Path, keep: bool) -> Result<Value> {
    let _lock = lock(dir, "lock", false)?;
    let mut r = read(p)?;
    if keep && r.get("busy_since").is_none() {
        r["busy_since"] = json!(timestamp());
        atomic(p, &r)?;
    } else if !keep && r.as_object_mut().unwrap().remove("busy_since").is_some() {
        atomic(p, &r)?;
    }
    Ok(r)
}
fn transient(e: &Error) -> bool {
    e.kind == "transport"
        || http_stuck(e)
        || e.message
            .strip_prefix("server answered ")
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.parse::<u16>().ok())
            .is_some_and(|s| s >= 500)
}
pub fn send(dir: &Path, raw: &str) -> Result<usize> {
    let mut sent = 0;
    send_count(dir, raw, &mut sent)?;
    Ok(sent)
}
#[derive(Debug, PartialEq)]
enum Outcome {
    Keep,
    Unknown,
    Refuse,
    Remove,
    Delivered,
}
fn outcome(argv: &[String], rep: &Value) -> Outcome {
    if taskr_core::db::busy_reply(rep["exit"].as_i64().unwrap_or(-1), &reply_error(rep)) {
        Outcome::Keep
    } else if rep["exit"] == 5
        && rep["stdout"]
            .as_str()
            .unwrap_or("")
            .contains("outcome unknown (still running")
    {
        Outcome::Unknown
    } else if command(argv).0 == "_hook"
        && rep["exit"] == 0
        && rep["stdout"].as_str().unwrap_or("").trim() == "expired"
    {
        Outcome::Remove
    } else if rep["exit"] != 0 {
        Outcome::Refuse
    } else {
        Outcome::Delivered
    }
}
fn send_count(dir: &Path, raw: &str, sent: &mut usize) -> Result<()> {
    if files(&root(dir).join("queue"))?.is_empty() {
        return Ok(());
    }
    let Some(_sender) = lock(dir, "send.lock", true)? else {
        return Ok(());
    };
    let Some(mut first) = head(dir)? else {
        return Ok(());
    };
    let cl = Client::new(raw)?;
    loop {
        let (p, r) = first;
        let mut req = r["request"].clone();
        req["queued_at"] = r["queued_at"].clone();
        let age = (super::now() - parsed_time(r["queued_at"].as_str().unwrap()).unwrap())
            .whole_milliseconds()
            .max(0);
        if age != 0 {
            req["queued_age_ms"] = json!(age as i64);
        } else {
            req.as_object_mut().unwrap().remove("queued_age_ms");
        }
        let argv: Vec<String> =
            serde_json::from_value(req["argv"].clone()).map_err(|e| Error::usage(e.to_string()))?;
        match cl.call(
            &req,
            super::rpc::budget(&argv) + Duration::from_secs(10),
            true,
        ) {
            Err(e) => {
                if e.retryable || e.kind == "transport" {
                    return Ok(());
                }
                if http_stuck(&e) {
                    stuck(dir, &p, &e.message)?;
                    return Ok(());
                }
                if transient(&e) {
                    return Ok(());
                }
                refuse(dir, &p, r, e.code as i64, &e.message)?;
            }
            Ok(rep) => {
                let outcome = outcome(&argv, &rep);
                let r = busy(dir, &p, outcome == Outcome::Keep)?;
                match outcome {
                    Outcome::Keep => {
                        if super::now() - parsed_time(r["busy_since"].as_str().unwrap()).unwrap()
                            < time::Duration::minutes(10)
                        {
                            log(dir, "spool head waiting: hub database is locked");
                            return Ok(());
                        }
                        refuse(
                            dir,
                            &p,
                            r,
                            rep["exit"].as_i64().unwrap(),
                            &reply_error(&rep),
                        )?;
                    }
                    Outcome::Unknown => {
                        let r = stuck(dir, &p, UNKNOWN)?;
                        if super::now() - parsed_time(r["stuck_since"].as_str().unwrap()).unwrap()
                            < time::Duration::minutes(10)
                        {
                            return Ok(());
                        }
                        refuse(dir, &p, r, 5, UNKNOWN)?;
                    }
                    Outcome::Remove => {
                        remove(dir, &p)?;
                    }
                    Outcome::Refuse => {
                        refuse(
                            dir,
                            &p,
                            r,
                            rep["exit"].as_i64().unwrap(),
                            &reply_error(&rep),
                        )?;
                    }
                    Outcome::Delivered => {
                        match super::doc::uploads(
                            &cl,
                            &rep,
                            req["cwd"].as_str().unwrap_or(""),
                            &req["env"],
                            r.get("document"),
                        ) {
                            Ok(()) => {
                                remove(dir, &p)?;
                                *sent += 1;
                            }
                            Err(e)
                                if e.message.starts_with("document upload refused:")
                                    || !transient(&e) =>
                            {
                                remove(dir, &p)?;
                            }
                            Err(e) => {
                                if http_stuck(&e) {
                                    stuck(dir, &p, &e.message)?;
                                }
                                return Err(e);
                            }
                        }
                    }
                }
            }
        }
        if let Some(next) = head(dir)? {
            first = next;
        } else {
            return Ok(());
        }
    }
}
fn task(req: &Value) -> i64 {
    if let Some(t) = req["env"]["TASKR_TASK"]
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|t| *t > 0)
    {
        return t;
    }
    let argv: Vec<String> = serde_json::from_value(req["argv"].clone()).unwrap_or_default();
    let (cmd, args) = command(&argv);
    if matches!(cmd, "note" | "decide") {
        return flag(args, "as")
            .and_then(|(s, _, _)| s.parse().ok())
            .unwrap_or(0);
    }
    if matches!(cmd, "next" | "close") {
        return args
            .iter()
            .find(|s| !s.starts_with('-'))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
    }
    0
}
fn age_text(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds >= 3600 {
        format!("{}h{}m{}s", seconds / 3600, seconds / 60 % 60, seconds % 60)
    } else if seconds >= 60 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}
// Go serializes these struct fields in declaration order.
#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct ListItem {
    seq: i64,
    name: String,
    age: String,
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<i64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    error: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    stuck_since: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    stuck_reason: String,
}
fn listing_json(value: Value) -> String {
    let items: std::collections::BTreeMap<String, Vec<ListItem>> =
        serde_json::from_value(value).unwrap();
    taskr_core::escape_json(&serde_json::to_string(&items).unwrap())
}
fn listing(dir: &Path) -> Result<Value> {
    let mut out = json!({"queued":[],"refused":[],"bad":[]});
    if !root(dir).exists() {
        return Ok(out);
    }
    let lock = lock(dir, "lock", false)?;
    let queue = quarantine(dir, "queue")?;
    let refused = quarantine(dir, "refused")?;
    drop(lock);
    for (key, records) in [("queued", queue), ("refused", refused)] {
        for (p, r) in records {
            let argv: Vec<String> =
                serde_json::from_value(r["request"]["argv"].clone()).unwrap_or_default();
            let age = (super::now() - parsed_time(r["queued_at"].as_str().unwrap()).unwrap())
                .whole_nanoseconds()
                .max(0);
            let mut item = json!({"seq":r["seq"],"name":p.file_name().unwrap().to_string_lossy(),"age":age_text(((age+500_000_000)/1_000_000_000) as i64),"command":command(&argv).0});
            let t = task(&r["request"]);
            if t != 0 {
                item["task"] = json!(t);
            }
            for field in ["stuck_since", "stuck_reason"] {
                if let Some(s) = r[field].as_str()
                    && !s.is_empty()
                {
                    item[field] = json!(s);
                }
            }
            if key == "refused"
                && let Some(s) = r["error"].as_str()
                && !s.is_empty()
            {
                item["error"] = json!(s);
            }
            out[key].as_array_mut().unwrap().push(item);
        }
    }
    for p in files(&root(dir).join("bad"))? {
        let mut item = json!({"seq":seq(&p),"name":p.file_name().unwrap().to_string_lossy(),"kind":"bad","age":"","command":""});
        if let Err(e) = read(&p) {
            item["error"] = json!(e.message);
        }
        out["bad"].as_array_mut().unwrap().push(item);
    }
    Ok(out)
}
fn rm(dir: &Path, target: &str) -> Result<()> {
    let n = target.parse::<i64>().ok();
    let error = || Error::rejected(format!("spool item {target} does not exist"));
    if !root(dir).exists() {
        return Err(error());
    }
    let _lock = lock(dir, "lock", false)?;
    for folder in ["queue", "refused", "bad"] {
        for p in files(&root(dir).join(folder))? {
            if n.is_some_and(|n| n > 0 && seq(&p) == n)
                || n.is_none() && p.file_name().unwrap() == target
            {
                fs::remove_file(&p).map_err(Error::io)?;
                if folder == "bad" {
                    let _ = fs::remove_file(format!("{}.shown", p.display()));
                }
                return sync(p.parent().unwrap());
            }
        }
    }
    Err(error())
}
pub fn dispatch(
    json_mode: bool,
    args: &[String],
    dir: &Path,
    raw: Option<&str>,
) -> taskr_core::ExitCode {
    let mut f = taskr_core::goflag::FlagSet::new("spool", json_mode);
    let mut run = || -> Result<String> {
        f.parse(args, 1, 2).map_err(Error::usage)?;
        if f.help() {
            return Ok(String::new());
        }
        let pos = &f.positional;
        match pos[0].as_str() {
            "ls" => {
                if pos.len() != 1 {
                    return Err(Error::usage("spool ls takes no arguments"));
                }
                listing(dir).map(listing_json)
            }
            "send" => {
                if pos.len() != 1 {
                    return Err(Error::usage("spool send takes no arguments"));
                }
                let raw = raw
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| Error::usage("spool send is only available on a client host"))?;
                let mut sent = 0;
                let result = send_count(dir, raw, &mut sent);
                let body = json!({"ok":result.is_ok(),"sent":sent,"queued":files(&root(dir).join("queue")).map_or(0, |f| f.len()),"refused":files(&root(dir).join("refused")).map_or(0, |f| f.len())});
                match result {
                    Ok(()) => Ok(taskr_core::compact_json(&body).unwrap()),
                    Err(mut e) => {
                        e.body = Some(body);
                        Err(e)
                    }
                }
            }
            "rm" => {
                if pos.len() != 2 {
                    return Err(Error::usage("spool rm needs SEQ or FILE"));
                }
                let t = &pos[1];
                if t.parse::<i64>().is_ok_and(|n| n <= 0)
                    || t.parse::<i64>().is_err()
                        && (Path::new(t).file_name().is_none_or(|s| s != t.as_str())
                            || !t.ends_with(".json"))
                {
                    return Err(Error::usage(
                        "spool rm needs a positive sequence or file name",
                    ));
                }
                rm(dir, t)?;
                Ok(taskr_core::compact_json(
                    &json!({"ok":true,"removed":t.parse::<i64>().map_or(json!(t),|n|json!(n))}),
                )
                .unwrap())
            }
            _ => Err(Error::usage("spool: expected ls, send or rm SEQ|FILE")),
        }
    };
    let result = run();
    if f.help() {
        print!("{}", f.usage(&crate::cli::usage_line("spool")));
        return taskr_core::ExitCode::Ok;
    }
    match result {
        Ok(v) => {
            println!("{}{}", if f.json() { "" } else { "j1 " }, v);
            taskr_core::ExitCode::Ok
        }
        Err(e) => e.emit(f.json(), "spool"),
    }
}

pub fn waiting(dir: &Path) -> bool {
    files(&root(dir).join("queue")).is_ok_and(|f| !f.is_empty())
}

pub fn summary(dir: &Path) -> Value {
    let count = |folder| files(&root(dir).join(folder)).map_or(0, |f| f.len());
    let stuck = files(&root(dir).join("queue"))
        .unwrap_or_default()
        .iter()
        .filter_map(|p| read(p).ok())
        .next()
        .is_some_and(|r| r["stuck_since"].as_str().is_some_and(|s| !s.is_empty()));
    json!({"queued":count("queue"),"refused":count("refused"),"bad":count("bad"),"stuck":stuck})
}
fn truncate(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    } else {
        s
    }
}
fn notice(sock: &str, title: &str, body: &str) -> bool {
    let body = truncate(body, 120);
    crate::write::herdr::command(
        sock,
        &[
            "notification",
            "show",
            title,
            "--body",
            &body,
            "--sound",
            "request",
        ],
        Duration::from_secs(10),
    )
    .is_ok_and(|out| out.code == Some(0))
}
pub fn log(dir: &Path, message: &str) {
    static LAST: std::sync::Mutex<Option<(PathBuf, std::time::Instant)>> =
        std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if last
        .as_ref()
        .is_some_and(|(p, t)| p == dir && t.elapsed() < Duration::from_secs(60))
    {
        return;
    }
    *last = Some((dir.to_path_buf(), std::time::Instant::now()));
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o644)
        .open(dir.join("daemon.log"))
    {
        if f.metadata().is_ok_and(|m| m.len() > 1 << 20) {
            let _ = f.set_len(0);
        }
        let stamp = super::now()
            .format(
                &time::format_description::parse_borrowed::<2>(
                    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
                )
                .unwrap(),
            )
            .unwrap();
        let _ = writeln!(f, "{stamp} {message}");
    }
}

pub fn notify(dir: &Path, sock: &str) -> Result<()> {
    let refused = {
        let _lock = lock(dir, "lock", false)?;
        quarantine(dir, "refused")?
    };
    for (p, mut r) in refused {
        if r["shown"] == true {
            continue;
        }
        let argv: Vec<String> =
            serde_json::from_value(r["request"]["argv"].clone()).unwrap_or_default();
        let body = format!(
            "taskr: queued {} for task {} was refused: {}",
            command(&argv).0,
            task(&r["request"]),
            r["error"].as_str().unwrap_or("")
        );
        if !notice(sock, "taskr: queued record refused", &body) {
            continue;
        }
        let _lock = lock(dir, "lock", false)?;
        if p.exists() {
            r["shown"] = json!(true);
            atomic(&p, &r)?;
        }
    }
    if let Some((p, r)) = head(dir)? {
        let reason = r["stuck_reason"].as_str().unwrap_or("");
        let status = Error::usage(reason);
        if r["stuck_shown"] != true
            && http_stuck(&status)
            && parsed_time(r["stuck_since"].as_str().unwrap_or(""))
                .is_some_and(|t| super::now() - t >= time::Duration::minutes(10))
        {
            let argv: Vec<String> =
                serde_json::from_value(r["request"]["argv"].clone()).unwrap_or_default();
            let body = format!(
                "taskr: queued {} for task {} has been stuck: {reason}",
                command(&argv).0,
                task(&r["request"])
            );
            if notice(sock, "taskr: queued record is stuck", &body) {
                let _lock = lock(dir, "lock", false)?;
                if let Ok(mut latest) = read(&p)
                    && latest["stuck_shown"] != true
                    && latest["stuck_since"] == r["stuck_since"]
                    && latest["stuck_reason"] == r["stuck_reason"]
                {
                    latest["stuck_shown"] = json!(true);
                    atomic(&p, &latest)?;
                }
            }
        }
    }
    let bad = {
        let _lock = lock(dir, "lock", false)?;
        files(&root(dir).join("bad"))?
    };
    for p in bad {
        let marker = PathBuf::from(format!("{}.shown", p.display()));
        if marker.exists() {
            continue;
        }
        let error = read(&p).err().map_or(String::new(), |e| e.message);
        let body = format!(
            "taskr: queued record {} is bad: {error}",
            p.file_name().unwrap().to_string_lossy()
        );
        if !notice(sock, "taskr: queued record is bad", &body) {
            continue;
        }
        let _lock = lock(dir, "lock", false)?;
        if p.exists() {
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&marker)
            {
                Ok(_) => sync(p.parent().unwrap())?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(Error::io(e)),
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reply_outcomes_keep_only_busy_database_errors() {
        let argv = ["note", "progress"].map(String::from);
        for stdout in [
            "x1 4 {\"err\":\"database is locked (5) (SQLITE_BUSY)\",\"k\":\"database\"}\n",
            "{\"error\":\"database is locked (517)\",\"kind\":\"database\"}\n",
        ] {
            assert_eq!(
                outcome(&argv, &json!({"exit":4,"stdout":stdout})),
                Outcome::Keep
            );
        }
        assert_eq!(
            outcome(
                &argv,
                &json!({"exit":4,"stdout":"x1 4 {\"err\":\"database table is locked (6)\"}\n"})
            ),
            Outcome::Refuse
        );
        assert_eq!(
            outcome(
                &argv,
                &json!({"exit":4,"stdout":"{\"error\":\"disk I/O error (10)\"}\n"})
            ),
            Outcome::Refuse
        );
        assert_eq!(
            outcome(
                &argv,
                &json!({"exit":5,"stdout":"outcome unknown (still running)"})
            ),
            Outcome::Unknown
        );
        assert_eq!(
            outcome(
                &["--json", "_hook"].map(String::from),
                &json!({"exit":0,"stdout":"expired\n"})
            ),
            Outcome::Remove
        );
        assert_eq!(
            outcome(&argv, &json!({"exit":0,"stdout":"ok\n"})),
            Outcome::Delivered
        );
    }
    #[test]
    fn malformed_argv_head_is_quarantined_and_next_record_survives() {
        let dir = std::env::temp_dir().join(format!(
            "taskr-badargv-{}",
            super::super::new_key().unwrap()
        ));
        let req = super::super::rpc::request(
            &["note", "valid"].map(String::from),
            "/tmp/client",
            "badargv-request-1",
            json!({}),
            None,
        );
        queue(&dir, &req, None, false).unwrap();
        let first = files(&root(&dir).join("queue")).unwrap().remove(0);
        let mut bad = read(&first).unwrap();
        bad["request"]["argv"] = json!(["note", 7]);
        atomic(&first, &bad).unwrap();
        let mut good = req.clone();
        good["request_key"] = json!("badargv-request-2");
        queue(&dir, &good, None, false).unwrap();
        assert_eq!(
            head(&dir).unwrap().unwrap().1["request"]["argv"],
            good["argv"]
        );
        assert_eq!(files(&root(&dir).join("queue")).unwrap().len(), 1);
        assert_eq!(files(&root(&dir).join("bad")).unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn listing_uses_go_struct_field_order() {
        assert_eq!(
            listing_json(
                json!({"queued":[{"seq":1,"name":"a","age":"0s","command":"note","task":2}],"refused":[],"bad":[]})
            ),
            r#"{"bad":[],"queued":[{"seq":1,"name":"a","age":"0s","command":"note","task":2}],"refused":[]}"#
        );
    }
    #[test]
    fn durable_queue_hashes_permissions_and_sender_lock() {
        let dir = std::env::temp_dir().join(format!(
            "taskr-net-unit-{}",
            super::super::new_key().unwrap()
        ));
        mkdir(&dir).unwrap();
        let req = super::super::rpc::request(
            &["note", "<&> Þ😀", "--as", "1"].map(String::from),
            "/tmp/client",
            "unit-request-0001",
            json!({}),
            None,
        );
        assert_eq!(queue(&dir, &req, None, true).unwrap(), None);
        assert_eq!(queue(&dir, &req, None, false).unwrap(), Some(1));
        assert_eq!(queue(&dir, &req, None, false).unwrap(), Some(1));
        let mut other = req.clone();
        other["argv"][1] = json!("different");
        assert_eq!(
            queue(&dir, &other, None, false).unwrap_err().code,
            taskr_core::ExitCode::Rejected
        );
        let entries = files(&root(&dir).join("queue")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&entries[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(read(&entries[0]).unwrap()["request"], req);
        assert_eq!(fs::read_to_string(root(&dir).join("lock")).unwrap(), "1\n");
        let held = lock(&dir, "send.lock", true).unwrap().unwrap();
        assert!(lock(&dir, "send.lock", true).unwrap().is_none());
        drop(held);
        assert!(lock(&dir, "send.lock", true).unwrap().is_some());
        fs::write(root(&dir).join("queue/000002-bad.json"), b"{truncated").unwrap();
        let _lock = lock(&dir, "lock", false).unwrap();
        assert_eq!(quarantine(&dir, "queue").unwrap().len(), 1);
        assert_eq!(
            fs::read(root(&dir).join("bad/000002-bad.json")).unwrap(),
            b"{truncated"
        );
        drop(_lock);
        fs::remove_dir_all(dir).unwrap();
    }
}
