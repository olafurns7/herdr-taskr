//! My PRs against a fake `gh` (a shell script answering from files) and a fake clock: the
//! schedule takes `now`, the processes and deadlines are real.
use super::*;
use serde_json::json;
use std::{fs, path::PathBuf, sync::atomic::AtomicUsize};

const WALL: i64 = 1_792_000_000;
const HEAD: &str = "aaaaaaa1";

fn stamp(at: i64) -> String {
    crate::github::utc_stamp(at)
}
fn ctx(name: &str, conclusion: &str, required: bool) -> Value {
    json!({"__typename":"CheckRun","name":name,"status":"COMPLETED","conclusion":conclusion,"isRequired":required})
}
/// An open PR on `HEAD` with one observed required success.
fn alias(n: u64) -> Value {
    json!({"pullRequest":{"number":n,"state":"OPEN","title":format!("pr {n}"),"isDraft":false,
        "updatedAt":stamp(WALL - n as i64),"headRefOid":HEAD,"mergeable":"MERGEABLE",
        "mergeStateStatus":"BLOCKED","reviewDecision":"REVIEW_REQUIRED",
        "commits":{"nodes":[{"commit":{"oid":HEAD,"statusCheckRollup":{"contexts":{
            "totalCount":1,"pageInfo":{"hasNextPage":false},"nodes":[ctx("build","SUCCESS",true)]}}}}]},
        "reviewThreads":{"totalCount":0,"pageInfo":{"hasNextPage":false},"nodes":[]}}})
}
fn contexts(v: &mut Value, nodes: Vec<Value>, total: usize) {
    let c = &mut v["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
    let more = total > nodes.len();
    *c = json!({"totalCount":total,"pageInfo":{"hasNextPage":more},"nodes":nodes});
}
fn reply(aliases: Vec<Value>) -> Value {
    let mut data = serde_json::Map::new();
    for (i, a) in aliases.into_iter().enumerate() {
        data.insert(format!("p{i}"), a);
    }
    data.insert(
        "rateLimit".into(),
        json!({"cost":1,"remaining":4000,"resetAt":stamp(WALL + 600)}),
    );
    json!({ "data": data })
}
fn keys(numbers: &[u64]) -> Vec<(String, u64)> {
    numbers.iter().map(|n| ("o/r".to_string(), *n)).collect()
}
fn one(v: Value) -> Pr {
    parse_read(&reply(vec![v]), &keys(&[1]))
        .unwrap()
        .prs
        .remove(0)
}
fn search(nodes: Vec<(&str, Value)>, count: usize) -> Value {
    let nodes: Vec<Value> = nodes
        .into_iter()
        .map(|(repo, n)| json!({"number":n,"repository":{"nameWithOwner":repo}}))
        .collect();
    json!({"data":{"search":{"issueCount":count,"nodes":nodes},
        "rateLimit":{"cost":1,"remaining":4001,"resetAt":stamp(WALL + 600)}}})
}

#[test]
fn an_observed_required_success_never_reads_as_the_required_set_passing() {
    // One required check reported success; another required check (or one that runs on
    // the test-merge commit, not the head) never reported. No merge state proves it.
    for merge in ["BLOCKED", "DRAFT", "BEHIND", "UNKNOWN", "CLEAN"] {
        let mut v = alias(1);
        v["pullRequest"]["mergeStateStatus"] = json!(merge);
        v["pullRequest"]["isDraft"] = json!(merge == "DRAFT");
        if merge == "CLEAN" {
            v["pullRequest"]["reviewDecision"] = json!("APPROVED");
        }
        let pr = one(v);
        let c = pr.read.as_ref().unwrap().checks.clone().unwrap();
        assert_eq!((c.required_pass, c.required_fail), (1, 0), "{merge}");
        assert!(!pr.required_failed(), "{merge}");
        let facts = pr.facts();
        assert!(facts.contains("req ?"), "{merge}: {facts}");
        assert!(!facts.contains("req ✓"), "{merge}: {facts}");
        let expected = match merge {
            "DRAFT" => Ci::Draft,
            _ => Ci::Passing,
        };
        // Observed checks pass; that is all the green there is.
        assert_eq!(pr.ci(), expected, "{merge}");
    }
    // UNKNOWN merge state stays unknown, never clean.
    let mut v = alias(1);
    v["pullRequest"]["mergeStateStatus"] = json!("UNKNOWN");
    v["pullRequest"]["mergeable"] = json!("UNKNOWN");
    assert_eq!(one(v).merge_word(), "?");
    // Only an observed required failure on the bound head gives req ✗.
    let mut v = alias(1);
    contexts(
        &mut v,
        vec![
            ctx("build", "SUCCESS", true),
            ctx("e2e", "TIMED_OUT", true),
            ctx("lint", "FAILURE", false),
        ],
        3,
    );
    let pr = one(v);
    assert!(pr.required_failed());
    assert_eq!(pr.ci(), Ci::Failing);
    let facts = pr.facts();
    assert!(
        facts.starts_with("✓1 ✗2 ◷0 · req ✗ · review · blocked · unres 0 · e2e, lint"),
        "{facts}"
    );
}

#[test]
fn a_head_that_moved_between_stages_keeps_its_scalars_and_unknowns_every_check() {
    // Discovery saw the PR at head A; by detail the branch is at B while the rollup read is
    // still A's: the failing check belongs to A and must not join B's row.
    let mut v = alias(1);
    v["pullRequest"]["headRefOid"] = json!("bbbbbbb2");
    contexts(&mut v, vec![ctx("e2e", "FAILURE", true)], 1);
    v["pullRequest"]["reviewDecision"] = json!("APPROVED");
    let pr = one(v);
    let f = pr.read.as_ref().unwrap();
    assert_eq!(f.checks, Err("head moved"));
    assert_eq!(f.head, "bbbbbbb2");
    assert!(!pr.required_failed());
    assert_eq!(pr.ci(), Ci::Unknown);
    assert_eq!(
        pr.facts(),
        "checks ? head moved · req ? · approved · blocked · unres 0"
    );
}

#[test]
fn closed_null_and_field_errors_are_scoped_to_their_row_or_field() {
    let mut merged = alias(1);
    merged["pullRequest"]["state"] = json!("MERGED");
    let mut threads = alias(4);
    threads["pullRequest"]["reviewThreads"] = Value::Null;
    let mut required = alias(5);
    required["pullRequest"]["commits"] = Value::Null;
    let mut body = reply(vec![
        merged,
        Value::Null,
        json!({"pullRequest":null}),
        threads,
        required,
        alias(6),
    ]);
    body["errors"] = json!([
        {"type":"NOT_FOUND","path":["p1"],"message":"secret repo name"},
        {"type":"FORBIDDEN","path":["p2","pullRequest"],"message":"no access"},
        {"path":["p3","pullRequest","reviewThreads"],"message":"x"},
        {"path":["p4","pullRequest","commits","nodes",0,"commit","statusCheckRollup","contexts","nodes",3,"isRequired"]},
        {"path":["p5","pullRequest","title"]}
    ]);
    let s = parse_read(&body, &keys(&[1, 2, 3, 4, 5, 6])).unwrap();
    assert_eq!(s.closed, 1);
    assert!(s.partial);
    let numbers: Vec<_> = s.prs.iter().map(|p| (p.number, p.read.is_some())).collect();
    assert_eq!(
        numbers,
        [(2, false), (3, false), (4, true), (5, true), (6, false)]
    );
    let threads = &s.prs[2];
    assert!(threads.read.as_ref().unwrap().threads.is_none());
    assert!(threads.facts().contains("unres ?"), "{}", threads.facts());
    assert_eq!(threads.review_word(), "review");
    let required = &s.prs[3];
    assert_eq!(required.read.as_ref().unwrap().checks, Err("not readable"));
    assert_eq!(required.ci(), Ci::Unknown);
    assert!(!required.facts().contains("✓0"));
    assert_eq!(s.prs[0].facts(), "not readable · every field ?");
    let mut mine = Mine::new(&["o/r".into()]);
    mine.login = Some("octo".into());
    mine.land(Ok(s), Instant::now(), WALL);
    let notes = mine.notes();
    // Dropped PRs have no row, so the card keeps that note; the others repeat their rows.
    assert!(
        notes.contains(&("1 closed since search".into(), true)),
        "{notes:?}"
    );
    assert!(
        notes.contains(&("3 not readable".into(), false)),
        "{notes:?}"
    );
    assert!(
        notes.contains(&("some fields could not be read".into(), false)),
        "{notes:?}"
    );
    assert!(!format!("{mine:?}").contains("secret repo name"));
    // Whole-reply failures: data null, an error with no path, an alias missing.
    let errors = json!([{"message":"boom"}]);
    assert!(parse_read(&json!({"data":null,"errors":errors}), &keys(&[1])).is_err());
    let mut unscoped = reply(vec![alias(1)]);
    unscoped["errors"] = errors;
    assert!(parse_read(&unscoped, &keys(&[1])).is_err());
    assert!(parse_read(&reply(vec![alias(1)]), &keys(&[1, 2])).is_err());
    let mut stray = reply(vec![alias(1)]);
    stray["errors"] = json!([{"path":["p01","pullRequest"]}]);
    assert!(parse_read(&stray, &keys(&[1])).is_err());
    // An unknown state, a number that is not the one asked, or a bad head: not readable.
    for (key, value) in [
        ("state", json!("LOCKED")),
        ("number", json!(2)),
        ("headRefOid", json!("nothex!")),
    ] {
        let mut v = alias(1);
        v["pullRequest"][key] = value;
        assert!(one(v).read.is_none(), "{key}");
    }
}

#[test]
fn more_than_a_page_of_contexts_or_threads_stays_a_lower_bound() {
    // 100 successes read, 30 more contexts unread: a failure may be among them.
    let mut v = alias(1);
    contexts(&mut v, vec![ctx("unit", "SUCCESS", false); 100], 130);
    let mut t = vec![json!({"isResolved":true,"isOutdated":false,"path":"a.rs","line":1}); 100];
    t[0] = json!({"isResolved":false,"isOutdated":true,"path":"b.rs\u{1b}[31m","line":null});
    v["pullRequest"]["reviewThreads"] =
        json!({"totalCount":150,"pageInfo":{"hasNextPage":true},"nodes":t});
    let pr = one(v);
    assert_eq!(pr.ci(), Ci::Unknown);
    assert_eq!(
        pr.facts(),
        "✓100+? ✗0+? ◷0+? · req ? · review · blocked · unres 1+? · ?"
    );
    let threads = pr.read.as_ref().unwrap().threads.clone().unwrap();
    assert_eq!(threads.unresolved, [(String::from("b.rs[31m"), None, true)]);
    // No unresolved thread in the first page of 120: still not zero.
    let mut v = alias(1);
    v["pullRequest"]["reviewThreads"] = json!({"totalCount":120,"pageInfo":{"hasNextPage":true},
        "nodes":vec![json!({"isResolved":true,"isOutdated":false,"path":"a.rs","line":1}); 100]});
    assert!(one(v).facts().contains("unres 0+?"));
    // A null rollup is unknown, not zero checks.
    let mut v = alias(1);
    v["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = Value::Null;
    let pr = one(v);
    assert_eq!(pr.ci(), Ci::Unknown);
    assert!(
        pr.facts().starts_with("checks ? no check rollup"),
        "{}",
        pr.facts()
    );
    // Neutral or skipped alone never pass; an unknown state blocks passing.
    for nodes in [
        vec![ctx("a", "SKIPPED", false), ctx("b", "NEUTRAL", false)],
        vec![ctx("a", "SUCCESS", false), ctx("b", "STALE", false)],
        vec![
            json!({"__typename":"FutureContext","isRequired":false}),
            ctx("a", "SUCCESS", false),
        ],
    ] {
        let mut v = alias(1);
        let n = nodes.len();
        contexts(&mut v, nodes, n);
        assert_eq!(one(v).ci(), Ci::Unknown);
    }
    // A malformed context makes every check fact unknown, never a partial count.
    let mut v = alias(1);
    contexts(
        &mut v,
        vec![json!({"__typename":"CheckRun","name":"x","status":"COMPLETED"})],
        1,
    );
    assert_eq!(one(v).read.unwrap().checks, Err("not readable"));
}

#[test]
fn discovery_validates_scope_cap_dedupe_and_reports_a_zero_sample_honestly() {
    let repos = vec!["Owner/Repo".to_string(), "o/r".to_string()];
    let f = parse_found(
        &search(
            vec![
                ("owner/repo", json!(3)),
                ("o/r", json!(2)),
                ("o/r", json!(2)),
                ("x/y", json!(9)),
            ],
            4,
        ),
        &repos,
    )
    .unwrap();
    assert_eq!(
        f.keys,
        [(String::from("Owner/Repo"), 3), (String::from("o/r"), 2)]
    );
    assert_eq!((f.found, f.outside), (4, 1));
    assert_eq!(f.rate.unwrap().remaining, 4001);
    for bad in [json!(0), json!(-1), json!("2"), Value::Null] {
        assert!(
            parse_found(&search(vec![("o/r", bad.clone())], 1), &repos).is_err(),
            "{bad}"
        );
    }
    let missing = json!({"data":{"search":{"issueCount":1,"nodes":[{"number":1}]}}});
    assert!(parse_found(&missing, &repos).is_err());
    let empty_node = json!({"data":{"search":{"issueCount":1,"nodes":[{}]}}});
    assert!(parse_found(&empty_node, &repos).is_err());
    let too_many: Vec<_> = (1..=31).map(|n| ("o/r", json!(n))).collect();
    assert!(parse_found(&search(too_many, 31), &repos).is_err());
    let mut errored = search(vec![], 0);
    errored["errors"] = json!([{"message":"x"}]);
    assert!(parse_found(&errored, &repos).is_err());
    assert!(parse_found(&json!({"data":null}), &repos).is_err());
    // Capped: first 30 of 45.
    let thirty: Vec<_> = (1..=30).map(|n| ("o/r", json!(n))).collect();
    let mut mine = Mine::new(&repos);
    mine.login = Some("octo".into());
    let f = parse_found(&search(thirty, 45), &repos).unwrap();
    assert_eq!(f.keys.len(), 30);
    mine.land(
        Ok(Sample {
            found: f.found,
            ..Sample::default()
        }),
        Instant::now(),
        WALL,
    );
    assert!(
        mine.notes()
            .contains(&("first 30 of 45; more may exist".into(), true))
    );
    // Zero: an observed search sample, never "no open PRs".
    mine.land(Ok(Sample::default()), Instant::now(), WALL);
    let status = mine.status(Instant::now());
    assert!(status.starts_with("0 observed · 2 repos"), "{status}");
    assert!(status.contains("search sample"), "{status}");
    assert!(mine.notes().is_empty());
}

#[test]
fn backoff_manual_gap_and_provider_deadlines_hold_on_a_fake_clock() {
    let t0 = Instant::now();
    let mut mine = Mine::new(&["o/r".into()]);
    mine.login = Some("octo".into());
    assert!(mine.ready(t0, false));
    mine.last_start = Some(t0);
    let prs = demo(WALL).prs;
    mine.land(
        Ok(Sample {
            prs,
            ..Sample::default()
        }),
        t0,
        WALL,
    );
    // Manual r: five seconds after the cycle's start, not before; scheduled: 60 s.
    assert!(!mine.ready(t0 + Duration::from_secs(4), true));
    assert!(mine.ready(t0 + Duration::from_secs(5), true));
    assert!(!mine.ready(t0 + Duration::from_secs(59), false));
    assert!(mine.ready(t0 + MINUTE, false));
    let good = mine.prs.clone();
    for secs in [60, 120, 240, 480, 900, 900] {
        mine.land(Err(Failure::unavailable()), t0, WALL);
        assert_eq!(mine.due, Some(t0 + Duration::from_secs(secs)));
        assert!(
            !mine.ready(t0 + Duration::from_secs(secs - 1), true),
            "{secs}"
        );
        assert!(mine.ready(t0 + Duration::from_secs(secs), false));
    }
    assert_eq!(mine.prs, good);
    assert!(mine.status(t0).starts_with("stale"));
    // A known provider deadline beyond the cap.
    mine.land(
        Err(Failure {
            message: "GitHub rate limited",
            retry: Duration::from_secs(7200),
        }),
        t0,
        WALL,
    );
    assert!(!mine.ready(t0 + CAP, true));
    assert!(mine.ready(t0 + Duration::from_secs(7200), true));
    // A success that leaves fewer points than a cycle costs waits for GitHub's reset.
    let rate = Rate {
        cost: 3,
        remaining: 2,
        reset: WALL + 1200,
    };
    mine.land(
        Ok(Sample {
            rate: Some(rate),
            ..Sample::default()
        }),
        t0,
        WALL,
    );
    assert!(mine.error.is_none());
    assert!(!mine.ready(t0 + Duration::from_secs(1199), true));
    assert!(mine.ready(t0 + Duration::from_secs(1200), false));
    // An unbounded deadline blocks for the run.
    mine.land(
        Err(Failure {
            message: "GitHub rate limited",
            retry: Duration::MAX,
        }),
        t0,
        WALL,
    );
    assert!(!mine.ready(t0 + CAP * 100, true));
}

/// A fake gh in a scratch directory: it logs each argv and answers from files.
struct Fake {
    dir: PathBuf,
}
const SCRIPT: &str = r#"test "$GH_PROMPT_DISABLED" = 1 || exit 9
printf '%s\n' "$*" >> "$D/calls"
case "$2" in
user) test -f "$D/user-fail" && { printf 'token ghp_secret\n' >&2; exit 1; }; cat "$D/login" ;;
graphql) case "$*" in
  *search\(*) test -f "$D/find-sleep" && sleep "$(cat "$D/find-sleep")"; cat "$D/find.json" ;;
  *) test -f "$D/read-sleep" && sleep "$(cat "$D/read-sleep")"; cat "$D/read.json" ;;
  esac ;;
