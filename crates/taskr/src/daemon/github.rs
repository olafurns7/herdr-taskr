//! Read-only GitHub PR poller for the ledger (hub) daemon. A worker thread runs only
//! `gh api graphql`; the resident thread applies the diff in one short transaction.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    process::{Command, Stdio},
    sync::mpsc,
};

const BATCH: usize = 30;
const PER_MINUTE: usize = 20;
const TIMEOUT: Duration = Duration::from_secs(20);
const MINUTE: Duration = Duration::from_secs(60);
const CAP: Duration = Duration::from_secs(15 * 60);
/// SQL predicate for poller bookkeeping that is not campaign activity: `pr` events
/// other than sub `merged`, their generated `after` notifications (a `pr:` target
/// whose sub is not material; `merged` stays activity, as does a task target), and
/// the refs the poller writes.
pub(crate) const PR_NOISE: &str = "((e.kind='pr' and coalesce(json_extract(e.data,'$.sub'),'')!='merged') or (e.kind='after' and coalesce(json_extract(e.data,'$.target'),'') like 'pr:%' and coalesce(json_extract(e.data,'$.on'),'')!='merged') or (e.kind='ref' and coalesce(json_extract(e.data,'$.source'),'')='github'))";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Pr {
    repo: String,
    number: u64,
}
impl Pr {
    fn key(&self) -> String {
        format!("{}#{}", self.repo, self.number)
    }
    fn short(&self) -> String {
        format!(
            "{}#{}",
            self.repo.rsplit('/').next().unwrap_or(""),
            self.number
        )
    }
}
fn repo_ok(repo: &str) -> bool {
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    repo.split_once('/').is_some_and(|(o, n)| ok(o) && ok(n))
}
/// `N` or `#N` (the default repo) or `owner/repo#N`; anything else is not polled.
pub(super) fn parse(value: &str, default: &str) -> Option<Pr> {
    let value = value.trim();
    let (repo, number) = match value.rsplit_once('#') {
        Some(("", n)) => (default, n),
        Some((repo, n)) => (repo, n),
        None => (default, value),
    };
    let number = number
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0 && number.bytes().all(|b| b.is_ascii_digit()))?;
    repo_ok(repo).then(|| Pr {
        repo: repo.into(),
        number,
    })
}
/// An `after pr:` target as stored: `pr:owner/repo#N`. `N` and `#N` need the
/// hub's default repo in watch.json.
pub(crate) fn target(value: &str) -> Result<String> {
    let default = state_dir()
        .ok()
        .and_then(|dir| config(&dir))
        .unwrap_or_default();
    parse(value, &default)
        .map(|pr| format!("pr:{}", pr.key()))
        .ok_or_else(|| {
            store::usage(format!(
                "after: pr:{value} is not a PR; use pr:owner/repo#N, or pr:N with default_repo in watch.json"
            ))
        })
}
/// `~/.local/state/taskr/watch.json` with `"github": {"enabled": true}`; returns the
/// default repo ("" when unset, so only `owner/repo#N` links are polled).
pub(super) fn config(dir: &Path) -> Option<String> {
    let v: Value = serde_json::from_slice(&fs::read(dir.join("watch.json")).ok()?).ok()?;
    (v["github"]["enabled"] == true)
        .then(|| v["github"]["default_repo"].as_str().unwrap_or("").into())
}
pub(super) struct Link {
    task: i64,
    pr: Pr,
}
/// Tasks whose latest `pr` ref parses: open tasks, and closed ones whose PR an
/// unfired `after pr:` subscription of an open waiter still targets.
fn links(db: &db::Connection, default: &str) -> Result<Vec<Link>> {
    let subs = db
        .prepare("select distinct substr(s.target,4) from subscriptions s join tasks w on w.id=s.waiter_task_id where s.fired_at is null and s.target like 'pr:%' and w.status!='closed'")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<std::collections::HashSet<_>, _>>()?;
    let open = if subs.is_empty() {
        "t.status!='closed'"
    } else {
        "1"
    };
    let mut stmt = db.prepare(&format!("select t.id,t.status='closed',json_extract(e.data,'$.value') from tasks t join events e on e.id=(select max(x.id) from events x where x.task_id=t.id and x.kind='ref' and json_extract(x.data,'$.key')='pr') where {open} order by t.id"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(task, closed, v)| {
            parse(&v?, default)
                .filter(|pr| !closed || subs.contains(&pr.key()))
                .map(|pr| Link { task, pr })
        })
        .collect())
}
/// Last-seen state per PR in meta `pr_state:<repo>#<n>`; checks are keyed on the head.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct State {
    head: String,
    state: String,
    merge: String,
    threads: u64,
    /// The checks verdict last reported (or settled on pending) for this head.
    checks: String,
    /// The verdict of the latest polls and how many polls in a row saw it.
    seen: String,
    stable: u32,
    done: bool,
}
fn state(db: &db::Connection, pr: &Pr) -> Result<State> {
    Ok(meta(db, &format!("pr_state:{}", pr.key()))?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default())
}
/// The GraphQL text for one batch: alias `pN` per PR, inline because `isRequired`
/// takes the PR number.
pub(super) fn query(batch: &[Pr]) -> String {
    let mut q = String::from("query{");
    for (i, pr) in batch.iter().enumerate() {
        let (owner, name) = pr.repo.split_once('/').expect("validated repo");
        let n = pr.number;
        q.push_str(&format!("p{i}:repository(owner:\"{owner}\",name:\"{name}\"){{pullRequest(number:{n}){{state merged headRefOid mergeStateStatus mergeCommit{{oid}} commits(last:1){{nodes{{commit{{statusCheckRollup{{contexts(first:50){{pageInfo{{hasNextPage}} nodes{{__typename ...on CheckRun{{name status conclusion isRequired(pullRequestNumber:{n})}} ...on StatusContext{{context state isRequired(pullRequestNumber:{n})}}}}}}}}}}}}}} reviewThreads(first:50){{pageInfo{{hasNextPage}} nodes{{isResolved}}}}}}}}"));
    }
    q.push('}');
    q
}
/// `green`, `failed` or `pending` over the required contexts (all of them when none
/// is required); None when GitHub has no rollup or no contexts yet, or more than one
/// page of them, or any context lost to a field error (null, or no requiredness or
/// result), where a failed check may be unseen (no change).
fn verdict(node: &Value) -> Option<&'static str> {
    let contexts = &node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
    if contexts["pageInfo"]["hasNextPage"] != false {
        return None;
    }
    let one = |c: &Value| -> Option<(bool, &'static str)> {
        let v = match c["__typename"].as_str()? {
            "StatusContext" => match c["state"].as_str()? {
                "SUCCESS" => "green",
                "FAILURE" | "ERROR" => "failed",
                _ => "pending",
            },
            "CheckRun" => match c["status"].as_str()? {
                "COMPLETED" => match c["conclusion"].as_str()? {
                    "SUCCESS" | "NEUTRAL" | "SKIPPED" => "green",
                    _ => "failed",
                },
                _ => "pending",
            },
            _ => return None,
        };
        Some((c["isRequired"].as_bool()?, v))
    };
    let all = contexts["nodes"]
        .as_array()?
        .iter()
        .map(one)
        .collect::<Option<Vec<_>>>()?;
    let any_required = all.iter().any(|(r, _)| *r);
    let got: Vec<&str> = all
        .into_iter()
        .filter(|(r, _)| *r || !any_required)
        .map(|(_, v)| v)
        .collect();
    if got.is_empty() {
        return None;
    }
    Some(if got.contains(&"failed") {
        "failed"
    } else if got.contains(&"pending") {
        "pending"
    } else {
        "green"
    })
}
/// The next state and the subs (name, summary text) that changed.
fn diff(prev: &State, node: &Value) -> (State, Vec<(&'static str, String)>) {
    let mut next = prev.clone();
    let mut subs = vec![];
    let head = node["headRefOid"].as_str().unwrap_or("");
    if head != prev.head {
        next.head = head.into();
        next.checks.clear();
        next.seen.clear();
        next.stable = 0;
    }
    next.state = node["state"].as_str().unwrap_or("").into();
    if node["merged"] == true || next.state == "MERGED" {
        let commit = node["mergeCommit"]["oid"].as_str().unwrap_or("");
        next.done = true;
        subs.push(("merged", format!("merged as {}", seven(commit))));
        return (next, subs);
    }
    if next.state == "CLOSED" {
        next.done = true;
        subs.push(("closed", "closed without merge".into()));
        return (next, subs);
    }
    if let Some(v) = verdict(node) {
        if v == next.seen {
            next.stable += 1;
        } else {
            next.seen = v.into();
            next.stable = 1;
        }
        // Two stable polls before a checks sub; a settled pending re-arms it.
        if next.stable >= 2 && next.checks != v {
            next.checks = v.into();
            match v {
                "green" => subs.push(("checks_green", "checks green".into())),
                "failed" => subs.push(("checks_failed", "checks failed".into())),
                _ => {}
            }
        }
    }
    let merge = node["mergeStateStatus"].as_str().unwrap_or("UNKNOWN");
    if merge != "UNKNOWN" && merge != prev.merge {
        next.merge = merge.into();
        match merge {
            "DIRTY" => subs.push(("dirty", "has merge conflicts".into())),
            "BEHIND" => subs.push(("behind", "is behind its base".into())),
            "BLOCKED" => subs.push(("blocked", "is blocked".into())),
            _ => {}
        }
    }
    // Only a complete list counts: a further page, or a node lost to a field error,
    // keeps the previous count.
    let threads = &node["reviewThreads"];
    if let Some(threads) = threads["nodes"].as_array().filter(|nodes| {
        threads["pageInfo"]["hasNextPage"] == false
            && nodes.iter().all(|t| t["isResolved"].is_boolean())
    }) {
        next.threads = threads.iter().filter(|t| t["isResolved"] == false).count() as u64;
        if next.threads > prev.threads {
            subs.push((
                "thread_opened",
                format!("has {} open review threads", next.threads),
            ));
        } else if next.threads == 0 && prev.threads > 0 {
            subs.push(("threads_clear", "review threads all resolved".into()));
        }
    }
    (next, subs)
}
fn seven(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}
/// One `pr` event: the linked task's lead gets it (a root gets its own); a closed
/// recipient's inbox write is skipped and the event stays in the log.
fn emit(
    tx: &db::Connection,
    task: i64,
    pr: &Pr,
    sub: &str,
    text: &str,
    mut data: Value,
) -> Result<i64> {
    let t = store::task(tx, task)?;
    let to = t.parent.unwrap_or(t.id);
    let to = (store::task(tx, to)?.status != "closed").then_some(to);
    let head = data["head"].as_str().unwrap_or("").to_string();
    let summary = if head.is_empty() || sub == "merged" || sub == "closed" {
        format!("PR {} {text}", pr.short())
    } else {
        format!("PR {} {text} at {}", pr.short(), seven(&head))
    };
    data["pr"] = json!(pr.key());
    data["sub"] = json!(sub);
    store::event(
        tx,
        store::Event {
            task,
            to,
            kind: "pr",
            summary: &summary,
            data: Some(data),
            ..store::Event::default()
        },
    )
}
/// Write each ref whose latest value differs, tagged as the poller's.
fn refs(tx: &db::Connection, task: i64, pairs: &[(&str, &str)]) -> Result<()> {
    for (k, v) in pairs {
        let have: Option<String> = tx.query_row("select coalesce(json_extract(data,'$.value'),'') from events where task_id=? and kind='ref' and json_extract(data,'$.key')=? order by id desc limit 1",params![task,k],|r|r.get(0)).optional()?;
        if have.as_deref().unwrap_or("") != *v {
            store::event(
                tx,
                store::Event {
                    task,
                    kind: "ref",
                    summary: &format!("{k}={v}"),
                    data: Some(json!({"key":k,"value":v,"source":"github"})),
                    ..store::Event::default()
                },
            )?;
        }
    }
    Ok(())
}
/// A linked task's `pr.state` and `pr.ci` refs from its PR's state. An empty `pr.ci`
/// clears it: a new head or a newly linked PR has no verdict yet. On the same head an
/// unknown result keeps `seen`, so the ref does not change.
fn task_refs(tx: &db::Connection, task: i64, s: &State) -> Result<()> {
    let ci = match s.seen.as_str() {
        "green" => "pass",
        "failed" => "fail",
        "pending" => "running",
        _ => "",
    };
    let state = s.state.to_lowercase();
    refs(tx, task, &[("pr.state", state.as_str()), ("pr.ci", ci)])
}
/// Apply one poll's JSON in one transaction. Links are re-read inside it, since the
/// fetch ran outside any lock. Returns the number of `pr` events written.
pub(super) fn apply(
    db: &mut db::Connection,
    batch: &[Pr],
    default: &str,
    v: &Value,
) -> Result<usize> {
    let errors = v["errors"].as_array().cloned().unwrap_or_default();
    store::transaction(db, |tx| {
        let links = links(tx, default)?;
        let mut n = 0;
        for (i, pr) in batch.iter().enumerate() {
            let alias = format!("p{i}");
            let tasks: Vec<i64> = links
                .iter()
                .filter(|l| l.pr == *pr)
                .map(|l| l.task)
                .collect();
            if tasks.is_empty() {
                continue;
            }
            let key = format!("pr_state:{}", pr.key());
            let prev = state(tx, pr)?;
            let node = &v["data"][&alias]["pullRequest"];
            if node.is_null() {
                // Gone only on GitHub's NOT_FOUND for this alias; other nulls change nothing.
                let Some(error) = errors
                    .iter()
                    .find(|e| e["path"][0] == alias.as_str() && e["type"] == "NOT_FOUND")
                else {
                    continue;
                };
                let reason = error["message"].as_str().unwrap_or("not found");
                for &task in &tasks {
                    emit(
                        tx,
                        task,
                        pr,
                        "closed",
                        &format!("is gone: {reason}"),
                        json!({"head":prev.head,"reason":reason}),
                    )?;
                    refs(tx, task, &[("pr", "")])?;
                    n += 1;
                }
                tx.execute("delete from meta where key=?", [&key])?;
                continue;
            }
            let (next, subs) = diff(&prev, node);
            set_meta(tx, &key, &serde_json::to_string(&next)?)?;
            for &task in &tasks {
                for (sub, text) in &subs {
                    let mut data = json!({"head":next.head,"checks":next.seen,"merge_state":next.merge,"threads_open":next.threads});
                    if *sub == "merged" {
                        data["merge_commit"] = node["mergeCommit"]["oid"].clone();
                    }
                    emit(tx, task, pr, sub, text, data)?;
                    n += 1;
                }
                task_refs(tx, task, &next)?;
            }
        }
        Ok(n)
    })
}
pub(super) enum Fetch {
    Json(Value),
    Auth(String),
    Failed(String),
}
fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}
/// Run `gh api graphql` once. JSON on stdout is used whatever the exit code.
fn fetch(query: &str, path: Option<&str>, timeout: Duration) -> Fetch {
    let mut cmd = Command::new("gh");
    cmd.args(["api", "graphql", "-f"])
        .arg(format!("query={query}"))
        .stdin(Stdio::null())
        .env("GH_PROMPT_DISABLED", "1");
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    let out = match herdr::run(cmd, timeout) {
        Ok(out) => out,
        Err(e) => return Fetch::Failed(format!("gh did not run: {e}")),
    };
    if out.code == Some(4) {
        return Fetch::Auth(first_line(&out.stderr));
    }
    match serde_json::from_slice::<Value>(&out.stdout) {
        Ok(v) if v["data"].is_object() => Fetch::Json(v),
        Ok(v) => Fetch::Failed(format!(
            "gh returned no data: {}",
            v["errors"][0]["message"].as_str().unwrap_or("")
        )),
        Err(_) if out.deadline => {
            Fetch::Failed(format!("gh timed out after {}s", timeout.as_secs()))
        }
        Err(_) => Fetch::Failed(format!(
            "gh exit {} without JSON: {}",
            out.code.unwrap_or(-1),
            first_line(&out.stderr)
        )),
    }
}
type Inflight = (Vec<Pr>, String, mpsc::Receiver<Fetch>);
pub(super) struct Poller {
    dir: PathBuf,
    /// Tests put a fake `gh` first on this PATH; the daemon inherits its own.
    path: Option<String>,
    /// The `gh` deadline: TIMEOUT, shorter in tests.
    timeout: Duration,
    pub(super) due: Instant,
    failures: u32,
    health: &'static str,
    cursor: usize,
    inflight: Option<Inflight>,
}
impl Poller {
    /// Only the ledger daemon polls; a client daemon has no ledger.
    pub(super) fn new(dir: &Path, ledger: bool) -> Option<Self> {
        ledger.then(|| Self {
            dir: dir.into(),
            path: None,
            timeout: TIMEOUT,
            due: Instant::now(),
            failures: 0,
            health: "",
            cursor: 0,
            inflight: None,
        })
    }
    fn health(&mut self, log: &Log, health: &'static str, text: &str) {
        if self.health != health {
            self.health = health;
            log.line(text);
        }
    }
    /// Called each resident iteration: apply a finished fetch, or start one when due.
    pub(super) fn tick(&mut self, db: &mut db::Connection, log: &Log) {
        if let Some((_, _, recv)) = &self.inflight {
            let fetched = match recv.try_recv() {
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => Fetch::Failed("gh worker exited".into()),
                Ok(f) => f,
            };
            let (batch, default, _) = self.inflight.take().expect("inflight");
            match fetched {
                Fetch::Json(v) => {
                    self.failures = 0;
                    self.health(log, "ok", &format!("github poll ok ({} PRs)", batch.len()));
                    if let Err(e) = apply(db, &batch, &default, &v) {
                        log.line(&format!("github poll apply failed: {}", e.message));
                    }
                }
                Fetch::Auth(text) => {
                    self.due = Instant::now() + CAP;
                    self.health(
                        log,
                        "auth",
                        &format!(
                            "github poll: gh is not authenticated (exit 4); retry every 15m: {text}"
                        ),
                    );
                }
                Fetch::Failed(text) => {
                    self.failures += 1;
                    let delay = (MINUTE * (1 << (self.failures - 1).min(4))).min(CAP);
                    self.due = Instant::now() + delay;
                    self.health(
                        log,
                        "failed",
                        &format!("github poll failed; backing off up to 15m: {text}"),
                    );
                }
            }
            return;
        }
        if Instant::now() < self.due {
            return;
        }
        self.due = Instant::now() + MINUTE;
        let Some(default) = config(&self.dir) else {
            return;
        };
        // A PR cached as merged or closed is not polled again; its links still get
        // its refs (a no-op once they have them), with no event. Links and cache are
        // read in the write transaction, so a concurrent relink or close is seen.
        let mut prs = match store::transaction(db, |tx| {
            let mut prs = vec![];
            for l in links(tx, &default)? {
                let s = state(tx, &l.pr)?;
                if s.done {
                    task_refs(tx, l.task, &s)?;
                } else {
                    prs.push(l.pr);
                }
            }
            Ok(prs)
        }) {
            Ok(prs) => prs,
            Err(e) => {
                log.line(&format!("github poll links failed: {}", e.message));
                return;
            }
        };
        prs.sort();
        prs.dedup();
        if prs.is_empty() {
            return;
        }
        // At most 30 per poll, rotating through the rest; 60 s per 20 PRs polled.
        let start = self.cursor % prs.len();
        let batch: Vec<Pr> = prs
            .iter()
            .cycle()
            .skip(start)
            .take(BATCH.min(prs.len()))
            .cloned()
            .collect();
        self.cursor = start + batch.len();
        self.due = Instant::now() + MINUTE * batch.len().div_ceil(PER_MINUTE) as u32;
        let (send, recv) = mpsc::channel();
        let query = query(&batch);
        let (path, timeout) = (self.path.clone(), self.timeout);
        std::thread::spawn(move || {
            let _ = send.send(fetch(&query, path.as_deref(), timeout));
        });
        self.inflight = Some((batch, default, recv));
    }
}
#[cfg(test)]
#[path = "github_tests.rs"]
mod tests;
