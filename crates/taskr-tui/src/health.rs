//! Health (key H): the hub's latest `HEALTH` note on its root, one configured repository and
//! environment. A report as of the time the hub checked, never a claim about production now.
use crate::{App, client, github};
use serde_json::{Map, Value};
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

/// Health notes remain scoped to production on the configured hub root.
pub const ENV: &str = "production";
/// The invented hub root of `--demo`, the frames and the tests.
const SAMPLE_ROOT: i64 = 2899;
/// At most one read a minute while shown, manual `r`, failures and hide-show included.
const EVERY: Duration = Duration::from_secs(60);
const CAP: Duration = Duration::from_secs(900);
/// One absolute budget for the read, parsing included.
const BUDGET: Duration = Duration::from_secs(10);
/// Every failure shows this, never the process's or the ledger's own text.
pub const UNAVAILABLE: &str = "Health notes unavailable";
const NO_NOTE: &str = "no HEALTH note in last 100 hub events";
const MALFORMED: &str = "latest HEALTH note malformed";
const OTHER: &str = "latest HEALTH note is for another repo or env";
const FUTURE: &str = "latest HEALTH note checked in the future";
const KEYS: [&str; 7] = [
    "repo", "env", "sha", "deploy", "sentry", "checked", "rollback",
];
/// The pagination trailer's keys; a record with only these is not an event.
const TRAILER: [&str; 6] = ["older", "before", "more", "next", "launches", "all"];