*) exit 9 ;;
esac"#;
impl Fake {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "taskr-tui-prs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("login"), "octo\n").unwrap();
        Self { dir }
    }
    fn put(&self, name: &str, body: impl AsRef<[u8]>) {
        fs::write(self.dir.join(name), body).unwrap();
    }
    fn poller(&self) -> Poller {
        let dir = self.dir.clone();
        Poller::new(move || {
            let mut c = Command::new("sh");
            c.args(["-c", SCRIPT, "gh"]).env("D", &dir);
            c
        })
    }
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
fn app(repos: &[&str]) -> App {
    let mut app = App::new(crate::model::Data::default());
    app.mine = Mine::new(&repos.iter().map(|r| r.to_string()).collect::<Vec<_>>());
    app
}
/// Waits (real time, bounded) for the stage in flight, then ticks at the fake `now`.
fn settle(p: &mut Poller, app: &mut App, now: Instant, visible: bool) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while p.job.as_ref().is_some_and(|j| !j.stage.finished()) {
        assert!(Instant::now() < end, "stage did not finish");
        thread::sleep(Duration::from_millis(5));
    }
    p.tick(app, now, WALL, visible, false)
}

#[test]
fn a_failed_or_invalid_identity_makes_no_pr_read_and_backs_off() {
    for broken in ["fail", "bad login!", "-octo", ""] {
        let fake = Fake::new();
        if broken == "fail" {
            fake.put("user-fail", "");
        } else {
            fake.put("login", format!("{broken}\n"));
        }
        let mut app = app(&["o/r"]);
        let mut p = fake.poller();
        let t0 = Instant::now();
        assert!(p.tick(&mut app, t0, WALL, true, false));
        assert_eq!(app.mine.status(t0), "reading your GitHub login");
        settle(&mut p, &mut app, t0, true);
        assert!(app.mine.login.is_none(), "{broken}");
        let status = app.mine.status(t0);
        assert!(
            status.starts_with("unavailable: your GitHub login could not be read"),
            "{status}"
        );
        assert!(!status.contains("ghp_secret"));
        // Manual r inside the backoff reads nothing, of either kind.
        for secs in [5, 30, 59] {
            assert!(!p.tick(&mut app, t0 + Duration::from_secs(secs), WALL, true, true));
        }
        assert_eq!(fake.calls(), ["api user --jq .login"], "{broken}");
        // The retry is another identity read, never a PR read.
        p.tick(&mut app, t0 + MINUTE, WALL, true, false);
        settle(&mut p, &mut app, t0 + MINUTE, true);
        assert_eq!(fake.calls().len(), 2);
        assert!(fake.calls().iter().all(|c| c.starts_with("api user")));
        assert_eq!(app.mine.due, Some(t0 + MINUTE * 3));
    }
}

