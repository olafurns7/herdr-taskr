//! The PR poller against a fake `gh` on PATH that replays canned GraphQL JSON.
use super::*;
use std::os::unix::fs::PermissionsExt;

const FAKE_GH: &str = r#"#!/bin/sh
d=$(dirname "$0")/..
printf '%s\n' "$@" > "$d/args"
echo x >> "$d/calls"
[ -f "$d/sleep" ] && sleep "$(cat "$d/sleep")"
cat "$d/reply" 2>/dev/null
exit "$(cat "$d/code" 2>/dev/null || echo 0)"
"#;

struct Fixture {
    dir: PathBuf,
    db: db::Connection,
    log: Log,
    poller: Poller,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
/// Root 1 with lane 2, root 3, and a closed root 4 with open lane 5.
fn fixture(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("taskr-gh-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("bin")).unwrap();
    let gh = dir.join("bin/gh");
    fs::write(&gh, FAKE_GH).unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        dir.join("watch.json"),
        r#"{"github":{"enabled":true,"default_repo":"demo-org/demo"}}"#,
    )
    .unwrap();
    let db = db::open(&dir.join("taskr.db")).unwrap();
    let at = store::now();
    for (id, parent, status) in [
        (1, None, "open"),
        (2, Some(1), "open"),
        (3, None, "open"),
        (4, None, "closed"),
        (5, Some(4), "open"),
    ] {
        db.execute("insert into tasks(id,parent_id,name,role,status,created_at,updated_at) values(?,?,?,'implementer',?,?,?)",params![id,parent,format!("t{id}"),status,at,at]).unwrap();
    }
    let mut poller = Poller::new(&dir, true).unwrap();
    poller.path = Some(format!("{}:/usr/bin:/bin", dir.join("bin").display()));
    Fixture {
        log: Log::open(&dir.join("daemon.log")),
        dir,
        db,
        poller,
    }
}
impl Fixture {
    fn link(&self, task: i64, value: &str) {
        self.db.execute("insert into events(task_id,kind,summary,data,created_at) values(?,'ref',?,json_object('key','pr','value',?),?)",params![task,format!("pr={value}"),value,store::now()]).unwrap();
    }
    fn reply(&self, body: &str, code: i32) {
        fs::write(self.dir.join("reply"), body).unwrap();
        fs::write(self.dir.join("code"), code.to_string()).unwrap();
    }
    fn calls(&self) -> usize {
        fs::read_to_string(self.dir.join("calls")).map_or(0, |s| s.lines().count())
    }
    /// Make the poll due, start it, and tick until it has been applied.
    fn poll(&mut self) {
        self.poller.due = Instant::now();
        self.poller.tick(&mut self.db, &self.log);
        let start = Instant::now();
        while self.poller.inflight.is_some() {
            assert!(start.elapsed() < Duration::from_secs(10), "poll stuck");
            std::thread::sleep(Duration::from_millis(10));
            self.poller.tick(&mut self.db, &self.log);
        }
    }
    /// `pr` events as (task, recipient, sub), oldest first.
    fn events(&self) -> Vec<(i64, Option<i64>, String)> {
        let mut stmt = self.db.prepare("select task_id,recipient_task_id,json_extract(data,'$.sub') from events where kind='pr' order by id").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }
    fn subs(&self) -> Vec<String> {
        self.events().into_iter().map(|e| e.2).collect()
    }
    fn latest_ref(&self, task: i64, key: &str) -> Option<String> {
        self.db.query_row("select json_extract(data,'$.value') from events where task_id=? and kind='ref' and json_extract(data,'$.key')=? order by id desc limit 1",params![task,key],|r|r.get(0)).optional().unwrap()
    }
    fn log_lines(&self, needle: &str) -> usize {
        fs::read_to_string(self.dir.join("daemon.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(needle))
            .count()
    }
}
/// One PR node: head, mergeStateStatus, check-run conclusions, unresolved threads.
fn pr(head: &str, merge: &str, checks: &[&str], open_threads: usize) -> Value {
    let contexts: Vec<Value> = checks
        .iter()
        .map(|c| match *c {
            "pending" => json!({"__typename":"CheckRun","name":"ci","status":"IN_PROGRESS","conclusion":null,"isRequired":true}),
            "status-ok" => json!({"__typename":"StatusContext","context":"legacy","state":"SUCCESS","isRequired":true}),
            c => json!({"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":c,"isRequired":true}),
        })
        .collect();
    let threads: Vec<Value> = (0..open_threads)
        .map(|_| json!({"isResolved":false}))
        .chain([json!({"isResolved":true})])
        .collect();
    json!({"state":"OPEN","merged":false,"headRefOid":head,"mergeStateStatus":merge,"mergeCommit":null,
        "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":contexts}}}}]},
        "reviewThreads":{"nodes":threads}})
}
fn data(nodes: &[Value]) -> String {
    let mut d = json!({});
    for (i, n) in nodes.iter().enumerate() {
        d[format!("p{i}")] = json!({"pullRequest":n});
    }
    json!({"data":d}).to_string()
}
const HEAD: &str = "5635105aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn parses_links() {
    let d = "demo-org/demo";
    let key = |v: &str| parse(v, d).map(|p| p.key());
    assert_eq!(key("31").as_deref(), Some("demo-org/demo#31"));
    assert_eq!(key("#31").as_deref(), Some("demo-org/demo#31"));
    assert_eq!(key(" other/x.y#7 ").as_deref(), Some("other/x.y#7"));
    for bad in [
        "", "0", "abc", "#", "a#1", "o/r#x", "o/r#-1", "o\"/r#1", "o/r/s#1", "+5",
    ] {
        assert_eq!(key(bad), None, "{bad}");
    }
    assert_eq!(parse("31", ""), None, "a bare number needs a default repo");
}

