use super::*;
#[test]
fn host_observation_is_scoped_and_notifications_are_blocking_only() {
    let mut db = db::Connection::open_in_memory().unwrap();
    db.execute_batch(taskr_core::schema::SCHEMA).unwrap();
    let at = store::now();
    for (id, parent, name, role, machine, workspace, pane) in [
        (1, None, "local", "orchestrator", None, "wLocal", "w:p0"),
        (
            2,
            None,
            "remote",
            "orchestrator",
            Some("client"),
            "wClient",
            "w:p0",
        ),
        (
            3,
            Some(1),
            "local-worker",
            "implementer",
            None,
            "wLocalLane",
            "w:p1",
        ),
        (
            4,
            Some(2),
            "remote-worker",
            "implementer",
            Some("client"),
            "wLane",
            "w:p1",
        ),
    ] {
        db.execute("insert into tasks(id,parent_id,name,role,status,machine,workspace_id,pane_id,created_at,updated_at) values(?,?,?,?,'open',?,?,?,?,?)",params![id,parent,name,role,machine,workspace,pane,at,at]).unwrap();
        if let Some(_parent) = parent {
            db.execute("insert into launches(id,task_id,provider,model,effort,machine,workspace_id,pane_id,recorded_at) values(?,?,'fixture','fixture','medium',?,?,?,?)",params![id,id,machine,workspace,pane,at]).unwrap();
            db.execute(
                "update tasks set current_launch_id=? where id=?",
                params![id, id],
            )
            .unwrap();
        }
    }
    for blocking in [true, false] {
        db.execute("insert into events(task_id,recipient_task_id,launch_id,kind,summary,data,created_at) values(4,2,4,'ask','decision',?,?)",params![format!("{{\"owner\":true,\"blocking\":{blocking}}}"),at]).unwrap();
    }
    let agents = json!([{"name":"worker","pane_id":"w:p1","agent_status":"blocked","state_change_seq":1},{"name":"lead","pane_id":"w:p0","agent_status":"idle","state_change_seq":2}]);
    let result = observe_host(&mut db, "client", &agents).unwrap();
    assert_eq!(result["observed"], 1);
    assert_eq!(result["watch"], json!(["w:p1"]));
    assert_eq!(result["owner_ask_tokens"], json!({"w:p1":2}));
    assert_eq!(result["owner_asks"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["workspace_tokens"],
        json!({"wLane":{"campaign":"remote","parent":"wClient"}})
    );
    assert!(meta(&db, "daemon_heartbeat:client").unwrap().is_some());
    assert!(meta(&db, "daemon_heartbeat").unwrap().is_none());
    let local:(Option<String>,Option<String>)=db.query_row("select(select observed_status from launches where id=3),(select lead_status from tasks where id=1)",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(local, (None, None));
    let event: (i64, i64) = db
        .query_row(
            "select task_id,recipient_task_id from events where kind='herdr'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(event, (4, 2));
    let again = observe_host(&mut db, "client", &agents).unwrap();
    assert_eq!(again["observed"], 0);
    assert!(again["owner_asks"].is_null());
    assert!(observe_host(&mut db, "", &agents).is_err());
    assert!(observe_host(&mut db, "client", &Value::Null).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_kernel_identity_matches_self() {
    let pid = std::process::id() as i32;
    let proc = identity::proc_identity(pid).unwrap();
    assert_eq!(
        fs::canonicalize(proc.exe).unwrap(),
        fs::canonicalize(std::env::current_exe().unwrap()).unwrap()
    );
    assert_eq!(proc.argv, std::env::args().collect::<Vec<_>>());
    assert_eq!(proc.uid, rustix::process::geteuid().as_raw());
    let (sec, usec) = proc.start.split_once('.').unwrap();
    assert!(sec.parse::<i64>().unwrap() > 0);
    assert_eq!(usec.len(), 6);
    assert!(usec.parse::<u32>().unwrap() < 1_000_000);
    assert!(identity::proc_identity(i32::MAX).is_err());
}

#[test]
fn slow_tmp_sweep_does_not_block_or_overlap_the_daemon() {
    let (release, blocked) = std::sync::mpsc::channel();
    let job = std::thread::spawn(move || {
        let _ = blocked.recv_timeout(Duration::from_secs(2));
    });
    let worker = job.thread().id();
    let path = std::env::temp_dir().join(format!("taskr-tmp-sweep-log-{}", std::process::id()));
    let mut state = State {
        checkin: false,
        tokens: tokens::Tokens::default(),
        watch: Vec::new(),
        raw: None,
        sock: String::new(),
        dir: PathBuf::new(),
        log: Arc::new(Log::open(&path)),
        missing: false,
        uplink: uplink::Uplink::default(),
        tmp_sweep: None,
        tmp_worker: Some(job),
        tmp_once: false,
    };
    let start = Instant::now();
    state.sweep_tmp(None);
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(state.tmp_worker.as_ref().unwrap().thread().id(), worker);
    assert!(state.tmp_sweep.is_none());
    release.send(()).unwrap();
    state.tmp_worker.take().unwrap().join().unwrap();
    drop(state);
    fs::remove_file(path).unwrap();
}

#[test]
fn tmp_lookup_errors_are_limited_but_removal_errors_remain_visible() {
    let path = std::env::temp_dir().join(format!("taskr-tmp-errors-{}", std::process::id()));
    let log = Log::open(&path);
    for id in 0..300 {
        log.tmp_error(&format!("tmp sweep {id}: hub unreachable"), true);
    }
    log.tmp_error("tmp sweep 1/2: unlink failed", false);
    log.tmp_error("tmp sweep 2/3: device refused", false);
    let lines = fs::read_to_string(&path).unwrap();
    assert_eq!(lines.lines().count(), 3);
    assert!(lines.contains("hub unreachable"));
    assert!(lines.contains("unlink failed") && lines.contains("device refused"));
    log.limited
        .lock()
        .unwrap()
        .get_mut("tmp-sweep-lookup")
        .unwrap()
        .0 = Instant::now() - Duration::from_secs(181);
    log.tmp_error("tmp sweep 300: unknown task", true);
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("299 similar suppressed")
    );
    drop(log);
    fs::remove_file(path).unwrap();
}

struct SafetyDir(PathBuf);
impl SafetyDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("taskr-{name}-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for SafetyDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct IdentityChild(std::process::Child);
impl Drop for IdentityChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn identity_fixture(dir: &Path) -> (IdentityChild, identity::Record) {
    use std::{
        io::Read,
        process::{Command, Stdio},
    };
    // A blocked shell builtin supplies exact argv[1] without starting a daemon.
    fs::write(dir.join("daemon"), "printf r; read ignored\n").unwrap();
    let mut child = Command::new("/bin/sh")
        .arg("daemon")
        .current_dir(dir)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = [0];
    child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
    assert_eq!(ready, [b'r']);
    let child = IdentityChild(child);
    let pid = child.0.id() as i32;
    let proc = identity::proc_identity(pid).unwrap();
    let record = identity::Record {
        pid,
        executable: fs::canonicalize(proc.exe).unwrap().to_str().unwrap().into(),
        argv: proc.argv,
        start_time: proc.start,
        uid: Some(proc.uid),
        version: "fixture".into(),
        ..Default::default()
    };
    (child, record)
}
fn identity_db(record: &identity::Record) -> db::Connection {
    let db = db::Connection::open_in_memory().unwrap();
    db.execute_batch(taskr_core::schema::SCHEMA).unwrap();
    for (key, value) in [
        ("daemon_pid", record.pid.to_string()),
        ("daemon_exe", record.executable.clone()),
        ("daemon_proc_start", record.start_time.clone()),
    ] {
        set_meta(&db, key, &value).unwrap();
    }
    db
}
fn identity_refused(db: Option<&db::Connection>, dir: &Path, pid: i32, message: &str) {
    let error = identity::verify(db, dir, pid).unwrap_err();
    assert_eq!(error.code, ExitCode::Rejected);
    assert!(error.message.contains(message), "{}", error.message);
    assert!(error.message.contains("not signalled") || error.message.contains("nothing signalled"));
}

#[test]
fn daemon_identity_refuses_non_taskr_missing_self_and_invalid_pids() {
    let dir = SafetyDir::new("identity-missing");
    let db = db::Connection::open_in_memory().unwrap();
    db.execute_batch(taskr_core::schema::SCHEMA).unwrap();
    for pid in [0, -1] {
        identity_refused(Some(&db), &dir.0, pid, "records no pid");
    }
    identity_refused(Some(&db), &dir.0, std::process::id() as i32, "this process");
    let (_child, record) = identity_fixture(&dir.0);
    identity_refused(Some(&db), &dir.0, record.pid, "records daemon pid 0");
    set_meta(&db, "daemon_pid", &record.pid.to_string()).unwrap();
    identity_refused(Some(&db), &dir.0, record.pid, "no executable or start time");
}

#[test]
fn daemon_identity_rejects_lookalike_executable_argv_and_reused_pid() {
    let dir = SafetyDir::new("identity-lookalike");
    let (mut child, record) = identity_fixture(&dir.0);
    let db = identity_db(&record);
    assert_eq!(
        identity::verify(Some(&db), &dir.0, record.pid).unwrap(),
        record.pid
    );
    set_meta(
        &db,
        "daemon_exe",
        std::env::current_exe().unwrap().to_str().unwrap(),
    )
    .unwrap();
    identity_refused(Some(&db), &dir.0, record.pid, "recorded daemon executable");
    set_meta(&db, "daemon_exe", &record.executable).unwrap();
    set_meta(&db, "daemon_proc_start", "impossible-start").unwrap();
    identity_refused(Some(&db), &dir.0, record.pid, "reused pid");
    set_meta(&db, "daemon_proc_start", &record.start_time).unwrap();
    let other = IdentityChild(
        std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap(),
    );
    let proc = identity::proc_identity(other.0.id() as i32).unwrap();
    let other_record = identity::Record {
        pid: other.0.id() as i32,
        executable: fs::canonicalize(proc.exe).unwrap().to_str().unwrap().into(),
        start_time: proc.start,
        ..Default::default()
    };
    identity_refused(
        Some(&identity_db(&other_record)),
        &dir.0,
        other_record.pid,
        "not running `taskr daemon`",
    );
    assert!(child.0.try_wait().unwrap().is_none());
    drop(other);
}

#[test]
fn client_identity_requires_a_complete_matching_record() {
    let dir = SafetyDir::new("identity-client");
    let (mut child, record) = identity_fixture(&dir.0);
    let path = dir.0.join("client-daemon.json");
    identity_refused(None, &dir.0, record.pid, "cannot read");
    record.write(&path).unwrap();
    assert_eq!(
        identity::verify(None, &dir.0, record.pid).unwrap(),
        record.pid
    );
    for field in [
        "pid",
        "executable",
        "start_time",
        "argv",
        "uid",
        "changed argv",
        "changed start",
        "changed uid",
    ] {
        let mut bad = record.clone();
        match field {
            "pid" => bad.pid += 1,
            "executable" => bad.executable.clear(),
            "start_time" => bad.start_time.clear(),
            "argv" => bad.argv.clear(),
            "uid" => bad.uid = None,
            "changed argv" => bad.argv[0].push_str(".other"),
            "changed start" => bad.start_time.push_str(".other"),
            "changed uid" => bad.uid = Some(record.uid.unwrap().wrapping_add(1)),
            _ => unreachable!(),
        }
        bad.write(&path).unwrap();
        identity_refused(None, &dir.0, record.pid, "");
        assert!(child.0.try_wait().unwrap().is_none(), "{field}");
    }
    fs::write(&path, b"{truncated").unwrap();
    identity_refused(None, &dir.0, record.pid, "cannot read");
}

#[test]
fn local_identity_rejects_a_stale_or_corrupt_sidecar() {
    let dir = SafetyDir::new("identity-sidecar");
    let (mut child, record) = identity_fixture(&dir.0);
    let db = identity_db(&record);
    let path = dir.0.join("daemon.json");
    record.write(&path).unwrap();
    assert_eq!(
        identity::verify(Some(&db), &dir.0, record.pid).unwrap(),
        record.pid
    );
    let mut stale = record.clone();
    stale.start_time.push_str(".stale");
    stale.write(&path).unwrap();
    identity_refused(
        Some(&db),
        &dir.0,
        record.pid,
        "local daemon identity record",
    );
    fs::write(&path, b"{corrupt").unwrap();
    identity_refused(
        Some(&db),
        &dir.0,
        record.pid,
        "cannot read the local daemon identity record",
    );
    assert!(
        identity::verify(Some(&db), &dir.0, record.pid)
            .unwrap_err()
            .message
            .contains(path.to_str().unwrap())
    );
    assert!(child.0.try_wait().unwrap().is_none());
}