/// What a HEALTH note reported, each value validated before it is kept.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The note's event ID and, when it parses, when the hub wrote it.
    pub note: i64,
    pub noted: Option<i64>,
    pub sha: String,
    pub deploy: String,
    pub sentry: String,
    /// The note's own text and the instant it names.
    pub checked: String,
    pub checked_at: i64,
    /// A note ID, never resolved or acted on.
    pub rollback: i64,
}
impl Report {
    /// Failed deploys and new Sentry issues take the attention colour, never red.
    pub fn attention(&self) -> bool {
        self.deploy == "failed" || self.sentry.starts_with("new:")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    Report(Report),
    /// A read that found no usable latest note: it replaces an older report.
    Unknown(&'static str),
}

/// A successful read and the UTC second it completed.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub read_at: i64,
    pub reading: Reading,
}

#[derive(Debug, Clone, Default)]
pub struct Health {
    /// Set only with `root`: tui.json's `health_repo` and `health_root` together.
    pub repo: Option<String>,
    pub root: i64,
    pub seen: Option<Seen>,
    /// The last read failed: `seen`, if any, is stale.
    pub error: Option<&'static str>,
    pub inflight: bool,
    last_start: Option<Instant>,
    retry_until: Option<Instant>,
    failures: u32,
}

fn hhmm(at: i64) -> String {
    OffsetDateTime::from_unix_timestamp(at)
        .map_or("?".into(), |t| format!("{:02}:{:02}", t.hour(), t.minute()))
}

impl Health {
    fn ready(&self, now: Instant) -> bool {
        !self.inflight
            && self
                .last_start
                .is_none_or(|at| now.saturating_duration_since(at) >= EVERY)
            && self.retry_until.is_none_or(|until| now >= until)
    }
    fn observe(&mut self, result: Result<Seen, &'static str>, now: Instant) {
        self.inflight = false;
        match result {
            Ok(seen) => {
                (self.seen, self.error, self.failures) = (Some(seen), None, 0);
                self.retry_until = None;
            }
            Err(e) => {
                self.error = Some(e);
                self.failures = self.failures.saturating_add(1);
                let delay = (EVERY * (1 << (self.failures - 1).min(4))).min(CAP);
                self.retry_until = now.checked_add(delay);
            }
        }
    }
    /// The source line: which read the panel shows, and whether it is the latest.
    pub fn state(&self) -> String {
        match (&self.seen, self.error) {
            (None, Some(_)) => "Unknown · no hub read yet · retrying".into(),
            (None, None) if self.inflight => "Unknown · reading hub notes".into(),
            (None, None) => "Unknown · hub notes not read yet".into(),
            (Some(s), Some(e)) => format!("Stale · {e} · showing read {}", hhmm(s.read_at)),
            (Some(s), None) => format!(
                "Hub report · {} · read {}",
                self.repo.as_deref().unwrap_or_default(),
                hhmm(s.read_at)
            ),
        }
    }
    /// The row's label. Stale, unknown and "as of" come first: a narrow row clips its end.
    pub fn label(&self, now: i64) -> String {
        let stale = if self.error.is_some() {
            "Stale · "
        } else {
            ""
        };
        match self.seen.as_ref().map(|s| &s.reading) {
            None => format!("Unknown · {}", self.error.unwrap_or("no hub read yet")),
            Some(Reading::Unknown(why)) => format!("{stale}Unknown · {why}"),
            Some(Reading::Report(r)) => format!(
                "{stale}as of {}Z {} ago · deploy {} · sentry {} · sha {}",
                crate::ui::hhmm(&r.checked),
                crate::ui::age((now - r.checked_at).saturating_mul(1000)),
                r.deploy,
                r.sentry,
                r.sha
            ),
        }
    }
    /// The line under the row: where the report came from, by its own clocks.
    pub fn source(&self) -> String {
        match self.seen.as_ref().map(|s| &s.reading) {
            Some(Reading::Report(r)) => format!(
                "checked {} · note #{} · rollback note #{}",
                r.checked, r.note, r.rollback
            ),
            _ => format!(
                "hub root {} · latest 100 events · no report shown",
                self.root
            ),
        }
    }
    pub fn attention(&self) -> bool {
        matches!(self.seen.as_ref().map(|s| &s.reading), Some(Reading::Report(r)) if r.attention())
    }
    /// What the frame shows that changes with time alone.
    pub fn face(&self, now: i64) -> String {
        format!("{}{}{}", self.state(), self.label(now), self.inflight)
    }
    /// The detail pane: every value with its clock.
    pub fn detail(&self, now: i64) -> String {
        match self.seen.as_ref().map(|s| (s, &s.reading)) {
            Some((s, Reading::Report(r))) => {
                format!(
                    "repo {} · env {ENV}\nsha {}\ndeploy {}\nsentry {}\nchecked {} ({} \
                     ago)\nnote #{} on hub root {}, written {}\nrollback note #{}\nread {} \
                     UTC from the latest 100 hub events",
                    self.repo.as_deref().unwrap_or_default(),
                    r.sha,
                    r.deploy,
                    r.sentry,
                    r.checked,
                    crate::ui::age((now - r.checked_at).saturating_mul(1000)),
                    r.note,
                    self.root,
                    r.noted.map_or("at an unknown time".into(), |at| format!(
                        "{} UTC",
                        hhmm(at)
                    )),
                    r.rollback,
                    hhmm(s.read_at)
                )
            }
            Some((s, Reading::Unknown(why))) => format!(
                "{why}\nread {} UTC from the latest 100 events on hub root {}",
                hhmm(s.read_at),
                self.root
            ),
            None => String::new(),
        }
    }
}

fn positive(o: &Map<String, Value>, key: &str) -> Option<i64> {
    o.get(key).and_then(Value::as_i64).filter(|n| *n > 0)
}

/// `taskr --json log` lines: launch records and a pagination trailer may come with the
/// events. Anything else that is not a well-formed root event, in ascending ID order,
/// fails the whole read.
pub fn parse(body: &[u8], read_at: i64, root: i64, repo: &str) -> Result<Reading, &'static str> {
    let text = std::str::from_utf8(body).map_err(|_| UNAVAILABLE)?;
    let mut last = 0;
    let mut latest = None;
    for line in text.lines() {
        let value: Value = serde_json::from_str(line).map_err(|_| UNAVAILABLE)?;
        let o = value.as_object().ok_or(UNAVAILABLE)?;
        // Launch IDs are not event IDs: known record shapes are set aside first. taskr
        // marks every event `record: event`; any other record value is unknown.
        match o.get("record").map(Value::as_str) {
            Some(Some("launch")) if positive(o, "id").is_some() => continue,
            Some(Some("event")) | None => {}
            Some(_) => return Err(UNAVAILABLE),
        }
        if !o.is_empty()
            && o.iter()
                .all(|(k, v)| TRAILER.contains(&k.as_str()) && (v.is_i64() || v.is_string()))
        {
            continue;
        }
        let (Some(id), Some(task), Some(kind), Some(_)) = (
            positive(o, "id"),
            positive(o, "task_id"),
            o.get("kind").and_then(Value::as_str),
            o.get("created_at").and_then(Value::as_str),
        ) else {
            return Err(UNAVAILABLE);
        };
        // Without --tree only the root's events come back, oldest first.
        if id <= last || task != root {
            return Err(UNAVAILABLE);
        }
        last = id;
        if kind != "note" {
            continue;
        }
        // taskr omits an empty note's text (or prints null): no HEALTH candidate.
        let summary = match o.get("summary") {
            None | Some(Value::Null) => continue,
            Some(text) => text.as_str().ok_or(UNAVAILABLE)?,
        };
        if summary.starts_with("HEALTH ") {
            let created = o["created_at"].as_str().unwrap_or_default();
            latest = Some((id, summary.to_string(), created.to_string()));
        }
    }
    // The latest note decides, valid or not: an older report never stands in for it.
    Ok(match latest {
        None => Reading::Unknown(NO_NOTE),
        Some((note, summary, created)) => payload(note, &summary, &created, read_at, repo),
    })
}

