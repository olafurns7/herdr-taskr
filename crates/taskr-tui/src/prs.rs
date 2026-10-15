//! My PRs (panel 4): the authenticated viewer's open PRs across the watched repositories.
//! The login is read once per run (`gh api user`, REST) and kept in memory only. Then one
//! global cycle of two `gh api graphql` stages: discovery (a search sample, first 30) learns
//! repository and number only; an aliased detail read gives every shown field of a row from
//! that PR's own alias in one response. Both stages share one ten-second deadline and one
//! output bound; stage 2 starts only if the card is still admitted when stage 1 lands.
use crate::{
    App,
    github::{CAP, Failure, LIMIT, MINUTE, clean, failure, run},
};
use serde_json::Value;
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// One absolute budget for a whole cycle, both stages and their parsing included.
const BUDGET: Duration = Duration::from_secs(10);
/// The shortest gap between two cycle starts, manual `r` included.
const GAP: Duration = Duration::from_secs(5);
/// Discovery's sample size.
pub const FIRST: usize = 30;
/// Contexts and threads read per PR; more is disclosed as `N+?`.
const PAGE: usize = 100;

/// A row's group, in sort order: failing, pending, unknown, passing, then drafts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ci {
    Failing,
    Running,
    Unknown,
    Passing,
    Draft,
}
impl Ci {
    pub fn mark(self) -> &'static str {
        match self {
            Self::Failing => "✗",
            Self::Running => "◷",
            Self::Unknown => "?",
            Self::Passing => "✓",
            Self::Draft => "□",
        }
    }
}

/// The checks of the bound head commit, from the first [`PAGE`] contexts read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Checks {
    pub pass: usize,
    pub fail: usize,
    pub pending: usize,
    /// Neutral or skipped: never passing alone.
    pub skipped: usize,
    /// A state this reader does not know.
    pub other: usize,
    pub read: usize,
    pub total: usize,
    /// Every context read: counts are exact and zero means zero.
    pub all: bool,
    /// Failing contexts read: name and whether GitHub marks it required for this PR.
    pub failing: Vec<(String, bool)>,
    /// Observed required contexts that passed or failed. They never prove the complete set.
    pub required_pass: usize,
    pub required_fail: usize,
}
impl Checks {
    /// `N`, or `N+?` (at least N; more contexts unread).
    pub fn count(&self, n: usize) -> String {
        bound(n, self.all)
    }
}
pub fn bound(n: usize, all: bool) -> String {
    if all { n.to_string() } else { format!("{n}+?") }
}

/// Unresolved threads among those read; paths and lines only, never comment text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Threads {
    pub unresolved: Vec<(String, Option<u64>, bool)>,
    pub read: usize,
    pub total: usize,
    pub all: bool,
}

/// What one detail alias said. Fields a field error made unknown are `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct Fields {
    pub title: String,
    pub draft: bool,
    pub updated: i64,
    pub head: String,
    /// Err: why every check fact is unknown (`head moved`, `no check rollup`, `not readable`).
    pub checks: Result<Checks, &'static str>,
    pub review: Option<&'static str>,
    pub merge: Option<&'static str>,
    pub mergeable: Option<&'static str>,
    pub threads: Option<Threads>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pr {
    pub repo: String,
    pub number: u64,
    /// None: the alias was null or broken; repository and number are all that is known.
    pub read: Option<Fields>,
}
impl Pr {
    pub fn ci(&self) -> Ci {
        let Some(f) = &self.read else {
            return Ci::Unknown;
        };
        if f.draft {
            return Ci::Draft;
        }
        match &f.checks {
            Ok(c) if c.fail > 0 => Ci::Failing,
            Ok(c) if c.pending > 0 => Ci::Running,
            Ok(c) if c.all && c.pass > 0 && c.other == 0 => Ci::Passing,
            _ => Ci::Unknown,
        }
    }
    /// `✗` only from an observed required failure on the bound head; otherwise unknown.
    /// Nothing here can say every required check passed.
    pub fn required_failed(&self) -> bool {
        self.read
            .as_ref()
            .is_some_and(|f| f.checks.as_ref().is_ok_and(|c| c.required_fail > 0))
    }
    /// The merge word: a conflict from `mergeable` wins over the state's summary.
    pub fn merge_word(&self) -> &'static str {
        let Some(f) = &self.read else { return "?" };
        if f.mergeable == Some("CONFLICTING") {
            return "conflict";
        }
        match f.merge {
            Some("BEHIND") => "behind",
            Some("BLOCKED") => "blocked",
            Some("CLEAN") => "clean",
            Some("DIRTY") => "conflict",
            Some("DRAFT") => "draft",
            Some("HAS_HOOKS") => "hooks",
            Some("UNSTABLE") => "unstable",
            _ => "?",
        }
    }
    pub fn review_word(&self) -> &'static str {
        match self.read.as_ref().and_then(|f| f.review) {
            Some("APPROVED") => "approved",
            Some("CHANGES_REQUESTED") => "changes",
            Some("REVIEW_REQUIRED") => "review",
            Some("NONE") => "none",
            _ => "?",
        }
    }
    /// The compact facts: `✓12 ✗2 ◷0 · req ✗ · changes · behind · unres 3 · e2e-web, lint`.
    pub fn facts(&self) -> String {
        let Some(f) = &self.read else {
            return "not readable · every field ?".into();
        };
        let checks = match &f.checks {
            Ok(c) => format!(
                "✓{} ✗{} ◷{}",
                c.count(c.pass),
                c.count(c.fail),
                c.count(c.pending)
            ),
            Err(why) => format!("checks ? {why}"),
        };
        let req = if self.required_failed() { "✗" } else { "?" };
        let unres = f
            .threads
            .as_ref()
            .map_or("?".into(), |t| bound(t.unresolved.len(), t.all));
        let mut out = format!(
            "{checks} · req {req} · {} · {} · unres {unres}",
            self.review_word(),
            self.merge_word()
        );
        if let Ok(c) = &f.checks {
            if !c.failing.is_empty() {
                let names: Vec<&str> = c.failing.iter().map(|(n, _)| n.as_str()).collect();
                out += &format!(" · {}{}", names.join(", "), if c.all { "" } else { " +?" });
            } else if !c.all {
                // A failure may sit among the contexts not read.
                out += " · ?";
            }
        }
        out
    }
}