#[test]
fn one_global_two_stage_cycle_reads_every_watched_repo_for_two_gh_calls() {
    let fake = Fake::new();
    let repos: Vec<String> = (0..8).map(|i| format!("o/r{i}")).collect();
    let refs: Vec<&str> = repos.iter().map(String::as_str).collect();
    fake.put(
        "find.json",
        search(vec![("o/r1", json!(7)), ("O/R5", json!(3))], 2).to_string(),
    );
    let mut a = alias(7);
    contexts(&mut a, vec![ctx("e2e", "FAILURE", true)], 1);
    fake.put("read.json", reply(vec![a, alias(3)]).to_string());
    let mut app = app(&refs);
    let mut p = fake.poller();
    let t0 = Instant::now();
    p.tick(&mut app, t0, WALL, true, false);
    // The identity read is not a cycle: the first cycle starts as it lands.
    settle(&mut p, &mut app, t0, true);
    assert_eq!(app.mine.login.as_deref(), Some("octo"));
    assert!(matches!(
        p.job.as_ref().map(|j| &j.stage),
        Some(Stage::Find(_))
    ));
    let t1 = t0 + Duration::from_millis(100);
    settle(&mut p, &mut app, t1, true);
    settle(&mut p, &mut app, t1, true);
    let calls = fake.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    let q = calls[1].split(" -f ").nth(2).unwrap();
    let wanted: String = repos.iter().map(|r| format!(" repo:{r}")).collect();
    assert_eq!(q, format!("q=is:pr is:open author:octo{wanted}"));
    assert!(calls[1].contains("search(query:$q,type:ISSUE,first:30)"));
    assert!(calls[2].contains("p0:repository(owner:\"o\",name:\"r1\"){pullRequest(number:7)"));
    assert!(calls[2].contains("p1:repository(owner:\"o\",name:\"r5\"){pullRequest(number:3)"));
    assert!(calls[2].contains("isRequired(pullRequestNumber:3)"));
    assert!(calls[2].contains("rateLimit{cost remaining resetAt}"));
    assert!(!calls[2].contains("body"));
    let rows: Vec<_> = app
        .mine
        .prs
        .iter()
        .map(|p| (p.repo.as_str(), p.number, p.ci()))
        .collect();
    assert_eq!(rows, [("o/r1", 7, Ci::Failing), ("o/r5", 3, Ci::Passing)]);
    // Both stages' points, as GitHub reported them.
    assert_eq!(
        app.mine.sample.rate,
        Some(Rate {
            cost: 2,
            remaining: 4000,
            reset: WALL + 600
        })
    );
    // Nothing more inside the minute; manual r only from five seconds after the start.
    for (secs, manual) in [(1, false), (4, true), (59, false)] {
        assert!(!p.tick(&mut app, t0 + Duration::from_secs(secs), WALL, true, manual));
    }
    assert_eq!(fake.calls().len(), 3);
    let t5 = t0 + Duration::from_secs(5);
    assert!(p.tick(&mut app, t5, WALL, true, true));
    settle(&mut p, &mut app, t5, true);
    settle(&mut p, &mut app, t5, true);
    assert_eq!(fake.calls().len(), 5);
}

