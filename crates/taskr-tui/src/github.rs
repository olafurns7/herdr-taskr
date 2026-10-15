//! Informational GitHub reads (Shipped, Build queue, Issues) and the bounded process runner
//! they share with My PRs and Health, independent of the ledger and its tracked-PR watcher.
use crate::{App, glance};
use serde_json::Value;
use std::{
    io::{ErrorKind, Read},
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub(crate) const MINUTE: Duration = Duration::from_secs(60);
pub(crate) const CAP: Duration = Duration::from_secs(900);
pub(crate) const LIMIT: usize = 8 * 1024 * 1024;
const ISSUES_EVERY: Duration = Duration::from_secs(300);
/// One absolute budget for both Issues reads, parsing included.
const PAIR_BUDGET: Duration = Duration::from_secs(10);
/// Labels the owner counts as blocked, compared with the raw validated name.
const BLOCKED: [&str; 2] = ["blocked", "needs-info"];
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(1024)
        .collect()
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Shipped,
    Build,
    /// Issues by assignee (key 0): one configured repository, one pair of reads per five minutes.
    Issues,
}
impl Kind {
    fn interval(self) -> Duration {
        match self {
            Self::Shipped => MINUTE * 2,
            Self::Issues => ISSUES_EVERY,
            Self::Build => MINUTE,
        }
    }
    /// The shortest gap between two starts, manual `r` included, and the first retry delay.
    fn gap(self) -> Duration {
        if self == Self::Issues {
            ISSUES_EVERY
        } else {
            Duration::from_secs(5)
        }
    }
    pub fn dashboard(self, app: &App) -> &Dashboard {
        match self {
            Self::Shipped => &app.shipped,
            Self::Build => &app.builds,
            Self::Issues => &app.issues,
        }
    }
    fn dashboard_mut(self, app: &mut App) -> &mut Dashboard {
        match self {
            Self::Shipped => &mut app.shipped,
            Self::Build => &mut app.builds,
            Self::Issues => &mut app.issues,
        }
    }
}
#[derive(Debug, Clone)]
pub struct Merged {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub author: String,
    pub merged: i64,
}
#[derive(Debug, Clone)]
pub struct Run {
    pub repo: String,
    pub id: u64,
    pub status: String,
    pub workflow: String,
    pub branch: String,
    pub created: Option<i64>,
}
impl Run {
    pub fn running(&self) -> bool {
        self.status == "in_progress"
    }
    pub fn known(&self) -> bool {
        matches!(
            self.status.as_str(),
            "in_progress" | "queued" | "waiting" | "pending" | "requested"
        )
    }
    pub fn label(&self) -> String {
        let word = if self.running() {
            "running"
        } else if self.known() {
            "queued"
        } else {
            "unknown"
        };
        format!("{word} ({})", self.status)
    }
}
/// One observed issue after the pair's deduplication. Logins are raw validated identities;
/// views sanitize them for display.
#[derive(Debug, Clone)]
pub struct Issue {
    pub number: u64,
    pub open: bool,
    pub closed: Option<i64>,
    pub blocked: bool,
    pub assignees: Vec<String>,
}
/// An assignee group: `login` None is Unassigned (an explicitly empty assignee list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub repo: String,
    pub login: Option<String>,
    pub open: usize,
    pub blocked: usize,
    pub closed: usize,
}
impl Group {
    pub fn name(&self) -> String {
        self.login
            .as_deref()
            .map_or("Unassigned".into(), |l| format!("@{}", clean(l)))
    }
    pub fn counts(&self) -> String {
        format!(
            "{} open · {} blocked · {} closed/24h",
            self.open, self.blocked, self.closed
        )
    }
}
#[derive(Debug, Default)]
struct Sample {
    merged: Vec<Merged>,
    runs: Vec<Run>,
    issues: Vec<Issue>,
    total: usize,
    /// Issues: the closed read's raw row count; `total` is the open read's.
    closed_total: usize,
}
fn valid_timestamp(v: &Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .map(|t| t.unix_timestamp())
}
fn parse_sample(kind: Kind, repo: &str, bytes: &[u8]) -> Result<Sample, Failure> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| Failure::unavailable())?;
    let rows = value
        .as_array()
        .filter(|r| r.len() <= 100)
        .ok_or_else(Failure::unavailable)?;
    let mut sample = Sample {
        total: rows.len(),
        ..Sample::default()
    };
    for v in rows {
        let number = v[if kind == Kind::Shipped {
            "number"
        } else {
            "databaseId"
        }]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(Failure::unavailable)?;
        if kind == Kind::Shipped {
            let title = v["title"]
                .as_str()
                .map(clean)
                .ok_or_else(Failure::unavailable)?;
            if let Some(merged) = valid_timestamp(&v["mergedAt"]) {
                let author = clean(v["author"]["login"].as_str().unwrap_or("unknown"));
                sample.merged.push(Merged {
                    repo: repo.into(),
                    number,
                    title,
                    author: if author.is_empty() {
                        "unknown".into()
                    } else {
                        author
                    },
                    merged,
                });
            }
        } else {
            let status = clean(v["status"].as_str().unwrap_or("unknown"));
            if status == "completed" {
                continue;
            }
            let workflow = clean(v["workflowName"].as_str().unwrap_or(""));
            sample.runs.push(Run {
                repo: repo.into(),
                id: number,
                status: if status.is_empty() {
                    "unknown".into()
                } else {
                    status
                },
                workflow: if workflow.is_empty() {
                    "Unnamed workflow".into()
                } else {
                    workflow
                },
                branch: clean(v["headBranch"].as_str().unwrap_or("unknown")),
                created: valid_timestamp(&v["createdAt"]),
            });
        }
    }
    Ok(sample)
}
/// One Issues read's rows with their updatedAt. Anything a count needs that is missing or
/// malformed fails the pair: unknown is never zero. A nested list at the conservative
/// 100-item cap may be incomplete, so it fails the pair too.
fn issue_rows(bytes: &[u8], completed: i64) -> Result<Vec<(Issue, i64)>, Failure> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| Failure::unavailable())?;
    let rows = value
        .as_array()
        .filter(|r| r.len() <= 100)
        .ok_or_else(Failure::unavailable)?;
    rows.iter()
        .map(|v| -> Result<(Issue, i64), Failure> {
            let bad = Failure::unavailable;
            let number = v["number"].as_u64().filter(|n| *n > 0).ok_or_else(bad)?;
            let updated = valid_timestamp(&v["updatedAt"]).ok_or_else(bad)?;
            let open = match v["state"].as_str() {
                Some("OPEN") => true,
                Some("CLOSED") => false,
                _ => return Err(bad()),
            };
            // A closure during the pair is valid; one after it completed is not.
            let closed = if open {
                None
            } else {
                Some(
                    valid_timestamp(&v["closedAt"])
                        .filter(|at| *at <= completed)
                        .ok_or_else(bad)?,
                )
            };
            let list = |key: &str| -> Result<Vec<String>, Failure> {
                v[key]
                    .as_array()
                    .filter(|l| l.len() < 100)
                    .ok_or_else(bad)?
                    .iter()
                    .map(|x| {
                        let field = if key == "labels" { "name" } else { "login" };
                        x[field]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .ok_or_else(bad)
                    })
                    .collect::<Result<Vec<_>, _>>()
            };
            let blocked = list("labels")?
                .iter()
                .any(|l| BLOCKED.contains(&l.as_str()));
            let mut assignees = list("assignees")?;
            assignees.sort();
            assignees.dedup();
            Ok((
                Issue {
                    number,
                    open,
                    closed,
                    blocked: blocked && open,
                    assignees,
                },
                updated,
            ))
        })
        .collect()
}
/// The open and closed reads as one observation. A number in both keeps the record with
/// the later updatedAt (the closed read on a tie), so no issue counts as open and closed.
fn parse_issues(open: &[u8], closed: &[u8], completed: i64) -> Result<Sample, Failure> {
    let (open, closed) = (issue_rows(open, completed)?, issue_rows(closed, completed)?);
    let (total, closed_total) = (open.len(), closed.len());
    let mut by_number = std::collections::BTreeMap::<u64, (Issue, i64)>::new();
    for (issue, updated) in open.into_iter().chain(closed) {
        match by_number.get(&issue.number) {
            Some((_, kept)) if *kept > updated => {}
            _ => {
                by_number.insert(issue.number, (issue, updated));
            }
        }
    }
    Ok(Sample {
        issues: by_number.into_values().map(|(i, _)| i).collect(),
        total,
        closed_total,
        ..Sample::default()
    })
}
pub(crate) fn utc_stamp(at: i64) -> String {
    let t = OffsetDateTime::from_unix_timestamp(at).expect("current UTC time");
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}
fn command(kind: Kind, repo: &str, now: i64) -> Command {
    let mut cmd = Command::new("gh");
    match kind {
        Kind::Shipped => {
            let cutoff = utc_stamp(now - 86400);
            cmd.args([
                "pr",
                "list",
                "--repo",
                repo,
                "--state",
                "merged",
                "--limit",
                "100",
                "--search",
                &format!("merged:>={cutoff}"),
                "--json",
                "number,title,author,mergedAt",
            ]);
        }
        Kind::Issues => return issue_command(repo, false, now),
        Kind::Build => {
            cmd.args([
                "run",
                "list",
                "--repo",
                repo,
                "--limit",
                "100",
                "--json",
                "databaseId,status,conclusion,createdAt,updatedAt,workflowName,headBranch,url",
            ]);
        }
    }
    cmd
}
/// One of the pair: open issues, or those closed since `start` less 24 hours.
fn issue_command(repo: &str, closed: bool, start: i64) -> Command {
    let mut cmd = Command::new("gh");
    let state = if closed { "closed" } else { "open" };
    cmd.args([
        "issue", "list", "--repo", repo, "--state", state, "--limit", "100",
    ]);
    if closed {
        cmd.args([
            "--search",
            &format!("closed:>={}", utc_stamp(start - 86400)),
        ]);
    }
    cmd.args(["--json", "number,state,assignees,labels,closedAt,updatedAt"]);
    cmd
}
#[derive(Debug, Clone)]
pub struct Failure {
    pub message: &'static str,
    pub retry: Duration,
}
impl Failure {
    pub(crate) fn unavailable() -> Self {
        Self {
            message: "GitHub unavailable",
            retry: Duration::ZERO,
        }
    }
}
pub(crate) fn failure(code: Option<i32>, stderr: &[u8], now: i64) -> Failure {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    let rate = text.contains("rate limit")
        || text.contains("retry-after:")
        || text.contains("x-ratelimit-reset:");
    let mut retry = if rate { CAP } else { Duration::ZERO };
    // Retain only a deadline, never the provider's possibly private error text.
    for line in text.lines() {
        let line = line.trim();
        if let Some(n) = line
            .strip_prefix("retry-after:")
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            retry = retry.max(Duration::from_secs(n));
        }
        if let Some(n) = line
            .strip_prefix("x-ratelimit-reset:")
            .and_then(|s| s.trim().parse::<i64>().ok())
        {
            retry = retry.max(Duration::from_secs(n.saturating_sub(now).max(0) as u64));
        }
    }
    Failure {
        message: if rate {
            "GitHub rate limited"
        } else if code == Some(4) {
            "GitHub authentication required"
        } else {
            "GitHub unavailable"
        },
        retry,
    }
}