fn payload(note: i64, summary: &str, created: &str, read_at: i64, repo: &str) -> Reading {
    match report(note, summary, created, read_at, repo) {
        Ok(r) => Reading::Report(r),
        Err(why) => Reading::Unknown(why),
    }
}

/// The raw line, validated as written: one space between tokens, printable ASCII only, so
/// no control, bidi or other text reaches the screen.
fn report(
    note: i64,
    summary: &str,
    created: &str,
    read_at: i64,
    repo: &str,
) -> Result<Report, &'static str> {
    if summary.len() > 1024 || !summary.bytes().all(|b| b == b' ' || b.is_ascii_graphic()) {
        return Err(MALFORMED);
    }
    let rest = summary.strip_prefix("HEALTH ").ok_or(MALFORMED)?;
    let mut fields = std::collections::BTreeMap::new();
    for token in rest.split(' ') {
        let (key, value) = token.split_once('=').ok_or(MALFORMED)?;
        if !KEYS.contains(&key) || value.is_empty() || fields.insert(key, value).is_some() {
            return Err(MALFORMED);
        }
    }
    if fields.len() != KEYS.len() {
        return Err(MALFORMED);
    }
    if fields["repo"] != repo || fields["env"] != ENV {
        return Err(OTHER);
    }
    let sha = fields["sha"];
    let deploy = fields["deploy"];
    let sentry = fields["sentry"];
    let issues = sentry.strip_prefix("new:");
    let rollback = fields["rollback"];
    let checked = OffsetDateTime::parse(fields["checked"], &Rfc3339)
        .ok()
        .filter(|t| t.offset().is_utc())
        .ok_or(MALFORMED)?
        .unix_timestamp();
    if sha.len() != 10
        || !sha.bytes().all(|b| b.is_ascii_hexdigit())
        || !["live", "failed", "pending"].contains(&deploy)
        || !(["clean", "pending"].contains(&sentry)
            || issues.is_some_and(|ids| {
                !ids.is_empty()
                    && ids
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b",-_.".contains(&b))
            }))
        || rollback.starts_with('0')
        || !rollback.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(MALFORMED);
    }
    if checked > read_at {
        return Err(FUTURE);
    }
    Ok(Report {
        note,
        noted: OffsetDateTime::parse(created, &Rfc3339)
            .ok()
            .map(|t| t.unix_timestamp()),
        sha: sha.into(),
        deploy: deploy.into(),
        sentry: sentry.into(),
        checked: fields["checked"].into(),
        checked_at: checked,
        rollback: rollback.parse().map_err(|_| MALFORMED)?,
    })
}

/// `taskr --json log ROOT --limit 100`, as the lane's own task never: no task or launch.
pub fn command(root: i64) -> Command {
    let taskr = client::taskr();
    let mut c = Command::new(&taskr[0]);
    c.args(&taskr[1..])
        .args(["--json", "log", &root.to_string(), "--limit", "100"])
        .env_remove("TASKR_TASK")
        .env_remove("TASKR_LAUNCH");
    c
}

/// One bounded read through GitHub's process runner; a parse that ends late fails.
fn fetch(
    command: Command,
    budget: Duration,
    cancel: &AtomicBool,
    root: i64,
    repo: &str,
) -> Result<Seen, &'static str> {
    let deadline = Instant::now() + budget;
    let body = github::output(command, deadline, cancel).map_err(|_| UNAVAILABLE)?;
    let read_at = OffsetDateTime::now_utc().unix_timestamp();
    let reading = parse(&body, read_at, root, repo)?;
    if Instant::now() >= deadline {
        return Err(UNAVAILABLE);
    }
    Ok(Seen { read_at, reading })
}