#[test]
fn stage_two_starts_only_if_the_card_is_admitted_when_stage_one_lands() {
    let fake = Fake::new();
    fake.put("find.json", search(vec![("o/r", json!(1))], 1).to_string());
    fake.put("read.json", reply(vec![alias(1)]).to_string());
    let mut app = app(&["o/r"]);
    app.mine.login = Some("octo".into());
    app.mine.prs = vec![Pr {
        repo: "o/r".into(),
        number: 99,
        read: None,
    }];
    let mut p = fake.poller();
    let t0 = Instant::now();
    assert!(p.tick(&mut app, t0, WALL, true, false));
    // Admission lost (another screen, a modal, the card hidden) before stage 1 lands.
    settle(&mut p, &mut app, t0 + Duration::from_secs(1), false);
    assert!(p.job.is_none());
    assert_eq!(fake.calls().len(), 1, "no detail read without admission");
    assert_eq!(
        app.mine.prs[0].number, 99,
        "discovery dropped, last good kept"
    );
    assert!(app.mine.error.is_none() && app.mine.failures == 0);
    assert!(!app.mine.inflight);
    // The schedule is kept: no reset, no catch-up burst on return.
    assert_eq!(app.mine.due, Some(t0 + MINUTE));
    assert!(!p.tick(&mut app, t0 + Duration::from_secs(30), WALL, true, false));
    assert!(p.tick(&mut app, t0 + MINUTE, WALL, true, false));
    // Admitted when stage 1 lands: stage 2 runs, and it finishes and lands while hidden.
    settle(&mut p, &mut app, t0 + MINUTE, true);
    assert!(matches!(
        p.job.as_ref().map(|j| &j.stage),
        Some(Stage::Read(_))
    ));
    settle(&mut p, &mut app, t0 + MINUTE, false);
    assert_eq!(fake.calls().len(), 3);
    assert_eq!(app.mine.prs[0].number, 1);
    // Hidden: due and manual, nothing starts.
    assert!(!p.tick(&mut app, t0 + MINUTE * 5, WALL, false, true));
    assert_eq!(fake.calls().len(), 3);
}