#[test]
fn off_without_config_and_on_client_daemons() {
    assert!(Poller::new(Path::new("/nonexistent"), false).is_none());
    let mut f = fixture("off");
    fs::remove_file(f.dir.join("watch.json")).unwrap();
    f.link(2, "31");
    f.reply(&data(&[pr(HEAD, "CLEAN", &["SUCCESS"], 0)]), 0);
    f.poll();
    fs::write(f.dir.join("watch.json"), r#"{"github":{"enabled":false}}"#).unwrap();
    f.poll();
    assert_eq!((f.calls(), f.subs().len()), (0, 0));
}

#[test]
fn each_sub_fires_once_and_checks_wait_for_two_stable_polls() {
    let mut f = fixture("subs");
    f.link(2, "31");
    let poll = |f: &mut Fixture, node: Value| {
        f.reply(&data(&[node]), 0);
        f.poll();
        f.subs()
    };
    // First sight: blocked and one open thread; checks green once is not enough.
    assert_eq!(
        poll(&mut f, pr(HEAD, "BLOCKED", &["SUCCESS"], 1)),
        ["blocked", "thread_opened"]
    );
    assert_eq!(f.latest_ref(2, "pr.ci").as_deref(), Some("pass"));
    assert_eq!(f.latest_ref(2, "pr.state").as_deref(), Some("open"));
    assert_eq!(poll(&mut f, pr(HEAD, "BLOCKED", &["SUCCESS"], 1)).len(), 3);
    assert_eq!(f.subs()[2], "checks_green");
    // A repeat fires nothing; nor do refs repeat.
    let refs: i64 =
        f.db.query_row("select count(*) from events where kind='ref'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(poll(&mut f, pr(HEAD, "BLOCKED", &["SUCCESS"], 1)).len(), 3);
    assert_eq!(
        f.db.query_row::<i64, _, _>("select count(*) from events where kind='ref'", [], |r| r
            .get(0))
            .unwrap(),
        refs
    );
    // A new head re-arms checks; a flap (fail, pass, fail) needs two stable polls.
    let head2 = "bbbbbbbcccccccccccccccccccccccccccccccc";
    for c in ["FAILURE", "SUCCESS", "FAILURE"] {
        assert_eq!(
            poll(&mut f, pr(head2, "BLOCKED", &[c, "status-ok"], 1)).len(),
            3,
            "{c}"
        );
    }
    assert_eq!(
        poll(&mut f, pr(head2, "BLOCKED", &["FAILURE", "status-ok"], 1))
            .last()
            .unwrap(),
        "checks_failed"
    );
    assert_eq!(f.latest_ref(2, "pr.ci").as_deref(), Some("fail"));
    // A settled pending re-arms the same verdict; UNKNOWN merge state is no change.
    poll(&mut f, pr(head2, "UNKNOWN", &["pending"], 1));
    poll(&mut f, pr(head2, "UNKNOWN", &["pending"], 1));
    assert_eq!(f.latest_ref(2, "pr.ci").as_deref(), Some("running"));
    poll(&mut f, pr(head2, "DIRTY", &["FAILURE"], 2));
    poll(&mut f, pr(head2, "BEHIND", &["FAILURE"], 0));
    assert_eq!(
        &f.subs()[4..],
        [
            "dirty",
            "thread_opened",
            "checks_failed",
            "behind",
            "threads_clear"
        ]
    );
    // The lead (root 1) gets the lane's events, with no launch.
    let (to, launch, summary): (i64, Option<i64>, String) = f.db.query_row("select recipient_task_id,launch_id,summary from events where kind='pr' and json_extract(data,'$.sub')='checks_green'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(
        (to, launch, summary.as_str()),
        (1, None, "PR demo#31 checks green at 5635105")
    );
    // pu stays out of search.
    let indexed: i64 =
        f.db.query_row("select count(*) from search_fts where kind='pr'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(indexed, 0);
    // Merged: one sub, then the PR is no longer polled.
    let mut merged = pr(head2, "CLEAN", &["SUCCESS"], 0);
    merged["state"] = json!("MERGED");
    merged["merged"] = json!(true);
    merged["mergeCommit"] = json!({"oid":"9999999deadbeef"});
    assert_eq!(poll(&mut f, merged).last().unwrap(), "merged");
    assert_eq!(f.latest_ref(2, "pr.state").as_deref(), Some("merged"));
    let calls = f.calls();
    f.poll();
    assert_eq!(f.calls(), calls, "a merged PR is not polled");
}

#[test]
fn recipients_lead_root_and_closed() {
    let mut f = fixture("recipients");
    f.link(2, "31");
    f.link(3, "demo-org/other#5");
    f.link(5, "#6");
    let node = pr(HEAD, "DIRTY", &[], 0);
    f.reply(&data(&[node.clone(), node.clone(), node]), 0);
    f.poll();
    let args = fs::read_to_string(f.dir.join("args")).unwrap();
    assert!(args.contains("repository(owner:\"demo-org\",name:\"other\"){pullRequest(number:5)"));
    let mut events = f.events();
    events.sort();
    assert_eq!(
        events,
        [
            (2, Some(1), "dirty".into()),
            (3, Some(3), "dirty".into()),
            (5, None, "dirty".into())
        ]
    );
}

#[test]
fn a_null_alias_with_not_found_closes_and_unlinks() {
    let mut f = fixture("gone");
    f.link(2, "31");
    f.link(3, "32");
    // A null without a matching error is no change.
    f.reply(r#"{"data":{"p0":{"pullRequest":null},"p1":null},"errors":[{"type":"NOT_FOUND","path":["p1"],"message":"Could not resolve to a Repository"}]}"#, 1);
    f.poll();
    assert_eq!(f.events(), [(3, Some(3), "closed".into())]);
    assert_eq!(f.latest_ref(3, "pr").as_deref(), Some(""));
    assert_eq!(f.latest_ref(2, "pr").as_deref(), Some("31"));
    let (summary, reason): (String, String) =
        f.db.query_row(
            "select summary,json_extract(data,'$.reason') from events where kind='pr'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        summary,
        "PR demo#32 is gone: Could not resolve to a Repository"
    );
    assert_eq!(reason, "Could not resolve to a Repository");
    assert_eq!(f.poller.failures, 0, "JSON with data is not a failure");
}

#[test]
fn backs_off_without_data_and_caps_on_auth() {
    let mut f = fixture("backoff");
    f.link(2, "31");
    let due = |f: &Fixture| {
        f.poller
            .due
            .saturating_duration_since(Instant::now())
            .as_secs()
    };
    for want in [60, 120, 240, 480, 900, 900] {
        f.reply("not json", 1);
        f.poll();
        assert!(
            (want - 2..=want).contains(&due(&f)),
            "want {want}, got {}",
            due(&f)
        );
    }
    f.reply(r#"{"data":null,"errors":[{"message":"rate limited"}]}"#, 1);
    f.poll();
    assert!(due(&f) >= 898);
    assert_eq!(
        f.log_lines("github poll failed"),
        1,
        "one line per state change"
    );
    f.reply("", 4);
    f.poll();
    assert!(due(&f) >= 898);
    f.poll();
    assert_eq!(f.log_lines("not authenticated"), 1);
    f.reply(&data(&[pr(HEAD, "CLEAN", &[], 0)]), 0);
    f.poll();
    assert_eq!((f.poller.failures, f.log_lines("github poll ok")), (0, 1));
    assert!((58..=60).contains(&due(&f)));
}

#[test]
fn polls_at_most_30_and_stretches_the_interval() {
    let mut f = fixture("cap");
    let at = store::now();
    for n in 1..=45 {
        f.db.execute("insert into tasks(id,parent_id,name,role,status,created_at,updated_at) values(?,1,?,'implementer','open',?,?)",params![100+n,format!("l{n}"),at,at]).unwrap();
        f.link(100 + n, &n.to_string());
    }
    f.reply(r#"{"data":{}}"#, 0);
    f.poll();
    let args = fs::read_to_string(f.dir.join("args")).unwrap();
    assert_eq!(args.matches("pullRequest(number:").count(), 30);
    let secs = f
        .poller
        .due
        .saturating_duration_since(Instant::now())
        .as_secs();
    assert!((118..=120).contains(&secs), "{secs}");
    // The next poll takes the other 15 and wraps.
    f.poll();
    let args = fs::read_to_string(f.dir.join("args")).unwrap();
    assert!(
        args.contains("p0:repository(owner:\"demo-org\",name:\"demo\"){pullRequest(number:31)")
    );
    assert_eq!(args.matches("pullRequest(number:").count(), 30);
}

#[test]
fn no_transaction_is_held_while_gh_runs() {
    let mut f = fixture("nolock");
    f.link(2, "31");
    f.reply(&data(&[pr(HEAD, "DIRTY", &[], 0)]), 0);
    fs::write(f.dir.join("sleep"), "2").unwrap();
    f.poller.due = Instant::now();
    f.poller.tick(&mut f.db, &f.log);
    assert!(f.poller.inflight.is_some());
    std::thread::sleep(Duration::from_millis(300));
    // Another connection takes the write lock at once while gh sleeps.
    let mut other = db::open(&f.dir.join("taskr.db")).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    let start = Instant::now();
    store::transaction(&mut other, |tx| {
        tx.execute("insert into meta(key,value) values('other','1')", [])?;
        Ok(())
    })
    .unwrap();
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(f.poller.inflight.is_some(), "gh is still running");
    while f.poller.inflight.is_some() {
        std::thread::sleep(Duration::from_millis(20));
        f.poller.tick(&mut f.db, &f.log);
    }
    assert_eq!(f.subs(), ["dirty"]);
}
