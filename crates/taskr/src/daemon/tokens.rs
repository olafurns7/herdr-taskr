use super::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PaneToken {
    pane: String,
    state: String,
    round: i64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct WorkspaceToken {
    pub campaign: Option<String>,
    pub parent: Option<String>,
}
#[derive(Default)]
pub(super) struct Tokens {
    panes: Option<BTreeMap<i64, PaneToken>>,
    workspaces: Option<BTreeMap<String, WorkspaceToken>>,
    failed_workspaces: Option<BTreeMap<String, WorkspaceToken>>,
    owner_asks: Option<BTreeMap<String, i64>>,
    failed_owner_asks: Option<BTreeMap<String, i64>>,
}
fn task_token(status: &str, waiting: bool, asks: i64) -> &str {
    if matches!(status, "done" | "failed") {
        "done"
    } else if waiting {
        "waiting"
    } else if asks > 0 {
        "ask"
    } else if status == "ready" {
        "ready"
    } else {
        "open"
    }
}
pub(super) fn run(sock: &str, args: &[&str], timeout: Duration) -> Result<()> {
    let out = herdr::command(sock, args, timeout).map_err(transport)?;
    if out.code != Some(0) {
        return Err(transport(format!("exit status {}", out.code.unwrap_or(-1))));
    }
    Ok(())
}
fn list(sock: &str, kind: &str, field: &str, id: &str) -> Result<Vec<Value>> {
    let out = herdr::command(sock, &[kind, "list"], Duration::from_secs(10)).map_err(transport)?;
    if out.code != Some(0) {
        return Err(transport(format!(
            "herdr {kind} list failed: exit status {}",
            out.code.unwrap_or(-1)
        )));
    }
    let value: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| transport(format!("herdr {kind} list returned malformed JSON: {e}")))?;
    let rows = value["result"][field]
        .as_array()
        .ok_or_else(|| transport(format!("herdr {kind} list returned no result.{field}")))?;
    if rows
        .iter()
        .any(|v| v[id].as_str().is_none_or(str::is_empty))
    {
        return Err(transport(format!("herdr {kind} list entry without {id}")));
    }
    Ok(rows.clone())
}
pub(super) fn wanted_workspaces(
    db: &db::Connection,
    host: Option<&str>,
) -> Result<BTreeMap<String, WorkspaceToken>> {
    let mut stmt=db.prepare("with recursive local_tasks as (select t.id,case when l.pane_id is not null then l.workspace_id else t.workspace_id end as workspace,coalesce(l.present,1) as present from tasks t left join launches l on l.id=t.current_launch_id where t.status not in('closed','planned') and coalesce(l.pane_id,t.pane_id) is not null and(case when l.id is null then t.machine else l.machine end) is ?), ancestors(id,ancestor) as(select id,id from local_tasks union select a.id,t.parent_id from ancestors a join tasks t on t.id=a.ancestor where t.parent_id is not null) select t.id,t.workspace,r.id,r.name,t.present from local_tasks t join ancestors a on a.id=t.id join tasks r on r.id=a.ancestor where r.parent_id is null order by t.id")?;
    let rows = stmt
        .query_map([host], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut want = BTreeMap::new();
    let mut roots = BTreeMap::new();
    let mut root_workspaces = BTreeMap::new();
    for (id, workspace, root, name, present) in rows {
        let Some(workspace) = workspace.filter(|s| !s.is_empty() && present) else {
            continue;
        };
        if id == root {
            root_workspaces.insert(root, workspace.clone());
        }
        match roots.get(&workspace) {
            None => {
                roots.insert(workspace.clone(), root);
                want.insert(
                    workspace,
                    WorkspaceToken {
                        campaign: Some(name),
                        parent: None,
                    },
                );
            }
            Some(previous) if *previous != root => {
                roots.insert(workspace.clone(), 0);
                want.insert(workspace, WorkspaceToken::default());
            }
            _ => {}
        }
    }
    for (workspace, root) in roots {
        if let Some(parent) = root_workspaces.get(&root) {
            if parent == &workspace {
                want.insert(workspace, WorkspaceToken::default());
            } else {
                want.get_mut(&workspace).expect("workspace").parent = Some(parent.clone());
            }
        }
    }
    Ok(want)
}
pub(super) fn wanted_owner_asks(
    db: &db::Connection,
    host: Option<&str>,
) -> Result<BTreeMap<String, i64>> {
    let mut stmt=db.prepare("select coalesce(l.pane_id,t.pane_id),count(*) from tasks t left join launches l on l.id=t.current_launch_id join events o on o.task_id=t.id and o.kind='ask' and o.answered_by is null and coalesce(json_extract(o.data,'$.owner'),0) where t.status not in('closed','planned') and coalesce(l.pane_id,t.pane_id) is not null and(case when l.id is null then t.machine else l.machine end) is ? and coalesce(l.present,1) group by 1")?;
    Ok(stmt
        .query_map([host], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?)
}
impl Tokens {
    pub fn fallback(&mut self) {
        self.failed_workspaces = None;
        self.failed_owner_asks = None;
        self.owner_asks = None;
    }
    pub fn reattach(&mut self) {
        *self = Self {
            panes: Some(BTreeMap::new()),
            ..Self::default()
        };
    }
    pub fn retry_workspaces(&mut self) {
        self.failed_workspaces = None;
    }
    pub fn panes(&mut self, db: &db::Connection, sock: &str, log: &Log) -> Result<usize> {
        let mut stmt=db.prepare("select t.id,coalesce(l.pane_id,t.pane_id),t.status,coalesce(t.waiting_until>?,0),(select count(*) from events a where a.task_id=t.id and a.kind='ask' and a.answered_by is null),(select count(*) from events p where p.task_id=t.id and p.kind='prompt' and p.launch_id is t.current_launch_id),coalesce(l.present,1) from tasks t left join launches l on l.id=t.current_launch_id where t.status not in('closed','planned') and coalesce(l.pane_id,t.pane_id) is not null and(case when l.id is null then t.machine else l.machine end) is null order by t.id")?;
        let rows = stmt
            .query_map([store::now()], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, bool>(6)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let cur: BTreeMap<_, _> = rows
            .into_iter()
            .filter(|r| r.6)
            .map(|(id, pane, status, waiting, asks, round, _)| {
                (
                    id,
                    PaneToken {
                        pane,
                        state: task_token(&status, waiting, asks).into(),
                        round,
                    },
                )
            })
            .collect();
        let Some(previous) = &mut self.panes else {
            self.panes = Some(cur);
            return Ok(0);
        };
        let mut count = 0;
        for (id, t) in cur {
            if previous.get(&id) == Some(&t) {
                continue;
            }
            if let Err(e) = run(
                sock,
                &[
                    "pane",
                    "report-metadata",
                    &t.pane,
                    "--source",
                    "taskr",
                    "--token",
                    &format!("taskr_state={}", t.state),
                    "--token",
                    &format!("taskr_round={}", t.round),
                ],
                Duration::from_secs(10),
            ) {
                log.line(&format!("token write for task {id} failed: {}", e.message));
                continue;
            }
            previous.insert(id, t);
            count += 1;
        }
        Ok(count)
    }
    pub fn workspaces(
        &mut self,
        want: BTreeMap<String, WorkspaceToken>,
        sock: &str,
        log: &Log,
    ) -> usize {
        if self.failed_workspaces.as_ref() == Some(&want)
            || self.workspaces.as_ref().is_some_and(|old| {
                old.iter()
                    .all(|(key, v)| want.get(key).cloned().unwrap_or_default() == *v)
                    && want
                        .iter()
                        .all(|(key, v)| old.get(key).cloned().unwrap_or_default() == *v)
            })
        {
            return 0;
        }
        let rows = match list(sock, "workspace", "workspaces", "workspace_id") {
            Ok(rows) => rows,
            Err(e) => {
                self.failed_workspaces = Some(want);
                log.line(&format!("workspace token list failed: {}", e.message));
                return 0;
            }
        };
        self.failed_workspaces = None;
        let mut listed: BTreeMap<_, _> = rows
            .into_iter()
            .map(|v| {
                (
                    v["workspace_id"].as_str().unwrap().to_string(),
                    WorkspaceToken {
                        campaign: v["tokens"]["taskr_campaign"].as_str().map(String::from),
                        parent: v["tokens"]["taskr_parent"].as_str().map(String::from),
                    },
                )
            })
            .collect();
        let mut count = 0;
        for (workspace, old) in listed.clone() {
            let t = want.get(&workspace).cloned().unwrap_or_default();
            if old == t {
                continue;
            }
            let mut args = vec![
                "workspace".into(),
                "report-metadata".into(),
                workspace.clone(),
                "--source".into(),
                "taskr".into(),
            ];
            for (key, value) in [("taskr_campaign", &t.campaign), ("taskr_parent", &t.parent)] {
                if let Some(value) = value {
                    args.extend(["--token".into(), format!("{key}={value}")]);
                } else {
                    args.extend(["--clear-token".into(), key.into()]);
                }
            }
            if let Err(e) = run(
                sock,
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
                Duration::from_secs(10),
            ) {
                log.line(&format!(
                    "workspace token write for {workspace} failed: {}",
                    e.message
                ));
                continue;
            }
            listed.insert(workspace, t);
            count += 1;
        }
        for (workspace, t) in want {
            listed.entry(workspace).or_insert(t);
        }
        // Empty pairs on listed, unowned workspaces compare like Go's zero map value.
        listed.retain(|_, t| *t != WorkspaceToken::default());
        self.workspaces = Some(listed);
        count
    }
    pub fn owner_asks(&mut self, want: BTreeMap<String, i64>, sock: &str, log: &Log) -> usize {
        if self.failed_owner_asks.as_ref() == Some(&want) || self.owner_asks.as_ref() == Some(&want)
        {
            return 0;
        }
        let rows = match list(sock, "pane", "panes", "pane_id") {
            Ok(rows) => rows,
            Err(e) => {
                self.failed_owner_asks = Some(want);
                log.line(&format!("owner ask token list failed: {}", e.message));
                return 0;
            }
        };
        self.failed_owner_asks = None;
        let mut listed = BTreeMap::new();
        let mut panes = BTreeMap::new();
        for v in rows {
            let p = v["pane_id"].as_str().unwrap().to_string();
            let value = v["tokens"]["taskr_owner_ask"].as_str();
            panes.insert(p.clone(), value.is_some());
            if let Some(s) = value {
                listed.insert(p, s.parse::<i64>().ok().filter(|n| *n > 0).unwrap_or(-1));
            }
        }
        let mut count = 0;
        for (pane, has) in panes {
            let desired = *want.get(&pane).unwrap_or(&0);
            let old = *listed.get(&pane).unwrap_or(&0);
            let extra = if desired > 0 && (!has || old != desired) {
                vec!["--token".into(), format!("taskr_owner_ask={desired}")]
            } else if desired == 0 && has {
                vec!["--clear-token".into(), "taskr_owner_ask".into()]
            } else {
                continue;
            };
            let mut args = vec!["pane", "report-metadata", &pane, "--source", "taskr"];
            args.extend(extra.iter().map(String::as_str));
            if let Err(e) = run(sock, &args, Duration::from_secs(10)) {
                log.line(&format!(
                    "owner ask token write for {pane} failed: {}",
                    e.message
                ));
                continue;
            }
            if desired > 0 {
                listed.insert(pane, desired);
            } else {
                listed.remove(&pane);
            }
            count += 1;
        }
        self.owner_asks = Some(listed);
        count
    }
}
pub(super) fn notify(db: &db::Connection, sock: &str, log: &Log) -> Result<usize> {
    Ok(notify_claimed(claim(db, None)?, sock, log).0)
}
pub(super) fn claim(db: &db::Connection, host: Option<&str>) -> Result<Vec<(i64, String)>> {
    let mut stmt=db.prepare("select e.id,coalesce(e.summary,'') from events e join tasks t on t.id=e.task_id where e.kind='ask' and e.answered_by is null and json_extract(e.data,'$.owner')=1 and t.machine is ? and (json_extract(e.data,'$.blocking')=1 or (? and e.created_at<=? and t.status!='closed')) and not exists(select 1 from meta m where m.key='notified:'||e.id) order by e.id")?;
    let rows = stmt
        .query_map(
            params![
                host,
                checkin::enabled(),
                store::stamp(taskr_core::frozen_now().expect("clock") - time::Duration::hours(4))
            ],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    let mut claimed = Vec::new();
    for (id, summary) in rows {
        match db.execute(
            "insert into meta(key,value) values(?,?) on conflict(key) do nothing",
            params![format!("notified:{id}"), store::now()],
        ) {
            Ok(1) => claimed.push((id, summary)),
            Ok(_) => {}
            Err(_) => {}
        }
    }
    Ok(claimed)
}
pub(super) fn notify_claimed(
    asks: Vec<(i64, String)>,
    sock: &str,
    log: &Log,
) -> (usize, Option<Error>) {
    let mut count = 0;
    let mut first_error = None;
    for (id, summary) in asks {
        if let Err(e) = run(
            sock,
            &[
                "notification",
                "show",
                "taskr: decision needed",
                "--body",
                &truncate(&summary, 120),
                "--sound",
                "request",
            ],
            Duration::from_secs(10),
        ) {
            log.line(&format!("notify ask {id} failed: {}", e.message));
            if first_error.is_none() {
                first_error = Some(e);
            }
            continue;
        }
        log.line(&format!("notified ask {id}"));
        count += 1;
    }
    (count, first_error)
}
pub(super) fn truncate(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n {
        s
    } else {
        s.chars().take(n - 1).chain(['…']).collect()
    }
}