#[test]
fn a_spent_deadline_at_handoff_or_another_generation_never_lands_a_row() {
    let fake = Fake::new();
    let mut app = app(&["o/r"]);
    app.mine.login = Some("octo".into());
    app.mine.generation = 2;
    app.mine.inflight = true;
    let mut p = fake.poller();
    let finished = |found: Found| {
        let h = thread::spawn(move || Ok::<_, Failure>(found));
        while !h.is_finished() {
            thread::yield_now();
        }
        h
    };
    let found = Found {
        keys: keys(&[1]),
        found: 1,
        ..Found::default()
    };
    // Stage 1 of this cycle landed after the whole cycle's ten seconds: no stage 2.
    p.job = Some(Job {
        generation: 2,
        deadline: Instant::now(),
        cancel: Arc::new(AtomicBool::new(false)),
        stage: Stage::Find(finished(found.clone())),
    });
    let t0 = Instant::now();
    assert!(p.tick(&mut app, t0, WALL, true, false));
    assert!(p.job.is_none());
    assert_eq!(app.mine.error, Some("GitHub read timed out"));
    assert!(fake.calls().is_empty());
    // An older cycle's stage, out of order: discarded, nothing mixed in.
    let mut p = fake.poller();
    app.mine.error = None;
    let before = app.mine.prs.clone();
    let h = thread::spawn(|| {
        Ok::<_, Failure>(Sample {
            prs: vec![Pr {
                repo: "o/r".into(),
                number: 5,
                read: None,
            }],
            ..Sample::default()
        })
    });
    while !h.is_finished() {
        thread::yield_now();
    }
    p.job = Some(Job {
        generation: 1,
        deadline: Instant::now() + BUDGET,
        cancel: Arc::new(AtomicBool::new(false)),
        stage: Stage::Read(h),
    });
    app.mine.generation = 7;
    assert!(p.tick(&mut app, t0, WALL, false, false));
    assert_eq!(app.mine.prs, before);
    assert!(app.mine.error.is_none() && !app.mine.inflight);
}