type Worker = (Arc<AtomicBool>, JoinHandle<Result<Seen, &'static str>>);

pub struct Poller {
    command: fn(i64) -> Command,
    worker: Option<Worker>,
}
impl Poller {
    pub fn new(command: fn(i64) -> Command) -> Self {
        Self {
            command,
            worker: None,
        }
    }
    /// A finished read lands even while hidden; a new one starts only while shown.
    pub fn tick(&mut self, app: &mut App, now: Instant, visible: bool) -> bool {
        let mut dirty = false;
        if self.worker.as_ref().is_some_and(|w| w.1.is_finished()) {
            let (_, handle) = self.worker.take().expect("a finished worker");
            let result = handle.join().unwrap_or(Err(UNAVAILABLE));
            app.health.observe(result, now);
            dirty = true;
        }
        if visible && app.health.repo.is_some() && self.worker.is_none() && app.health.ready(now) {
            app.health.last_start = Some(now);
            app.health.inflight = true;
            let cancel = Arc::new(AtomicBool::new(false));
            let flag = cancel.clone();
            let root = app.health.root;
            let command = (self.command)(root);
            let repo = app
                .health
                .repo
                .clone()
                .expect("configured health repository");
            self.worker = Some((
                cancel,
                thread::spawn(move || fetch(command, BUDGET, &flag, root, &repo)),
            ));
            dirty = true;
        }
        dirty
    }
}
impl Drop for Poller {
    fn drop(&mut self) {
        if let Some((cancel, handle)) = self.worker.take() {
            cancel.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    }
}

/// Invented notes, through the real parser: `--demo` and the frames read nothing.
pub fn sample(now: i64, payload: &str) -> Health {
    let checked = OffsetDateTime::from_unix_timestamp(now - 240).expect("sample time");
    let created = OffsetDateTime::from_unix_timestamp(now - 200).expect("sample time");
    let stamp = |t: OffsetDateTime| {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            t.year(),
            t.month() as u8,
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        )
    };
    let summary = payload.replace("{checked}", &stamp(checked));
    let body = format!(
        "{{\"record\":\"event\",\"id\":7001,\"task_id\":{SAMPLE_ROOT},\"kind\":\"note\",\"summary\":\"lane started\",\"created_at\":\"{}\"}}\n\
         {{\"record\":\"event\",\"id\":7002,\"task_id\":{SAMPLE_ROOT},\"kind\":\"note\",\"summary\":{},\"created_at\":\"{}\"}}\n",
        stamp(checked),
        serde_json::to_string(&summary).expect("sample JSON"),
        stamp(created)
    );
    Health {
        repo: Some("example-org/app".into()),
        root: SAMPLE_ROOT,
        seen: Some(Seen {
            read_at: now - 30,
            reading: parse(body.as_bytes(), now - 30, SAMPLE_ROOT, "example-org/app")
                .expect("sample parses"),
        }),
        ..Health::default()
    }
}
pub const CLEAN: &str = "HEALTH repo=example-org/app env=production sha=0a1b2c3d4e \
    deploy=live sentry=clean checked={checked} rollback=7000";
pub fn demo(now: i64) -> Health {
    sample(now, CLEAN)
}

#[cfg(test)]
mod tests {
    use super::*;
    const ROOT: i64 = SAMPLE_ROOT;
    const REPO: &str = "example-org/app";

    const NOW: i64 = 1_792_000_000;
    /// 2026-10-14T17:46:40Z is NOW.
    const AT: &str = "2026-10-14T17:40:00Z";

    /// An event row as `taskr --json log` prints it: its columns and `record: event`.
    fn event(id: i64, kind: &str, summary: &str) -> String {
        serde_json::json!({"id": id, "task_id": ROOT, "task_name": "hub",
            "recipient_task_id": null, "launch_id": 99, "kind": kind, "summary": summary,
            "data": null, "related_event_id": null, "answered_by": null, "event_key": null,
            "created_at": "2026-10-14T17:41:00.000Z", "record": "event"})
        .to_string()
    }
    fn good(order: &[&str]) -> String {
        let fields = [
            ("repo", REPO),
            ("env", ENV),
            ("sha", "0123456789"),
            ("deploy", "live"),
            ("sentry", "clean"),
            ("checked", AT),
            ("rollback", "41"),
        ];
        let mut out = "HEALTH".to_string();
        for key in order {
            let v = fields.iter().find(|(k, _)| k == key).unwrap().1;
            out += &format!(" {key}={v}");
        }
        out
    }
    fn read(lines: &[String]) -> Result<Reading, &'static str> {
        parse((lines.join("\n") + "\n").as_bytes(), NOW, ROOT, REPO)
    }
    fn unknown(lines: &[String]) -> &'static str {
        match read(lines).unwrap() {
            Reading::Unknown(why) => why,
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn exact_root_note_any_key_order_and_every_bad_key() {
        let mut reversed = KEYS;
        reversed.reverse();
        for order in [KEYS, reversed] {
            let Reading::Report(r) = read(&[event(5, "note", &good(&order))]).unwrap() else {
                panic!()
            };
            assert_eq!(
                (r.note, r.sha.as_str(), r.deploy.as_str(), r.rollback),
                (5, "0123456789", "live", 41)
            );
            assert_eq!(r.checked, AT);
            assert_eq!(r.noted, Some(NOW - 340));
        }
        let base = good(&KEYS);
        for key in KEYS {
            // Missing, duplicated, empty.
            let missing: Vec<&str> = KEYS.iter().copied().filter(|k| *k != key).collect();
            let dup = format!("{base} {key}=x");
            let empty = base
                .split(' ')
                .map(|t| {
                    if t.starts_with(&format!("{key}=")) {
                        format!("{key}=")
                    } else {
                        t.into()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            for bad in [good(&missing), dup, empty] {
                assert_eq!(unknown(&[event(5, "note", &bad)]), MALFORMED, "{bad}");
            }
        }
        for bad in [
            format!("{base} extra=1"),
            format!("{base} extra"),
            format!("{base} "),
            base.replacen(' ', "  ", 2),
            base.replacen(" env=", "\tenv=", 1),
            format!("{base}\nsha=0123456789"),
            base.replace("sha=0123456789", "sha=012345678"),
            base.replace("sha=0123456789", "sha=012345678g"),
            base.replace("sha=0123456789", "sha=0123456789a"),
            base.replace("deploy=live", "deploy=Live"),
            base.replace("deploy=live", "deploy=ok"),
            base.replace("sentry=clean", "sentry=new:"),
            base.replace("sentry=clean", "sentry=new:https://x.io/1"),
            base.replace("sentry=clean", "sentry=new:A\u{202e}B"),
            base.replace("sentry=clean", "sentry=quiet"),
            base.replace(AT, "2026-10-14T17:40:00+02:00"),
            base.replace(AT, "2026-10-14 17:40:00"),
            base.replace(AT, "yesterday"),
            base.replace("rollback=41", "rollback=0"),
            base.replace("rollback=41", "rollback=041"),
            base.replace("rollback=41", "rollback=-4"),
            base.replace("rollback=41", "rollback=#41"),
            format!("{base}{}", " sentry=clean".repeat(80)),
        ] {
            assert_eq!(unknown(&[event(5, "note", &bad)]), MALFORMED, "{bad}");
        }
        let Reading::Report(r) = read(&[event(
            5,
            "note",
            &base.replace("sentry=clean", "sentry=new:TRIP-1A,TRIP-2_b.c"),
        )])
        .unwrap() else {
            panic!()
        };
        assert!(r.attention());
        assert_eq!(
            unknown(&[event(
                5,
                "note",
                &base.replace("env=production", "env=staging")
            )]),
            OTHER
        );
        assert_eq!(
            unknown(&[event(5, "note", &base.replace(REPO, "a/b"))]),
            OTHER
        );
        assert_eq!(
            unknown(&[event(5, "note", &base.replace(AT, "2026-10-14T17:46:41Z"))]),
            FUTURE
        );
        // Only an exact prefix on a note: no substring, no other kind, no tab for its space.
        for (kind, text) in [
            ("note", base.replacen(' ', "\t", 1)),
            ("note", format!("x {base}")),
            ("note", base.to_lowercase()),
            ("note", base.replacen("HEALTH ", "HEALTH:", 1)),
            ("ready", base.clone()),
            ("decision", base.clone()),
        ] {
            assert_eq!(unknown(&[event(5, kind, &text)]), NO_NOTE);
        }
    }

    #[test]
    fn latest_event_decides_and_launches_trailers_and_other_events_are_safe() {
        let old = event(3, "note", &good(&KEYS));
        let newer_bad = event(9, "note", "HEALTH repo=example-org/app");
        // Invalid latest never falls back to the older clean note.
        assert_eq!(unknown(&[old.clone(), newer_bad.clone()]), MALFORMED);
        // A later event with an older checked instant stays the latest.
        let early = good(&KEYS).replace(AT, "2026-10-14T10:00:00Z");
        let Reading::Report(r) = read(&[old.clone(), event(9, "note", &early)]).unwrap() else {
            panic!()
        };
        assert_eq!((r.note, r.checked.as_str()), (9, "2026-10-14T10:00:00Z"));
        // Launch records (their IDs are not event IDs), non-note events without summaries,
        // and the trailer are set aside.
        let launch = serde_json::json!({"id":99,"task_id":ROOT,"provider":"x","model":"y",
            "effort":null,"account":null,"native_home":null,"session_ref":null,
            "session_kind":null,"session_source":null,"herdr_scope":null,"pane_id":null,
            "observed_status":null,"observed_seq":null,"observed_version":null,"present":1,
            "observed_at":null,"recorded_at":"2026-10-14T17:00:00.000Z","workspace_id":null,
            "tab_id":null,"record":"launch"})
        .to_string();
        let ready = event(4, "ready", "x").replace("\"x\"", "null");
        let trailer = r#"{"older":12,"before":3}"#.to_string();
        let Reading::Report(r) = read(&[launch.clone(), old.clone(), ready, trailer]).unwrap()
        else {
            panic!()
        };
        assert_eq!(r.note, 3);
        assert_eq!(unknown(std::slice::from_ref(&launch)), NO_NOTE);
        // record=event rows keep event validation and event-ID order.
        assert_eq!(
            read(&[event(5, "ready", "x"), old.clone()]),
            Err(UNAVAILABLE)
        );
        assert_eq!(parse(b"", NOW, ROOT, REPO), Ok(Reading::Unknown(NO_NOTE)));
        // Structural failures fail the whole read.
        for lines in [
            vec!["not json".to_string()],
            vec![old.clone(), String::new()],
            vec!["[]".into()],
            vec!["{}".into()],
            vec![r#"{"record":"other","id":1}"#.into()],
            vec![event(4, "note", "x").replace("\"event\"", "\"daemon\"")],
            vec![event(4, "note", "x").replace("\"record\":\"event\"", "\"record\":1")],
            vec![event(4, "note", "x").replace("\"id\":4,", "")],
            vec![r#"{"record":"launch"}"#.into()],
            vec![r#"{"older":"x","id":1}"#.into()],
            vec![old.clone(), old.clone()],
            vec![event(9, "note", "x"), old.clone()],
            vec![event(0, "note", "x")],
            vec![event(4, "note", "x").replace("2899", "2900")],
            vec![event(4, "note", "x").replace("\"x\"", "1")],
            vec![event(4, "note", "x").replace("created_at", "at")],
        ] {
            assert_eq!(read(&lines), Err(UNAVAILABLE), "{lines:?}");
        }
        assert_eq!(parse(b"\xff\n", NOW, ROOT, REPO), Err(UNAVAILABLE));
        // A note without text, as taskr prints it (the key omitted) or as null, is no
        // candidate: it neither fails the read nor displaces the latest HEALTH note.
        let absent = event(9, "note", "x").replace("\"summary\":\"x\",", "");
        let null = event(10, "note", "x").replace("\"x\"", "null");
        assert!(!absent.contains("summary"));
        assert_eq!(unknown(&[absent.clone(), null.clone()]), NO_NOTE);
        let Reading::Report(r) = read(&[old.clone(), absent, null]).unwrap() else {
            panic!()
        };
        assert_eq!(r.note, 3);
    }

    #[test]
    fn unknown_replaces_failures_keep_stale_and_admission_is_hard() {
        let now = Instant::now();
        let mut h = Health::default();
        assert!(h.ready(now));
        assert_eq!(h.label(NOW), "Unknown · no hub read yet");
        h.last_start = Some(now);
        h.observe(Err(UNAVAILABLE), now);
        assert_eq!(h.label(NOW), format!("Unknown · {UNAVAILABLE}"));
        assert!(h.state().starts_with("Unknown"));
        let report = read(&[event(5, "note", &good(&KEYS))]).unwrap();
        let seen = |reading| Seen {
            read_at: NOW,
            reading,
        };
        h.observe(Ok(seen(report.clone())), now);
        assert!(
            h.label(NOW)
                .starts_with("as of 17:40Z 6m ago · deploy live")
        );
        // A failure keeps the report, says stale first, and backs off to 15 minutes.
        for secs in [60, 120, 240, 480, 900, 900] {
            h.observe(Err(UNAVAILABLE), now);
            assert!(!h.ready(now + Duration::from_secs(secs - 1)));
            assert!(h.ready(now + Duration::from_secs(secs)));
        }
        assert!(
            h.label(NOW).starts_with("Stale · as of 17:40Z"),
            "{}",
            h.label(NOW)
        );
        assert!(h.state().starts_with("Stale"));
        // An unknown result replaces the report: no stale clean fallback.
        h.observe(Ok(seen(Reading::Unknown(MALFORMED))), now);
        assert_eq!(h.label(NOW), format!("Unknown · {MALFORMED}"));
        h.observe(Err(UNAVAILABLE), now);
        assert_eq!(h.label(NOW), format!("Stale · Unknown · {MALFORMED}"));
        // Sixty seconds between starts, whatever happens in between.
        let mut h = Health {
            last_start: Some(now),
            ..Health::default()
        };
        h.observe(Ok(seen(report)), now);
        assert!(!h.ready(now + Duration::from_secs(59)));
        assert!(h.ready(now + Duration::from_secs(60)));
        h.inflight = true;
        assert!(!h.ready(now + Duration::from_secs(600)));
    }

    #[test]
    fn fake_taskr_argv_env_deadline_bound_cancel_and_privacy() {
        let args: Vec<_> = command(ROOT)
            .get_args()
            .map(|a| a.to_str().unwrap().to_string())
            .collect();
        assert_eq!(args, ["--json", "log", "2899", "--limit", "100"]);
        let envs: Vec<_> = command(ROOT)
            .get_envs()
            .map(|(k, v)| (k.to_str().unwrap().to_string(), v.is_none()))
            .collect();
        assert!(envs.contains(&("TASKR_TASK".into(), true)));
        assert!(envs.contains(&("TASKR_LAUNCH".into(), true)));
        // The real argv and env removals behind a fake taskr, in a lane that has both set.
        let fake = |script: &str| {
            let mut c = Command::new("sh");
            c.args(["-c", script, "taskr"])
                .args(command(ROOT).get_args());
            c.env("TASKR_TASK", "1").env("TASKR_LAUNCH", "1");
            for (k, v) in command(ROOT).get_envs() {
                assert!(v.is_none());
                c.env_remove(k);
            }
            c
        };
        let body = event(5, "note", &good(&KEYS)).replace(AT, "2026-10-14T00:00:00Z");
        let ok = format!(
            r#"test -z "$TASKR_TASK$TASKR_LAUNCH" || exit 9
            read unused && exit 1
            test "$*" = "--json log 2899 --limit 100" || exit 1
            printf '%s\n' '{body}'"#
        );
        let seen = fetch(
            fake(&ok),
            Duration::from_secs(5),
            &AtomicBool::new(false),
            ROOT,
            REPO,
        )
        .unwrap();
        assert!(matches!(seen.reading, Reading::Report(ref r) if r.note == 5));
        let start = Instant::now();
        assert_eq!(
            fetch(
                fake("exec sleep 20"),
                Duration::from_millis(150),
                &AtomicBool::new(false),
                ROOT,
                REPO,
            ),
            Err(UNAVAILABLE)
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        // Over the 8 MiB bound, a cancel, a failing exit with private text: all one message.
        let big = "head -c 9000000 /dev/zero | tr '\\0' 'x'";
        let private = "printf 'secret-token private payload' >&2; exit 3";
        for (script, cancel) in [(big, false), ("exec sleep 20", true), (private, false)] {
            let start = Instant::now();
            let e = fetch(
                fake(script),
                Duration::from_secs(5),
                &AtomicBool::new(cancel),
                ROOT,
                REPO,
            );
            assert_eq!(e, Err(UNAVAILABLE));
            assert!(start.elapsed() < Duration::from_secs(3));
        }
        let mut h = Health::default();
        h.observe(Err(UNAVAILABLE), Instant::now());
        assert!(!format!("{h:?}{}{}", h.state(), h.detail(NOW)).contains("secret"));
    }

    fn fake_command(_: i64) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", "printf ''"]);
        c
    }
    fn slow_command(_: i64) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", "exec sleep 20"]);
        c
    }

    #[test]
    fn poller_reads_only_while_shown_lands_hidden_and_drop_joins() {
        let mut app = App::new(crate::model::Data::default());
        app.health.repo = Some(REPO.into());
        let mut poller = Poller::new(fake_command);
        let now = Instant::now();
        assert!(!poller.tick(&mut app, now, false));
        assert!(poller.worker.is_none() && app.health.last_start.is_none());
        assert!(poller.tick(&mut app, now, true));
        assert!(app.health.inflight);
        // It lands even after the panel is hidden, and no other starts.
        while !poller.worker.as_ref().is_none_or(|w| w.1.is_finished()) {
            thread::yield_now();
        }
        assert!(poller.tick(&mut app, now + Duration::from_secs(1), false));
        assert!(poller.worker.is_none() && !app.health.inflight);
        assert_eq!(
            app.health.seen.as_ref().map(|s| &s.reading),
            Some(&Reading::Unknown(NO_NOTE))
        );
        // Shown again within the minute: nothing starts.
        assert!(!poller.tick(&mut app, now + Duration::from_secs(59), true));
        assert!(poller.tick(&mut app, now + Duration::from_secs(60), true));
        // One in flight: a running read admits no other, however long it takes.
        let mut slow = Poller::new(slow_command);
        let mut app = App::new(crate::model::Data::default());
        app.health.repo = Some(REPO.into());
        assert!(slow.tick(&mut app, now, true));
        assert!(!slow.tick(&mut app, now + Duration::from_secs(600), true));
        assert!(app.health.inflight && slow.worker.is_some());
        let start = Instant::now();
        drop(slow);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn configured_repo_controls_health_scope_and_unset_does_not_read() {
        let body = event(5, "note", &good(&KEYS).replace(REPO, "other/health"));
        assert!(matches!(
            parse(body.as_bytes(), NOW, ROOT, "other/health"),
            Ok(Reading::Report(_))
        ));
        assert_eq!(
            parse(body.as_bytes(), NOW, ROOT, REPO),
            Ok(Reading::Unknown(OTHER))
        );
        let mut app = App::new(crate::model::Data::default());
        let mut poller = Poller::new(|_| panic!("unset health repo must not start a command"));
        assert!(!poller.tick(&mut app, Instant::now(), true));
        assert!(poller.worker.is_none());
        // A configured root is the one read, and another root's events are refused.
        let args: Vec<_> = command(4242).get_args().map(|a| a.to_owned()).collect();
        assert_eq!(args, ["--json", "log", "4242", "--limit", "100"]);
        assert_eq!(
            parse(body.as_bytes(), NOW, 4242, "other/health"),
            Err(UNAVAILABLE)
        );
        let mut app = App::new(crate::model::Data::default());
        (app.health.repo, app.health.root) = (Some(REPO.into()), 4242);
        let mut poller = Poller::new(|root| {
            assert_eq!(root, 4242);
            fake_command(root)
        });
        assert!(poller.tick(&mut app, Instant::now(), true));
    }

    #[test]
    fn demo_sample_parses_and_names_its_clocks() {
        let h = demo(NOW);
        let label = h.label(NOW);
        assert!(
            label.starts_with("as of 17:42Z 4m ago · deploy live · sentry clean"),
            "{label}"
        );
        assert!(
            h.source()
                .starts_with("checked 2026-10-14T17:42:40Z · note #7002")
        );
        let d = h.detail(NOW);
        assert!(!d.contains("not compared"), "{d}");
        for part in [
            "sha 0a1b2c3d4e",
            "checked 2026-10-14T17:42:40Z (4m ago)",
            "rollback note #7000",
            "read 17:46 UTC",
        ] {
            assert!(d.contains(part), "{part}: {d}");
        }
    }
}
