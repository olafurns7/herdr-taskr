use super::queries::{self as q, TREES, age, host_fresh, meta, one, rows};
use super::*;
use std::collections::{BTreeMap, BTreeSet};
mod render;
pub(super) fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or_default()
}
pub(super) fn n(v: &Value, k: &str) -> i64 {
    v[k].as_i64().unwrap_or_default()
}
pub(super) fn clip(s: &str, len: usize) -> String {
    if s.chars().count() > len {
        s.chars().take(len - 1).chain(['…']).collect()
    } else {
        s.into()
    }
}
pub(super) fn line(s: &str) -> String {
    s.lines()
        .find_map(|l| {
            let l = l.trim();
            (!l.is_empty()).then_some(l.to_string())
        })
        .unwrap_or_default()
}
pub(super) fn elapsed(s: &str) -> i64 {
    if s.is_empty() { 0 } else { age(s).max(0) }
}
pub(super) fn omit(v: &mut Value, k: &str, value: &str) {
    if !value.is_empty() {
        v[k] = json!(value);
    }
}
pub(super) fn mark(t: &Value) -> &'static str {
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

use taskr_core::store::plan::{owner_has_items, owner_value};
fn rank(kind: &str) -> i64 {
    match kind {
        "lead_blocked" => 1,
        "lead_gone" => 3,
        "host_stale" | "lead_unknown" => 4,
        "daemon_unhealthy" => 5,
        "lead_idle_results" | "parked_active" | "lead_unregistered_silent" => 6,
        _ => 0,
    }
}
// Go's typed glance records use declaration order; campaign maps use sorted keys.
fn go_json(v: &Value) -> String {
    if let Some(a) = v.as_array() {
        return format!("[{}]", a.iter().map(go_json).collect::<Vec<_>>().join(","));
    }
    let Some(map) = v.as_object() else {
        return taskr_core::compact_json(v).unwrap();
    };
    let keys: &[&str] = if map.contains_key("verdict") {
        &[
            "server_host",
            "caller_host",
            "now",
            "verdict",
            "needs_you",
            "attention",
            "campaigns",
            "quiet",
            "owner_notes_pending",
            "owner_note_root_ids",
        ]
    } else if map.contains_key("working") {
        &["working", "ready", "open"]
    } else if map.contains_key("root_ids") {
        &["root_ids", "count", "names"]
    } else if map.contains_key("activity_age_ms") {
        // activity_id stays in the snapshot for --brief but is not printed (Go parity).
        &[
            "spark",
            "id",
            "name",
            "host",
            "pane_id",
            "lanes",
            "lead",
            "lead_waiting",
            "last",
            "owner_note",
            "parked",
            "parked_active",
            "park_age_ms",
            "activity_age_ms",
        ]
    } else if map.contains_key("ask_id") {
        &[
            "asker_task_id",
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
        ]
    } else if map.contains_key("event_id") {
        &["event_id", "kind", "text", "age_ms"]
    } else {
        &[
            "kind",
            "campaign",
            "root_id",
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
    };
    format!(
        "{{{}}}",
        keys.iter()
            .filter_map(|k| map.get(*k).map(|v| format!("\"{k}\":{}", go_json(v))))
            .collect::<Vec<_>>()
            .join(",")
    )
}
pub(super) fn lead(db: &Connection, t: &Value) -> Result<(&'static str, String)> {
    if matches!(s(t, "status"), "planned" | "closed") {
        return Ok(("unknown", String::new()));
    }
    let live = if t["machine"].is_null() {
        meta(db, "daemon_heartbeat")?.is_some_and(|s| age(&s) < 30_000)
            && meta(db, "lead_listed_at")?.is_some_and(|s| age(&s) < 90_000)
    } else {
        host_fresh(db, s(t, "machine"))?
    };
    let status = if t["pane_id"].is_null() || t["lead_present"].is_null() || !live {
        "unknown"
    } else if t["lead_present"] == false {
        "gone"
    } else {
        match s(t, "lead_status") {
            "working" => "working",
            "idle" => "idle",
            "done" => "done",
            "blocked" => "blocked",
            _ => "unknown",
        }
    };
    Ok((status, s(t, "lead_observed_at").into()))
}
pub(super) fn sparks(db: &Connection, root: i64) -> Result<BTreeMap<i64, Vec<i64>>> {
    let at = taskr_core::frozen_now().expect("clock");
    let start_seconds = at.unix_timestamp().div_euclid(600) * 600 - 23 * 600;
    let start = taskr_core::store::stamp(
        time::OffsetDateTime::from_unix_timestamp(start_seconds).expect("bucket"),
    );
    let mut out = BTreeMap::new();
    for row in rows(
        db,
        "with recursive tree(root,id) as (select id,id from tasks where parent_id is null and ((?=0 and status!='closed') or id=?) union all select tree.root,t.id from tasks t join tree on t.parent_id=tree.id) select tree.root,cast(strftime('%s',e.created_at) as integer)/600-? as bucket,count(e.id) as count from tree left join events e on e.task_id=tree.id and e.created_at>=? and e.created_at<=? group by tree.root,2",
        vec![
            root.into(),
            root.into(),
            (start_seconds / 600).into(),
            start.into(),
            q::now().into(),
        ],
    )? {
        let buckets = out.entry(n(&row, "root")).or_insert_with(|| vec![0; 24]);
        if let Some(bucket) = row["bucket"].as_i64().filter(|b| (0..24).contains(b)) {
            buckets[bucket as usize] += n(&row, "count");
        }
    }
    Ok(out)
}
struct Root {
    v: Value,
    activity: String,
    activity_id: i64,
    active: bool,
    lead_at: String,
    park_id: i64,
}
pub(super) fn snapshot(db: &Connection) -> Result<Value> {
    let mut roots = BTreeMap::<i64, Root>::new();
    let mut tasks = BTreeMap::new();
    let mut hosts = BTreeMap::new();
    let mut asked = BTreeSet::new();
    let mut needs = vec![];
    let mut attention = vec![];
    let hub_at = meta(db, "daemon_heartbeat")?;
    let hub_live = hub_at.as_ref().is_some_and(|s| age(s) < 30_000);
    for mut t in rows(
        db,
        &format!(
            "{TREES}select tree.root,t.id,coalesce(t.parent_id,0) as parent,t.name,t.role,t.status,coalesce(l.pane_id,t.pane_id,'') as pane,coalesce(l.machine,t.machine,'') as host,coalesce(l.machine,'') as launch_host,t.current_launch_id,coalesce(t.waiting_until>?,0) as waiting,coalesce(l.observed_status,'') as observed_status,l.present,t.pane_id,t.machine,t.lead_status,t.lead_present,t.lead_observed_at,t.created_at from tree join tasks t on t.id=tree.id left join launches l on l.id=t.current_launch_id where t.status!='closed' order by tree.root,(t.id=tree.root) desc,t.id"
        ),
        vec![q::now().into()],
    )? {
        let tid = n(&t, "id");
        let mut m = mark(&t);
        if t["role"] == "gate" && t["current_launch_id"].is_null() && m == "working" {
            m = "planned";
        }
        t["mark"] = json!(m);
        if n(&t, "parent") == 0 {
            let (status, lead_at) = lead(db, &t)?;
            let mut v = json!({"id":tid,"name":t["name"],"lanes":{"working":0,"ready":0,"open":0},"lead":status,"activity_age_ms":0});
            omit(&mut v, "host", s(&t, "host"));
            omit(&mut v, "pane_id", s(&t, "pane"));
            if t["waiting"] == true {
                v["lead_waiting"] = json!(true);
            }
            roots.insert(
                tid,
                Root {
                    v,
                    activity: s(&t, "created_at").into(),
                    activity_id: 0,
                    active: false,
                    lead_at,
                    park_id: 0,
                },
            );
        }
        for host in [
            s(&t, "launch_host"),
            if n(&t, "parent") == 0 {
                s(&t, "host")
            } else {
                ""
            },
        ] {
            if !host.is_empty() {
                hosts.insert(host.to_string(), host_fresh(db, host)?);
            }
        }
        tasks.insert(tid, t);
    }
    let sparks = sparks(db, 0)?;
    for (&rid, r) in &mut roots {
        r.v["spark"] = json!(sparks[&rid]);
        if let Some(a) = one(
            db,
            &format!(
                "{TREES}select e.id,e.created_at from tree join events e on e.task_id=tree.id where tree.root=? and not (e.kind='prompt' and json_extract(e.data,'$.nudge') is not null) and not (e.kind='prompt_outcome' and exists(select 1 from events p where p.id=e.related_event_id and json_extract(p.data,'$.nudge') is not null)) order by e.id desc limit 1"
            ),
            vec![rid.into()],
        )? {
            r.activity = s(&a, "created_at").into();
            r.activity_id = n(&a, "id");
        }
        if let Some(ms) = one(
            db,
            &format!(
                "{TREES}select e.id,e.kind,coalesce(e.summary,'') as summary,e.created_at,coalesce(json_extract(e.data,'$.key'),'') as key,coalesce(json_extract(e.data,'$.value'),'') as value,coalesce(json_extract(e.data,'$.owner'),0) as owner from tree join tasks t on t.id=tree.id join events e on e.task_id=t.id where tree.root=? and (e.kind in ('ready','done','fail','handover','adopt','decision') or (e.kind in ('note','next') and t.parent_id is null) or (e.kind='ref' and json_extract(e.data,'$.key') in ('pr','release','tag','merged') and coalesce(json_extract(e.data,'$.value'),'')!='')) order by e.id desc limit 1"
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
            r.v["last"] = json!({"event_id":ms["id"],"kind":ms["kind"],"text":clip(&line(&text),120),"age_ms":elapsed(s(&ms,"created_at"))});
        }
        if let Some(park) = one(
            db,
            "select id,created_at,json_extract(data,'$.value') as value from events where task_id=? and kind='ref' and json_extract(data,'$.key')='glance.state' order by id desc limit 1",
            vec![rid.into()],
        )? && park["value"] == "parked"
        {
            r.v["parked"] = json!(true);
            r.park_id = n(&park, "id");
            // Bookkeeping on the root itself (notes, handovers, refs, by the lead or the hub)
            // is not activity after a park; lane events and prompts to the lead are.
            r.activity_id = one(
                db,
                &format!(
                    "{TREES}select max(e.id) as id from tree join events e on e.task_id=tree.id where tree.root=? and (e.task_id!=tree.root or e.kind='prompt') and not (e.kind='prompt' and json_extract(e.data,'$.nudge') is not null)"
                ),
                vec![rid.into()],
            )?
            .map_or(0, |a| n(&a, "id"));
            let ms = elapsed(s(&park, "created_at"));
            if ms != 0 {
                r.v["park_age_ms"] = json!(ms);
            }
        }
    }
    for t in tasks.values_mut().filter(|t| n(t, "parent") != 0) {
        if !s(t, "launch_host").is_empty()
            && !hosts[s(t, "launch_host")]
            && matches!(s(t, "status"), "open" | "ready")
        {
            t["mark"] = json!("unknown");
        }
        let r = roots.get_mut(&n(t, "root")).unwrap();
        r.v["lanes"]["open"] = json!(n(&r.v["lanes"], "open") + 1);
        if matches!(s(t, "mark"), "working" | "ready") {
            let m = s(t, "mark");
            r.v["lanes"][m] = json!(n(&r.v["lanes"], m) + 1);
        }
        if matches!(
            s(t, "mark"),
            "working" | "ready" | "blocked" | "missing" | "unknown"
        ) {
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
        let mut need = json!({"asker_task_id":t["id"],"kind":"owner_ask","campaign":r.v["name"],"root_id":r.v["id"],"age_ms":elapsed(s(&ask,"since")),"since":ask["since"],"ask_id":ask["id"],"blocking":ask["blocking"],"asker_waiting":t["waiting"]});
        omit(&mut need, "host", s(t, "host"));
        omit(&mut need, "pane_id", s(t, "pane"));
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
    let mut owner_ids = vec![];
    for (&rid, r) in &mut roots {
        let notes = rows(
            db,
            "select id,coalesce(summary,'') as text,created_at from events where task_id=? and kind='note' and json_extract(data,'$.owner')=1 order by id desc",
            vec![rid.into()],
        )?;
        if let Some(note) = notes.first() {
            r.v["owner_note"] = json!({"event_id":note["id"],"kind":"note","text":clip(&line(s(note,"text")),120),"age_ms":elapsed(s(note,"created_at"))});
        }
        if notes
            .iter()
            .find(|note| owner_value(s(note, "text")).is_some())
            .is_some_and(|note| owner_has_items(s(note, "text")))
            && !needs.iter().any(|n| n["root_id"] == rid)
        {
            owner_ids.push(rid);
        }
    }
    let cutoff = taskr_core::store::stamp(
        taskr_core::frozen_now().unwrap() - taskr_core::Duration::minutes(30),
    );
    // ponytail: one history scan, matching Go; use recipient-index ranges if history dominates.
    for b in rows(
        db,
        &format!(
            "{TREES},signals as materialized ({signals} and e.created_at<?),backlog as (select r.id as recipient,count(*) as count,min(e.created_at) as since,max(e.id) as newest from signals e join tasks r on r.id=e.recipient_task_id join tree on tree.id=r.id where r.parent_id is null and r.status!='closed' and e.id>r.acked_event_id group by r.id) select b.recipient,b.count,b.since,sender.name||' '||e.kind||': '||coalesce(e.summary,'') as text from backlog b join events e on e.id=b.newest join tasks sender on sender.id=e.task_id order by b.recipient",
            signals = crate::daemon::LEAD_IDLE_SIGNALS
        ),
        vec![cutoff.into()],
    )? {
        let t = &tasks[&n(&b, "recipient")];
        let r = roots.get_mut(&n(t, "root")).unwrap();
        if t["waiting"] == true || !matches!(s(&r.v, "lead"), "idle" | "done") {
            continue;
        }
        r.active = true;
        let mut v = json!({"kind":"lead_idle_results","campaign":r.v["name"],"root_id":r.v["id"],"text":clip(&line(s(&b,"text")),200),"age_ms":elapsed(s(&b,"since")),"recipient":t["name"],"recipient_id":t["id"],"count":b["count"],"waiting":t["waiting"]});
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
    for r in roots.values_mut() {
        r.v["activity_age_ms"] = json!(elapsed(&r.activity));
        if r.activity_id > 0 {
            r.v["activity_id"] = json!(r.activity_id);
        }
        r.active = r.active
            || !r.activity.is_empty() && n(&r.v, "activity_age_ms") < 43_200_000
            || s(&r.v, "pane_id").is_empty() && n(&r.v["lanes"], "open") > 0;
    }
    attention.retain(|a| {
        n(a, "root_id") == 0
            || roots[&n(a, "root_id")].active && roots[&n(a, "root_id")].v["parked"] != true
    });
    let mut ordered: Vec<_> = roots.values_mut().collect();
    ordered.sort_by(|a, b| {
        b.activity
            .cmp(&a.activity)
            .then(n(&a.v, "id").cmp(&n(&b.v, "id")))
    });
    let mut campaigns = vec![];
    let mut quiet_names = vec![];
    let mut quiet_ids = vec![];
    for r in ordered {
        if r.v["parked"] == true {
            if r.activity_id > r.park_id {
                r.v["parked_active"] = json!(true);
                let mut a = json!({"kind":"parked_active","campaign":r.v["name"],"root_id":r.v["id"],"text":"parked but active","age_ms":r.v["activity_age_ms"]});
                omit(&mut a, "since", &r.activity);
                omit(&mut a, "host", s(&r.v, "host"));
                omit(&mut a, "pane_id", s(&r.v, "pane_id"));
                attention.push(a);
            }
            campaigns.push(r.v.clone());
            continue;
        }
        if s(&r.v, "pane_id").is_empty() {
            r.v["lead"] = json!("unregistered");
        }
        if r.active {
            let (kind, text, since) = match s(&r.v, "lead") {
                "gone" => (
                    "lead_gone",
                    format!(
                        "lead pane {} is not in its host's agent list",
                        s(&r.v, "pane_id")
                    ),
                    r.lead_at.as_str(),
                ),
                "blocked" => (
                    "lead_blocked",
                    "Herdr sees an approval or question dialog in the lead's pane".into(),
                    r.lead_at.as_str(),
                ),
                "unknown" => (
                    "lead_unknown",
                    "lead liveness unknown: never observed or its host is not reporting".into(),
                    r.lead_at.as_str(),
                ),
                "unregistered"
                    if n(&r.v["lanes"], "open") > 0 && n(&r.v, "activity_age_ms") >= 7_200_000 =>
                {
                    (
                        "lead_unregistered_silent",
                        "lead unregistered and silent".into(),
                        r.activity.as_str(),
                    )
                }
                _ => ("", String::new(), ""),
            };
            if !kind.is_empty() {
                let mut a = json!({"kind":kind,"campaign":r.v["name"],"root_id":r.v["id"],"text":text,"age_ms":elapsed(since)});
                omit(&mut a, "since", since);
                omit(&mut a, "host", s(&r.v, "host"));
                omit(&mut a, "pane_id", s(&r.v, "pane_id"));
                attention.push(a);
            }
            if r.v["lead_waiting"] == true
                && !s(&r.v, "pane_id").is_empty()
                && matches!(s(&r.v, "lead"), "idle" | "done" | "working")
            {
                r.v["lead"] = json!("waiting");
            }
            campaigns.push(r.v.clone());
        } else {
            quiet_names.push(r.v["name"].clone());
            quiet_ids.push(r.v["id"].clone());
        }
    }
    needs.sort_by(|a, b| {
        (b["blocking"] == true)
            .cmp(&(a["blocking"] == true))
            .then(n(b, "age_ms").cmp(&n(a, "age_ms")))
    });
    attention.sort_by(|a, b| {
        rank(s(a, "kind"))
            .cmp(&rank(s(b, "kind")))
            .then(n(b, "age_ms").cmp(&n(a, "age_ms")))
            .then(n(a, "root_id").cmp(&n(b, "root_id")))
            .then(s(a, "host").cmp(s(b, "host")))
    });
    let mut verdict = "rolling";
    for a in &attention {
        if matches!(
            s(a, "kind"),
            "lead_blocked"
                | "lead_gone"
                | "lead_idle_results"
                | "parked_active"
                | "lead_unregistered_silent"
        ) {
            verdict = "attention";
        } else if verdict == "rolling" {
            verdict = "unknown";
        }
    }
    if !needs.is_empty() {
        verdict = "needs_you";
    }
    Ok(
        json!({"server_host":taskr_core::store::local_machine(),"caller_host":taskr_core::store::caller_machine().unwrap_or_default(),"now":q::now(),"verdict":verdict,"needs_you":needs,"attention":attention,"campaigns":campaigns,"quiet":{"root_ids":quiet_ids,"count":quiet_names.len(),"names":quiet_names},"owner_notes_pending":owner_ids.len(),"owner_note_root_ids":owner_ids}),
    )
}
/// `--since`: all digits is a cursor (campaigns with a newer `activity_id`), else a Go
/// duration (campaigns active within it). Returns (cursor, max activity age in ms).
fn since_filter(since: &str) -> Result<(i64, i64)> {
    if since.is_empty() {
        return Ok((-1, i64::MAX));
    }
    if since.bytes().all(|b| b.is_ascii_digit()) {
        let cursor = since
            .parse()
            .map_err(|_| usage("--since: event id out of range"))?;
        return Ok((cursor, i64::MAX));
    }
    let ns = taskr_core::goflag::parse_duration(since)
        .ok()
        .filter(|ns| *ns > 0)
        .ok_or_else(|| {
            usage("--since must be the header's cursor= (an event id) or a Go duration such as 30m or 2h")
        })?;
    Ok((-1, ns / 1_000_000))
}
/// One `frame()` at width 120, untagged; NEEDS YOU and ATTENTION always print in full.
fn brief(mut v: Value, since: &str) -> Result<String> {
    let (cursor, max_age) = since_filter(since)?;
    let campaigns = v["campaigns"].as_array().unwrap();
    v["cursor"] = json!(
        campaigns
            .iter()
            .map(|c| n(c, "activity_id"))
            .max()
            .unwrap_or(0)
    );
    let campaigns = v["campaigns"].as_array_mut().unwrap();
    let before = campaigns.len();
    campaigns.retain(|c| n(c, "activity_id") > cursor && n(c, "activity_age_ms") <= max_age);
    v["unchanged"] = json!(before - v["campaigns"].as_array().unwrap().len());
    Ok(render::frame(&v, 120, usize::MAX, 0, true).join("\n") + "\n")
}
pub fn run(f: &FlagSet) -> Result<()> {
    if f.get_bool("brief") && f.get_bool("watch") {
        return Err(usage("give --brief or --watch, not both"));
    }
    if f.get_bool("brief") && f.json() {
        return Err(usage(
            "--brief prints plain text; drop --json and unset TASKR_FORMAT=json",
        ));
    }
    if f.was_set("since") && !f.get_bool("brief") {
        return Err(usage("--since needs --brief"));
    }
    if f.get_bool("brief") {
        since_filter(f.get_string("since"))?;
    }
    let every = f.get_int("every");
    if !(1_000_000_000..=300_000_000_000).contains(&every)
        || f.was_set("every") && !f.get_bool("watch")
    {
        return Err(usage(
            "--every requires --watch and must be between 1s and 5m",
        ));
    }
    if f.get_bool("watch") && taskr_core::store::caller_machine().is_some() {
        return Err(usage("glance --watch runs on the invoking host"));
    }
    if f.get_bool("watch")
        && let Some(result) = crate::net::glance_watch_client()
    {
        let client = result.map_err(|(code, text)| Error { code, text })?;
        return watch(std::time::Duration::from_nanos(every as u64), || {
            let mut v = client
                .snapshot(std::time::Duration::from_nanos(every as u64))
                .map_err(|(code, text)| Error { code, text })?;
            v["now"] = json!(q::now());
            Ok(v)
        });
    }
    let db = open().map_err(|mut e| {
        if f.get_bool("watch") {
            e.code = ExitCode::Watch;
        }
        e
    })?;
    let fetch = || {
        db.execute_batch("begin")?;
        let v = snapshot(&db)?;
        db.execute_batch("rollback")?;
        Ok(v)
    };
    if f.get_bool("watch") {
        return watch(std::time::Duration::from_nanos(every as u64), fetch);
    }
    let v = fetch().map_err(|mut e: Error| {
        if f.get_bool("watch") {
            e.code = ExitCode::Watch;
        }
        e
    })?;
    if f.get_bool("brief") {
        print!("{}", brief(v, f.get_string("since"))?);
        return Ok(());
    }
    println!("{}{}", if f.json() { "" } else { "j1 " }, go_json(&v));
    Ok(())
}

fn watch(every: std::time::Duration, mut fetch: impl FnMut() -> Result<Value>) -> Result<()> {
    use std::io::{IsTerminal, Write};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};
    let tty = std::io::stdout().is_terminal()
        && std::io::stdin().is_terminal()
        && std::env::var("TERM").as_deref() != Ok("dumb");
    let stopped = Arc::new(AtomicBool::new(false));
    let mut signals = Vec::new();
    let mut run = || -> std::io::Result<()> {
        if tty {
            for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
                signals.push(signal_hook::flag::register(signal, stopped.clone())?);
            }
        }
        let mut out = std::io::stdout().lock();
        let mut last = None;
        let mut fetched_at = Instant::now();
        while !stopped.load(Ordering::Relaxed) {
            let start = Instant::now();
            let v = fetch();
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            let error = match v {
                Ok(v) => {
                    last = Some(v);
                    fetched_at = Instant::now();
                    None
                }
                Err(e) if tty => Some(e.text),
                Err(e) => return Err(std::io::Error::other(e.text)),
            };
            let size = tty
                .then(|| rustix::termios::tcgetwinsize(std::io::stdout()).ok())
                .flatten();
            let width = size
                .filter(|s| s.ws_col > 0)
                .map_or(80, |s| usize::from(s.ws_col));
            let height = if tty {
                size.filter(|s| s.ws_row > 0)
                    .map_or(24, |s| usize::from(s.ws_row))
            } else {
                usize::MAX
            };
            let mut lines = Vec::new();
            if let Some(error) = error {
                lines.push(render::pad(&format!("taskr: {error}"), width));
            }
            if let Some(v) = &last {
                lines.extend(render::frame(
                    v,
                    width,
                    height.saturating_sub(lines.len()),
                    fetched_at.elapsed().as_millis().min(i64::MAX as u128) as i64,
                    false,
                ));
            }
            if tty {
                write!(out, "\x1b[H\x1b[2J")?;
                write!(out, "{}", lines.join("\n"))?;
            } else {
                writeln!(out, "{}", lines.join("\n"))?;
            }
            out.flush()?;
            if !tty {
                break;
            }
            while start.elapsed() < every && !stopped.load(Ordering::Relaxed) {
                std::thread::sleep(
                    (every - start.elapsed().min(every)).min(Duration::from_millis(50)),
                );
            }
        }
        Ok(())
    };
    let result = run();
    for signal in signals {
        signal_hook::low_level::unregister(signal);
    }
    result.map_err(|e| Error {
        code: ExitCode::Watch,
        text: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn glance_json_omits_activity_id() {
        let c = json!({"activity_id":7,"activity_age_ms":5,"name":"x"});
        assert_eq!(go_json(&c), r#"{"name":"x","activity_age_ms":5}"#);
    }
    #[test]
    fn owner_context_boundaries() {
        assert_eq!(
            owner_value("DONE: built OWNER: ship NOW: wait").as_deref(),
            Some("ship")
        );
        assert!(owner_value("(OWNER: glued").is_none());
        assert_eq!(owner_value("x\u{85}OWNER: ship").as_deref(), Some("ship"));
        assert!(!owner_has_items("OWNER: nothing now (waiting)"));
        assert!(owner_has_items("OWNER: nothing until approval"));
        assert!(owner_has_items("OWNER:"));
    }

    /// Root 1 (hub host) with lane 2; root 3 is another hub-host root.
    fn fixture() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(taskr_core::schema::SCHEMA).unwrap();
        let at = q::now();
        for (id, parent, role) in [
            (1, None, "orchestrator"),
            (2, Some(1), "implementer"),
            (3, None, "orchestrator"),
        ] {
            db.execute("insert into tasks(id,parent_id,name,role,status,pane_id,created_at,updated_at) values(?,?,?,?,'open','w:p1',?,?)",params![id,parent,format!("t{id}"),role,at,at]).unwrap();
        }
        db
    }
    fn event(db: &Connection, task: i64, kind: &str, data: &str) {
        db.execute("insert into events(task_id,recipient_task_id,kind,summary,data,created_at) values(?,?,?,'x',?,?)",params![task,task,kind,data,q::now()]).unwrap();
    }
    fn parked_active(db: &Connection) -> bool {
        let v = snapshot(db).unwrap();
        let root = v["campaigns"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == 1)
            .unwrap();
        assert_eq!(root["parked"], true);
        root["parked_active"] == true
    }
    #[test]
    fn park_ignores_root_bookkeeping() {
        let park = r#"{"key":"glance.state","value":"parked"}"#;
        for (task, kind, data, active) in [
            (1, "note", "{}", false),
            (1, "handover", "{}", false),
            (1, "note", r#"{"owner":true}"#, false),
            (1, "decision", "{}", false),
            (1, "next", "{}", false),
            (1, "ref", r#"{"key":"pr","value":"1"}"#, false),
            (2, "ready", "{}", true),
            (1, "prompt", "{}", true),
        ] {
            let db = fixture();
            event(&db, 2, "note", "{}");
            event(&db, 1, "ref", park);
            assert!(!parked_active(&db), "park alone");
            event(&db, task, kind, data);
            assert_eq!(parked_active(&db), active, "{task} {kind} {data}");
        }
    }
    #[test]
    fn withdrawn_ask_leaves_needs_you() {
        let db = fixture();
        for _ in 0..2 {
            db.execute("insert into events(task_id,recipient_task_id,kind,summary,data,created_at) values(2,1,'ask','merge?','{\"owner\":true}',?)",params![q::now()]).unwrap();
        }
        assert_eq!(
            snapshot(&db).unwrap()["needs_you"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        // Authority is covered by tests/withdraw.rs; this records the withdraw directly.
        for (ask, text) in [(1, "stale"), (2, "stale too")] {
            db.execute("insert into events(task_id,recipient_task_id,kind,summary,data,related_event_id,created_at) values(2,2,'answer',?,'{\"owner\":false,\"withdrawn\":true}',?,?)",params![text,ask,q::now()]).unwrap();
            db.execute(
                "update events set answered_by=last_insert_rowid() where id=?",
                [ask],
            )
            .unwrap();
        }
        assert!(
            snapshot(&db).unwrap()["needs_you"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let mut f = super::super::flags("asks", false);
        f.parse(&["--all".into()], 0, 0).unwrap();
        let rows = super::super::queries::ask_rows(&db, &f).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r["withdrawn"] == true));
        let detail = super::super::campaign::snapshot(&db, 1, 1, false).unwrap();
        assert!(
            detail["asks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["text"] == "withdrawn: stale")
        );
        assert!(detail["decisions"].as_array().unwrap().is_empty());
    }
}
