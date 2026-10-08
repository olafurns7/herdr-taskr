use super::*;
use std::{io::Read, time::Duration};
pub fn run(args: &[String]) -> ExitCode {
    if args.len() == 1 && ["--help", "-h"].contains(&args[0].as_str()) {
        println!("hooks:        hook <harness> <event> (JSON on stdin)");
        return ExitCode::Ok;
    }
    if store::env("TASKR_LAUNCH").is_empty()
        || store::env("HERDR_ENV") != "1"
        || args.len() != 2
        || !store::hook::supported(&args[0], &args[1])
    {
        return ExitCode::Ok;
    }
    let args = args.to_vec();
    let (send, recv) = std::sync::mpsc::channel();
    // Returning from the CLI at the deadline also ends held stdin/SQLite work,
    // matching Go's fire-and-forget hook timer without starting a service.
    std::thread::spawn(move || {
        apply(&args);
        let _ = send.send(());
    });
    let _ = recv.recv_timeout(Duration::from_millis(500));
    ExitCode::Ok
}
fn apply(args: &[String]) {
    let mut bytes = Vec::new();
    if std::io::stdin()
        .take((8 << 20) + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return;
    }
    let Some(record) = store::hook::parse(&args[0], &args[1], &bytes) else {
        return;
    };
    let Ok(path) = db::path() else {
        return;
    };
    if !path.exists() {
        return;
    }
    let Ok(mut db) =
        db::Connection::open_with_flags(path, db::rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
    else {
        return;
    };
    let _ = db.busy_timeout(Duration::from_millis(150));
    let _ = db.pragma_update(None, "foreign_keys", true);
    let _ = store::hook::apply(&mut db, &record);
}
