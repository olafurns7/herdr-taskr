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