impl Pr {
    /// Every field, for Detail: what is known in GitHub's wording, what is not as `?`.
    pub fn detail(&self, now: i64) -> Vec<String> {
        let mut out = vec![];
        let Some(f) = &self.read else {
            out.push(format!("{} #{} · not readable", self.repo, self.number));
            out.push(
                "Deleted, renamed, no access, or a field error: every field is ? (not zero, not gone)."
                    .into(),
            );
            return out;
        };
        out.push(f.title.clone());
        let head: String = f.head.chars().take(8).collect();
        out.push(format!(
            "{} #{} · {} · head {head}",
            self.repo,
            self.number,
            if f.draft { "DRAFT" } else { "not draft" }
        ));
        // The row's age, which a long facts tail may crowd, in full; a future stamp is `?`.
        let ago = if f.updated <= now {
            format!(
                "{} ago",
                crate::ui::age(now.saturating_sub(f.updated).saturating_mul(1000))
            )
        } else {
            "?".into()
        };
        out.push(format!(
            "updated {ago} · {}",
            crate::github::utc_stamp(f.updated)
        ));
        out.push(String::new());
        match &f.checks {
            Err(why) => {
                out.push(format!("CHECKS · ? {why}"));
                out.push(match *why {
                    "head moved" => "The checks read belong to another commit than the head; every check fact is ? until the next admitted cycle.".into(),
                    "no check rollup" => "GitHub returned no check rollup for the head: unknown, not zero checks.".into(),
                    _ => "The checks could not be read: every check fact is ?.".into(),
                });
            }
            Ok(c) => {
                out.push(format!(
                    "CHECKS · {} of {} read · on head {head}",
                    c.read, c.total
                ));
                out.push(format!(
                    "✓ {} ✗ {} ◷ {} · {} skipped or neutral · {} other",
                    c.count(c.pass),
                    c.count(c.fail),
                    c.count(c.pending),
                    c.count(c.skipped),
                    c.count(c.other)
                ));
                for (name, required) in &c.failing {
                    out.push(format!(
                        "✗ {name} · {}",
                        if *required {
                            "required"
                        } else {
                            "not required"
                        }
                    ));
                }
                if !c.all {
                    out.push(format!(
                        "{} contexts not read: a failing check may be among them (failing names ?).",
                        c.total.saturating_sub(c.read)
                    ));
                }
                out.push(format!(
                    "observed required: {} passed, {} failed",
                    c.required_pass, c.required_fail
                ));
            }
        }
        out.push("complete required set: unknown".into());
        out.push(String::new());
        let raw = |v: Option<&str>| v.unwrap_or("?").to_string();
        out.push(format!("merge state: {}", raw(f.merge)));
        out.push(format!("mergeable: {}", raw(f.mergeable)));
        out.push(format!("review: {}", raw(f.review)));
        match &f.threads {
            None => out.push("threads: ? (could not be read)".into()),
            Some(t) => {
                out.push(format!(
                    "threads: {} unresolved of {}{}",
                    bound(t.unresolved.len(), t.all),
                    t.total,
                    if t.all {
                        " (all read)".to_string()
                    } else {
                        format!(" ({} read; more may be unresolved)", t.read)
                    }
                ));
                for (path, line, outdated) in &t.unresolved {
                    out.push(format!(
                        "{path}:{} · {}",
                        line.map_or("?".into(), |l| l.to_string()),
                        if *outdated { "outdated" } else { "current" }
                    ));
                }
            }
        }
        out
    }
}

/// The last cycle's GraphQL accounting, as GitHub reported it (both stages' costs summed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub cost: u64,
    pub remaining: u64,
    pub reset: i64,
}

/// One completed cycle.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub prs: Vec<Pr>,
    /// Discovery's `issueCount`: more than [`FIRST`] means more may exist.
    pub found: usize,
    pub closed: usize,
    pub outside: usize,
    pub partial: bool,
    pub rate: Option<Rate>,
}

