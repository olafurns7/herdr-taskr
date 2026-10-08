use super::protocol::{DocWant, RpcRequest};
use base64::Engine;
use serde_json::{Value, json};
use taskr_core::{
    Document,
    db::{self, OptionalExtension},
    store::{self, documents as docs},
};

pub(super) fn input(p: &Document) -> store::Result<docs::Input> {
    if p.task <= 0
        || !["brief", "prompt", "report", "handover", "goal", "plan"].contains(&p.kind.as_str())
        || !std::path::Path::new(&p.path).is_absolute()
    {
        return Err(store::reject(
            "document task, kind and absolute path are required",
        ));
    }
    if p.kind == "goal" && !p.name.is_empty()
        || !p.name.is_empty()
            && (p.name.len() > 64
                || !p.name.as_bytes()[0].is_ascii_lowercase()
                    && !p.name.as_bytes()[0].is_ascii_digit()
                || !p
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)))
    {
        return Err(store::reject("invalid document name"));
    }
    if p.event_id.is_some_and(|id| id <= 0) {
        return Err(store::reject("event_id must be positive"));
    }
    if !p.sha256.is_empty()
        && (p.sha256.len() != 64 || !p.sha256.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(store::reject("invalid document sha256"));
    }
    if p.bytes.is_some_and(|n| n < 0) {
        return Err(store::reject("invalid document byte count"));
    }
    let mut input = if let Some(body) = &p.body {
        if !p.reason.is_empty() || p.sha256.is_empty() {
            return Err(store::reject("document body needs a sha256 and no reason"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(body.replace(['\r', '\n'], ""))
            .map_err(|_| store::reject("document body is not base64"))?;
        let input = docs::body(&bytes, &p.path);
        if !input.reason.is_empty()
            || input.hash != p.sha256
            || p.bytes.is_some_and(|n| n != bytes.len() as i64)
        {
            return Err(store::reject(
                "document body failed size, text or sha256 validation",
            ));
        }
        input
    } else {
        if !["missing", "too_large", "binary"].contains(&p.reason.as_str()) {
            return Err(store::reject(
                "document body or a supported miss reason is required",
            ));
        }
        if p.reason == "too_large" && p.bytes.is_none_or(|n| n <= 1 << 20) {
            return Err(store::reject("too_large document is below the size cap"));
        }
        if p.reason == "binary" && (p.bytes.is_none_or(|n| n > 1 << 20) || p.sha256.is_empty()) {
            return Err(store::reject("binary document needs its size and sha256"));
        }
        docs::Input {
            path: p.path.clone(),
            hash: p.sha256.clone(),
            bytes: p.bytes,
            reason: p.reason.clone(),
            ..docs::Input::default()
        }
    };
    input.host = store::caller_machine().unwrap_or_default();
    Ok(input)
}

pub(super) fn put(req: &RpcRequest, args: &[String]) -> store::Result<Value> {
    let mut flags = taskr_core::goflag::FlagSet::new("_doc put", false);
    flags.parse(args, 0, 0).map_err(store::usage)?;
    let p = req
        .document
        .as_ref()
        .ok_or_else(|| store::usage("_doc put needs a document payload"))?;
    let input = input(p)?;
    if p.reason.is_empty() && p.body.is_none() || p.body.is_some() && !p.reason.is_empty() {
        return Err(store::reject("document upload body and reason conflict"));
    }
    let mut db = super::child::open()?;
    store::transaction(&mut db, |tx| {
        store::task(tx, p.task)?;
        if let Some(id) = p.event_id {
            let belongs: bool = tx.query_row(
                "select exists(select 1 from events where id=? and task_id=?)",
                db::params![id, p.task],
                |r| r.get(0),
            )?;
            if !belongs {
                return Err(store::reject(format!(
                    "event {id} does not belong to task {}",
                    p.task
                )));
            }
        }
        let (backfill, event) = if p.backfill {
            let candidates = candidates(tx, p.task)?;
            let Some(candidate) = candidates
                .iter()
                .find(|c| c.want.kind == p.kind && c.want.name == p.name && c.want.path == p.path)
            else {
                return Err(store::reject("document is not a backfill candidate"));
            };
            if candidate.host != input.host {
                return Err(store::reject("backfill task host changed"));
            }
            let event = if p.kind == "report" || input.hash.is_empty() {
                None
            } else {
                tx.query_row("select id from events where task_id=? and kind='prompt' and json_extract(data,'$.file')=? and json_extract(data,'$.sha256')=? order by id desc limit 1", db::params![p.task,p.path,input.hash], |r| r.get::<_,i64>(0)).optional()?
            };
            if let Some(id) = p.event_id
                && Some(id) != event
            {
                return Err(store::reject(format!(
                    "event {} does not match the backfill document",
                    id
                )));
            }
            if skipped(tx, &candidate.want)? {
                return Ok(json!({"ok":true,"same":true,"count":"unchanged"}));
            }
            (if event.is_some() { 1 } else { 2 }, event)
        } else {
            let latest = tx.query_row("select coalesce(source_host,''),coalesce(source_path,'') from documents where task_id=? and kind=? and name=? order by version desc limit 1", db::params![p.task,p.kind,p.name], |r| Ok((r.get::<_, String>(0)?, r.get::<_,String>(1)?))).optional()?.ok_or_else(|| store::reject("document upload has no matching capture"))?;
            if latest != (input.host.clone(), input.path.clone()) {
                return Err(store::reject(
                    "document upload does not match the latest capture",
                ));
            }
            (0, p.event_id)
        };
        if p.dry_run {
            tx.execute_batch("savepoint doc_upload_dry_run")?;
        }
        let (doc, same) = docs::store(tx, p.task, &p.kind, &p.name, &input, event, backfill)?;
        if p.dry_run {
            tx.execute_batch("rollback to doc_upload_dry_run; release doc_upload_dry_run")?;
        }
        Ok(
            json!({"ok":true,"action":"put","doc_id":doc.id,"version":doc.version,"same":same,"count":if same { "unchanged" } else if input.reason.is_empty() { "captured" } else { &input.reason }}),
        )
    })
}

struct Candidate {
    want: DocWant,
    host: String,
}
fn candidates(db: &db::Connection, id: i64) -> store::Result<Vec<Candidate>> {
    let task = store::task(db, id)?;
    let brief: String = db.query_row(
        "select coalesce(brief_path,'') from tasks where id=?",
        [id],
        |r| r.get(0),
    )?;
    let host = docs::host(db, id)?;
    let mut paths = Vec::new();
    if !brief.is_empty() {
        paths.push((
            brief.clone(),
            if task.parent.is_none() {
                "goal"
            } else {
                "brief"
            },
            String::new(),
        ));
    }
    let mut query = db.prepare("select distinct json_extract(data,'$.file') from events where task_id=? and kind='prompt' and json_extract(data,'$.file') is not null order by id")?;
    for path in query.query_map([id], |r| r.get::<_, String>(0))? {
        let path = path?;
        if !path.is_empty() && path != brief {
            let name = std::path::Path::new(&path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            paths.push((path, "prompt", name));
        }
    }
    let report: String = db.query_row("select coalesce((select json_extract(data,'$.report') from events where task_id=t.id and kind='ready' order by id desc limit 1),t.report_path,'') from tasks t where id=?", [id], |r| r.get(0))?;
    if !report.is_empty() {
        paths.push((report, "report", String::new()));
    }
    paths.into_iter().map(|(path,kind,name)| {
        let source = if matches!(kind,"brief" | "prompt") {
            db.query_row("select coalesce(source_host,'') from documents where task_id=? and kind=? and name=? and captured=0 order by version desc limit 1", db::params![id,kind,name], |r| r.get::<_, String>(0)).optional()?.unwrap_or_else(|| host.clone())
        } else { host.clone() };
        Ok(Candidate { want:DocWant {task:id,kind:kind.into(),name,path,event_id:None},host:source })
    }).collect()
}

fn skipped(db: &db::Connection, want: &DocWant) -> store::Result<bool> {
    Ok(db.query_row("select exists(select 1 from documents where task_id=? and kind=? and name=? and captured=1 and(backfill=0 or source_path=?))", db::params![want.task,want.kind,want.name,want.path], |r| r.get(0))?)
}

pub(super) fn wanted(args: &[String], json: bool) -> store::Result<Value> {
    let mut flags = taskr_core::goflag::FlagSet::new("_doc wanted", json);
    flags
        .int("tree", 0, "only this task and descendants")
        .int("offset", 0, "candidate offset");
    flags.parse(args, 0, 0).map_err(store::usage)?;
    let tree = flags.get_int("tree");
    let offset = flags.get_int("offset");
    if tree < 0 || offset < 0 {
        return Err(store::usage("invalid _doc wanted page"));
    }
    let db = super::child::open()?;
    let mut query = db.prepare(if tree == 0 {"select id from tasks where ?>=0 order by id"} else {"with recursive sub(id) as(select ? union all select t.id from tasks t join sub on t.parent_id=sub.id) select id from sub order by id"})?;
    if tree != 0 {
        store::task(&db, tree)?;
    }
    let ids = query
        .query_map([tree], |r| r.get::<_, i64>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut all = Vec::new();
    for id in ids {
        all.extend(
            candidates(&db, id)?
                .into_iter()
                .filter(|c| Some(&c.host) == store::caller_machine().as_ref())
                .map(|c| c.want),
        );
    }
    let start = (offset as usize).min(all.len());
    let end = (start + 200).min(all.len());
    let more = end < all.len();
    let mut wanted = Vec::new();
    let mut unchanged = 0;
    for want in all.drain(start..end) {
        if skipped(&db, &want)? {
            unchanged += 1;
        } else {
            wanted.push(want);
        }
    }
    Ok(json!({"documents":wanted,"offset":end,"more":more,"unchanged":unchanged}))
}
