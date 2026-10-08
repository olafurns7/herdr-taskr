use super::queries::{self as q, TREES, age, host_fresh, meta, one, rows};
use super::*;
use std::collections::{BTreeMap, BTreeSet};
mod render;
fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or_default()
}
fn n(v: &Value, k: &str) -> i64 {
    v[k].as_i64().unwrap_or_default()
}
fn clip(s: &str, len: usize) -> String {
    if s.chars().count() > len {
        s.chars().take(len - 1).chain(['…']).collect()
    } else {
        s.into()
    }
}
fn line(s: &str) -> String {
    s.lines()
        .find_map(|l| {
            let l = l.trim();
            (!l.is_empty()).then_some(l.to_string())
        })
        .unwrap_or_default()
}
fn elapsed(s: &str) -> i64 {
    if s.is_empty() { 0 } else { age(s).max(0) }
}
fn omit(v: &mut Value, k: &str, value: &str) {
    if !value.is_empty() {
        v[k] = json!(value);
    }
}
fn rank(kind: &str) -> i64 {
    match kind {
        "lane_blocked" | "lead_blocked" => 1,
        "lane_failed" => 2,
        "lane_missing" | "lead_gone" => 3,
        "lane_unknown" | "host_stale" | "lead_unknown" => 4,
        "daemon_unhealthy" => 5,
        "results_waiting" | "owner_unclear" => 6,
        _ => 0,
    }
}
fn mark(t: &Value) -> &'static str {
    match s(t, "status") {
        "failed" => "failed",
        "done" => "done",
        "planned" => "planned",
        _ => {
            if s(t, "status") == "open" && t["present"] == false {
                "missing"
            } else if s(t, "observed_status") == "blocked" && t["present"] != false {
                "blocked"
            } else if s(t, "status") == "ready" {
                "ready"
            } else {
                "working"
            }
        }
    }
}
fn owner_value(text: &str) -> Option<String> {
    let value = text.trim().strip_prefix("OWNER:")?;
    let end = ["DONE:", "HAPPENED:", "NOW:"]
        .iter()
        .filter_map(|marker| {
            value
                .match_indices(marker)
                .find(|(i, _)| {
                    *i == 0 || value.as_bytes()[i - 1] == b' ' || value.as_bytes()[i - 1] == b'\n'
                })
                .map(|(i, _)| {
                    if i > 0 && value.as_bytes()[i - 1] == b' ' {
                        i - 1
                    } else {
                        i
                    }
                })
        })
        .min()
        .unwrap_or(value.len());
    Some(value[..end].trim().to_string())
}
fn marker(value: &str, num: usize) -> Option<(usize, usize)> {
    let number = num.to_string();
    for (at, _) in value.match_indices(&number) {
        if at != 0 && !value[..at].chars().last().unwrap().is_ascii_whitespace() {
            continue;
        }
        if at == 0 && num > 1 {
            continue;
        }
        let suffix = &value[at + number.len()..];
        let end = if suffix.starts_with(')') {
            at + number.len() + 1
        } else if suffix.starts_with('.')
            && (suffix.len() == 1 || suffix[1..].starts_with(|c: char| c.is_ascii_whitespace()))
        {
            at + number.len() + 1 + suffix[1..].chars().next().map_or(0, char::len_utf8)
        } else {
            continue;
        };
        let start = if at == 0 {
            0
        } else {
            at - value[..at].chars().last().unwrap().len_utf8()
        };
        return Some((start, end));
    }
    None
}
fn owner_items(text: &str) -> Vec<String> {
    let Some(mut value) = owner_value(text) else {
        return vec![];
    };
    let lower = value.to_lowercase();
    if let Some(mut rest) = lower.strip_prefix("nothing") {
        if rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            let trimmed = rest.trim_start();
            for word in ["yet", "new", "now"] {
                if let Some(after) = trimmed.strip_prefix(word) {
                    rest = after;
                    break;
                }
            }
        }
        let rest = rest.trim_start();
        if rest.is_empty() || rest.starts_with(['.', '(']) {
            return vec![];
        }
    }
    let mut items = vec![];
    let mut num = 1;
    loop {
        let Some((a, b)) = marker(&value, num) else {
            if num == 1 {
                return vec![value];
            }
            let v = value.trim();
            if !v.is_empty() {
                items.push(v.into());
            }
            return items;
        };
        let v = value[..a].trim();
        if !v.is_empty() {
            items.push(v.into());
        }
        value = value[b..].to_string();
        num += 1;
    }
}
fn unclear(text: &str) -> bool {
    let lower = text.to_lowercase();
    let value = lower.trim().trim_end_matches(['.', ';']).trim();
    matches!(
        value,
        "nothing"
            | "nothing urgent"
            | "nothing to do"
            | "nothing needed"
            | "no decision"
            | "no action"
            | "no decision needed"
            | "no action needed"
            | "no decision now"
            | "no decision yet"
            | "no action now"
            | "no action yet"
            | "no decision needed now"
            | "no decision needed yet"
            | "no action needed now"
            | "no action needed yet"
    )
}
// Go emits these typed records in declaration order rather than map-key order.
fn go_json(v: &Value) -> String {
    if let Some(a) = v.as_array() {
        return format!("[{}]", a.iter().map(go_json).collect::<Vec<_>>().join(","));
    }
    let Some(map) = v.as_object() else {
        return taskr_core::compact_json(v).unwrap();
    };
    let keys: &[&str] = if map.contains_key("verdict") {
        &[
            "now",
            "verdict",
            "needs_you",
            "attention",
            "campaigns",
            "quiet",
        ]
    } else if map.contains_key("working") {
        &["working", "ready", "open"]
    } else if map.contains_key("with_backlog") {
        &["count", "with_backlog", "names"]
    } else if map.contains_key("activity_age_ms") {
        &[
            "id",
            "name",
            "host",
            "pane_id",
            "lanes",
            "lead",
            "lead_waiting",
            "last",
            "activity_age_ms",
        ]
    } else if map.contains_key("ask_id") || map.contains_key("items") {
        &[
            "kind",
            "campaign",
            "root_id",
            "host",
            "pane_id",
            "age_ms",
            "since",
            "ask_id",
            "text",
            "blocking",
            "asker",
            "asker_waiting",
            "also",
            "note_id",
            "items",
        ]
    } else if map.contains_key("since")
        || map.contains_key("lane_id")
        || map.contains_key("campaign")
        || map.contains_key("host")
    {
        &[
            "kind",
            "campaign",
            "root_id",
            "note_id",
            "lane",
            "lane_id",
            "text",
            "age_ms",
            "since",
            "host",
            "pane_id",
            "recipient",
            "recipient_id",
            "count",
            "waiting",
        ]
    } else {
        &["kind", "text", "age_ms"]
    };
    format!(
        "{{{}}}",
        keys.iter()
            .filter_map(|k| map.get(*k).map(|v| format!("\"{k}\":{}", go_json(v))))
            .collect::<Vec<_>>()
            .join(",")
    )
}
#[derive(Clone)]
struct Root {
    v: Value,
    activity: String,
    active: bool,
    lead_at: String,
}
pub(super) fn snapshot(db: &Connection) -> Result<Value> {
    let mut roots: BTreeMap<i64, Root> = BTreeMap::new();
    let mut tasks: BTreeMap<i64, Value> = BTreeMap::new();
    let mut hosts: BTreeMap<String, bool> = BTreeMap::new();
    let mut asked = BTreeSet::new();
    let mut needs = vec![];
    let mut attention = vec![];
    let hub_at = meta(db, "daemon_heartbeat")?;
    let hub_live = hub_at.as_ref().is_some_and(|s| age(s) < 30_000);
    let listed = meta(db, "lead_listed_at")?.is_some_and(|s| age(&s) < 90_000);
    let ts = rows(
        db,
        &format!(
            "{TREES}select tree.root,t.id,coalesce(t.parent_id,0) as parent,t.name,t.role,t.status,coalesce(l.pane_id,t.pane_id,'') as pane,coalesce(l.machine,t.machine,'') as host,coalesce(l.machine,'') as launch_host,t.current_launch_id,coalesce(t.waiting_until>?,0) as waiting,coalesce(l.observed_status,'') as observed_status,coalesce(l.observed_at,'') as observed_at,l.present,t.pane_id,t.machine,t.lead_status,t.lead_present,t.lead_observed_at from tree join tasks t on t.id=tree.id left join launches l on l.id=t.current_launch_id where t.status!='closed' order by tree.root,(t.id=tree.root) desc,t.id"
        ),
        vec![q::now().into()],
    )?;
    for mut t in ts {
        let tid = n(&t, "id");
        let parent = n(&t, "parent");
        let last = one(
            db,
            "select kind,coalesce(summary,'') as summary,created_at from events where task_id=? and kind not in ('next','ref','herdr','got','prompt','prompt_outcome','doc') order by id desc limit 1",
            vec![tid.into()],
        )?;
        if let Some(mut last) = last {
            last["summary"] = json!(clip(s(&last, "summary"), 300));
            t["last"] = last;
        }
        let mut m = mark(&t);
        if s(&t, "role") == "gate" && t["current_launch_id"].is_null() && m == "working" {
            m = "planned";
        }
        t["mark"] = json!(m);
        if parent == 0 {
            let mut v = json!({"id":tid,"name":t["name"],"lanes":{"working":0,"ready":0,"open":0},"lead":"unknown","activity_age_ms":0});
            omit(&mut v, "host", s(&t, "host"));
            omit(&mut v, "pane_id", s(&t, "pane"));
            if t["waiting"] == true {
                v["lead_waiting"] = json!(true);
            }
            let live = if t["machine"].is_null() {
                hub_live && listed
            } else {
                host_fresh(db, s(&t, "machine"))?
            };
            if t["status"] != "planned"
                && !t["pane_id"].is_null()
                && !t["lead_present"].is_null()
                && live
            {
                v["lead"] = json!(if t["lead_present"] == false {
                    "gone"
                } else {
                    match s(&t, "lead_status") {
                        "working" => "working",
                        "idle" => "idle",
                        "done" => "done",
                        "blocked" => "blocked",
                        _ => "unknown",
                    }
                });
            }
            roots.insert(
                tid,
                Root {
                    v,
                    activity: String::new(),
                    active: false,
                    lead_at: if t["status"] == "planned" {
                        String::new()
                    } else {
                        s(&t, "lead_observed_at").into()
                    },
                },
            );
        }
        if !s(&t, "launch_host").is_empty() {
            hosts.insert(
                s(&t, "launch_host").into(),
                host_fresh(db, s(&t, "launch_host"))?,
            );
        }
        if parent == 0 && !s(&t, "host").is_empty() {
            hosts.insert(s(&t, "host").into(), host_fresh(db, s(&t, "host"))?);
        }
        tasks.insert(tid, t);
    }
    // Tree heads include closed descendants as in the Go projection.
    for (&rid, r) in &mut roots {
        let activity = one(
            db,
            &format!(
                "{TREES}select e.created_at from tree join events e on e.task_id=tree.id where tree.root=? order by e.id desc limit 1"
            ),
            vec![rid.into()],
        )?;
        r.activity = activity.as_ref().map_or("", |v| s(v, "created_at")).into();
        if let Some(ms) = one(
            db,
            &format!(
                "{TREES}select e.id,e.kind,coalesce(e.summary,'') as summary,e.created_at,coalesce(json_extract(e.data,'$.key'),'') as key,coalesce(json_extract(e.data,'$.value'),'') as value,coalesce(json_extract(e.data,'$.owner'),0) as owner from tree join tasks t on t.id=tree.id join events e on e.task_id=t.id where tree.root=? and (e.kind in ('ready','done','fail','handover','adopt','decision') or (e.kind='note' and t.parent_id is null) or (e.kind='ref' and json_extract(e.data,'$.key') in ('pr','release','tag','merged') and coalesce(json_extract(e.data,'$.value'),'')!='')) order by e.id desc limit 1"
            ),
            vec![rid.into()],
        )? {
            let text = if ms["kind"] == "ref" {
                format!("{} {}", s(&ms, "key"), s(&ms, "value"))
            } else if ms["kind"] == "note" && ms["owner"] == 1 {
                owner_value(s(&ms, "summary")).unwrap_or_else(|| s(&ms, "summary").into())
            } else {
                s(&ms, "summary").into()
            };
            r.v["last"] = json!({"kind":ms["kind"],"text":clip(&line(&text),120),"age_ms":elapsed(s(&ms,"created_at"))});
        }
    }
    for t in tasks.values_mut().filter(|t| n(t, "parent") != 0) {
        if !s(t, "launch_host").is_empty()
            && !hosts[s(t, "launch_host")]
            && (t["status"] == "open" || t["status"] == "ready")
        {
            t["mark"] = json!("unknown");
        }
        let r = roots.get_mut(&n(t, "root")).unwrap();
        let lanes = &mut r.v["lanes"];
        lanes["open"] = json!(n(lanes, "open") + 1);
        let m = s(t, "mark");
        if matches!(m, "working" | "ready") {
            lanes[m] = json!(n(lanes, m) + 1);
        }
        if matches!(m, "working" | "ready" | "blocked" | "missing" | "unknown") {
            r.active = true;
        }
    }
    for ask in rows(
        db,
        &format!(
            "{TREES}select e.id,e.task_id,coalesce(e.summary,'') as text,e.created_at as since,coalesce(json_extract(e.data,'$.blocking'),0) as blocking from tree join tasks t on t.id=tree.id join events e on e.task_id=t.id where t.status!='closed' and e.kind='ask' and e.answered_by is null and json_extract(e.data,'$.owner')=1 order by e.id"
        ),
        vec![],
    )? {
        let t = &tasks[&n(&ask, "task_id")];
        let r = roots.get_mut(&n(t, "root")).unwrap();
        r.active = true;
        let mut need = json!({"kind":"owner_ask","campaign":r.v["name"],"root_id":r.v["id"],"age_ms":elapsed(s(&ask,"since")),"since":ask["since"],"ask_id":ask["id"],"blocking":ask["blocking"],"asker_waiting":t["waiting"]});
        omit(&mut need, "host", s(&r.v, "host"));
        omit(&mut need, "pane_id", s(&r.v, "pane_id"));
        omit(&mut need, "text", s(&ask, "text"));
        if n(t, "parent") != 0 {
            omit(&mut need, "asker", s(t, "name"));
            if !asked.contains(&n(t, "id"))
                && matches!(s(t, "mark"), "failed" | "blocked" | "missing")
            {
                need["also"] = json!([format!("lane {}", s(t, "mark"))]);
            }
            asked.insert(n(t, "id"));
        }
        needs.push(need);
    }
    let mut seen = BTreeSet::new();
    let mut todos = vec![];
    for note in rows(
        db,
        "select t.id,e.id as note_id,coalesce(e.summary,'') as text,e.created_at as since from tasks t join events e on e.task_id=t.id where t.parent_id is null and t.status!='closed' and e.kind='note' and json_extract(e.data,'$.owner')=1 order by t.id,e.id desc",
        vec![],
    )? {
        let rid = n(&note, "id");
        if seen.contains(&rid) || !s(&note, "text").trim().starts_with("OWNER:") {
            continue;
        }
        seen.insert(rid);
        let items = owner_items(s(&note, "text"));
        if items.is_empty() {
            continue;
        }
        let r = roots.get_mut(&rid).unwrap();
        r.active = true;
        let mut v = json!({"kind":"owner_todo","campaign":r.v["name"],"root_id":rid,"note_id":note["note_id"],"since":note["since"],"age_ms":elapsed(s(&note,"since"))});
        if items.len() == 1
            && marker(&owner_value(s(&note, "text")).unwrap(), 1).is_none()
            && unclear(&items[0])
        {
            v["kind"] = json!("owner_unclear");
            v["text"] = json!(items[0]);
            attention.push(v);
        } else {
            v["items"] = json!(items);
            omit(&mut v, "host", s(&r.v, "host"));
            omit(&mut v, "pane_id", s(&r.v, "pane_id"));
            todos.push(v);
        }
    }
    todos.sort_by_key(|v| n(v, "note_id"));
    needs.extend(todos);
    for t in tasks.values().filter(|t| n(t, "parent") != 0) {
        let kind = match s(t, "mark") {
            "failed" => "lane_failed",
            "blocked" => "lane_blocked",
            "missing" => "lane_missing",
            "unknown" => "lane_unknown",
            _ => continue,
        };
        if asked.contains(&n(t, "id")) && kind != "lane_unknown" {
            continue;
        }
        let r = &roots[&n(t, "root")];
        let last = if t["last"]["kind"] == "ask" {
            "it asked a question"
        } else {
            s(&t["last"], "summary")
        };
        let (text, since) = match kind {
            "lane_failed" => (
                if last.is_empty() {
                    "the lane reported fail".into()
                } else {
                    last.into()
                },
                s(&t["last"], "created_at"),
            ),
            "lane_blocked" => (
                format!(
                    "Herdr sees an approval or question dialog in its pane{}",
                    if last.is_empty() {
                        String::new()
                    } else {
                        format!("; last: {last}")
                    }
                ),
                s(t, "observed_at"),
            ),
            "lane_missing" => (
                format!(
                    "pane {} is gone; last: {}",
                    if s(t, "pane").is_empty() {
                        "?"
                    } else {
                        s(t, "pane")
                    },
                    if last.is_empty() {
                        "nothing reported"
                    } else {
                        last
                    }
                ),
                s(t, "observed_at"),
            ),
            _ => (
                "the lane's host has not reported".into(),
                s(t, "observed_at"),
            ),
        };
        let mut v = json!({"kind":kind,"campaign":r.v["name"],"root_id":r.v["id"],"lane":t["name"],"lane_id":t["id"],"text":clip(&line(&text),200),"age_ms":elapsed(since)});
        omit(&mut v, "since", since);
        omit(&mut v, "host", s(t, "host"));
        omit(&mut v, "pane_id", s(t, "pane"));
        attention.push(v);
    }
    let cutoff =
        taskr_core::store::stamp(taskr_core::frozen_now().unwrap() - time_duration_minutes(30));
    let backlog = format!(
        "{TREES},signals as materialized (select e.id,e.recipient_task_id,e.created_at from events e where e.created_at<? and (e.kind in ('ready','done','fail') or (e.kind='prompt_outcome' and json_extract(e.data,'$.outcome')='no_receipt' and e.launch_id is (select current_launch_id from tasks where id=e.task_id)) or (e.kind='herdr' and (json_extract(e.data,'$.quota')='limit' or (json_extract(e.data,'$.reason')='stall' and e.launch_id is (select current_launch_id from tasks where id=e.task_id)))))),backlog as (select r.id as recipient,count(*) as count,min(e.created_at) as since,max(e.id) as newest from signals e join tasks r on r.id=e.recipient_task_id join tree on tree.id=r.id where r.status!='closed' and e.id>r.acked_event_id group by r.id) select b.recipient,b.count,b.since,sender.name||' '||e.kind||': '||coalesce(e.summary,'') as text from backlog b join events e on e.id=b.newest join tasks sender on sender.id=e.task_id order by b.recipient"
    );
    for b in rows(db, &backlog, vec![cutoff.into()])? {
        let t = &tasks[&n(&b, "recipient")];
        let r = &roots[&n(t, "root")];
        let mut v = json!({"kind":"results_waiting","campaign":r.v["name"],"root_id":r.v["id"],"text":clip(&line(s(&b,"text")),200),"age_ms":elapsed(s(&b,"since")),"recipient":t["name"],"recipient_id":t["id"],"count":b["count"],"waiting":t["waiting"]});
        omit(&mut v, "since", s(&b, "since"));
        omit(&mut v, "host", s(t, "host"));
        omit(&mut v, "pane_id", s(t, "pane"));
        attention.push(v);
    }
    for (host, fresh) in hosts {
        if !fresh {
            let at = meta(db, &format!("daemon_heartbeat:{host}"))?.unwrap_or_default();
            let mut v = json!({"kind":"host_stale","host":host,"text":format!("{host} daemon has not reported; its lanes show unknown"),"age_ms":elapsed(&at)});
            omit(&mut v, "since", &at);
            attention.push(v);
        }
    }
    if !hub_live {
        let mut v = json!({"kind":"daemon_unhealthy","text":if hub_at.is_some(){"the taskr daemon's heartbeat stopped; Herdr events are not arriving"}else{"the taskr daemon has no live Herdr connection; no heartbeat"},"age_ms":elapsed(hub_at.as_deref().unwrap_or_default())});
        omit(&mut v, "since", hub_at.as_deref().unwrap_or_default());
        attention.push(v);
    }
    let mut backlog_roots = BTreeSet::new();
    for r in roots.values_mut() {
        r.v["activity_age_ms"] = json!(elapsed(&r.activity));
        r.active = r.active || !r.activity.is_empty() && n(&r.v, "activity_age_ms") < 43_200_000;
    }
    attention.retain(|a| {
        let rid = n(a, "root_id");
        if rid != 0 && !roots[&rid].active {
            backlog_roots.insert(rid);
            false
        } else {
            true
        }
    });
    let mut ordered: Vec<_> = roots.values().collect();
    ordered.sort_by(|a, b| {
        b.activity
            .cmp(&a.activity)
            .then(n(&a.v, "id").cmp(&n(&b.v, "id")))
    });
    let mut campaigns = vec![];
    let mut quiet_names = vec![];
    let mut quiet_backlog = 0;
    for r in ordered {
        if r.active {
            let kind = match s(&r.v, "lead") {
                "gone" => "lead_gone",
                "blocked" => "lead_blocked",
                "unknown" => "lead_unknown",
                _ => "",
            };
            if !kind.is_empty() {
                let text=match kind{"lead_gone"=>format!("lead pane {} is not in its host's agent list",s(&r.v,"pane_id")),"lead_blocked"=>"Herdr sees an approval or question dialog in the lead's pane".into(),_=>"lead liveness unknown: no pane, never observed, or its host is not reporting".into()};
                let mut v = json!({"kind":kind,"campaign":r.v["name"],"root_id":r.v["id"],"text":text,"age_ms":elapsed(&r.lead_at)});
                omit(&mut v, "since", &r.lead_at);
                omit(&mut v, "host", s(&r.v, "host"));
                omit(&mut v, "pane_id", s(&r.v, "pane_id"));
                attention.push(v);
            }
            campaigns.push(r.v.clone());
        } else {
            quiet_names.push(r.v["name"].clone());
            if backlog_roots.contains(&n(&r.v, "id")) {
                quiet_backlog += 1;
            }
        }
    }
    needs.sort_by(|a, b| {
        let ab = a["kind"] == "owner_ask" && a["blocking"] == true;
        let bb = b["kind"] == "owner_ask" && b["blocking"] == true;
        bb.cmp(&ab).then(n(b, "age_ms").cmp(&n(a, "age_ms")))
    });
    attention.sort_by(|a, b| {
        rank(s(a, "kind"))
            .cmp(&rank(s(b, "kind")))
            .then(n(b, "age_ms").cmp(&n(a, "age_ms")))
            .then(n(a, "lane_id").cmp(&n(b, "lane_id")))
            .then(s(a, "host").cmp(s(b, "host")))
    });
    let mut verdict = "rolling";
    for a in &attention {
        if matches!(
            s(a, "kind"),
            "lane_failed"
                | "lane_blocked"
                | "lane_missing"
                | "lead_blocked"
                | "lead_gone"
                | "results_waiting"
                | "owner_unclear"
        ) {
            verdict = "attention";
        } else if verdict == "rolling" {
            verdict = "unknown";
        }
    }
    if quiet_backlog > 0 {
        verdict = "attention";
    }
    if !needs.is_empty() {
        verdict = "needs_you";
    }
    Ok(
        json!({"now":q::now(),"verdict":verdict,"needs_you":needs,"attention":attention,"campaigns":campaigns,"quiet":{"count":quiet_names.len(),"with_backlog":quiet_backlog,"names":quiet_names}}),
    )
}
fn time_duration_minutes(n: i64) -> taskr_core::Duration {
    taskr_core::Duration::minutes(n)
}
pub fn run(f: &FlagSet) -> Result<()> {
    let every = f.get_int("every");
    if !(1_000_000_000..=300_000_000_000).contains(&every)
        || f.was_set("every") && !f.get_bool("watch")
    {
        return Err(usage(
            "--every requires --watch and must be between 1s and 5m",
        ));
    }
    if f.get_bool("watch") && std::env::var("TASKR_RPC_CALLER").is_ok_and(|s| !s.is_empty()) {
        return Err(usage("glance --watch runs on the invoking host"));
    }
    use std::io::IsTerminal;
    if f.get_bool("watch")
        && std::io::stdout().is_terminal()
        && std::io::stdin().is_terminal()
        && std::env::var("TERM").is_ok_and(|s| s != "dumb")
    {
        return Err(Error {
            code: ExitCode::NotImplemented,
            text: "not implemented: glance TTY watch (R4)".into(),
        });
    }
    if f.get_bool("watch")
        && let Some(result) =
            crate::net::glance_watch_snapshot(std::time::Duration::from_nanos(every as u64))
    {
        let mut v = result.map_err(|(code, text)| Error { code, text })?;
        v["now"] = json!(q::now());
        println!("{}", render::frame(&v, 80, usize::MAX, 0).join("\n"));
        return Ok(());
    }
    let db = open()?;
    db.execute_batch("begin")?;
    let v = snapshot(&db)?;
    db.execute_batch("rollback")?;
    if f.get_bool("watch") {
        println!("{}", render::frame(&v, 80, usize::MAX, 0).join("\n"));
    } else {
        println!("{}{}", if f.json() { "" } else { "j1 " }, go_json(&v));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_parsing() {
        assert!(owner_items("OWNER: nothing now.").is_empty());
        assert_eq!(
            owner_items("OWNER: 1. approve 2. ship NOW: working"),
            ["approve", "ship"]
        );
        assert_eq!(
            owner_items("OWNER: nothing until 2027"),
            ["nothing until 2027"]
        );
    }
}