/// What discovery learned: repository and number only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found {
    pub keys: Vec<(String, u64)>,
    pub found: usize,
    pub outside: usize,
    pub rate: Option<Rate>,
    /// Output spent, for stage 2's share of the bound.
    pub bytes: usize,
}

/// The card's source: scope, login, the last good sample and one global schedule.
#[derive(Debug, Clone, Default)]
pub struct Mine {
    /// The watched repositories (`tui.json` `repos`, at most eight).
    pub repos: Vec<String>,
    /// The authenticated viewer, read once per run; never saved.
    pub login: Option<String>,
    pub sample: Sample,
    pub prs: Vec<Pr>,
    pub at: Option<Instant>,
    pub error: Option<&'static str>,
    pub inflight: bool,
    pub due: Option<Instant>,
    pub retry_until: Option<Instant>,
    pub last_start: Option<Instant>,
    pub failures: u32,
    /// The current cycle; a result of any other is discarded.
    pub generation: u64,
    /// Whether `retry_until` is unbounded (a provider deadline too far to hold).
    pub blocked: bool,
}
impl Mine {
    pub fn new(repos: &[String]) -> Self {
        Self {
            repos: repos.to_vec(),
            ..Self::default()
        }
    }
    pub fn ready(&self, now: Instant, manual: bool) -> bool {
        !self.inflight
            && !self.blocked
            && self.retry_until.is_none_or(|until| now >= until)
            && (self.due.is_none_or(|due| now >= due) || manual)
            && self
                .last_start
                .is_none_or(|at| now.saturating_duration_since(at) >= GAP)
    }
    fn fail(&mut self, e: Failure, now: Instant) {
        self.inflight = false;
        self.error = Some(e.message);
        self.failures = self.failures.saturating_add(1);
        let delay = (MINUTE * (1 << (self.failures - 1).min(4)))
            .min(CAP)
            .max(e.retry);
        self.retry_until = now.checked_add(delay);
        self.blocked = self.retry_until.is_none();
        self.due = self.retry_until;
    }
    fn identity(&mut self, result: Result<String, Failure>, now: Instant) {
        match result {
            Ok(login) => {
                self.inflight = false;
                self.login = Some(login);
                (self.error, self.failures) = (None, 0);
                // The first cycle is due at once; the REST read is not a cycle start.
                (self.due, self.retry_until, self.last_start) = (None, None, None);
                self.blocked = false;
            }
            Err(e) => self.fail(e, now),
        }
    }
    /// A cycle's end: the new sample, or the last good one kept with the error.
    pub fn land(&mut self, result: Result<Sample, Failure>, now: Instant, wall: i64) {
        match result {
            Ok(sample) => {
                self.inflight = false;
                self.prs = sample.prs.clone();
                sort(&mut self.prs);
                self.sample = sample;
                (self.at, self.error, self.failures) = (Some(now), None, 0);
                self.due = Some(now + MINUTE);
                (self.retry_until, self.blocked) = (None, false);
                // Known exhaustion: no next cycle before GitHub's reset.
                if let Some(r) = self.sample.rate.filter(|r| r.remaining < r.cost.max(1)) {
                    let wait = Duration::from_secs(r.reset.saturating_sub(wall).max(0) as u64);
                    self.retry_until = now.checked_add(wait);
                    self.blocked = self.retry_until.is_none();
                    self.due = self.due.max(self.retry_until);
                }
            }
            Err(e) => self.fail(e, now),
        }
    }
    /// Admission lost after discovery: the discovery is dropped, the schedule kept as if the
    /// cycle had run, and nothing is marked failed or stale.
    fn dropped(&mut self) {
        self.inflight = false;
        self.due = self.last_start.map(|at| at + MINUTE);
    }
    /// The card's source line.
    pub fn status(&self, now: Instant) -> String {
        let age = self
            .at
            .map(|at| crate::ui::age(now.saturating_duration_since(at).as_millis() as i64));
        let error = self.error.map(|e| e.trim_start_matches("GitHub "));
        match (&self.login, error, age) {
            (None, Some(e), _) => format!("unavailable: your GitHub login could not be read; {e}"),
            (None, None, _) => "reading your GitHub login".into(),
            (Some(_), Some(e), Some(age)) => format!("stale {age}; {e}"),
            (Some(_), Some(e), None) => format!("unavailable; {e}"),
            // The sample's count: a filter narrows the rows, never the source's total.
            (Some(_), None, Some(age)) => format!(
                "{} observed · {} · {age} ago · search sample, first {FIRST}",
                self.sample.prs.len(),
                match self.repos.len() {
                    1 => self.repos[0].clone(),
                    n => format!("{n} repos"),
                }
            ),
            (Some(_), None, None) => "reading GitHub".into(),
        }
    }
    /// Honest disclosures about the last good sample, each with whether the card keeps it at
    /// every height: PRs the rows cannot show are always disclosed; a row's own `not readable`
    /// or `?` already says the rest wherever that row is drawn.
    pub fn notes(&self) -> Vec<(String, bool)> {
        if self.at.is_none() {
            return vec![];
        }
        let s = &self.sample;
        let unreadable = s.prs.iter().filter(|p| p.read.is_none()).count();
        [
            (s.found > FIRST).then(|| {
                (
                    format!("first {FIRST} of {}; more may exist", s.found),
                    true,
                )
            }),
            (s.closed > 0).then(|| (format!("{} closed since search", s.closed), true)),
            (unreadable > 0).then(|| (format!("{unreadable} not readable"), false)),
            s.partial
                .then(|| ("some fields could not be read".into(), false)),
            (s.outside > 0).then(|| (format!("{} outside watched repos ignored", s.outside), true)),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// Failing, pending, unknown, passing, then drafts; newest update first within a group.
fn sort(prs: &mut [Pr]) {
    let updated = |p: &Pr| p.read.as_ref().map_or(0, |f| f.updated);
    prs.sort_by(|a, b| {
        a.ci()
            .cmp(&b.ci())
            .then(updated(b).cmp(&updated(a)))
            .then(a.repo.cmp(&b.repo))
            .then(b.number.cmp(&a.number))
    });
}

pub fn gh() -> Command {
    Command::new("gh")
}
fn login_command(mut c: Command) -> Command {
    c.args(["api", "user", "--jq", ".login"]);
    c
}
fn find_command(mut c: Command, login: &str, repos: &[String]) -> Command {
    let mut q = format!("is:pr is:open author:{login}");
    for r in repos {
        q += &format!(" repo:{r}");
    }
    c.args([
        "api",
        "graphql",
        "-f",
        &format!(
            "query=query($q:String!){{search(query:$q,type:ISSUE,first:{FIRST}){{issueCount \
             nodes{{...on PullRequest{{number repository{{nameWithOwner}}}}}}}}\
             rateLimit{{cost remaining resetAt}}}}"
        ),
        "-f",
        &format!("q={q}"),
    ]);
    c
}
/// One alias per PR: `isRequired` needs the PR's number, which a search node cannot supply.
/// Repository names are `tui.json`'s validated `owner/name` (letters, digits, `-_.`).
fn read_command(mut c: Command, keys: &[(String, u64)]) -> Command {
    let mut query = String::from("query{");
    for (i, (repo, n)) in keys.iter().enumerate() {
        let (owner, name) = repo.split_once('/').unwrap_or((repo.as_str(), ""));
        query += &format!(
            "p{i}:repository(owner:\"{owner}\",name:\"{name}\"){{pullRequest(number:{n}){{\
             number state title isDraft updatedAt headRefOid mergeable mergeStateStatus \
             reviewDecision commits(last:1){{nodes{{commit{{oid statusCheckRollup{{\
             contexts(first:{PAGE}){{totalCount pageInfo{{hasNextPage}} nodes{{__typename \
             ...on CheckRun{{name status conclusion isRequired(pullRequestNumber:{n})}} \
             ...on StatusContext{{context state isRequired(pullRequestNumber:{n})}}}}}}}}}}}}}} \
             reviewThreads(first:{PAGE}){{totalCount pageInfo{{hasNextPage}} \
             nodes{{isResolved isOutdated path line}}}}}}}}"
        );
    }
    query += "rateLimit{cost remaining resetAt}}";
    c.args(["api", "graphql", "-f", &format!("query={query}")]);
    c
}

fn late() -> Failure {
    Failure {
        message: "GitHub read timed out",
        retry: Duration::ZERO,
    }
}
fn wall() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}
fn timestamp(v: &Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .map(|t| t.unix_timestamp())
}
/// A GitHub login: 1-39 letters, digits or inner hyphens.
fn valid_login(s: &str) -> bool {
    (1..=39).contains(&s.len())
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

/// The identity read: exactly one valid login line, else unavailable.
fn identity(command: Command, cancel: &AtomicBool) -> Result<String, Failure> {
    let deadline = Instant::now() + BUDGET;
    let bytes = crate::github::output(command, deadline, cancel)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| Failure::unavailable())?;
    let login = text.strip_suffix('\n').unwrap_or(text);
    if valid_login(login) {
        Ok(login.into())
    } else {
        Err(Failure::unavailable())
    }
}

/// One stage's reply. gh exits 1 when a GraphQL reply carries `errors`, with the body on
/// stdout; that body is the answer unless gh reported a rate limit. Nothing gh or GitHub
/// wrote is kept beyond the parse: messages stay generic.
fn graphql(
    command: Command,
    deadline: Instant,
    cancel: &AtomicBool,
    limit: usize,
) -> Result<(Value, usize), Failure> {
    let (status, out, err) = run(command, deadline, cancel, limit)?;
    let body = serde_json::from_slice::<Value>(&out)
        .ok()
        .filter(Value::is_object);
    if !status.success() {
        let f = failure(status.code(), &err, wall());
        if f.message != "GitHub unavailable"
            || !body.as_ref().is_some_and(|b| b["errors"].is_array())
        {
            return Err(f);
        }
    }
    let body = body.ok_or_else(Failure::unavailable)?;
    // A rate-limited reply names no field; it fails the cycle until the reset.
    if let Some(errors) = body.get("errors") {
        let errors = errors.as_array().ok_or_else(Failure::unavailable)?;
        if errors.iter().any(|e| e["type"] == "RATE_LIMITED") {
            let rate = rate(&body["data"]["rateLimit"]);
            return Err(Failure {
                message: "GitHub rate limited",
                retry: rate.map_or(CAP, |r| {
                    Duration::from_secs(r.reset.saturating_sub(wall()).max(0) as u64)
                }),
            });
        }
    }
    Ok((body, out.len()))
}
fn rate(v: &Value) -> Option<Rate> {
    Some(Rate {
        cost: v["cost"].as_u64()?,
        remaining: v["remaining"].as_u64()?,
        reset: timestamp(&v["resetAt"])?,
    })
}

/// Discovery: repository and number of each node, in watched scope, positive, at most
/// [`FIRST`], deduplicated. Any error or malformed node fails the cycle.
pub fn parse_found(body: &Value, repos: &[String]) -> Result<Found, Failure> {
    let bad = Failure::unavailable;
    if body
        .get("errors")
        .is_some_and(|e| e.as_array().is_none_or(|e| !e.is_empty()))
    {
        return Err(bad());
    }
    let search = &body["data"]["search"];
    let found = search["issueCount"].as_u64().ok_or_else(bad)? as usize;
    let nodes = search["nodes"]
        .as_array()
        .filter(|n| n.len() <= FIRST)
        .ok_or_else(bad)?;
    let mut out = Found {
        found,
        rate: rate(&body["data"]["rateLimit"]),
        ..Found::default()
    };
    for node in nodes {
        let number = node["number"].as_u64().filter(|n| *n > 0).ok_or_else(bad)?;
        let name = node["repository"]["nameWithOwner"]
            .as_str()
            .ok_or_else(bad)?;
        let Some(repo) = repos.iter().find(|r| r.eq_ignore_ascii_case(name)) else {
            out.outside += 1;
            continue;
        };
        if !out.keys.iter().any(|(r, n)| r == repo && *n == number) {
            out.keys.push((repo.clone(), number));
        }
    }
    Ok(out)
}

/// Detail: one row per alias. A closed or merged PR is dropped and counted; a null alias or
/// a field error on anything but the facets below makes the row not readable; a field error
/// on a facet makes only that facet unknown. An unscoped error or a missing alias fails the
/// cycle; `data: null` too.
pub fn parse_read(body: &Value, keys: &[(String, u64)]) -> Result<Sample, Failure> {
    let bad = Failure::unavailable;
    let data = body["data"].as_object().ok_or_else(bad)?;
    let mut broken: Vec<(usize, Option<String>)> = vec![];
    for e in body["errors"].as_array().map_or(&[][..], Vec::as_slice) {
        let path = e["path"].as_array().ok_or_else(bad)?;
        let alias = path.first().and_then(Value::as_str).ok_or_else(bad)?;
        let i = alias
            .strip_prefix('p')
            .and_then(|i| i.parse::<usize>().ok())
            .filter(|i| *i < keys.len() && alias == format!("p{i}"))
            .ok_or_else(bad)?;
        let field = path
            .get(2)
            .filter(|_| path.get(1).and_then(Value::as_str) == Some("pullRequest"))
            .and_then(Value::as_str)
            .map(String::from);
        broken.push((i, field));
    }
    let mut sample = Sample {
        partial: !broken.is_empty(),
        rate: rate(&data.get("rateLimit").cloned().unwrap_or_default()),
        ..Sample::default()
    };
    for (i, (repo, number)) in keys.iter().enumerate() {
        let alias = data.get(&format!("p{i}")).ok_or_else(bad)?;
        let errors: Vec<Option<&str>> = broken
            .iter()
            .filter(|(at, _)| *at == i)
            .map(|(_, f)| f.as_deref())
            .collect();
        const FACETS: [&str; 5] = [
            "reviewDecision",
            "mergeable",
            "mergeStateStatus",
            "commits",
            "reviewThreads",
        ];
        let whole = errors
            .iter()
            .any(|f| f.is_none_or(|f| !FACETS.contains(&f)));
        let failed = |facet: &str| errors.contains(&Some(facet));
        let pr = &alias["pullRequest"];
        let read = if whole {
            None
        } else {
            fields(pr, *number, &failed)
        };
        match read {
            Some(Some(fields)) => sample.prs.push(Pr {
                repo: repo.clone(),
                number: *number,
                read: Some(fields),
            }),
            Some(None) => sample.closed += 1,
            None => sample.prs.push(Pr {
                repo: repo.clone(),
                number: *number,
                read: None,
            }),
        }
    }
    Ok(sample)
}

/// None: not readable. Some(None): closed or merged since discovery.
fn fields(pr: &Value, number: u64, failed: &dyn Fn(&str) -> bool) -> Option<Option<Fields>> {
    if pr["number"].as_u64() != Some(number) {
        return None;
    }
    match pr["state"].as_str()? {
        "OPEN" => {}
        "CLOSED" | "MERGED" => return Some(None),
        _ => return None,
    }
    let head = pr["headRefOid"]
        .as_str()
        .filter(|h| (7..=64).contains(&h.len()) && h.bytes().all(|c| c.is_ascii_hexdigit()))?;
    let pick = |facet: &str, known: &[&'static str]| -> Option<&'static str> {
        if failed(facet) {
            return None;
        }
        let v = &pr[facet];
        // A null review decision is a fact: no review requirement or decision.
        if facet == "reviewDecision" && v.is_null() {
            return Some("NONE");
        }
        let s = v.as_str()?;
        known.iter().copied().find(|k| *k == s)
    };
    Some(Some(Fields {
        title: clean(pr["title"].as_str()?),
        draft: pr["isDraft"].as_bool()?,
        updated: timestamp(&pr["updatedAt"])?,
        head: head.to_ascii_lowercase(),
        checks: if failed("commits") {
            Err("not readable")
        } else {
            checks(&pr["commits"], head)
        },
        review: pick(
            "reviewDecision",
            &["APPROVED", "CHANGES_REQUESTED", "REVIEW_REQUIRED"],
        ),
        merge: pick(
            "mergeStateStatus",
            &[
                "BEHIND",
                "BLOCKED",
                "CLEAN",
                "DIRTY",
                "DRAFT",
                "HAS_HOOKS",
                "UNSTABLE",
            ],
        ),
        mergeable: pick("mergeable", &["MERGEABLE", "CONFLICTING"]),
        threads: if failed("reviewThreads") {
            None
        } else {
            threads(&pr["reviewThreads"])
        },
    }))
}

/// The head commit's contexts, bound to `head`; anything malformed makes them unknown.
fn checks(commits: &Value, head: &str) -> Result<Checks, &'static str> {
    let unread = "not readable";
    let nodes = commits["nodes"].as_array().ok_or(unread)?;
    let [node] = nodes.as_slice() else {
        return Err(unread);
    };
    let commit = &node["commit"];
    if !commit["oid"]
        .as_str()
        .ok_or(unread)?
        .eq_ignore_ascii_case(head)
    {
        return Err("head moved");
    }
    let rollup = &commit["statusCheckRollup"];
    if rollup.is_null() {
        return Err("no check rollup");
    }
    let contexts = &rollup["contexts"];
    let total = contexts["totalCount"].as_u64().ok_or(unread)? as usize;
    let more = contexts["pageInfo"]["hasNextPage"]
        .as_bool()
        .ok_or(unread)?;
    let nodes = contexts["nodes"]
        .as_array()
        .filter(|n| n.len() <= PAGE)
        .ok_or(unread)?;
    let mut c = Checks {
        read: nodes.len(),
        total,
        all: !more && nodes.len() == total,
        ..Checks::default()
    };
    for n in nodes {
        let required = n["isRequired"].as_bool().ok_or(unread)?;
        let (name, state) = match n["__typename"].as_str().ok_or(unread)? {
            "CheckRun" => {
                let status = n["status"].as_str().ok_or(unread)?;
                let state = if status == "COMPLETED" {
                    n["conclusion"].as_str().unwrap_or("")
                } else {
                    status
                };
                (n["name"].as_str().ok_or(unread)?, state)
            }
            "StatusContext" => (
                n["context"].as_str().ok_or(unread)?,
                n["state"].as_str().ok_or(unread)?,
            ),
            _ => ("", ""),
        };
        match state {
            "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
            | "STARTUP_FAILURE" => {
                c.fail += 1;
                c.required_fail += usize::from(required);
                c.failing.push((clean(name), required));
            }
            "PENDING" | "QUEUED" | "IN_PROGRESS" | "WAITING" | "REQUESTED" | "EXPECTED" => {
                c.pending += 1
            }
            "SUCCESS" => {
                c.pass += 1;
                c.required_pass += usize::from(required);
            }
            "NEUTRAL" | "SKIPPED" => c.skipped += 1,
            _ => c.other += 1,
        }
    }
    Ok(c)
}

fn threads(v: &Value) -> Option<Threads> {
    let total = v["totalCount"].as_u64()? as usize;
    let more = v["pageInfo"]["hasNextPage"].as_bool()?;
    let nodes = v["nodes"].as_array().filter(|n| n.len() <= PAGE)?;
    let mut t = Threads {
        read: nodes.len(),
        total,
        all: !more && nodes.len() == total,
        ..Threads::default()
    };
    for n in nodes {
        let outdated = n["isOutdated"].as_bool()?;
        if !n["isResolved"].as_bool()? {
            let line = match &n["line"] {
                Value::Null => None,
                l => Some(l.as_u64()?),
            };
            t.unresolved
                .push((clean(n["path"].as_str()?), line, outdated));
        }
    }
    Some(t)
}

/// Stage 1 on its thread: the reply parsed before the shared deadline.
fn find(
    command: Command,
    deadline: Instant,
    cancel: &AtomicBool,
    repos: &[String],
) -> Result<Found, Failure> {
    let (body, bytes) = graphql(command, deadline, cancel, LIMIT)?;
    let mut found = parse_found(&body, repos)?;
    found.bytes = bytes;
    // Discovery spent the points: stage 2 would be refused until the reset.
    if let Some(r) = found.rate.filter(|r| r.remaining == 0) {
        return Err(Failure {
            message: "GitHub rate limited",
            retry: Duration::from_secs(r.reset.saturating_sub(wall()).max(0) as u64),
        });
    }
    if Instant::now() >= deadline {
        return Err(late());
    }
    Ok(found)
}
/// Stage 2 on its thread, within what is left of the cycle's deadline and output bound.
fn read(
    command: Command,
    deadline: Instant,
    cancel: &AtomicBool,
    found: Found,
) -> Result<Sample, Failure> {
    let (body, _) = graphql(command, deadline, cancel, LIMIT.saturating_sub(found.bytes))?;
    let mut sample = parse_read(&body, &found.keys)?;
    (sample.found, sample.outside) = (found.found, found.outside);
    sample.rate = match (found.rate, sample.rate) {
        (Some(a), Some(b)) => Some(Rate {
            cost: a.cost + b.cost,
            ..b
        }),
        (a, b) => b.or(a),
    };
    if Instant::now() >= deadline {
        return Err(late());
    }
    Ok(sample)
}

enum Stage {
    Login(JoinHandle<Result<String, Failure>>),
    Find(JoinHandle<Result<Found, Failure>>),
    Read(JoinHandle<Result<Sample, Failure>>),
}
impl Stage {
    fn finished(&self) -> bool {
        match self {
            Self::Login(h) => h.is_finished(),
            Self::Find(h) => h.is_finished(),
            Self::Read(h) => h.is_finished(),
        }
    }
}
struct Job {
    generation: u64,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    stage: Stage,
}

/// At most one identity read or one cycle stage in flight.
pub struct Poller {
    gh: Box<dyn Fn() -> Command>,
    job: Option<Job>,
}
impl Poller {
    pub fn new(gh: impl Fn() -> Command + 'static) -> Self {
        Self {
            gh: Box::new(gh),
            job: None,
        }
    }
    /// A finished stage lands even while hidden. Stage 2, the identity read and a new cycle
    /// start only while `visible`, which the caller reads afresh on every tick: the stage-1
    /// handoff checks the admission of the tick it lands on, not the cycle's start.
    pub fn tick(
        &mut self,
        app: &mut App,
        now: Instant,
        wall: i64,
        visible: bool,
        manual: bool,
    ) -> bool {
        let mut dirty = false;
        if self.job.as_ref().is_some_and(|j| j.stage.finished()) {
            let job = self.job.take().expect("a finished job");
            let view = app.glance_view();
            let before = crate::glance::ids(&view).get(view.row).cloned();
            dirty = true;
            let mine = &mut app.mine;
            let current = job.generation == mine.generation;
            mine.inflight = false;
            match job.stage {
                Stage::Login(h) if current => mine.identity(
                    h.join().unwrap_or_else(|_| Err(Failure::unavailable())),
                    now,
                ),
                Stage::Find(h) if current => {
                    match h.join().unwrap_or_else(|_| Err(Failure::unavailable())) {
                        Err(e) => mine.land(Err(e), now, wall),
                        Ok(found) if found.keys.is_empty() => mine.land(
                            Ok(Sample {
                                found: found.found,
                                outside: found.outside,
                                rate: found.rate,
                                ..Sample::default()
                            }),
                            now,
                            wall,
                        ),
                        Ok(_) if !visible => mine.dropped(),
                        Ok(_) if Instant::now() >= job.deadline => {
                            mine.land(Err(late()), now, wall)
                        }
                        Ok(found) => {
                            mine.inflight = true;
                            let command = read_command((self.gh)(), &found.keys);
                            let flag = job.cancel.clone();
                            let deadline = job.deadline;
                            self.job = Some(Job {
                                stage: Stage::Read(thread::spawn(move || {
                                    read(command, deadline, &flag, found)
                                })),
                                ..job
                            });
                        }
                    }
                }
                Stage::Read(h) if current => mine.land(
                    h.join().unwrap_or_else(|_| Err(Failure::unavailable())),
                    now,
                    wall,
                ),
                // Another cycle's stage: never mixed into this one.
                _ => {}
            }
            app.restore_row(before);
        }
        let mine = &mut app.mine;
        if visible && self.job.is_none() && !mine.repos.is_empty() && mine.ready(now, manual) {
            mine.inflight = true;
            mine.last_start = Some(now);
            mine.generation += 1;
            let cancel = Arc::new(AtomicBool::new(false));
            let flag = cancel.clone();
            let deadline = Instant::now() + BUDGET;
            let stage = match &mine.login {
                None => {
                    let command = login_command((self.gh)());
                    Stage::Login(thread::spawn(move || identity(command, &flag)))
                }
                Some(login) => {
                    let command = find_command((self.gh)(), login, &mine.repos);
                    let repos = mine.repos.clone();
                    Stage::Find(thread::spawn(move || {
                        find(command, deadline, &flag, &repos)
                    }))
                }
            };
            self.job = Some(Job {
                generation: mine.generation,
                deadline,
                cancel,
                stage,
            });
            dirty = true;
        }
        dirty
    }
}
impl Drop for Poller {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
            let _ = match job.stage {
                Stage::Login(h) => h.join().map(drop),
                Stage::Find(h) => h.join().map(drop),
                Stage::Read(h) => h.join().map(drop),
            };
        }
    }
}

