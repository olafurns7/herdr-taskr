use super::*;
use rusqlite::types::ValueRef;
use std::fmt::Write;

fn rows(db: &Connection, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<Vec<Value>> {
    let mut stmt = db.prepare(sql)?;
    let names: Vec<_> = stmt.column_names().into_iter().map(String::from).collect();
    Ok(stmt
        .query_map(values, |r| {
            let mut v = json!({});
            for (i, name) in names.iter().enumerate() {
                v[name] = match r.get_ref(i)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => json!(n),
                    ValueRef::Real(n) => json!(n),
                    ValueRef::Text(s) => json!(String::from_utf8_lossy(s)),
                    ValueRef::Blob(_) => Value::Null,
                };
            }
            Ok(v)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?)
}
fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn n(v: &Value, k: &str) -> i64 {
    v[k].as_i64().unwrap_or(0)
}
pub fn md(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => {
                if "\\`*_{}[]()#+-.!|".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
        }
    }
    out
}
fn md_or(s: &str, none: &str) -> String {
    if s.trim().is_empty() {
        none.into()
    } else {
        md(s)
    }
}
fn cell(s: String) -> String {
    if s.is_empty() { "–".into() } else { s }
}
fn age(at: &str, now: time::OffsetDateTime) -> String {
    let Some(at) = parse_time(at) else {
        return String::new();
    };
    let s = (now - at).whole_seconds().max(0);
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 48 * 3600 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86400)
    }
}
fn next(db: &Connection, id: i64, now: time::OffsetDateTime) -> Result<String> {
    let rows = rows(
        db,
        "select summary,created_at,data from events where task_id=? and kind='next' order by id desc limit 1",
        &[&id],
    )?;
    let Some(r) = rows.first() else {
        return Ok(String::new());
    };
    let data: Value = serde_json::from_str(text(r, "data")).unwrap_or(Value::Null);
    if data["clear"] == true {
        return Ok(String::new());
    }
    Ok(format!(
        "{} ({})",
        md(text(r, "summary")),
        age(text(r, "created_at"), now)
    ))
}
fn refs(db: &Connection, id: i64, sep: &str) -> Result<String> {
    let rows = rows(
        db,
        "select json_extract(data,'$.key') as key,json_extract(data,'$.value') as value from events where id in(select max(id) from events where task_id=? and kind='ref' group by json_extract(data,'$.key')) order by 1",
        &[&id],
    )?;
    Ok(rows
        .iter()
        .filter(|r| !text(r, "value").is_empty())
        .map(|r| format!("{}={}", md(text(r, "key")), md(text(r, "value"))))
        .collect::<Vec<_>>()
        .join(sep))
}
pub struct Rendered {
    pub text: String,
    pub counts: Value,
}
pub fn render(db: &Connection, id: i64, note: &str, adopted: bool) -> Result<Rendered> {
    let t = task(db, id)?;
    let now = frozen_now().expect("clock");
    let marks = rows(
        db,
        "select id,created_at,coalesce(summary,'') as summary from events where task_id=? and kind='handover' order by id desc limit 2",
        &[&id],
    )?;
    let since = marks.get(if adopted { 1 } else { 0 });
    let latest = marks.first();
    let note = if adopted {
        latest.map_or("", |v| text(v, "summary"))
    } else {
        note
    };
    let mut out = String::new();
    writeln!(out, "# taskr handover: {} (task {id})\n", md(&t.name)).expect("string");
    if adopted && let Some(latest) = latest {
        writeln!(out,"Handover event {} ({}), re-rendered {} from the taskr ledger for the session that adopted it.",n(latest,"id"),md(text(latest,"created_at")),md(&stamp(now))).expect("string");
    } else if adopted {
        writeln!(out,"No handover was recorded; rendered {} from the taskr ledger for the session that adopted it.",md(&stamp(now))).expect("string");
    } else {
        writeln!(out, "Rendered {} from the taskr ledger.", md(&stamp(now))).expect("string");
    }
    writeln!(
        out,
        "A new session takes over with `taskr adopt {id}`, then `taskr wait --as {id}`.\n"
    )
    .expect("string");
    render_documents(db, id, &mut out)?;
    let root=rows(db,"select coalesce(workspace_id,'') as workspace,coalesce(tab_id,'') as tab,coalesce(cwd,'') as cwd,coalesce(pane_id,'') as pane,(select count(*) from events where recipient_task_id=tasks.id and id>tasks.acked_event_id) as unacked from tasks where id=?",&[&id])?.remove(0);
    writeln!(
        out,
        "## Identity\n\n- Orchestrator: {}, task {id}, {}, status {}",
        md(&t.name),
        md(&t.role),
        md(&t.status)
    )
    .expect("string");
    writeln!(
        out,
        "- Where: workspace {}, tab {}, pane {}",
        md_or(text(&root, "workspace"), "none"),
        md_or(text(&root, "tab"), "none"),
        md_or(text(&root, "pane"), "none")
    )
    .expect("string");
    writeln!(out, "- Cwd: {}", md_or(text(&root, "cwd"), "none")).expect("string");
    write!(
        out,
        "- Inbox: acked through event {}, {} unacked",
        t.acked,
        n(&root, "unacked")
    )
    .expect("string");
    if let Some(p) = t.pending {
        write!(
            out,
            ", event {p} offered and not acked (the next wait replays it)"
        )
        .expect("string");
    }
    out.push('\n');
    let nx = next(db, id, now)?;
    writeln!(
        out,
        "- Next: {}\n- Refs: {}\n",
        if nx.is_empty() { "none" } else { &nx },
        md_or_raw(&refs(db, id, ", ")?, "none")
    )
    .expect("string");
    let decisions = rows(
        db,
        "with recursive sub(id) as(select ?1 union all select t.id from tasks t join sub on t.parent_id=sub.id) select e.id,e.kind,coalesce(e.summary,'') as summary,coalesce(a.created_at,e.created_at) as created_at,t.name as name,coalesce(a.summary,'') as answer,coalesce(a.id,0) as answer_id from events e join tasks t on t.id=e.task_id left join events a on a.id=e.answered_by where ((e.kind='decision' and e.task_id=?1) or(e.kind='ask' and e.answered_by is not null and json_extract(e.data,'$.owner')=1 and coalesce(json_extract(a.data,'$.withdrawn'),0)=0 and e.task_id in(select id from sub))) and not exists(select 1 from events r where r.kind='revoke' and r.related_event_id=e.id and r.task_id=?1) order by e.id",
        &[&id],
    )?;
    out.push_str("## Decisions in force\n\n");
    if decisions.is_empty() {
        out.push_str("None.\n");
    }
    for d in &decisions {
        if d["kind"] == "ask" {
            writeln!(
                out,
                "- Owner answer to ask {} from {} (answer event {}): {}\n  Answer: {}",
                n(d, "id"),
                md(text(d, "name")),
                n(d, "answer_id"),
                md(text(d, "summary")),
                md(text(d, "answer"))
            )
            .expect("string");
        } else {
            writeln!(
                out,
                "- Decision {} ({}): {}",
                n(d, "id"),
                md(text(d, "created_at")),
                md(text(d, "summary"))
            )
            .expect("string");
        }
    }
    let all = rows(
        db,
        "with recursive tree(id,depth,path) as(select id,0,printf('%012d',id) from tasks where id=? union all select t.id,tree.depth+1,tree.path||'/'||printf('%012d',t.id) from tasks t join tree on t.parent_id=tree.id) select t.id,tree.depth,t.name,t.role,t.status,coalesce(p.name,'') as parent,coalesce(t.agent_name,'') as agent,coalesce(l.provider||'/'||l.model||'/'||l.effort,'') as launch,coalesce(l.pane_id,t.pane_id,'') as pane,coalesce(t.report_path,'') as report,coalesce(l.observed_status,'') as obs_status,coalesce(l.observed_at,'') as obs_at,l.present,coalesce(t.closed_at,'') as closed_at,coalesce((select max(c.id) from events c where c.task_id=t.id and c.kind='closed'),0) as closed_event,coalesce((select f.summary from events f where f.task_id=t.id and f.kind in('done','fail','ready') order by f.id desc limit 1),'') as final from tree join tasks t on t.id=tree.id left join tasks p on p.id=t.parent_id left join launches l on l.id=t.current_launch_id where tree.depth>0 order by tree.path",
        &[&id],
    )?;
    let live: Vec<_> = all
        .iter()
        .filter(|r| !matches!(text(r, "status"), "closed" | "planned"))
        .collect();
    let planned: Vec<_> = all.iter().filter(|r| r["status"] == "planned").collect();
    let mut closed: Vec<_> = all
        .iter()
        .filter(|r| {
            r["status"] == "closed" && since.is_none_or(|s| n(r, "closed_event") > n(s, "id"))
        })
        .collect();
    closed.sort_by_key(|r| (n(r, "closed_event"), n(r, "id")));
    out.push_str("\n## Live lanes\n\n");
    if live.is_empty() {
        out.push_str("None.\n");
    } else {
        out.push_str("| Lane | Role | Agent | Provider/model/effort | Pane | Status | Herdr | Next | Refs | Report |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
    }
    for r in &live {
        let rid = n(r, "id");
        let herdr = if text(r, "obs_at").is_empty() {
            "not observed".into()
        } else if r["present"] == 0 {
            format!("pane gone, {}", age(text(r, "obs_at"), now))
        } else {
            format!(
                "{}, {}",
                md(text(r, "obs_status")),
                age(text(r, "obs_at"), now)
            )
        };
        let cells = vec![
            label(r),
            md(text(r, "role")),
            md(text(r, "agent")),
            md(text(r, "launch")),
            md(text(r, "pane")),
            md(text(r, "status")),
            herdr,
            next(db, rid, now)?,
            refs(db, rid, ", ")?,
            md(text(r, "report")),
        ];
        writeln!(
            out,
            "| {} |",
            cells.into_iter().map(cell).collect::<Vec<_>>().join(" | ")
        )
        .expect("string");
    }
    out.push_str("\n## Planned lanes\n\n");
    if planned.is_empty() {
        out.push_str("None.\n");
    }
    for r in &planned {
        let nx = next(db, n(r, "id"), now)?;
        let rf = refs(db, n(r, "id"), ", ")?;
        write!(
            out,
            "- {}, {}: {}",
            label(r),
            md(text(r, "role")),
            if nx.is_empty() {
                "no next step recorded".into()
            } else {
                format!("next: {nx}")
            }
        )
        .expect("string");
        if !rf.is_empty() {
            write!(out, "; refs {rf}").expect("string");
        }
        out.push('\n');
    }
    let asks = rows(
        db,
        "with recursive sub(id) as(select ? union all select t.id from tasks t join sub on t.parent_id=sub.id) select e.id,coalesce(json_extract(e.data,'$.owner'),0) as owner,coalesce(json_extract(e.data,'$.blocking'),0) as blocking,t.name,t.id as from_id,coalesce(e.summary,'') as summary from events e join tasks t on t.id=e.task_id where e.kind='ask' and e.answered_by is null and t.status!='closed' and e.task_id in(select id from sub) order by coalesce(json_extract(e.data,'$.owner'),0) desc,e.id",
        &[&id],
    )?;
    out.push_str("\n## Open asks\n\n");
    if asks.is_empty() {
        out.push_str("None.\n");
    }
    for a in &asks {
        let mut tags = Vec::new();
        if a["owner"] == 1 {
            tags.push("owner");
        }
        if a["blocking"] == 1 {
            tags.push("blocking");
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!(" ({})", tags.join(", "))
        };
        writeln!(
            out,
            "- ask {}{tags} from {} (task {}): {}",
            n(a, "id"),
            md(text(a, "name")),
            n(a, "from_id"),
            md(text(a, "summary"))
        )
        .expect("string");
    }
    if let Some(s) = since {
        writeln!(
            out,
            "\n## Closed since the previous handover (event {}, {})\n",
            n(s, "id"),
            md(text(s, "created_at"))
        )
        .expect("string");
    } else {
        out.push_str("\n## Closed lanes (no earlier handover)\n\n");
    }
    if closed.is_empty() {
        out.push_str("None.\n");
    }
    for r in &closed {
        write!(
            out,
            "- {}, {}, closed {}: {}",
            label(r),
            md(text(r, "role")),
            md(text(r, "closed_at")),
            md_or(text(r, "final"), "no final summary")
        )
        .expect("string");
        if !text(r, "report").is_empty() {
            write!(out, " (report {})", md(text(r, "report"))).expect("string");
        }
        out.push('\n');
    }
    if !note.trim().is_empty() {
        writeln!(out, "\n## Note\n\n{}", md(note)).expect("string");
    }
    Ok(Rendered {
        text: out,
        counts: json!({"live":live.len(),"planned":planned.len(),"decisions":decisions.len(),"open_asks":asks.len(),"closed_since":closed.len()}),
    })
}
fn label(r: &Value) -> String {
    if n(r, "depth") > 1 {
        format!(
            "{} (task {}, under {})",
            md(text(r, "name")),
            n(r, "id"),
            md(text(r, "parent"))
        )
    } else {
        format!("{} (task {})", md(text(r, "name")), n(r, "id"))
    }
}
fn md_or_raw(s: &str, none: &str) -> String {
    if s.is_empty() { none.into() } else { s.into() }
}
fn render_documents(db: &Connection, id: i64, out: &mut String) -> Result<()> {
    let docs = rows(
        db,
        "select id,kind,name,version,captured,coalesce(source_host,'') as host,coalesce(source_path,'') as path,coalesce(reason,'') as reason,event_id,coalesce(sha256,'') as sha256 from documents where task_id=? and kind in('goal','plan') and id=(select id from documents d where d.task_id=documents.task_id and d.kind=documents.kind and d.name=documents.name order by case when d.kind='goal' then d.captured else 0 end desc,d.version desc limit 1) order by kind,name",
        &[&id],
    )?;
    if let Some(goal) = docs.iter().find(|d| d["kind"] == "goal") {
        if goal["captured"] == 1 {
            writeln!(
                out,
                "Goal (doc {}, v{}):",
                n(goal, "id"),
                n(goal, "version")
            )
            .expect("string");
            let body = db
                .query_row(
                    "select body from doc_blobs where sha256=?",
                    [text(goal, "sha256")],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_default();
            for line in body.lines().filter(|s| !s.trim().is_empty()).take(3) {
                writeln!(out, "{}", md(&line.chars().take(160).collect::<String>()))
                    .expect("string");
            }
            writeln!(out, "Full text: `taskr doc get {}`\n", n(goal, "id")).expect("string");
        } else {
            writeln!(
                out,
                "Goal: recorded at {} on {}, not captured ({})\n",
                md(text(goal, "path")),
                md_or(text(goal, "host"), "server host"),
                md(text(goal, "reason"))
            )
            .expect("string");
        }
    } else {
        writeln!(
            out,
            "Goal: none recorded; run `taskr doc set {id} goal --file PATH`\n"
        )
        .expect("string");
    }
    if let Some(plan) = docs
        .iter()
        .find(|d| d["kind"] == "plan" && text(d, "name").is_empty())
    {
        let eid = n(plan, "event_id");
        let (decisions,closed):(i64,i64)=db.query_row("with recursive tree(id) as(select ? union all select t.id from tasks t join tree on t.parent_id=tree.id) select coalesce(sum(kind='decision'),0),coalesce(sum(kind='closed'),0) from events where task_id in(select id from tree) and id>?",params![id,eid],|r|Ok((r.get(0)?,r.get(1)?)))?;
        writeln!(out,"Plan (doc {}, v{}, event {}): since then {decisions} decisions, {closed} lanes closed.\n`taskr doc get {}`\n",n(plan,"id"),n(plan,"version"),eid,n(plan,"id")).expect("string");
    }
    let named: Vec<_> = docs
        .iter()
        .filter(|d| d["kind"] == "plan" && !text(d, "name").is_empty())
        .map(|d| {
            format!(
                "{} (doc {}, v{})",
                md(text(d, "name")),
                n(d, "id"),
                n(d, "version")
            )
        })
        .collect();
    if !named.is_empty() {
        writeln!(out, "Documents: {}\n", named.join(", ")).expect("string");
    }
    Ok(())
}
