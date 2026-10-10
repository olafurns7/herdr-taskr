use super::*;
use rusqlite::types::{Value as Sql, ValueRef};
use taskr_core::store;
const CAP: usize = (32 << 10) - 96;
pub(super) const TREES: &str = "with recursive tree(root,id) as (select id,id from tasks where parent_id is null and status != 'closed' union all select tree.root,t.id from tasks t join tree on t.parent_id=tree.id) ";
const EVENT_COLS: &str = "e.id,e.task_id,t.name as task_name,e.recipient_task_id,e.launch_id,e.kind,e.summary,e.data,e.related_event_id,e.answered_by,e.event_key,e.created_at";
pub(super) fn rows(db: &Connection, sql: &str, args: Vec<Sql>) -> Result<Vec<Value>> {
    let mut stmt = db.prepare(sql)?;
    let names: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut cursor = stmt.query(params_from_iter(args))?;
    let mut out = vec![];
    while let Some(row) = cursor.next()? {
        let mut object = serde_json::Map::new();
        for (i, key) in names.iter().enumerate() {
            let value = match row.get_ref(i)? {
                ValueRef::Null => continue,
                ValueRef::Integer(n) => {
                    if matches!(
                        key.as_str(),
                        "waiting" | "present" | "captured" | "blocking" | "clear" | "lead_present"
                    ) {
                        json!(n != 0)
                    } else {
                        json!(n)
                    }
                }
                ValueRef::Real(n) => json!(n),
                ValueRef::Text(s) => json!(String::from_utf8_lossy(s)),
                ValueRef::Blob(s) => json!(s),
            };
            if key == "data" {
                if let Some(s) = value.as_str()
                    && let Ok(v) = serde_json::from_str(s)
                {
                    object.insert(key.clone(), taskr_core::event_data(v));
                }
            } else {
                object.insert(key.clone(), value);
            }
        }
        out.push(Value::Object(object));
    }
    Ok(out)
}
pub(super) fn one(db: &Connection, sql: &str, args: Vec<Sql>) -> Result<Option<Value>> {
    Ok(rows(db, sql, args)?.into_iter().next())
}
pub(super) fn subtree(db: &Connection, id: i64) -> Result<Vec<i64>> {
    let v = rows(
        db,
        "with recursive sub(id,depth) as (select id,0 from tasks where id=? union all select t.id,sub.depth+1 from tasks t join sub on t.parent_id=sub.id) select id from sub order by depth,id",
        vec![id.into()],
    )?;
    if v.is_empty() {
        return Err(reject(format!("task {id} does not exist")));
    }
    Ok(v.iter().map(|r| r["id"].as_i64().unwrap()).collect())
}
fn check_task(db: &Connection, id: i64) -> Result<()> {
    if db
        .query_row("select id from tasks where id=?", [id], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?
        .is_none()
    {
        return Err(reject(format!("task {id} does not exist")));
    }
    Ok(())
}
fn in_ids(ids: &[i64]) -> String {
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}
pub(super) fn now() -> String {
    taskr_core::store::now()
}
pub(super) fn age(at: &str) -> i64 {
    let now = taskr_core::frozen_now().unwrap();
    let parsed = if at.len() == 24 && at.as_bytes()[19] == b'.' && at.ends_with('Z') {
        taskr_core::store::parse_time(at)
    } else {
        None
    };
    parsed.map_or(i64::MAX / 1_000_000, |t| {
        ((now - t)
            .whole_nanoseconds()
            .clamp(i64::MIN as i128, i64::MAX as i128)
            / 1_000_000) as i64
    })
}
pub(super) fn meta(db: &Connection, key: &str) -> Result<Option<String>> {
    Ok(db
        .query_row("select value from meta where key=?", [key], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .map(|v| v.unwrap_or_default()))
}
pub(super) fn host_fresh(db: &Connection, host: &str) -> Result<bool> {
    Ok(meta(db, &format!("daemon_heartbeat:{host}"))?.is_some_and(|s| age(&s) < 30_000))
}
fn total(v: &[Value]) -> usize {
    v.iter().map(|r| cli::read_line(r).len()).sum()
}
pub fn status(f: &FlagSet) -> Result<()> {
    let db = open()?;
    let root = f.get_int("tree");
    let all = f.get_bool("all");
    let ids = if root != 0 {
        subtree(&db, root)?
    } else {
        rows(
            &db,
            "select id from tasks where ? or status != 'closed' order by id",
            vec![Sql::Integer(i64::from(all))],
        )?
        .iter()
        .map(|v| v["id"].as_i64().unwrap())
        .collect()
    };
    let mut omitted = 0;
    for id in ids {
        let mut m=one(&db,"select t.id,t.name,t.role,t.status,t.updated_at,t.acked_event_id,(select count(*) from events a where a.task_id=t.id and a.kind='ask' and a.answered_by is null) as open_asks,(select count(*) from events a where a.task_id=t.id and a.kind='ask' and a.answered_by is null and json_extract(a.data,'$.blocking')=1) as blocking_asks,(select count(*) from events p where p.task_id=t.id and p.kind='prompt' and p.launch_id is t.current_launch_id) as round,(select g.related_event_id from events g where g.task_id=t.id and g.kind='got' and g.launch_id is t.current_launch_id order by g.id desc limit 1) as last_receipt,coalesce(t.waiting_until>?,0) as waiting,t.waiting_until,t.parent_id,t.current_launch_id,t.pending_event_id,t.agent_name,t.workspace_id,t.tab_id,t.pane_id,t.report_path,l.observed_status,l.observed_at,l.observed_seq,l.present,l.machine from tasks t left join launches l on l.id=t.current_launch_id where t.id=?",vec![now().into(),id.into()])?.unwrap();
        if !f.json() && !all && root != 0 && id != root && m["status"] == "closed" {
            omitted += 1;
            continue;
        }
        let oat = m["observed_at"].as_str().map(str::to_string);
        if let Some(at) = &oat {
            let mut obs = json!({"at":at,"present":m["present"].as_bool().unwrap_or(false)});
            if !m["observed_status"].is_null() {
                obs["agent_status"] = m["observed_status"].clone();
            }
            if !m["observed_seq"].is_null() {
                obs["state_change_seq"] = m["observed_seq"].clone();
            }
            m["observed"] = obs;
        }
        if let Some(host) = m["machine"].as_str()
            && (m["status"] == "open" || m["status"] == "ready")
            && !host_fresh(&db, host)?
        {
            let mut obs = json!({"agent_status":"unknown","host":host});
            if let Some(at) = oat {
                obs["at"] = json!(at);
            }
            m["observed"] = obs;
        }
        let obj = m.as_object_mut().unwrap();
        for k in [
            "observed_at",
            "observed_seq",
            "observed_status",
            "present",
            "machine",
        ] {
            obj.remove(k);
        }
        if obj.get("waiting") != Some(&json!(true)) {
            obj.remove("waiting_until");
        }
        if let Some(n) = one(
            &db,
            "select coalesce(summary,'') as text,coalesce(json_extract(data,'$.clear'),0) as clear from events where task_id=? and kind='next' order by id desc limit 1",
            vec![id.into()],
        )? && n["clear"] != true
        {
            m["next"] = n["text"].clone();
        }
        if m["parent_id"].is_null()
            && let Some(bytes) = store::tmp::total(&db, id)?
        {
            m["tmp_bytes"] = json!(bytes);
            m["tmp"] = json!(format!("tmp {}", store::tmp::compact(bytes)));
        }
        cli::emit(f.json(), &m, true);
    }
    let mut daemon = json!({"record":"daemon","daemon":"none"});
    if let Some(at) = meta(&db, "daemon_heartbeat")? {
        let ms = age(&at);
        daemon["daemon"] = json!(if ms < 30_000 { "fresh" } else { "stale" });
        if !at.is_empty() {
            daemon["heartbeat_at"] = json!(at);
            daemon["heartbeat_age_ms"] = json!(ms);
        }
    }
    cli::emit(f.json(), &daemon, true);
    if omitted > 0 {
        cli::ordered_trailer(
            f.json(),
            &json!({"closed":omitted,"all":"--all"}),
            &["closed", "all"],
        );
    }
    Ok(())
}
fn event_rows(db: &Connection, sql: &str, args: Vec<Sql>) -> Result<Vec<Value>> {
    let mut v = rows(db, sql, args)?;
    for m in &mut v {
        m["record"] = json!("event");
    }
    Ok(v)
}
pub fn asks(f: &FlagSet) -> Result<()> {
    let limit = f.get_int("limit");
    if limit < 0 {
        return Err(usage(format!("--limit must be >= 0, got {limit}")));
    }
    let db = open()?;
    let mut lines = ask_rows(&db, f)?;
    let mut dropped = 0;
    if !f.json() || f.was_set("limit") {
        let keep = if f.was_set("limit") { limit } else { 20 };
        let newest = lines.last().map_or(0, |m| m["id"].as_i64().unwrap());
        let count = lines.iter().filter(|m| !m["answered_by"].is_null()).count();
        if keep > 0 && count > keep as usize {
            let n = count - keep as usize;
            lines.retain(|m| {
                if !m["answered_by"].is_null() && dropped < n {
                    dropped += 1;
                    false
                } else {
                    true
                }
            });
        }
        if !f.json() && !(f.was_set("limit") && limit == 0) {
            let mut size = total(&lines);
            let mut i = 0;
            while size > CAP && i < lines.len() {
                if !lines[i]["answered_by"].is_null() && lines[i]["id"] != newest {
                    size -= cli::read_line(&lines[i]).len();
                    lines.remove(i);
                    dropped += 1;
                } else {
                    i += 1;
                }
            }
        }
    }
    for m in lines {
        cli::emit(f.json(), &m, true);
    }
    if dropped > 0 {
        cli::ordered_trailer(
            f.json(),
            &json!({"answered":dropped,"all":"--limit 0"}),
            &["answered", "all"],
        );
    }
    Ok(())
}
/// The asks a read shows, before budgets; a withdrawn answer carries withdrawn:true.
pub(super) fn ask_rows(db: &Connection, f: &FlagSet) -> Result<Vec<Value>> {
    let mut prefix = "";
    let mut predicates = vec!["e.kind='ask'".to_string()];
    if !f.get_bool("all") {
        prefix = TREES;
        predicates.push("(e.answered_by is not null or (t.status != 'closed' and e.task_id in (select id from tree)))".into());
    }
    if f.get_bool("open") {
        predicates.push("e.answered_by is null".into());
    }
    if f.get_bool("owner") {
        predicates.push("json_extract(e.data,'$.owner')=1".into());
    }
    if f.get_int("tree") != 0 {
        predicates.push(format!(
            "e.task_id in ({})",
            in_ids(&subtree(db, f.get_int("tree"))?)
        ));
    }
    let mut lines = event_rows(
        db,
        &format!(
            "{prefix}select {EVENT_COLS},ans.summary as answer,coalesce(json_extract(ans.data,'$.withdrawn'),0) as withdrawn from events e join tasks t on t.id=e.task_id left join events ans on ans.id=e.answered_by where {} order by e.id",
            predicates.join(" and ")
        ),
        vec![],
    )?;
    for m in &mut lines {
        if m["withdrawn"] != 1 {
            m.as_object_mut().unwrap().remove("withdrawn");
        } else {
            m["withdrawn"] = json!(true);
        }
    }
    Ok(lines)
}
pub fn log(f: &FlagSet) -> Result<()> {
    let limit = f.get_int("limit");
    if limit < 0 {
        return Err(usage(format!("--limit must be >= 0, got {limit}")));
    }
    let id = id(&f.positional[0], "task id")?;
    let db = open()?;
    let ids = if f.get_bool("tree") {
        subtree(&db, id)?
    } else {
        check_task(&db, id)?;
        vec![id]
    };
    let ids = in_ids(&ids);
    let mut launches = rows(
        &db,
        &format!(
            "select id,task_id,provider,model,effort,account,native_home,session_ref,session_kind,session_source,herdr_scope,pane_id,observed_status,observed_seq,observed_version,present,observed_at,recorded_at,workspace_id,tab_id from launches where task_id in ({ids}) order by id"
        ),
        vec![],
    )?;
    for m in &mut launches {
        m["record"] = json!("launch");
    }
    let mut sql = format!(
        "select {EVENT_COLS} from events e join tasks t on t.id=e.task_id where e.task_id in ({ids})"
    );
    let mut args = vec![];
    if f.was_set("since") {
        sql.push_str(" and e.id>?");
        args.push(f.get_int("since").into());
    }
    if f.was_set("before") {
        sql.push_str(" and e.id<?");
        args.push(f.get_int("before").into());
    }
    sql.push_str(" order by e.id");
    let mut events = event_rows(&db, &sql, args)?;
    let count = events.len();
    let mut trailer = None;
    if !f.json() || f.was_set("limit") || f.was_set("since") || f.was_set("before") {
        let keep = if f.was_set("limit") { limit } else { 100 };
        let forward = f.was_set("since");
        if keep > 0 && events.len() > keep as usize {
            if forward {
                events.truncate(keep as usize);
            } else {
                events.drain(..events.len() - keep as usize);
            }
        }
        let states = rows(
            &db,
            &format!("select id,status,current_launch_id from tasks where id in ({ids})"),
            vec![],
        )?;
        launches.retain(|l| {
            states.iter().any(|t| {
                l["task_id"] == t["id"]
                    && l["id"] == t["current_launch_id"]
                    && (t["status"] != "closed" || events.iter().any(|e| e["task_id"] == t["id"]))
            })
        });
        let mut dropped = 0;
        if !f.json() && !(f.was_set("limit") && limit == 0) {
            let mut size = total(&launches) + total(&events);
            while events.len() > 1 && size > CAP {
                let at = if forward { events.len() - 1 } else { 0 };
                size -= cli::read_line(&events.remove(at)).len();
            }
            let mut i = 0;
            while i < launches.len() && size > CAP {
                if states
                    .iter()
                    .any(|t| t["id"] == launches[i]["task_id"] && t["status"] == "closed")
                {
                    size -= cli::read_line(&launches.remove(i)).len();
                    dropped += 1;
                } else {
                    i += 1;
                }
            }
        }
        if count > events.len() {
            let mut t = if forward {
                json!({"more":count-events.len(),"next":events.last().unwrap()["id"]})
            } else {
                json!({"older":count-events.len(),"before":events[0]["id"]})
            };
            if dropped > 0 {
                t["launches"] = json!(dropped);
            }
            trailer = Some(t);
        } else if dropped > 0 {
            trailer = Some(json!({"launches":dropped,"all":"--limit 0"}));
        }
    }
    for m in launches.iter().chain(&events) {
        cli::emit(f.json(), m, true);
    }
    if let Some(t) = trailer {
        cli::ordered_trailer(
            f.json(),
            &t,
            &["older", "before", "more", "next", "launches", "all"],
        );
    }
    Ok(())
}
fn document_kind(kind: &str) -> bool {
    matches!(
        kind,
        "brief" | "prompt" | "report" | "handover" | "goal" | "plan"
    )
}
pub fn search(f: &FlagSet) -> Result<()> {
    let root = f.get_int("root");
    let limit = f.get_int("limit");
    let kind = f.get_string("kind");
    let raw = f.get_bool("raw");
    if root < 0 || f.was_set("root") && root == 0 {
        return Err(usage("--root must be a positive campaign id"));
    }
    if !(1..=100).contains(&limit) {
        return Err(usage("--limit must be between 1 and 100"));
    }
    if !kind.is_empty()
        && !document_kind(kind)
        && !matches!(kind, "decision" | "ask" | "answer" | "note")
    {
        return Err(usage(format!(
            "unknown search kind {}",
            goflag::quote(kind)
        )));
    }
    let mut query = f.positional[0].clone();
    if query.contains('\0') {
        return Err(usage("search query must not contain a NUL byte"));
    }
    if !raw {
        query = query
            .split_whitespace()
            .map(|s| {
                let prefix = s.ends_with('*');
                let s = if prefix { &s[..s.len() - 1] } else { s };
                format!(
                    "\"{}\"{}",
                    s.replace('"', "\"\""),
                    if prefix { "*" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        if query.is_empty() {
            return Ok(());
        }
    }
    let db = open()?;
    let mut sql="select src,ref,root_id as root,task_id as task,kind,name,at,snippet(search_fts,0,'[',']','...',12) as snippet from search_fts where search_fts match ?".to_string();
    let mut args = vec![query.into()];
    if root != 0 {
        sql.push_str(" and root_id=?");
        args.push(root.into());
    }
    if kind == "answer" {
        sql.push_str(" and kind in ('answer','owner_answer')");
    } else if !kind.is_empty() {
        sql.push_str(" and kind=?");
        args.push(kind.to_string().into());
    }
    sql.push_str(" order by bm25(search_fts),at desc,ref desc limit ?");
    args.push(limit.into());
    let hits = rows(&db, &sql, args).map_err(|e| {
        if raw
            && (e.text.contains("fts5:")
                || e.text.contains("unterminated string")
                || e.text.contains("no such column:"))
        {
            usage(format!("invalid FTS5 query: {}", e.text))
        } else {
            e
        }
    })?;
    for mut hit in hits {
        if hit["src"] != "doc" {
            hit.as_object_mut().unwrap().remove("name");
        }
        let s = hit["snippet"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .map(|c| {
                if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
                    ' '
                } else {
                    c
                }
            })
            .take(200)
            .collect::<String>();
        hit["snippet"] = json!(s);
        cli::emit(f.json(), &hit, false);
    }
    Ok(())
}
pub fn doc_ls(f: &FlagSet) -> Result<()> {
    let id = id(&f.positional[0], "task id")?;
    let limit = f.get_int("limit");
    let kind = f.get_string("kind");
    if limit < 0 {
        return Err(usage("--limit must be >= 0"));
    }
    if !kind.is_empty() && !document_kind(kind) {
        return Err(usage(format!(
            "unknown document kind {}",
            goflag::quote(kind)
        )));
    }
    let db = open()?;
    check_task(&db, id)?;
    let ids = if f.get_bool("tree") {
        subtree(&db, id)?
    } else {
        vec![id]
    };
    let mut sql = format!(
        "select id as doc_id,task_id,kind,name,version,bytes,format,captured,reason,event_id,source_path,source_host,backfill,created_at from documents where task_id in ({})",
        in_ids(&ids)
    );
    let mut args = vec![];
    if !kind.is_empty() {
        sql.push_str(" and kind=?");
        args.push(kind.to_string().into());
    }
    if !f.get_bool("versions") {
        sql.push_str(" and version=(select max(version) from documents d where d.task_id=documents.task_id and d.kind=documents.kind and d.name=documents.name)");
    }
    sql.push_str(" order by id desc");
    let mut lines = rows(&db, &sql, args)?;
    for m in &mut lines {
        for k in [
            "bytes",
            "format",
            "reason",
            "event_id",
            "source_path",
            "source_host",
        ] {
            if m.get(k).is_none() {
                m[k] = Value::Null;
            }
        }
    }
    let mut kept = 0;
    let mut budget = 0;
    for m in &lines {
        let size = cli::read_line(m).len();
        if limit > 0 && (kept >= limit as usize || kept > 0 && budget + size > CAP) {
            break;
        }
        cli::emit(f.json(), m, true);
        budget += size;
        kept += 1;
    }
    if kept < lines.len() {
        cli::trailer(
            f.json(),
            json!({"older":lines.len()-kept,"all":"--limit 0"}),
        );
    }
    Ok(())
}
pub fn doc_get(f: &FlagSet) -> Result<()> {
    let id = id(&f.positional[0], "document id")?;
    let db = open()?;
    let d=one(&db,"select captured,coalesce(reason,'') as reason,coalesce(source_path,'') as path,sha256 from documents where id=?",vec![id.into()])?.ok_or_else(||reject(format!("document {id} does not exist")))?;
    if d["captured"] != true {
        return Err(reject(format!(
            "document {id} not captured ({}): {}",
            d["reason"].as_str().unwrap(),
            d["path"].as_str().unwrap()
        )));
    }
    let body: String = db.query_row(
        "select body from doc_blobs where sha256=?",
        params![d["sha256"].as_str()],
        |r| r.get(0),
    )?;
    print!("{body}");
    Ok(())
}

pub fn notes(f: &FlagSet) -> Result<()> {
    let root = f.get_int("root");
    let limit = f.get_int("limit");
    let since = f.get_string("since");
    if root < 0 || f.was_set("root") && root == 0 {
        return Err(usage("--root must be a positive campaign id"));
    }
    if limit < 0 {
        return Err(usage("--limit must be >= 0"));
    }
    let mut predicate = "t.parent_id is null and e.kind='note'".to_string();
    let mut args = vec![];
    if let Ok(n) = since.parse::<i64>()
        && n >= 0
    {
        predicate.push_str(" and e.id>?");
        args.push(n.into());
    } else if let Ok(n) = goflag::parse_duration(since)
        && n > 0
    {
        predicate.push_str(" and e.created_at>?");
        args.push(
            taskr_core::store::stamp(
                taskr_core::frozen_now().unwrap() - taskr_core::Duration::nanoseconds(n),
            )
            .into(),
        );
    } else {
        return Err(usage(
            "--since must be a nonnegative event id or positive Go duration",
        ));
    }
    if f.get_bool("owner") {
        predicate.push_str(" and json_extract(e.data,'$.owner')=1");
    }
    if root != 0 {
        predicate.push_str(" and t.id=?");
        args.push(root.into());
    }
    let db = open()?;
    let mut notes = event_rows(
        &db,
        &format!(
            "select {EVENT_COLS} from events e join tasks t on t.id=e.task_id where {predicate} order by e.id desc"
        ),
        args,
    )?;
    let matched = notes.len();
    if limit > 0 {
        notes.truncate(limit as usize);
        if !f.json() {
            let mut size = total(&notes);
            while notes.len() > 1 && size > CAP {
                size -= cli::read_line(&notes.pop().unwrap()).len();
            }
        }
    }
    for note in &notes {
        cli::emit(f.json(), note, true);
    }
    if matched > notes.len() {
        cli::ordered_trailer(
            f.json(),
            &json!({"older":matched-notes.len(),"all":"--limit 0"}),
            &["older", "all"],
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn go_timestamp_layout_and_duration_saturation() {
        for timestamp in [
            "",
            "bad",
            "2026-10-08T00:00:00Z",
            "2026-10-08T00:00:00.0000Z",
            "2026-10-08T00:00:00.000+00:00",
        ] {
            assert_eq!(age(timestamp), i64::MAX / 1_000_000);
        }
        assert_eq!(age("0001-01-01T00:00:00.000Z"), i64::MAX / 1_000_000);
        assert_eq!(age("9999-01-01T00:00:00.000Z"), i64::MIN / 1_000_000);
    }
}