/// Nonblocking socket-backed pipes keep the deadline on the child lifetime, including
/// a process that closes stdout without exiting. No pipe-reader threads can outlive it.
/// The deadline is absolute, so the two Issues reads share one through child exit.
pub(crate) fn output(
    command: Command,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, Failure> {
    let (status, out, err) = run(command, deadline, cancel, LIMIT)?;
    if status.success() {
        Ok(out)
    } else {
        Err(failure(
            status.code(),
            &err,
            OffsetDateTime::now_utc().unix_timestamp(),
        ))
    }
}
/// [`output`]'s runner: the exit status with both bodies, each within `limit` bytes, so a
/// caller can read a body gh printed before a non-zero exit (a GraphQL `errors` reply).
pub(crate) fn run(
    mut command: Command,
    deadline: Instant,
    cancel: &AtomicBool,
    limit: usize,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), Failure> {
    let mut run = || -> Result<(ExitStatus, Vec<u8>, Vec<u8>), Failure> {
        let (mut stdout, out) = UnixStream::pair().map_err(|_| Failure::unavailable())?;
        let (mut stderr, err) = UnixStream::pair().map_err(|_| Failure::unavailable())?;
        stdout
            .set_nonblocking(true)
            .map_err(|_| Failure::unavailable())?;
        stderr
            .set_nonblocking(true)
            .map_err(|_| Failure::unavailable())?;
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(out)))
            .stderr(Stdio::from(OwnedFd::from(err)))
            .env("GH_PROMPT_DISABLED", "1")
            .spawn()
            .map_err(|_| Failure::unavailable())?;
        let (mut out, mut err) = (vec![], vec![]);
        let drain = |pipe: &mut UnixStream, bytes: &mut Vec<u8>| -> bool {
            let mut buf = [0; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => return true,
                    Ok(n) if bytes.len() + n <= limit => bytes.extend_from_slice(&buf[..n]),
                    Ok(_) => return false,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => return false,
                }
            }
        };
        loop {
            let bounded = drain(&mut stdout, &mut out) && drain(&mut stderr, &mut err);
            if !bounded || cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure {
                    message: if bounded {
                        "GitHub read timed out"
                    } else {
                        "GitHub response too large"
                    },
                    retry: Duration::ZERO,
                });
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !drain(&mut stdout, &mut out) || !drain(&mut stderr, &mut err) {
                        return Err(Failure::unavailable());
                    }
                    return Ok((status, out, err));
                }
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Failure::unavailable());
                }
            }
        }
    };
    run()
}
/// Both Issues reads under one absolute deadline: the runner enforces it on each child
/// through exit, a cancel or a spent budget stops before the next spawn, and a parse
/// that finishes late is rejected.
fn fetch_pair(
    command: impl Fn(bool) -> Command,
    budget: Duration,
    cancel: &AtomicBool,
) -> Result<Sample, Failure> {
    let deadline = Instant::now() + budget;
    let late = || Failure {
        message: "GitHub read timed out",
        retry: Duration::ZERO,
    };
    let mut bodies = vec![];
    for closed in [false, true] {
        if cancel.load(Ordering::Relaxed) {
            return Err(Failure::unavailable());
        }
        if Instant::now() >= deadline {
            return Err(late());
        }
        bodies.push(output(command(closed), deadline, cancel)?);
    }
    let sample = parse_issues(
        &bodies[0],
        &bodies[1],
        OffsetDateTime::now_utc().unix_timestamp(),
    )?;
    if Instant::now() >= deadline {
        return Err(late());
    }
    Ok(sample)
}
fn fetch(kind: Kind, repo: &str, now: i64, cancel: &AtomicBool) -> Result<Sample, Failure> {
    if kind == Kind::Issues {
        return fetch_pair(
            |closed| issue_command(repo, closed, now),
            PAIR_BUDGET,
            cancel,
        );
    }
    parse_sample(
        kind,
        repo,
        &output(
            command(kind, repo, now),
            Instant::now() + Duration::from_secs(10),
            cancel,
        )?,
    )
}
#[derive(Debug, Clone)]
pub struct Source {
    pub repo: String,
    pub merged: Vec<Merged>,
    pub runs: Vec<Run>,
    pub issues: Vec<Issue>,
    pub kind: Kind,
    pub at: Option<Instant>,
    pub error: Option<&'static str>,
    pub total: usize,
    pub closed_total: usize,
    pub inflight: bool,
    pub due: Instant,
    pub retry_until: Option<Instant>,
    pub last_start: Option<Instant>,
    pub failures: u32,
}
impl Source {
    fn new(repo: String, kind: Kind, now: Instant) -> Self {
        Self {
            repo,
            merged: vec![],
            runs: vec![],
            issues: vec![],
            kind,
            at: None,
            error: None,
            total: 0,
            closed_total: 0,
            inflight: false,
            due: now,
            retry_until: Some(now),
            last_start: None,
            failures: 0,
        }
    }
    fn ready(&self, now: Instant, manual: bool) -> bool {
        !self.inflight
            && self.retry_until.is_some_and(|until| now >= until)
            // Manual `r` never hurries Issues past its five-minute budget.
            && (now >= self.due || (manual && self.kind != Kind::Issues))
            && self
                .last_start
                .is_none_or(|at| now.saturating_duration_since(at) >= self.kind.gap())
    }
    fn observe(&mut self, result: Result<Sample, Failure>, now: Instant) {
        self.inflight = false;
        match result {
            Ok(sample) => {
                self.total = sample.total;
                self.closed_total = sample.closed_total;
                self.merged = sample.merged;
                self.runs = sample.runs;
                self.issues = sample.issues;
                self.at = Some(now);
                self.error = None;
                self.failures = 0;
                self.due = now + self.kind.interval();
                self.retry_until = Some(now);
            }
            Err(e) => {
                self.error = Some(e.message);
                self.failures = self.failures.saturating_add(1);
                let base = if self.kind == Kind::Issues {
                    ISSUES_EVERY
                } else {
                    MINUTE
                };
                let delay = (base * (1 << (self.failures - 1).min(4)))
                    .min(CAP)
                    .max(e.retry);
                self.retry_until = now.checked_add(delay);
                self.due = self.retry_until.unwrap_or(now);
            }
        }
    }
    pub fn status(&self, now: Instant) -> String {
        let age = self
            .at
            .map(|at| crate::ui::age(now.saturating_duration_since(at).as_millis() as i64));
        let state = match (self.error, age) {
            (Some(e), Some(age)) => format!("stale {age}; {}", e.trim_start_matches("GitHub ")),
            (Some(e), None) => format!("unavailable; {}", e.trim_start_matches("GitHub ")),
            (None, Some(age)) => match self.kind {
                Kind::Shipped | Kind::Build => format!("{} sampled; {age} ago", self.total),
                Kind::Issues => format!(
                    "{} open + {} closed sampled; {age} ago",
                    self.total, self.closed_total
                ),
            },
            _ => "reading GitHub".into(),
        };
        format!("{state} · {}", self.repo)
    }
}
#[derive(Debug, Clone, Default)]
pub struct Dashboard {
    pub sources: Vec<Source>,
}
impl Dashboard {
    pub fn for_kind(repos: &[String], kind: Kind) -> Self {
        Self {
            sources: repos
                .iter()
                .map(|r| Source::new(r.clone(), kind, Instant::now()))
                .collect(),
        }
    }
    pub fn authors(&self, now: i64) -> std::collections::BTreeMap<String, Vec<&Merged>> {
        let mut groups = std::collections::BTreeMap::<String, Vec<&Merged>>::new();
        for p in self
            .sources
            .iter()
            .flat_map(|s| &s.merged)
            .filter(|p| p.merged >= now - 86400 && p.merged <= now)
        {
            groups.entry(p.author.clone()).or_default().push(p);
        }
        for rows in groups.values_mut() {
            rows.sort_by(|a, b| {
                b.merged
                    .cmp(&a.merged)
                    .then(a.repo.cmp(&b.repo))
                    .then(a.number.cmp(&b.number))
            });
        }
        groups
    }
    /// Groups by current assignee: open, blocked (open only) and closed in [now-24h, now].
    /// An issue with several assignees counts in each, so groups overlap.
    pub fn assignees(&self, now: i64) -> Vec<Group> {
        let mut groups = std::collections::BTreeMap::<(String, Option<String>), Group>::new();
        for s in &self.sources {
            for i in &s.issues {
                let closed = i.closed.is_some_and(|at| at >= now - 86400 && at <= now);
                if !i.open && !closed {
                    continue;
                }
                let logins: Vec<Option<String>> = if i.assignees.is_empty() {
                    vec![None]
                } else {
                    i.assignees.iter().cloned().map(Some).collect()
                };
                for login in logins {
                    let g = groups
                        .entry((s.repo.clone(), login.clone()))
                        .or_insert_with(|| Group {
                            repo: s.repo.clone(),
                            login,
                            open: 0,
                            blocked: 0,
                            closed: 0,
                        });
                    g.open += usize::from(i.open);
                    g.blocked += usize::from(i.blocked);
                    g.closed += usize::from(closed);
                }
            }
        }
        let mut rows: Vec<Group> = groups.into_values().collect();
        rows.sort_by(|a, b| {
            b.blocked
                .cmp(&a.blocked)
                .then(b.open.cmp(&a.open))
                .then(b.closed.cmp(&a.closed))
                .then(a.login.is_none().cmp(&b.login.is_none()))
                .then(a.login.cmp(&b.login))
                .then(a.repo.cmp(&b.repo))
        });
        rows
    }
    /// Unique observed issues: open, blocked, closed in the rolling 24 hours.
    pub fn issue_totals(&self, now: i64) -> [usize; 3] {
        let issues = self.sources.iter().flat_map(|s| &s.issues);
        let closed = |i: &&Issue| i.closed.is_some_and(|at| at >= now - 86400 && at <= now);
        [
            issues.clone().filter(|i| i.open).count(),
            issues.clone().filter(|i| i.blocked).count(),
            issues.filter(closed).count(),
        ]
    }
    pub fn runs(&self) -> Vec<&Run> {
        let mut rows: Vec<_> = self.sources.iter().flat_map(|s| &s.runs).collect();
        rows.sort_by(|a, b| {
            b.running()
                .cmp(&a.running())
                .then(b.known().cmp(&a.known()))
                .then(a.repo.cmp(&b.repo))
                .then(a.id.cmp(&b.id))
        });
        rows
    }
}
struct Worker {
    index: usize,
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<Result<Sample, Failure>>,
}
pub struct Poller {
    kind: Kind,
    workers: Vec<Worker>,
}
impl Poller {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            workers: vec![],
        }
    }
    pub fn tick(&mut self, app: &mut App, now: Instant, visible: bool, manual: bool) -> bool {
        let mut dirty = false;
        let mut before = None;
        let mut i = 0;
        while i < self.workers.len() {
            if !self.workers[i].handle.is_finished() {
                i += 1;
                continue;
            }
            if !dirty {
                let view = app.glance_view();
                before = glance::ids(&view).get(view.row).cloned();
            }
            let worker = self.workers.swap_remove(i);
            let result = worker
                .handle
                .join()
                .unwrap_or_else(|_| Err(Failure::unavailable()));
            self.kind.dashboard_mut(app).sources[worker.index].observe(result, now);
            dirty = true;
        }
        if dirty {
            app.restore_row(before);
        }
        if visible {
            let observed = app.observed_at;
            for (index, source) in self.kind.dashboard_mut(app).sources.iter_mut().enumerate() {
                if !source.ready(now, manual) {
                    continue;
                }
                source.last_start = Some(now);
                source.inflight = true;
                let repo = source.repo.clone();
                let cancel = Arc::new(AtomicBool::new(false));
                let flag = cancel.clone();
                let kind = self.kind;
                self.workers.push(Worker {
                    index,
                    cancel,
                    handle: thread::spawn(move || fetch(kind, &repo, observed, &flag)),
                });
                dirty = true;
            }
        }
        dirty
    }
}
impl Drop for Poller {
    fn drop(&mut self) {
        for w in &self.workers {
            w.cancel.store(true, Ordering::Relaxed);
        }
        for w in self.workers.drain(..) {
            let _ = w.handle.join();
        }
    }
}
pub fn demo_shipped(now: i64) -> Dashboard {
    let mut d = Dashboard::for_kind(
        &["example-org/app".into(), "example-org/tools".into()],
        Kind::Shipped,
    );
    for (i, s) in d.sources.iter_mut().enumerate() {
        s.observe(
            Ok(Sample {
                total: 2,
                merged: vec![
                    Merged {
                        repo: s.repo.clone(),
                        number: 38 + i as u64,
                        title: "Preserve owner action identity".into(),
                        author: "alex".into(),
                        merged: now - 3600,
                    },
                    Merged {
                        repo: s.repo.clone(),
                        number: 36 + i as u64,
                        title: "Clarify bounded sample labels".into(),
                        author: "sam".into(),
                        merged: now - 7200,
                    },
                ],
                ..Sample::default()
            }),
            Instant::now(),
        );
    }
    d
}
pub fn demo_builds(now: i64) -> Dashboard {
    let mut d = Dashboard::for_kind(&["example-org/app".into()], Kind::Build);
    let repo = d.sources[0].repo.clone();
    d.sources[0].observe(
        Ok(Sample {
            total: 6,
            runs: vec![
                Run {
                    repo: repo.clone(),
                    id: 200,
                    status: "in_progress".into(),
                    workflow: "Build and test".into(),
                    branch: "dashboard".into(),
                    created: Some(now - 300),
                },
                Run {
                    repo,
                    id: 201,
                    status: "queued".into(),
                    workflow: "Release checks".into(),
                    branch: "main".into(),
                    created: Some(now - 60),
                },
            ],
            ..Sample::default()
        }),
        Instant::now(),
    );
    d
}
/// Invented Issues for the demo and frames: overlapping groups, Unassigned, one closure.
pub fn demo_issues(now: i64) -> Dashboard {
    let at = |ago: i64| utc_stamp(now - ago);
    let (open, closed) = issue_fixture(&at);
    let mut d = Dashboard::for_kind(&["example-org/app".into()], Kind::Issues);
    d.sources[0].observe(parse_issues(&open, &closed, now), Instant::now());
    d
}
/// The demo's two responses, as `gh issue list --json` prints them.
pub fn issue_fixture(at: &dyn Fn(i64) -> String) -> (Vec<u8>, Vec<u8>) {
    let open = serde_json::json!([
        {"number":12,"state":"OPEN","assignees":[{"login":"alex"},{"login":"sam"}],"labels":[{"name":"blocked"}],"closedAt":null,"updatedAt":at(600)},
        {"number":11,"state":"OPEN","assignees":[{"login":"alex"}],"labels":[{"name":"needs-info"},{"name":"bug"}],"closedAt":null,"updatedAt":at(900)},
        {"number":10,"state":"OPEN","assignees":[{"login":"sam"}],"labels":[],"closedAt":null,"updatedAt":at(1200)},
        {"number":9,"state":"OPEN","assignees":[],"labels":[{"name":"docs"}],"closedAt":null,"updatedAt":at(1500)}
    ]);
    let closed = serde_json::json!([
        {"number":8,"state":"CLOSED","assignees":[{"login":"kim"}],"labels":[],"closedAt":at(3600),"updatedAt":at(3600)}
    ]);
    let bytes = |v| serde_json::to_vec(&v).expect("fixture JSON");
    (bytes(open), bytes(closed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn shipped_validates_window_groups_and_raw_sample_coverage() {
        let now = 1_792_000_000;
        let stamp = utc_stamp;
        let rows: Vec<_> = [now - 86401, now - 86400, now, now + 1].into_iter().enumerate()
            .map(|(i, at)| json!({"number":i+1,"title":"merged","author":{"login":"alex"},"mergedAt":stamp(at)})).collect();
        let mut d = Dashboard::for_kind(&["a/b".into()], Kind::Shipped);
        d.sources[0].observe(
            parse_sample(Kind::Shipped, "a/b", &serde_json::to_vec(&rows).unwrap()),
            Instant::now(),
        );
        assert_eq!(d.authors(now)["alex"].len(), 2);
        assert_eq!(d.authors(now + 1)["alex"].len(), 2);
        assert_eq!(d.sources[0].total, 4);
        let mut rows = vec![json!({"number":9,"title":"old","mergedAt":stamp(now-86401)}); 99];
        rows.push(json!({"number":10,"title":"bad","mergedAt":"invalid"}));
        d.sources[0].observe(
            parse_sample(Kind::Shipped, "a/b", &serde_json::to_vec(&rows).unwrap()),
            Instant::now(),
        );
        assert_eq!(d.sources[0].total, 100);
        assert!(d.authors(now).is_empty());
        let demo = demo_shipped(now);
        assert_eq!(demo.authors(now)["alex"].len(), 2);
        let unknown = parse_sample(
            Kind::Shipped,
            "a/b",
            &serde_json::to_vec(&json!([{"number":1,"title":"x","mergedAt":stamp(now)}])).unwrap(),
        )
        .unwrap();
        assert_eq!(unknown.merged[0].author, "unknown");
    }
    #[test]
    fn run_states_ordering_missing_names_and_identity_are_explicit() {
        let statuses = [
            "completed",
            "queued",
            "in_progress",
            "waiting",
            "pending",
            "requested",
            "future_state",
        ];
        let rows: Vec<_> = statuses.iter().enumerate().map(|(i,status)| json!({"databaseId":i+1,"status":status,"conclusion":"success","headBranch":"main\u{001b}"})).collect();
        let mut d = Dashboard::for_kind(&["a/b".into()], Kind::Build);
        d.sources[0].observe(
            parse_sample(Kind::Build, "a/b", &serde_json::to_vec(&rows).unwrap()),
            Instant::now(),
        );
        assert_eq!(d.sources[0].total, 7);
        assert_eq!(d.runs().len(), 6);
        assert_eq!(d.runs()[0].id, 3);
        assert_eq!(d.runs().last().unwrap().label(), "unknown (future_state)");
        assert_eq!(d.runs()[0].workflow, "Unnamed workflow");
        assert_eq!(d.runs()[0].branch, "main");
        assert!(d.runs()[0].created.is_none());
        assert_eq!(d.runs().iter().filter(|r| r.known()).count(), 5);
    }
    #[test]
    fn argv_intervals_throttle_and_backoff_are_per_source() {
        let now = Instant::now();
        for kind in [Kind::Shipped, Kind::Build] {
            let mut d = Dashboard::for_kind(&["a/b".into()], kind);
            let s = &mut d.sources[0];
            s.last_start = Some(now);
            s.observe(Ok(Sample::default()), now);
            assert!(!s.ready(now + Duration::from_secs(4), true));
            assert!(s.ready(now + Duration::from_secs(5), true));
            assert!(!s.ready(now + kind.interval() - Duration::from_secs(1), false));
            assert!(s.ready(now + kind.interval(), false));
            for seconds in [60, 120, 240, 480, 900, 900] {
                s.observe(Err(Failure::unavailable()), now);
                assert_eq!(s.due.duration_since(now), Duration::from_secs(seconds));
                assert!(!s.ready(now + Duration::from_secs(seconds - 1), true));
            }
            s.observe(Err(failure(Some(1), b"retry-after: 7200", 0)), now);
            assert!(!s.ready(now + Duration::from_secs(7199), true));
            let cmd = command(kind, "a/b", 1_792_000_000);
            let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
            assert_eq!(cmd.get_program(), "gh");
            assert!(args.windows(2).any(|w| w == ["--limit", "100"]));
            if kind == Kind::Shipped {
                assert!(args.contains(&"merged:>=2026-10-13T17:46:40Z"));
            }
            if kind == Kind::Build {
                assert_eq!(&args[..2], ["run", "list"]);
                assert!(!args.contains(&"--status"));
            }
        }
    }
    #[test]
    fn fake_gh_commands_use_the_bounded_process_path_and_samples() {
        for kind in [Kind::Shipped, Kind::Build] {
            let cmd = command(kind, "a/b", 1_792_000_000);
            let mut fake = Command::new("sh");
            fake.args(["-c", r#"
                test "$GH_PROMPT_DISABLED" = 1 || exit 1
                read unused && exit 1
                test "$2" = list && test "$3" = --repo && test "$4" = a/b || exit 1
                if test "$1" = pr; then
                    test "$5" = --state && test "$6" = merged && test "$7" = --limit && test "$8" = 100 || exit 1
                    test "$9" = --search && test "${10}" = merged:\>=2026-10-13T17:46:40Z || exit 1
                    test "${11}" = --json && test "${12}" = number,title,author,mergedAt || exit 1
                    printf '[{"number":1,"title":"fixture","mergedAt":"2026-10-14T13:00:00Z"}]'
                else
                    test "$1" = run && test "$5" = --limit && test "$6" = 100 && test "$7" = --json || exit 1
                    test "$8" = databaseId,status,conclusion,createdAt,updatedAt,workflowName,headBranch,url || exit 1
                    printf '[{"databaseId":1,"status":"queued","workflowName":"fixture"}]'
                fi
            "#, "gh"]);
            fake.args(cmd.get_args());
            let bytes = output(
                fake,
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(false),
            )
            .unwrap();
            let sample = parse_sample(kind, "a/b", &bytes).unwrap();
            assert_eq!(sample.total, 1);
            if kind == Kind::Shipped {
                assert_eq!(sample.merged[0].title, "fixture");
            } else {
                assert_eq!(sample.runs[0].status, "queued");
            }
        }
    }
    #[test]
    fn new_pollers_complete_hidden_reads_independently_without_starting_more() {
        let mut app = App::new(crate::model::Data::default());
        app.mine = crate::prs::demo(app.observed_at);
        app.shipped = demo_shipped(app.observed_at);
        app.builds = demo_builds(app.observed_at);
        for kind in [Kind::Shipped, Kind::Build] {
            let mut poller = Poller::new(kind);
            assert!(!poller.tick(&mut app, Instant::now() + MINUTE * 3, false, true));
            assert!(poller.workers.is_empty());
            let handle = thread::spawn(|| Ok(Sample::default()));
            while !handle.is_finished() {
                thread::yield_now();
            }
            poller.workers.push(Worker {
                index: 0,
                cancel: Arc::new(AtomicBool::new(false)),
                handle,
            });
            assert!(poller.tick(&mut app, Instant::now(), false, true));
            assert!(poller.workers.is_empty());
            assert_eq!(kind.dashboard(&app).sources[0].total, 0);
            assert_eq!(app.mine.prs.len(), 6);
        }
    }
    #[test]
    fn scheduling_retains_last_good_and_deadlines() {
        let now = Instant::now();
        let mut source = Source::new("a/b".into(), Kind::Build, now);
        assert!(source.ready(now, false));
        source.last_start = Some(now);
        source.inflight = true;
        assert!(!source.ready(now + MINUTE, true));
        let runs = demo_builds(0).sources.remove(0).runs;
        source.observe(
            Ok(Sample {
                runs,
                ..Sample::default()
            }),
            now,
        );
        assert!(!source.ready(now + Duration::from_secs(4), true));
        assert!(source.ready(now + Duration::from_secs(5), true));
        source.observe(
            Err(Failure {
                message: "GitHub rate limited",
                retry: Duration::from_secs(7200),
            }),
            now,
        );
        assert_eq!(source.runs.len(), 2);
        assert!(source.at.is_some());
        assert!(!source.ready(now + CAP, true));
        assert!(source.ready(now + Duration::from_secs(7200), false));
        assert_eq!(
            failure(Some(1), b"rate limit\nretry-after: 7200", 0)
                .retry
                .as_secs(),
            7200
        );
        assert_eq!(
            failure(Some(1), b"x-ratelimit-reset: 8000", 1000)
                .retry
                .as_secs(),
            7000
        );
        assert_eq!(clean("x\x1b[31m\r\n\x07\u{202e}y"), "x[31my");
    }
    #[test]
    fn process_deadline_includes_exit_and_errors_are_generic() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec 1>&-; exec sleep 20"]);
        let start = Instant::now();
        let e = output(
            cmd,
            Instant::now() + Duration::from_millis(80),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(e.message, "GitHub read timed out");
        assert!(start.elapsed() < Duration::from_secs(2));
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf 'secret-token private-title' >&2; exit 4"]);
        let e = output(
            cmd,
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(e.message, "GitHub authentication required");
        assert!(!format!("{e:?}").contains("secret-token"));
    }
    #[test]
    fn bad_responses_and_unknown_data_never_become_empty_or_green() {
        for bytes in [b"not JSON".as_slice(), b"{}", b"[{\"number\":0}]"] {
            assert!(parse_sample(Kind::Shipped, "a/b", bytes).is_err());
        }
        let rows = serde_json::to_vec(&json!(vec![json!({"number":1,"title":"ok"}); 101])).unwrap();
        assert!(parse_sample(Kind::Shipped, "a/b", &rows).is_err());
        let now = Instant::now();
        let mut s = Source::new("a/b".into(), Kind::Build, now);
        s.observe(Err(failure(Some(1), b"retry-after: 1000000", 0)), now);
        assert!(!s.ready(now + Duration::from_secs(999999), true));
        s.observe(
            Err(Failure {
                message: "GitHub rate limited",
                retry: Duration::MAX,
            }),
            now,
        );
        assert!(s.retry_until.is_none());
        assert!(!s.ready(now + CAP, true));
    }

    #[test]
    fn completion_preserves_unchanged_selection_scroll_and_resets_changed_identity() {
        let mut app = App::new(crate::frames::fixture());
        app.builds = demo_builds(app.observed_at);
        app.scroll = 7;
        let mut poller = Poller::new(Kind::Build);
        let handle = thread::spawn(|| Ok(Sample::default()));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        poller.workers.push(Worker {
            index: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            handle,
        });
        assert!(poller.tick(&mut app, Instant::now(), false, false));
        assert_eq!(app.scroll, 7);
        assert!(!poller.tick(&mut app, Instant::now(), false, false));
        assert_eq!(app.scroll, 7);
        let before = glance::ids(&app.view()).get(app.row).cloned();
        app.data.glance.needs_you.remove(0);
        app.restore_row(before);
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn hidden_panel_does_not_start_workers_and_drop_joins() {
        let mut app = App::new(crate::model::Data::default());
        app.builds = demo_builds(app.observed_at);
        let mut poller = Poller::new(Kind::Build);
        assert!(!poller.tick(&mut app, Instant::now() + MINUTE * 2, false, true));
        assert!(poller.workers.is_empty());
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let handle = thread::spawn(move || {
            let mut c = Command::new("sh");
            c.args(["-c", "exec sleep 20"]);
            output(c, Instant::now() + Duration::from_secs(10), &flag).map(|_| Sample::default())
        });
        poller.workers.push(Worker {
            index: 0,
            cancel,
            handle,
        });
        let start = Instant::now();
        drop(poller);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    fn issues_json(rows: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&rows).unwrap()
    }
    fn issue(
        n: u64,
        state: &str,
        logins: &[&str],
        labels: &[&str],
        closed: Option<i64>,
        updated: i64,
    ) -> Value {
        json!({"number":n,"state":state,
            "assignees":logins.iter().map(|l| json!({"login":l})).collect::<Vec<_>>(),
            "labels":labels.iter().map(|l| json!({"name":l})).collect::<Vec<_>>(),
            "closedAt":closed.map(utc_stamp),"updatedAt":utc_stamp(updated)})
    }
    #[test]
    fn issues_group_overlap_unassigned_exact_labels_and_dedup() {
        let now = 1_792_000_000;
        let open = issues_json(vec![
            issue(
                1,
                "OPEN",
                &["alex", "sam", "alex"],
                &["blocked", "needs-info"],
                None,
                now - 50,
            ),
            issue(2, "OPEN", &[], &["needs-info"], None, now - 50),
            // A label that only sanitizes into a match is not the owner's label.
            issue(
                3,
                "OPEN",
                &["sam"],
                &["block\u{7}ed", "Blocked", "needs_info"],
                None,
                now - 50,
            ),
            // Seen open first, closed later: the later record wins.
            issue(4, "OPEN", &["kim"], &["blocked"], None, now - 100),
            // Seen closed in the closed read, but reopened later: open wins.
            issue(5, "OPEN", &["kim"], &[], None, now - 10),
            issue(6, "OPEN", &["lee"], &[], None, now - 20),
        ]);
        let closed = issues_json(vec![
            issue(
                4,
                "CLOSED",
                &["kim"],
                &["blocked"],
                Some(now - 60),
                now - 60,
            ),
            issue(5, "CLOSED", &["kim"], &[], Some(now - 30), now - 30),
            // A tie goes to the closed read.
            issue(6, "CLOSED", &["lee"], &[], Some(now - 20), now - 20),
            issue(
                7,
                "CLOSED",
                &["alex"],
                &["blocked"],
                Some(now - 86401),
                now - 86401,
            ),
        ]);
        let sample = parse_issues(&open, &closed, now).unwrap();
        assert_eq!(
            (sample.total, sample.closed_total, sample.issues.len()),
            (6, 4, 7)
        );
        let mut d = Dashboard::for_kind(&["example-org/app".into()], Kind::Issues);
        d.sources[0].observe(Ok(sample), Instant::now());
        assert_eq!(d.issue_totals(now), [4, 2, 2]);
        let groups = d.assignees(now);
        let row = |login: Option<&str>| {
            let g = groups.iter().find(|g| g.login.as_deref() == login).unwrap();
            (g.open, g.blocked, g.closed)
        };
        assert_eq!(row(Some("alex")), (1, 1, 0));
        assert_eq!(row(Some("sam")), (2, 1, 0));
        assert_eq!(row(None), (1, 1, 0));
        assert_eq!(row(Some("kim")), (1, 0, 1));
        assert_eq!(row(Some("lee")), (0, 0, 1));
        let order: Vec<_> = groups.iter().map(Group::name).collect();
        assert_eq!(order, ["@sam", "@alex", "Unassigned", "@kim", "@lee"]);
        // Overlap: groups sum to more than the unique headline.
        assert!(groups.iter().map(|g| g.open).sum::<usize>() > d.issue_totals(now)[0]);
        // Display sanitizes; identity keeps the raw login.
        let odd = issues_json(vec![issue(9, "OPEN", &["x\u{1b}[31m"], &[], None, now)]);
        let s = parse_issues(&odd, b"[]", now).unwrap();
        assert_eq!(s.issues[0].assignees, ["x\u{1b}[31m"]);
        let mut d = Dashboard::for_kind(&["example-org/app".into()], Kind::Issues);
        d.sources[0].observe(Ok(s), Instant::now());
        assert_eq!(d.assignees(now)[0].name(), "@x[31m");
    }
    #[test]
    fn issues_reject_malformed_nested_cap_and_future_but_disclose_top_level_cap() {
        let now = 1_792_000_000;
        let ok = issue(1, "OPEN", &["a"], &[], None, now);
        let mut cases = vec![
            json!({"number":1,"state":"OPEN","labels":[],"updatedAt":utc_stamp(now)}),
            json!({"number":1,"state":"OPEN","assignees":[],"updatedAt":utc_stamp(now)}),
            json!({"number":1,"state":"OPEN","assignees":[],"labels":[]}),
            json!({"number":1,"state":"OPEN","assignees":[{}],"labels":[],"updatedAt":utc_stamp(now)}),
            json!({"number":1,"state":"OPEN","assignees":[],"labels":[{"name":""}],"updatedAt":utc_stamp(now)}),
            json!({"number":1,"state":"MERGED","assignees":[],"labels":[],"updatedAt":utc_stamp(now)}),
            json!({"number":0,"state":"OPEN","assignees":[],"labels":[],"updatedAt":utc_stamp(now)}),
            json!({"number":1,"state":"CLOSED","assignees":[],"labels":[],"closedAt":null,"updatedAt":utc_stamp(now)}),
            json!({"number":1,"state":"CLOSED","assignees":[],"labels":[],"closedAt":"bad","updatedAt":utc_stamp(now)}),
            issue(1, "CLOSED", &[], &[], Some(now + 1), now),
        ];
        let many: Vec<String> = (0..100).map(|i| format!("u{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        cases.push(issue(1, "OPEN", &many, &[], None, now));
        cases.push(issue(1, "OPEN", &[], &many, None, now));
        for case in cases {
            for (open, closed) in [
                (issues_json(vec![case.clone()]), b"[]".to_vec()),
                (b"[]".to_vec(), issues_json(vec![case.clone()])),
            ] {
                assert!(parse_issues(&open, &closed, now).is_err(), "{case}");
            }
        }
        for bad in [b"not JSON".as_slice(), b"{}"] {
            assert!(parse_issues(bad, b"[]", now).is_err());
            assert!(parse_issues(b"[]", bad, now).is_err());
        }
        assert!(parse_issues(&issues_json(vec![ok.clone(); 101]), b"[]", now).is_err());
        let full = parse_issues(&issues_json(vec![ok.clone(); 100]), b"[]", now).unwrap();
        assert_eq!((full.total, full.issues.len()), (100, 1));
        // 99 nested entries are below the conservative cap.
        assert!(
            parse_issues(
                &issues_json(vec![issue(1, "OPEN", &many[..99], &many[..99], None, now)]),
                b"[]",
                now
            )
            .is_ok()
        );
    }
    #[test]
    fn issues_closure_during_the_pair_counts_then_expires_on_the_ui_clock() {
        // Pair starts 12:00:00, the issue closes 12:00:02, the pair completes 12:00:04.
        let start = 1_791_979_200;
        let cmd = issue_command("a/b", true, start);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(args.contains(&"closed:>=2026-10-13T12:00:00Z"), "{args:?}");
        let closed = issues_json(vec![issue(
            1,
            "CLOSED",
            &["a"],
            &[],
            Some(start + 2),
            start + 2,
        )]);
        let mut d = Dashboard::for_kind(&["a/b".into()], Kind::Issues);
        d.sources[0].observe(parse_issues(b"[]", &closed, start + 4), Instant::now());
        assert_eq!(d.issue_totals(start + 4), [0, 0, 1]);
        assert_eq!(d.assignees(start + 2 + 86400)[0].closed, 1);
        assert!(d.assignees(start + 3 + 86400).is_empty());
        assert_eq!(d.issue_totals(start + 3 + 86400), [0, 0, 0]);
        // Beyond completion stays invalid.
        assert!(parse_issues(b"[]", &closed, start + 1).is_err());
    }
    #[test]
    fn issues_pair_argv_absolute_deadline_and_cancel_before_spawn() {
        let open = issue_command("a/b", false, 1_792_000_000);
        let args: Vec<_> = open.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "issue",
                "list",
                "--repo",
                "a/b",
                "--state",
                "open",
                "--limit",
                "100",
                "--json",
                "number,state,assignees,labels,closedAt,updatedAt"
            ]
        );
        let closed = issue_command("a/b", true, 1_792_000_000);
        let args: Vec<_> = closed.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "issue",
                "list",
                "--repo",
                "a/b",
                "--state",
                "closed",
                "--limit",
                "100",
                "--search",
                "closed:>=2026-10-13T17:46:40Z",
                "--json",
                "number,state,assignees,labels,closedAt,updatedAt"
            ]
        );
        assert_eq!(
            command(Kind::Issues, "a/b", 1_792_000_000)
                .get_args()
                .count(),
            10
        );
        let spawns = std::cell::Cell::new(0);
        // A fake gh behind the real argv: both reads through the bounded runner.
        let fake = |script: &'static str| {
            let spawns = &spawns;
            move |closed: bool| {
                spawns.set(spawns.get() + 1);
                let mut c = Command::new("sh");
                c.args(["-c", script, "gh"])
                    .args(issue_command("a/b", closed, 1_792_000_000).get_args());
                c
            }
        };
        let script = r#"test "$GH_PROMPT_DISABLED" = 1 || exit 1
            read unused && exit 1
            test "$1 $2 $4" = "issue list a/b" || exit 1
            if test "$6" = closed; then printf '[]'; else printf '[{"number":1,"state":"OPEN","assignees":[],"labels":[],"closedAt":null,"updatedAt":"2026-10-14T12:00:00Z"}]'; fi"#;
        let sample = fetch_pair(
            fake(script),
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!((sample.total, sample.closed_total, spawns.get()), (1, 0, 2));
        // The first read spends the budget: the second never spawns, and the pair stays bounded.
        spawns.set(0);
        let start = Instant::now();
        let e = fetch_pair(
            fake("exec sleep 20"),
            Duration::from_millis(150),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!((e.message, spawns.get()), ("GitHub read timed out", 1));
        assert!(start.elapsed() < Duration::from_secs(2));
        // Slow but successful first read: the second child dies at the pair's absolute
        // deadline (~500 ms), not 500 ms after its own spawn (~800 ms).
        spawns.set(0);
        let start = Instant::now();
        let e = fetch_pair(
            fake("test \"$6\" = closed && exec sleep 20; sleep 0.3; printf '[]'"),
            Duration::from_millis(500),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!((e.message, spawns.get()), ("GitHub read timed out", 2));
        assert!(
            start.elapsed() < Duration::from_millis(750),
            "{:?}",
            start.elapsed()
        );
        // A cancel during the second child kills it promptly.
        spawns.set(0);
        let cancel = AtomicBool::new(false);
        let start = Instant::now();
        let e = thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(300));
                cancel.store(true, Ordering::Relaxed);
            });
            fetch_pair(
                fake("test \"$6\" = closed && exec sleep 20; printf '[]'"),
                Duration::from_secs(5),
                &cancel,
            )
            .unwrap_err()
        });
        assert_eq!((e.message, spawns.get()), ("GitHub read timed out", 2));
        assert!(start.elapsed() < Duration::from_secs(2));
        spawns.set(0);
        assert!(fetch_pair(fake(script), Duration::from_secs(5), &AtomicBool::new(true)).is_err());
        assert_eq!(spawns.get(), 0);
        // A failure in either read fails the pair.
        let e = fetch_pair(
            fake("test \"$6\" = closed && exit 1; printf '[]'"),
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(e.message, "GitHub unavailable");
    }
    #[test]
    fn issues_admission_is_five_minutes_manual_included_and_keeps_the_last_good_pair() {
        // A new source is due from its creation instant on.
        let mut d = Dashboard::for_kind(&["example-org/app".into()], Kind::Issues);
        let now = Instant::now();
        let s = &mut d.sources[0];
        assert!(s.ready(now, false));
        assert_eq!(s.status(now), "reading GitHub · example-org/app");
        s.last_start = Some(now);
        s.observe(Err(Failure::unavailable()), now);
        // First failure: unavailable, never zero.
        assert!(s.status(now).starts_with("unavailable"));
        assert!(!s.ready(now + Duration::from_secs(299), true));
        s.last_start = Some(now);
        let fixture = issue_fixture(&|ago| utc_stamp(1_792_000_000 - ago));
        s.observe(parse_issues(&fixture.0, b"[]", 1_792_000_000), now);
        assert_eq!(s.issues.len(), 4);
        for secs in [5, 60, 299] {
            assert!(!s.ready(now + Duration::from_secs(secs), true), "{secs}");
        }
        assert!(s.ready(now + Duration::from_secs(300), false));
        for seconds in [300, 600, 900, 900] {
            s.observe(Err(Failure::unavailable()), now);
            assert_eq!(s.due.duration_since(now), Duration::from_secs(seconds));
            assert!(!s.ready(now + Duration::from_secs(seconds - 1), true));
            assert!(s.ready(now + Duration::from_secs(seconds), false));
        }
        assert_eq!(s.issues.len(), 4);
        assert!(s.status(now).starts_with("stale"));
        s.observe(Err(failure(Some(1), b"retry-after: 7200", 0)), now);
        assert!(!s.ready(now + Duration::from_secs(7199), true));
        // Hidden: no admission, even manual and due.
        let mut app = App::new(crate::model::Data::default());
        app.issues = Dashboard::for_kind(&["example-org/app".into()], Kind::Issues);
        let mut poller = Poller::new(Kind::Issues);
        assert!(!poller.tick(&mut app, Instant::now() + MINUTE * 10, false, true));
        assert!(poller.workers.is_empty());
        assert!(app.issues.sources[0].last_start.is_none());
    }
    #[test]
    fn issues_scope_is_separate_from_pr_repos() {
        let app = App::new(crate::model::Data::default());
        assert!(app.issues.sources.is_empty());
        let d = Dashboard::for_kind(&["example-org/app".into()], Kind::Issues);
        assert_eq!(d.sources[0].repo, "example-org/app");
        assert_eq!(demo_issues(1_792_000_000).assignees(1_792_000_000).len(), 4);
    }
}
