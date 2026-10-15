//! Informational repository PRs, independent of the ledger and its tracked-PR watcher.
use crate::{App, glance};
use serde_json::Value;
use std::{
    io::{ErrorKind, Read},
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const MINUTE: Duration = Duration::from_secs(60);
const CAP: Duration = Duration::from_secs(900);
const LIMIT: usize = 8 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ci {
    Failing,
    Running,
    Unknown,
    Passing,
    Draft,
}
impl Ci {
    pub fn word(self) -> &'static str {
        match self {
            Self::Failing => "failing",
            Self::Running => "running",
            Self::Unknown => "unknown",
            Self::Passing => "checks passing",
            Self::Draft => "draft",
        }
    }
}
pub fn classify(checks: &Value, draft: bool) -> Ci {
    if draft {
        return Ci::Draft;
    }
    let Some(checks) = checks.as_array().filter(|c| !c.is_empty()) else {
        return Ci::Unknown;
    };
    let (mut failed, mut running, mut success, mut unknown) =
        (false, false, false, checks.len() >= 100);
    for c in checks {
        let state = if c.get("state").is_some() {
            c["state"].as_str().unwrap_or("")
        } else if c["status"].as_str() == Some("COMPLETED") {
            c["conclusion"].as_str().unwrap_or("")
        } else {
            c["status"].as_str().unwrap_or("")
        };
        match state {
            "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
            | "STARTUP_FAILURE" => failed = true,
            "PENDING" | "QUEUED" | "IN_PROGRESS" | "WAITING" | "REQUESTED" | "EXPECTED" => {
                running = true
            }
            "SUCCESS" => success = true,
            "NEUTRAL" | "SKIPPED" => {}
            _ => unknown = true,
        }
    }
    if failed {
        Ci::Failing
    } else if running {
        Ci::Running
    } else if success && !unknown {
        Ci::Passing
    } else {
        Ci::Unknown
    }
}
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(1024)
        .collect()
}
#[derive(Debug, Clone)]
pub struct Pr {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub author: String,
    pub created: i64,
    pub updated: i64,
    pub ci: Ci,
    pub review: String,
    pub truncated: bool,
}
fn timestamp(v: &Value) -> i64 {
    v.as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .map_or(0, |t| t.unix_timestamp())
}
fn parse(repo: &str, bytes: &[u8]) -> Result<Vec<Pr>, Failure> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| Failure::unavailable())?;
    let rows = value
        .as_array()
        .filter(|r| r.len() <= 100)
        .ok_or_else(Failure::unavailable)?;
    let mut prs = vec![];
    for v in rows {
        let number = v["number"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(Failure::unavailable)?;
        let text = |key: &str| v[key].as_str().map(clean).ok_or_else(Failure::unavailable);
        prs.push(Pr {
            repo: repo.into(),
            number,
            title: text("title")?,
            author: clean(v["author"]["login"].as_str().unwrap_or("unknown")),
            created: timestamp(&v["createdAt"]),
            updated: timestamp(&v["updatedAt"]),
            ci: classify(
                &v["statusCheckRollup"],
                v["isDraft"].as_bool().unwrap_or(false),
            ),
            review: clean(v["reviewDecision"].as_str().unwrap_or(""))
                .replace('_', " ")
                .to_lowercase(),
            truncated: v["statusCheckRollup"]
                .as_array()
                .is_some_and(|c| c.len() >= 100),
        });
    }
    prs.sort_by(|a, b| {
        a.ci.cmp(&b.ci)
            .then(b.updated.cmp(&a.updated))
            .then(b.number.cmp(&a.number))
    });
    Ok(prs)
}
#[derive(Debug, Clone)]
pub struct Failure {
    pub message: &'static str,
    pub retry: Duration,
}
impl Failure {
    fn unavailable() -> Self {
        Self {
            message: "GitHub unavailable",
            retry: Duration::ZERO,
        }
    }
}
fn failure(code: Option<i32>, stderr: &[u8], now: i64) -> Failure {
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
fn output(
    mut command: Command,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, Failure> {
    let mut run = || -> Result<Vec<u8>, Failure> {
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
        let deadline = Instant::now() + timeout;
        let (mut out, mut err) = (vec![], vec![]);
        let drain = |pipe: &mut UnixStream, bytes: &mut Vec<u8>| -> bool {
            let mut buf = [0; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => return true,
                    Ok(n) if bytes.len() + n <= LIMIT => bytes.extend_from_slice(&buf[..n]),
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
                    return if status.success() {
                        Ok(out)
                    } else {
                        Err(failure(
                            status.code(),
                            &err,
                            OffsetDateTime::now_utc().unix_timestamp(),
                        ))
                    };
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
fn fetch(repo: &str, cancel: &AtomicBool) -> Result<Vec<Pr>, Failure> {
    let mut cmd = Command::new("gh");
    cmd.args([
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "open",
        "--limit",
        "100",
        "--json",
        "number,title,author,createdAt,updatedAt,isDraft,statusCheckRollup,reviewDecision,url",
    ]);
    parse(repo, &output(cmd, Duration::from_secs(10), cancel)?)
}
#[derive(Debug, Clone)]
pub struct Source {
    pub repo: String,
    pub prs: Vec<Pr>,
    pub at: Option<Instant>,
    pub error: Option<&'static str>,
    pub total: usize,
    pub inflight: bool,
    pub due: Instant,
    pub retry_until: Option<Instant>,
    pub last_start: Option<Instant>,
    pub failures: u32,
}
impl Source {
    fn new(repo: String, now: Instant) -> Self {
        Self {
            repo,
            prs: vec![],
            at: None,
            error: None,
            total: 0,
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
            && (now >= self.due || manual)
            && self
                .last_start
                .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(5))
    }
    fn complete(&mut self, result: Result<Vec<Pr>, Failure>, now: Instant) {
        self.inflight = false;
        match result {
            Ok(prs) => {
                self.total = prs.len();
                self.prs = prs;
                self.at = Some(now);
                self.error = None;
                self.failures = 0;
                self.due = now + MINUTE;
                self.retry_until = Some(now);
            }
            Err(e) => {
                self.error = Some(e.message);
                self.failures = self.failures.saturating_add(1);
                let delay = (MINUTE * (1 << (self.failures - 1).min(4)))
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
            (None, Some(age)) => format!("{} open; {age} ago", self.total),
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
    pub fn new(repos: &[String]) -> Self {
        Self {
            sources: repos
                .iter()
                .map(|r| Source::new(r.clone(), Instant::now()))
                .collect(),
        }
    }
    pub fn rows(&self) -> Vec<&Pr> {
        let mut rows: Vec<_> = self.sources.iter().flat_map(|s| &s.prs).collect();
        rows.sort_by(|a, b| {
            a.ci.cmp(&b.ci)
                .then(b.updated.cmp(&a.updated))
                .then(b.number.cmp(&a.number))
                .then(a.repo.cmp(&b.repo))
        });
        rows
    }
}
struct Worker {
    index: usize,
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<Result<Vec<Pr>, Failure>>,
}
#[derive(Default)]
pub struct Poller {
    workers: Vec<Worker>,
}
impl Poller {
    pub fn tick(&mut self, app: &mut App, now: Instant, visible: bool, manual: bool) -> bool {
        let mut dirty = false;
        let mut before = None;
        let mut i = 0;
        while i < self.workers.len() {
            if !self.workers[i].handle.is_finished() {
                i += 1;
                continue;
            }
            if !dirty && app.screen == crate::Screen::Glance {
                before = glance::ids(&app.view()).get(app.row).cloned();
            }
            let worker = self.workers.swap_remove(i);
            let result = worker
                .handle
                .join()
                .unwrap_or_else(|_| Err(Failure::unavailable()));
            app.github.sources[worker.index].complete(result, now);
            dirty = true;
        }
        if dirty && app.screen == crate::Screen::Glance {
            app.restore_row(before);
        }
        if visible {
            for (index, source) in app.github.sources.iter_mut().enumerate() {
                if !source.ready(now, manual) {
                    continue;
                }
                source.last_start = Some(now);
                source.inflight = true;
                let repo = source.repo.clone();
                let cancel = Arc::new(AtomicBool::new(false));
                let flag = cancel.clone();
                self.workers.push(Worker {
                    index,
                    cancel,
                    handle: thread::spawn(move || fetch(&repo, &flag)),
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
pub fn demo() -> Dashboard {
    let repo = "trip-to-japan/trip";
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let bytes = serde_json::to_vec(&serde_json::json!([
        {"number":42,"title":"Keep pending action labels readable","author":{"login":"alex"},"createdAt":"2026-10-14T08:00:00Z","updatedAt":"2026-10-14T10:00:00Z","statusCheckRollup":[{"state":"FAILURE"}]},
        {"number":41,"title":"Refresh campaign milestones","author":{"login":"sam"},"createdAt":"2026-10-14T07:00:00Z","updatedAt":"2026-10-14T09:00:00Z","statusCheckRollup":[{"status":"IN_PROGRESS"}]},
        {"number":40,"title":"Clarify owner answer copy","author":{"login":"alex"},"createdAt":"2026-10-13T08:00:00Z","updatedAt":"2026-10-14T08:00:00Z","statusCheckRollup":[{"state":"SUCCESS"}],"reviewDecision":"APPROVED"}
    ])).expect("demo JSON");
    let mut dashboard = Dashboard::new(&[repo.into()]);
    let mut prs = parse(repo, &bytes).expect("demo PRs");
    for (i, p) in prs.iter_mut().enumerate() {
        p.created = now - (i as i64 + 1) * 3600;
    }
    dashboard.sources[0].complete(Ok(prs), Instant::now());
    dashboard
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn classifier_covers_legacy_mixed_and_truncation() {
        let success = json!({"status":"COMPLETED","conclusion":"SUCCESS"});
        assert_eq!(classify(&json!([]), false), Ci::Unknown);
        assert_eq!(classify(&json!([{"state":"SUCCESS"}]), false), Ci::Passing);
        assert_eq!(classify(&json!([{"state":"PENDING"}]), false), Ci::Running);
        for state in [
            "FAILURE",
            "ERROR",
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
        ] {
            assert_eq!(
                classify(&json!([{"state":state},{"state":"PENDING"}]), false),
                Ci::Failing
            );
        }
        assert_eq!(
            classify(&json!([success,{"conclusion":"SUCCESS"}]), false),
            Ci::Unknown
        );
        assert_eq!(
            classify(
                &json!([{"status":"COMPLETED","conclusion":"SKIPPED"}]),
                false
            ),
            Ci::Unknown
        );
        assert_eq!(
            classify(&json!([success,{"state":"NEW_STATE"}]), false),
            Ci::Unknown
        );
        assert_eq!(
            classify(&json!(vec![success.clone(); 99]), false),
            Ci::Passing
        );
        assert_eq!(classify(&json!(vec![success; 100]), false), Ci::Unknown);
        assert_eq!(classify(&json!([{"state":"FAILURE"}]), true), Ci::Draft);
    }
    #[test]
    fn review_decisions_are_sanitized_readable_words() {
        for (raw, expected) in [
            ("APPROVED", "approved"),
            ("CHANGES_REQUESTED", "changes requested"),
            ("REVIEW_REQUIRED", "review required"),
            ("", ""),
        ] {
            let bytes =
                serde_json::to_vec(&json!([{"number":1,"title":"example","reviewDecision":raw}]))
                    .unwrap();
            assert_eq!(parse("owner/repo", &bytes).unwrap()[0].review, expected);
        }
    }

    #[test]
    fn sort_and_sanitization_are_deterministic() {
        let mut d = demo();
        let mut p = d.sources[0].prs[0].clone();
        p.number = 100;
        p.updated += 10;
        d.sources[0].prs.push(p);
        assert_eq!(
            d.rows().iter().map(|p| p.number).collect::<Vec<_>>(),
            [100, 42, 41, 40]
        );
        assert_eq!(clean("x\x1b[31m\r\n\x07\u{202e}y"), "x[31my");
    }
    #[test]
    fn scheduling_retains_last_good_and_deadlines() {
        let now = Instant::now();
        let mut source = Source::new("a/b".into(), now);
        assert!(source.ready(now, false));
        source.last_start = Some(now);
        source.inflight = true;
        assert!(!source.ready(now + MINUTE, true));
        source.complete(Ok(demo().sources.remove(0).prs), now);
        assert!(!source.ready(now + Duration::from_secs(4), true));
        assert!(source.ready(now + Duration::from_secs(5), true));
        source.complete(
            Err(Failure {
                message: "GitHub rate limited",
                retry: Duration::from_secs(7200),
            }),
            now,
        );
        assert_eq!(source.prs.len(), 3);
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
    }
    #[test]
    fn process_deadline_includes_exit_and_errors_are_generic() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec 1>&-; exec sleep 20"]);
        let start = Instant::now();
        let e = output(cmd, Duration::from_millis(80), &AtomicBool::new(false)).unwrap_err();
        assert_eq!(e.message, "GitHub read timed out");
        assert!(start.elapsed() < Duration::from_secs(2));
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf 'secret-token private-title' >&2; exit 4"]);
        let e = output(cmd, Duration::from_secs(1), &AtomicBool::new(false)).unwrap_err();
        assert_eq!(e.message, "GitHub authentication required");
        assert!(!format!("{e:?}").contains("secret-token"));
    }
    #[test]
    fn bad_responses_and_unknown_data_never_become_empty_or_green() {
        for bytes in [b"not JSON".as_slice(), b"{}", b"[{\"number\":0}]"] {
            assert!(parse("a/b", bytes).is_err());
        }
        let rows = serde_json::to_vec(&json!(vec![json!({"number":1,"title":"ok"}); 101])).unwrap();
        assert!(parse("a/b", &rows).is_err());
        assert_eq!(classify(&Value::Null, false), Ci::Unknown);
        let now = Instant::now();
        let mut s = Source::new("a/b".into(), now);
        s.complete(Err(failure(Some(1), b"retry-after: 1000000", 0)), now);
        assert!(!s.ready(now + Duration::from_secs(999999), true));
        s.complete(
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
    fn pr_completion_preserves_unchanged_selection_scroll_and_resets_changed_identity() {
        let mut app = App::new(crate::frames::fixture());
        app.github = demo();
        app.detail = true;
        app.scroll = 7;
        let mut poller = Poller::default();
        let handle = thread::spawn(|| Ok(vec![]));
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
        assert!(app.detail);
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
        app.github = demo();
        let mut poller = Poller::default();
        assert!(!poller.tick(&mut app, Instant::now() + MINUTE * 2, false, true));
        assert!(poller.workers.is_empty());
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let handle = thread::spawn(move || {
            let mut c = Command::new("sh");
            c.args(["-c", "exec sleep 20"]);
            output(c, Duration::from_secs(10), &flag).map(|_| vec![])
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
}