/// The demo's two replies, as `gh api graphql` prints them, for an invented login. `at`
/// gives a timestamp `ago` seconds before now.
pub fn fixture(at: &dyn Fn(i64) -> String) -> (Value, Value) {
    let ctx = |name: &str, conclusion: &str, required: bool| serde_json::json!({"__typename":"CheckRun","name":name,"status":"COMPLETED","conclusion":conclusion,"isRequired":required});
    let pending = |name: &str| serde_json::json!({"__typename":"CheckRun","name":name,"status":"IN_PROGRESS","conclusion":null,"isRequired":false});
    let pr = |n: u64,
              title: &str,
              draft: bool,
              ago: i64,
              head: &str,
              oid: &str,
              merge: &str,
              review: Value,
              contexts: Vec<Value>,
              total: usize,
              threads: Vec<Value>| {
        let mergeable = if merge == "DIRTY" {
            "CONFLICTING"
        } else {
            "MERGEABLE"
        };
        let (more, count) = (total > contexts.len(), threads.len());
        serde_json::json!({"pullRequest":{"number":n,"state":"OPEN","title":title,"isDraft":draft,
            "updatedAt":at(ago),"headRefOid":head,"mergeable":mergeable,
            "mergeStateStatus":merge,"reviewDecision":review,
            "commits":{"nodes":[{"commit":{"oid":oid,"statusCheckRollup":{"contexts":{
                "totalCount":total,"pageInfo":{"hasNextPage":more},"nodes":contexts}}}}]},
            "reviewThreads":{"totalCount":count,"pageInfo":{"hasNextPage":false},"nodes":threads}}})
    };
    let thread = |path: &str, line: u64, resolved: bool, outdated: bool| serde_json::json!({"isResolved":resolved,"isOutdated":outdated,"path":path,"line":line});
    let (a, b, c) = ("9f3c2ab1", "4e1d0c77", "7a6b5c4d");
    let mut many: Vec<Value> = (0..88)
        .map(|i| ctx(&format!("unit-{i}"), "SUCCESS", false))
        .collect();
    many.extend((0..12).map(|i| pending(&format!("e2e-{i}"))));
    let found = serde_json::json!({"data":{"search":{"issueCount":6,"nodes":[
        {"number":42,"repository":{"nameWithOwner":"demo-org/demo"}},
        {"number":41,"repository":{"nameWithOwner":"demo-org/demo"}},
        {"number":40,"repository":{"nameWithOwner":"demo-org/demo"}},
        {"number":39,"repository":{"nameWithOwner":"demo-org/demo"}},
        {"number":38,"repository":{"nameWithOwner":"demo-org/demo"}}]},
        "rateLimit":{"cost":1,"remaining":4990,"resetAt":at(-1800)}}});
    let read = serde_json::json!({"data":{
        "p0":pr(42,"Keep pending action labels readable",false,720,a,a,"BEHIND",
            Value::from("CHANGES_REQUESTED"),
            vec![ctx("build","SUCCESS",true),ctx("e2e-web","FAILURE",true),ctx("lint","FAILURE",false),ctx("unit","SUCCESS",false)],4,
            vec![thread("src/view.rs",142,false,false),thread("src/view.rs",201,false,false),thread("docs/tui.md",12,false,true),thread("src/lib.rs",9,true,false)]),
        "p1":pr(41,"Refresh campaign milestones",false,1440,b,b,"BLOCKED",
            Value::from("REVIEW_REQUIRED"),vec![ctx("build","SUCCESS",true),pending("e2e")],2,vec![]),
        "p2":pr(40,"Clarify owner answer copy",false,2220,c,c,"CLEAN",
            Value::from("APPROVED"),vec![ctx("build","SUCCESS",true),ctx("lint","SUCCESS",false)],2,vec![]),
        "p3":pr(39,"Split the answer editor state",false,7200,"0b0b0b0b","0b0b0b0b","UNSTABLE",
            Value::from("APPROVED"),many,140,vec![]),
        "p4":pr(38,"Sketch the fleet totals line",true,9000,"1c1c1c1c","2d2d2d2d","DRAFT",
            Value::Null,vec![],0,vec![]),
        "rateLimit":{"cost":1,"remaining":4989,"resetAt":at(-1800)}}});
    (found, read)
}
/// Invented My PRs for `--demo` and the frames, through the real parsers: never a read.
pub fn demo(now: i64) -> Mine {
    let repos = vec!["demo-org/demo".to_string()];
    let stamp = |ago: i64| crate::github::utc_stamp(now - ago);
    let (found, read) = fixture(&stamp);
    let found = parse_found(&found, &repos).expect("demo discovery");
    let mut sample = parse_read(&read, &found.keys).expect("demo detail");
    sample.found = found.found;
    // The demo row that GitHub could not read: repository and number only.
    sample.prs.push(Pr {
        repo: repos[0].clone(),
        number: 37,
        read: None,
    });
    let mut mine = Mine::new(&repos);
    mine.login = Some("demo-owner".into());
    mine.land(Ok(sample), Instant::now(), now);
    mine
}

#[cfg(test)]
mod tests;
