//! One ledger snapshot; paging applies only to the event log.
use super::glance::{clip, elapsed, lead, mark, n, s, sparks};
use super::queries::{host_fresh, one, rows};
use super::*;
use std::collections::BTreeMap;
const TREE: &str = "with recursive tree(id) as (select ? union all select t.id from tasks t join tree on t.parent_id=tree.id) ";
fn document(db: &Connection, root: i64, kind: &str) -> Result<Option<Value>> {
    let order = if kind == "goal" {
        "captured desc,version desc"
    } else {
        "version desc"
    };
    let mut d = one(
        db,
        &format!(
            "select id as doc_id,task_id,kind,name,version,bytes,format,captured,reason,event_id,source_path,source_host,backfill,created_at from documents where task_id=? and kind=? and name='' order by {order} limit 1"
        ),
        vec![root.into(), kind.to_string().into()],
    )?;
    if let Some(d) = &mut d {
        for k in [
            "bytes",
            "format",
            "reason",
            "event_id",
            "source_path",
            "source_host",
        ] {
            if d.get(k).is_none() {
                d[k] = Value::Null;
            }
        }
    }
    Ok(d)
}
pub(super) fn snapshot(db: &Connection, root: i64, page: i64, all: bool) -> Result<Value> {
    let t = one(db,"select id,name,role,status,coalesce(machine,'') as host,machine,coalesce(pane_id,'') as pane,created_at,coalesce(closed_at,'') as closed_at,pane_id,lead_status,lead_present,lead_observed_at,coalesce(waiting_until>?,0) as waiting from tasks where id=? and parent_id is null",vec![super::queries::now().into(),root.into()])?.ok_or_else(||reject(format!("campaign {root} does not exist or is not a root")))?;
    let (status, _) = lead(db, &t)?;
    let status = if t["status"] == "closed" {
        "closed"
    } else if s(&t, "pane").is_empty() {
        "unregistered"
    } else if t["waiting"] == true && matches!(status, "working" | "idle" | "done") {
        "waiting"
    } else {
        status
    };
    let next = one(
        db,
        "select coalesce(summary,'') as text,coalesce(json_extract(data,'$.clear'),0) as clear from events where task_id=? and kind='next' order by id desc limit 1",
        vec![root.into()],
    )?;
    let next = next
        .as_ref()
        .filter(|n| n["clear"] != true)
        .map_or("", |n| s(n, "text"));
    let r = json!({"id":root,"name":t["name"],"role":t["role"],"status":t["status"],"host":t["host"],"created_at":t["created_at"],"closed_at":t["closed_at"],"next":next,"pane_id":t["pane"],"lead":status,"age_ms":elapsed(s(&t,"created_at"))});
    let goal_doc = document(db, root, "goal")?;
    let mut goal: Vec<String> = vec![];
    if let Some(d) = goal_doc.filter(|d| d["captured"] == true) {
        let body: String = db.query_row(
            "select b.body from doc_blobs b join documents d on d.sha256=b.sha256 where d.id=?",
            [n(&d, "doc_id")],
            |r| r.get(0),
        )?;
        goal = body.split('\n').map(str::to_string).collect();
    }
    let mut plan = json!({"version":0,"age_ms":0,"decisions_since":0,"closed_since":0});
    if let Some(mut d) = document(db, root, "plan")? {
        let counts = one(db,&format!("{TREE}select coalesce(sum(kind='decision'),0) as decisions_since,coalesce(sum(kind='closed'),0) as closed_since from events where task_id in (select id from tree) and id>?"),vec![root.into(),n(&d,"event_id").into()])?.unwrap();
        d["id"] = d["doc_id"].clone();
        d["age_ms"] = json!(elapsed(s(&d, "created_at")));
        d["decisions_since"] = counts["decisions_since"].clone();
        d["closed_since"] = counts["closed_since"].clone();
        plan = d;
    }
    let mut lanes = rows(
        db,
        "with recursive tree(id,depth) as (select ?,0 union all select t.id,tree.depth+1 from tasks t join tree on t.parent_id=tree.id) select t.id,t.parent_id,tree.depth,t.name,t.role,t.status,coalesce(case when t.current_launch_id is null then t.machine else l.machine end,'') as host,t.created_at,coalesce(t.closed_at,'') as closed_at,coalesce(l.provider,'') as provider,coalesce(l.model,'') as model,coalesce(l.effort,'') as effort,coalesce((select summary from events e where e.task_id=t.id and kind in ('done','fail','ready') order by id desc limit 1),'') as summary,coalesce(l.pane_id,t.pane_id,'') as pane_id,coalesce(l.observed_status,'') as observed_status,l.present,coalesce((select created_at from events e where e.task_id=t.id order by id desc limit 1),t.created_at) as activity,coalesce((select json_extract(data,'$.outcome') from events e where e.task_id=t.id and kind='closed' order by id desc limit 1),'') as outcome from tree join tasks t on t.id=tree.id left join launches l on l.id=t.current_launch_id where depth>0 and (? or t.status!='closed') order by t.id",
        vec![root.into(), i64::from(all).into()],
    )?;
    for lane in &mut lanes {
        let mut state = mark(lane);
        if state == "working" && matches!(s(lane, "observed_status"), "idle" | "done" | "unknown") {
            state = match s(lane, "observed_status") {
                "idle" => "idle",
                "done" => "done",
                _ => "unknown",
            };
        }
        if lane["status"] == "closed" {
            state = "closed";
        }
        if !s(lane, "host").is_empty()
            && matches!(s(lane, "status"), "open" | "ready")
            && !host_fresh(db, s(lane, "host"))?
        {
            state = "unknown";
        }
        lane["state"] = json!(state);
        lane["age_ms"] = json!(elapsed(s(lane, "activity")));
        lane["summary"] = json!(clip(s(lane, "summary"), 300));
        for kind in ["brief", "report"] {
            let captured: bool=db.query_row("select coalesce((select captured from documents where task_id=? and kind=? and name='' order by version desc limit 1),0)",params![n(lane,"id"),kind],|r|r.get(0))?;
            lane[kind] = json!(captured);
        }
        for k in ["activity", "observed_status", "present"] {
            lane.as_object_mut().unwrap().remove(k);
        }
    }
    let mut decisions = rows(
        db,
        &format!(
            "{TREE}select e.id,e.kind,coalesce(e.summary,'') as text,coalesce(a.created_at,e.created_at) as at,t.name as lane,coalesce(a.summary,'') as answer from events e join tasks t on t.id=e.task_id left join events a on a.id=e.answered_by where ((e.kind='decision' and e.task_id=?) or (e.kind='ask' and e.answered_by is not null and json_extract(e.data,'$.owner')=1 and coalesce(json_extract(a.data,'$.withdrawn'),0)=0 and e.task_id in (select id from tree))) and not exists(select 1 from events r where r.kind='revoke' and r.related_event_id=e.id and r.task_id=?) order by e.id"
        ),
        vec![root.into(), root.into(), root.into()],
    )?;
    for d in &mut decisions {
        let owner = d["kind"] == "ask";
        d["owner"] = json!(owner);
        if owner {
            d["kind"] = json!("owner_ask");
            d["text"] = json!(format!("{}\nAnswer: {}", s(d, "text"), s(d, "answer")));
        } else {
            d["lane"] = json!("");
        }
        d.as_object_mut().unwrap().remove("answer");
    }
    let docs = rows(
        db,
        &format!(
            "{TREE}select d.id,d.kind,d.name,t.name as lane,d.version,d.captured from documents d join tasks t on t.id=d.task_id where d.task_id in (select id from tree) and d.id=(select x.id from documents x where x.task_id=d.task_id and x.kind=d.kind and x.name=d.name order by version desc limit 1) order by d.id desc"
        ),
        vec![root.into()],
    )?;
    let mut asks = vec![];
    for a in rows(
        db,
        &format!(
            "{TREE},asks as (select e.*,row_number() over (partition by (e.answered_by is null) order by e.id desc) as n from events e join tasks t on t.id=e.task_id where e.task_id in (select id from tree) and e.kind='ask' and (e.answered_by is not null or ? or (t.status!='closed' and (select status from tasks where id=?)!='closed'))) select e.id,e.created_at as at,t.name as lane,coalesce(e.summary,'') as text,e.answered_by is null as open,coalesce(json_extract(e.data,'$.owner'),0) as owner,coalesce(json_extract(e.data,'$.blocking'),0) as blocking,a.id as answer_id,a.created_at as answer_at,a.summary as answer_text,a.kind as answer_kind,coalesce(json_extract(a.data,'$.withdrawn'),0) as withdrawn from asks e join tasks t on t.id=e.task_id left join events a on a.id=e.answered_by where e.answered_by is null or e.n<=20 order by (e.answered_by is null) desc,e.id desc"
        ),
        vec![root.into(), i64::from(all).into(), root.into()],
    )? {
        asks.push(json!({"id":a["id"],"kind":"ask","at":a["at"],"lane":a["lane"],"text":a["text"],"open":n(&a,"open")!=0,"owner":n(&a,"owner")!=0,"blocking":a["blocking"]}));
        if !a["answer_id"].is_null() {
            let text = a["answer_text"].as_str().unwrap_or_default();
            let text = if n(&a, "withdrawn") != 0 {
                format!("withdrawn: {text}")
            } else {
                text.into()
            };
            asks.push(json!({"id":a["answer_id"],"kind":a["answer_kind"],"at":a["answer_at"],"lane":a["lane"],"text":text,"owner":n(&a,"owner")!=0}));
        }
    }
    let total: i64 = db.query_row(
        &format!("{TREE}select count(*) from events where task_id in (select id from tree)"),
        [root],
        |r| r.get(0),
    )?;
    let pages = ((total + 99) / 100).max(1);
    let page = page.min(pages);
    let mut log = rows(
        db,
        &format!(
            "{TREE}select e.id,e.kind,e.created_at as at,t.name as lane,coalesce(e.summary,'') as text,e.kind='ask' and e.answered_by is null as open,coalesce(json_extract(e.data,'$.owner'),0) as owner,coalesce(json_extract(e.data,'$.blocking'),0) as blocking from events e join tasks t on t.id=e.task_id where e.task_id in (select id from tree) order by e.id desc limit 100 offset ?"
        ),
        vec![root.into(), ((page - 1) * 100).into()],
    )?;
    for row in &mut log {
        row["open"] = json!(n(row, "open") != 0);
        row["owner"] = json!(n(row, "owner") != 0);
    }
    let mut prs = vec![];
    let mut refs = BTreeMap::<i64, Vec<Value>>::new();
    let mut metadata = BTreeMap::<i64, Value>::new();
    for row in rows(
        db,
        &format!(
            "{TREE}select e.task_id,t.name as lane,json_extract(e.data,'$.key') as key,json_extract(e.data,'$.value') as value from events e join tasks t on t.id=e.task_id where e.task_id in (select id from tree) and e.kind='ref' and (json_extract(e.data,'$.key')='pr' or json_extract(e.data,'$.key') like 'pr.%') and e.id=(select max(x.id) from events x where x.task_id=e.task_id and x.kind='ref' and json_extract(x.data,'$.key')=json_extract(e.data,'$.key')) and coalesce(json_extract(e.data,'$.value'),'')!='' order by e.task_id,e.id"
        ),
        vec![root.into()],
    )? {
        let task = n(&row, "task_id");
        refs.entry(task)
            .or_default()
            .push(json!({"key":row["key"],"value":row["value"]}));
        if let Some(field) = s(&row, "key")
            .strip_prefix("pr.")
            .filter(|f| ["title", "state", "ci", "review"].contains(f))
        {
            metadata.entry(task).or_insert_with(|| json!({}))[field] = row["value"].clone();
            continue;
        }
        let mut pr = row;
        let value = s(&pr, "value");
        if value.bytes().all(|b| b.is_ascii_digit())
            && let Ok(number) = value.parse::<u32>()
        {
            pr["number"] = json!(number);
        }
        prs.push(pr);
    }
    for pr in &mut prs {
        let task = n(pr, "task_id");
        pr["refs"] = json!(refs[&task]);
        if pr["key"] == "pr"
            && let Some(fields) = metadata.get(&task)
        {
            pr.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
        }
    }
    let spark = sparks(db, root)?
        .remove(&root)
        .unwrap_or_else(|| vec![0; 24]);
    Ok(
        json!({"root":r,"goal":goal,"plan":plan,"lanes":lanes,"asks":asks,"decisions":decisions,"docs":docs,"log":log,"prs":prs,"spark":spark,"log_page":{"page":page,"pages":pages,"total":total}}),
    )
}
pub fn run(f: &FlagSet) -> Result<()> {
    let root = id(&f.positional[0], "campaign id")?;
    if f.get_int("page") < 1 {
        return Err(usage("--page must be >= 1"));
    }
    let db = open()?;
    db.execute_batch("begin")?;
    let value = snapshot(&db, root, f.get_int("page"), f.get_bool("all"))?;
    db.execute_batch("rollback")?;
    cli::emit(f.json(), &value, false);
    Ok(())
}