#[test]
fn the_cycle_shares_one_deadline_and_output_bound_and_drop_reaps_the_child() {
    let script = |s: &str| {
        let mut c = Command::new("sh");
        c.args(["-c", s]);
        c
    };
    // Stage 1 hangs: killed at the deadline, well inside two seconds.
    let start = Instant::now();
    let e = find(
        script("exec sleep 20"),
        Instant::now() + Duration::from_millis(150),
        &AtomicBool::new(false),
        &["o/r".into()],
    )
    .unwrap_err();
    assert_eq!(e.message, "GitHub read timed out");
    assert!(start.elapsed() < Duration::from_secs(2));
    // Stage 2 gets what stage 1 left of the deadline and of the output bound.
    let found = Found {
        keys: keys(&[1]),
        bytes: LIMIT - 64,
        ..Found::default()
    };
    let body = reply(vec![alias(1)]).to_string();
    let e = read(
        script(&format!("printf '%s' '{body}'; exec sleep 1")),
        Instant::now() + Duration::from_secs(5),
        &AtomicBool::new(false),
        found.clone(),
    )
    .unwrap_err();
    assert_eq!(e.message, "GitHub response too large");
    let e = read(
        script("printf '{}'"),
        Instant::now(),
        &AtomicBool::new(false),
        found,
    )
    .unwrap_err();
    assert_eq!(e.message, "GitHub read timed out");
    // A cancel (the poller's drop) kills the running stage and joins it.
    let fake = Fake::new();
    fake.put("find-sleep", "20");
    fake.put("find.json", "{}");
    let mut app = app(&["o/r"]);
    app.mine.login = Some("octo".into());
    let mut p = fake.poller();
    assert!(p.tick(&mut app, Instant::now(), WALL, true, false));
    thread::sleep(Duration::from_millis(100));
    let start = Instant::now();
    drop(p);
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_graphql_errors_reply_survives_gh_exit_one_but_a_rate_limit_does_not() {
    let gh = |out: &str, err: &str, code: i32| {
        let mut c = Command::new("sh");
        c.args([
            "-c",
            &format!("printf '%s' '{out}'; printf '%s' '{err}' >&2; exit {code}"),
        ]);
        c
    };
    let partial = r#"{"data":{"p0":null},"errors":[{"path":["p0"],"message":"x"}]}"#;
    let deadline = Instant::now() + Duration::from_secs(5);
    let no = AtomicBool::new(false);
    let (body, _) = graphql(
        gh(partial, "gh: Could not resolve", 1),
        deadline,
        &no,
        LIMIT,
    )
    .unwrap();
    assert!(
        parse_read(&body, &keys(&[1])).unwrap().prs[0]
            .read
            .is_none()
    );
    let e = graphql(
        gh(partial, "gh: API rate limit exceeded", 1),
        deadline,
        &no,
        LIMIT,
    )
    .unwrap_err();
    assert_eq!((e.message, e.retry), ("GitHub rate limited", CAP));
    let limited = format!(
        r#"{{"data":{{"rateLimit":{{"cost":1,"remaining":0,"resetAt":"{}"}}}},"errors":[{{"type":"RATE_LIMITED","message":"x"}}]}}"#,
        stamp(wall() + 3000)
    );
    let e = graphql(gh(&limited, "", 1), deadline, &no, LIMIT).unwrap_err();
    assert_eq!(e.message, "GitHub rate limited");
    assert!(e.retry > Duration::from_secs(2900), "{:?}", e.retry);
    let e = graphql(gh("{}", "ghp_token private", 4), deadline, &no, LIMIT).unwrap_err();
    assert_eq!(e.message, "GitHub authentication required");
    assert!(!format!("{e:?}").contains("ghp_token"));
    // A non-GraphQL failure body is not a reply.
    let e = graphql(
        gh(r#"{"message":"Bad credentials"}"#, "", 1),
        deadline,
        &no,
        LIMIT,
    )
    .unwrap_err();
    assert_eq!(e.message, "GitHub unavailable");
    // Discovery that spent the last point fails the cycle until the reset.
    let mut spent = search(vec![("o/r", json!(1))], 1);
    spent["data"]["rateLimit"]["remaining"] = json!(0);
    spent["data"]["rateLimit"]["resetAt"] = json!(stamp(wall() + 900));
    let e = find(
        gh(&spent.to_string(), "", 0),
        deadline,
        &no,
        &["o/r".into()],
    )
    .unwrap_err();
    assert_eq!(e.message, "GitHub rate limited");
    assert!(e.retry > Duration::from_secs(800));
}

#[test]
fn rows_sort_failing_pending_unknown_passing_then_drafts_newest_first() {
    let mine = demo(WALL);
    let order: Vec<_> = mine.prs.iter().map(|p| (p.number, p.ci())).collect();
    assert_eq!(
        order,
        [
            (42, Ci::Failing),
            (41, Ci::Running),
            (39, Ci::Running),
            (37, Ci::Unknown),
            (40, Ci::Passing),
            (38, Ci::Draft)
        ]
    );
    assert_eq!(mine.login.as_deref(), Some("demo-owner"));
    assert_eq!(
        mine.prs[0].facts(),
        "✓2 ✗2 ◷0 · req ✗ · changes · behind · unres 3 · e2e-web, lint"
    );
    assert_eq!(
        mine.prs[2].facts(),
        "✓88+? ✗0+? ◷12+? · req ? · approved · unstable · unres 0 · ?"
    );
    assert_eq!(
        mine.prs[5].facts(),
        "checks ? head moved · req ? · none · draft · unres 0"
    );
    assert!(valid_login("a-b1") && !valid_login("a--") && !valid_login(&"a".repeat(40)));
}

#[test]
fn detail_dates_the_update_a_long_row_may_crowd() {
    let now = 1_792_000_000;
    let mine = demo(now);
    assert_eq!(mine.prs[0].number, 42);
    let detail = mine.prs[0].detail(now).join("\n");
    let at = crate::github::utc_stamp(now - 720);
    assert!(
        detail.contains(&format!("updated 12m ago · {at}")),
        "{detail}"
    );
    // A stamp after the read is not an age.
    let early = mine.prs[0].detail(now - 3600).join("\n");
    assert!(early.contains(&format!("updated ? · {at}")), "{early}");
}
