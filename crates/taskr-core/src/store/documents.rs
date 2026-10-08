use super::*;
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};

const CAP: u64 = 1 << 20;
#[derive(Default, Debug)]
pub struct Input {
    pub path: String,
    pub host: String,
    pub hash: String,
    pub body: String,
    pub format: String,
    pub reason: String,
    pub bytes: Option<i64>,
}
pub fn body(bytes: &[u8], path: &str) -> Input {
    let mut d = Input {
        path: path.into(),
        hash: format!("{:x}", Sha256::digest(bytes)),
        bytes: Some(bytes.len() as i64),
        ..Input::default()
    };
    if bytes.len() as u64 > CAP {
        d.reason = "too_large".into();
    } else if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        d.reason = "binary".into();
    } else {
        d.body = String::from_utf8(bytes.to_vec()).expect("checked UTF8");
        d.format = match Path::new(path)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase()
            .as_str()
        {
            "md" | "markdown" => "md",
            _ => "text",
        }
        .into();
    }
    d
}
pub fn file(path: &str, host: &str) -> Input {
    let mut d = Input {
        path: path.into(),
        host: host.into(),
        reason: if host.is_empty() { "missing" } else { "client" }.into(),
        ..Input::default()
    };
    if !host.is_empty() {
        return d;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return d;
    };
    if !meta.is_file() {
        return d;
    }
    d.bytes = Some(meta.len() as i64);
    if meta.len() > CAP {
        d.reason = "too_large".into();
        return d;
    }
    let Ok(f) = std::fs::File::open(path) else {
        return d;
    };
    let mut bytes = Vec::new();
    if f.take(CAP + 1).read_to_end(&mut bytes).is_err() {
        return d;
    }
    if bytes.len() as u64 > CAP {
        d.reason = "too_large".into();
        d.bytes = Some(
            std::fs::metadata(path).map_or(bytes.len() as u64, |m| m.len().max(bytes.len() as u64))
                as i64,
        );
        return d;
    }
    body(&bytes, path)
}
#[derive(Debug)]
pub struct Document {
    pub id: i64,
    pub version: i64,
    pub captured: bool,
    pub hash: String,
    pub reason: String,
    pub path: String,
    pub host: String,
}
fn scan(r: &rusqlite::Row<'_>) -> rusqlite::Result<Document> {
    Ok(Document {
        id: r.get(0)?,
        version: r.get(1)?,
        captured: r.get(2)?,
        hash: r.get(3)?,
        reason: r.get(4)?,
        path: r.get(5)?,
        host: r.get(6)?,
    })
}
const COLS: &str = "id,version,captured,coalesce(sha256,''),coalesce(reason,''),coalesce(source_path,''),coalesce(source_host,'')";
pub fn store(
    db: &Connection,
    task: i64,
    kind: &str,
    name: &str,
    input: &Input,
    event: Option<i64>,
    backfill: i64,
) -> Result<(Document, bool)> {
    let old=db.query_row(&format!("select {COLS} from documents where task_id=? and kind=? and name=? order by version desc limit 1"),params![task,kind,name],scan).optional()?;
    if let Some(ref old) = old
        && (if input.reason.is_empty() {
            old.captured && old.hash == input.hash
        } else {
            !old.captured
                && old.reason == input.reason
                && old.path == input.path
                && old.host == input.host
        })
    {
        record_upload(task, kind, name, input, event, backfill);
        return Ok((old_to_owned(old), true));
    }
    if input.reason == "missing" || input.reason == "client" {
        let captured=db.query_row(&format!("select {COLS} from documents where task_id=? and kind=? and name=? and captured=1 order by version desc limit 1"),params![task,kind,name],scan).optional()?;
        if let Some(c) = captured
            && (input.reason == "missing" || (c.host == input.host && c.path == input.path))
        {
            record_upload(task, kind, name, input, event, backfill);
            return Ok((c, true));
        }
    }
    let root = root(db, task)?;
    if input.reason.is_empty() {
        db.execute(
            "insert or ignore into doc_blobs(sha256,bytes,body) values(?,?,?)",
            params![input.hash, input.bytes, input.body],
        )?;
    }
    let version = old.map_or(1, |d| d.version + 1);
    let at = now();
    db.execute("insert into documents(root_id,task_id,kind,name,version,sha256,bytes,format,captured,reason,source_path,source_host,event_id,backfill,created_at) values(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",params![root,task,kind,name,version,null(&input.hash),input.bytes,null(&input.format),input.reason.is_empty(),null(&input.reason),null(&input.path),null(&input.host),event,backfill,at])?;
    let id = db.last_insert_rowid();
    if input.reason.is_empty() {
        db.execute(
            "delete from search_fts where src='doc' and task_id=? and kind=? and name=?",
            params![task, kind, name],
        )?;
        db.execute("insert into search_fts(body,src,ref,kind,name,root_id,task_id,at) values(?,'doc',?,?,?,?,?,?)",params![input.body,id,kind,name,root,task,at])?;
    }
    record_upload(task, kind, name, input, event, backfill);
    Ok((
        db.query_row(
            &format!("select {COLS} from documents where id=?"),
            [id],
            scan,
        )?,
        false,
    ))
}
fn old_to_owned(d: &Document) -> Document {
    Document {
        id: d.id,
        version: d.version,
        captured: d.captured,
        hash: d.hash.clone(),
        reason: d.reason.clone(),
        path: d.path.clone(),
        host: d.host.clone(),
    }
}
pub fn capture(db: &Connection, f: impl FnOnce() -> Result<()>) -> Result<()> {
    if db.execute_batch("savepoint doc_capture").is_err() {
        return Ok(());
    }
    if f().is_err() {
        db.execute_batch("rollback to doc_capture; release doc_capture")?;
    } else {
        let _ = db.execute_batch("release doc_capture");
    }
    Ok(())
}
pub fn host(db: &Connection, id: i64) -> Result<String> {
    Ok(db.query_row("select coalesce(case when t.current_launch_id is null then t.machine else l.machine end,'') from tasks t left join launches l on l.id=t.current_launch_id where t.id=?",[id],|r|r.get(0))?)
}
fn report_path(db: &Connection, id: i64) -> Result<String> {
    Ok(db.query_row("select coalesce((select json_extract(data,'$.report') from events where task_id=t.id and kind='ready' order by id desc limit 1),t.report_path,'') from tasks t where id=?",[id],|r|r.get(0))?)
}
fn newest_report(db: &Connection, id: i64) -> Result<i64> {
    Ok(db.query_row(
        "select coalesce(max(id),0) from documents where task_id=? and kind='report'",
        [id],
        |r| r.get(0),
    )?)
}
pub struct Report {
    input: Input,
    latest: i64,
}
pub fn prepare_report(db: &Connection, id: i64, explicit: &str, ready: bool) -> Option<Report> {
    let path = if !explicit.is_empty() {
        explicit.into()
    } else if ready {
        db.query_row(
            "select coalesce(report_path,'') from tasks where id=?",
            [id],
            |r| r.get::<_, String>(0),
        )
        .ok()?
    } else {
        report_path(db, id).ok()?
    };
    if path.is_empty() {
        return None;
    }
    Some(Report {
        input: file(&path, &host(db, id).ok()?),
        latest: newest_report(db, id).ok()?,
    })
}
pub fn capture_report(db: &Connection, id: i64, event: i64, report: Option<&Report>) -> Result<()> {
    if let Some(r) = report {
        capture(db, || {
            if report_path(db, id)? == r.input.path
                && host(db, id)? == r.input.host
                && newest_report(db, id)? == r.latest
            {
                store(db, id, "report", "", &r.input, Some(event), 0)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}
pub fn capture_brief(db: &Connection, id: i64, input: Option<&Input>) -> Result<()> {
    if let Some(input) = input {
        capture(db, || {
            let kind = if task(db, id)?.parent.is_some() {
                "brief"
            } else {
                "goal"
            };
            store(db, id, kind, "", input, None, 0)?;
            Ok(())
        })?;
    }
    Ok(())
}
pub fn set(db: &mut Connection, id: i64, kind: &str, name: &str, input: &Input) -> Result<Value> {
    transaction(db, |tx| {
        let t = open_task(tx, id)?;
        if kind == "goal" && t.parent.is_some() {
            return Err(reject("goal is only accepted for a root task"));
        }
        let (d, same) = store(tx, id, kind, name, input, None, 0)?;
        if !same {
            let summary = if name.is_empty() {
                format!("{kind} v{}", d.version)
            } else {
                format!("{kind}/{name} v{}", d.version)
            };
            let eid = event(
                tx,
                Event {
                    task: id,
                    kind: "doc",
                    summary: &summary,
                    data: Some(
                        json!({"doc_id":d.id,"kind":kind,"name":name,"version":d.version,"sha256":input.hash,"bytes":input.bytes}),
                    ),
                    ..Event::default()
                },
            )?;
            tx.execute(
                "update documents set event_id=? where id=?",
                params![eid, d.id],
            )?;
        }
        Ok(json!({"ok":true,"action":"set","doc_id":d.id,"version":d.version,"same":same}))
    })
}

pub fn purge(db: &mut Connection, id: i64) -> Result<Value> {
    let out = transaction(db, |tx| {
        let (task, kind, name) = tx
            .query_row(
                "select task_id,kind,name from documents where id=?",
                [id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| reject(format!("document {id} does not exist")))?;
        let mut stmt=tx.prepare("select distinct sha256 from documents where task_id=? and kind=? and name=? and sha256 is not null")?;
        let hashes = stmt
            .query_map(params![task, kind, name], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        tx.execute_batch("pragma secure_delete=on")?;
        tx.execute(
            "delete from search_fts where src='doc' and task_id=? and kind=? and name=?",
            params![task, kind, name],
        )?;
        let removed = tx.execute(
            "delete from documents where task_id=? and kind=? and name=?",
            params![task, kind, name],
        )?;
        for hash in hashes {
            tx.execute("delete from doc_blobs where sha256=? and not exists(select 1 from documents where sha256=? and captured=1)",params![hash,hash])?;
        }
        let label = if name.is_empty() {
            kind
        } else {
            format!("{kind}/{name}")
        };
        event(
            tx,
            Event {
                task,
                kind: "doc",
                summary: &format!("purged {label} ({removed} versions)"),
                ..Event::default()
            },
        )?;
        Ok(json!({"ok":true,"action":"rm","doc_id":id,"removed":removed}))
    })?;
    let _ = db.busy_timeout(std::time::Duration::ZERO);
    let _ = db.execute_batch("pragma wal_checkpoint(truncate)");
    let _ = db.busy_timeout(std::time::Duration::from_secs(5));
    Ok(out)
}
struct Backfill {
    kind: String,
    name: String,
    input: Input,
    event: Option<i64>,
    backfill: i64,
}
fn prepare_backfill(db: &Connection, id: i64) -> Result<Vec<Backfill>> {
    let t = task(db, id)?;
    let brief: String = db.query_row(
        "select coalesce(brief_path,'') from tasks where id=?",
        [id],
        |r| r.get(0),
    )?;
    let host = host(db, id)?;
    let mut candidates = Vec::new();
    if !brief.is_empty() {
        candidates.push((
            brief.clone(),
            if t.parent.is_none() { "goal" } else { "brief" }.to_string(),
            String::new(),
        ));
    }
    let mut stmt=db.prepare("select distinct json_extract(data,'$.file') from events where task_id=? and kind='prompt' and json_extract(data,'$.file') is not null order by id")?;
    for path in stmt.query_map([id], |r| r.get::<_, String>(0))? {
        let path = path?;
        if !path.is_empty() && path != brief {
            let name = Path::new(&path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            candidates.push((path, "prompt".into(), name));
        }
    }
    let report = report_path(db, id)?;
    if !report.is_empty() {
        candidates.push((report, "report".into(), String::new()));
    }
    let mut files = Vec::new();
    for (path, kind, name) in candidates {
        let candidate_host = if kind == "brief" || kind == "prompt" {
            db.query_row("select coalesce(source_host,'') from documents where task_id=? and kind=? and name=? and captured=0 order by version desc limit 1",params![id,kind,name],|r|r.get::<_,String>(0)).optional()?.unwrap_or_else(||host.clone())
        } else {
            host.clone()
        };
        let input = file(&path, &candidate_host);
        let event = if kind == "report" || input.hash.is_empty() {
            None
        } else {
            db.query_row("select id from events where task_id=? and kind='prompt' and json_extract(data,'$.file')=? and json_extract(data,'$.sha256')=? order by id desc limit 1",params![id,input.path,input.hash],|r|r.get::<_,i64>(0)).optional()?
        };
        let backfill = if event.is_some() { 1 } else { 2 };
        files.push(Backfill {
            kind,
            name,
            input,
            event,
            backfill,
        });
    }
    Ok(files)
}
pub fn backfill(db: &mut Connection, tree: i64, dry: bool) -> Result<Value> {
    let ids = if tree == 0 {
        let mut stmt = db.prepare("select id from tasks order by id")?;
        stmt.query_map([], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        task(db, tree)?;
        let mut stmt=db.prepare("with recursive sub(id) as(select ? union all select t.id from tasks t join sub on t.parent_id=sub.id) select id from sub order by id")?;
        stmt.query_map([tree], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut counts =
        json!({"captured":0,"too_large":0,"binary":0,"missing":0,"client":0,"unchanged":0});
    for id in ids {
        let files = prepare_backfill(db, id)?;
        transaction(db, |tx| {
            if dry {
                tx.execute_batch("savepoint doc_dry_run")?;
            }
            for f in files {
                let input = &f.input;
                let skip:bool=tx.query_row("select exists(select 1 from documents where task_id=? and kind=? and name=? and(backfill=0 or(?!='' and captured=1)))",params![id,f.kind,f.name,input.reason],|r|r.get(0))?;
                let exists:bool=tx.query_row("select exists(select 1 from documents where task_id=? and kind=? and name=? and source_path=? and((?='' and captured=1 and sha256=?) or(?!='' and captured=0 and reason=?)))",params![id,f.kind,f.name,input.path,input.reason,input.hash,input.reason,input.reason],|r|r.get(0))?;
                let key = if skip || exists {
                    "unchanged"
                } else {
                    let (_, same) = store(tx, id, &f.kind, &f.name, input, f.event, f.backfill)?;
                    if same {
                        "unchanged"
                    } else if input.reason.is_empty() {
                        "captured"
                    } else {
                        &input.reason
                    }
                };
                counts[key] = json!(counts[key].as_i64().unwrap_or(0) + 1);
            }
            if dry {
                tx.execute_batch("rollback to doc_dry_run")?;
            }
            Ok(())
        })?;
    }
    Ok(counts)
}

fn record_upload(
    task: i64,
    kind: &str,
    name: &str,
    input: &Input,
    event: Option<i64>,
    backfill: i64,
) {
    use std::io::Write;
    let Some(context) = rpc_context() else {
        return;
    };
    if backfill != 0
        || input.reason != "client"
        || context.caller != input.host
        || !context.doc_upload
        || context.upload_file.as_os_str().is_empty()
    {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .open(&context.upload_file)
    {
        let want = json!({"task":task,"kind":kind,"name":name,"path":input.path,"event_id":event});
        let _ = writeln!(file, "{}", compact_json(&want).expect("upload JSON"));
    }
}
