//! `taskr slotr`: slotr's lock and queue state as the hub sees it, for taskr-tui. The argv
//! to slotr is fixed and the command takes no arguments, so a client can run nothing else.
use super::*;
use std::{
    io::Read,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(2);

/// `slotr status --json`, or why it could not be read.
fn status() -> std::result::Result<Value, String> {
    // The hub child's environment is cleared, and slotr needs the user bus to check its
    // holders: give it the runtime dir systemd would.
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("/run/user/{}", rustix::process::getuid().as_raw()));
    let child = Command::new("slotr")
        .args(["status", "--json"])
        .env("XDG_RUNTIME_DIR", runtime)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run slotr: {e}"))?;
    let pid = child.id();
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(child.wait_with_output());
    });
    let Ok(out) = recv.recv_timeout(TIMEOUT) else {
        taskr_core::store::terminate_group(pid);
        return Err(format!("slotr status took over {}s", TIMEOUT.as_secs()));
    };
    let out = out.map_err(|e| format!("slotr: {e}"))?;
    if !out.status.success() {
        let mut why = String::new();
        let _ = out.stderr.as_slice().take(4096).read_to_string(&mut why);
        let why = glance::line(&why);
        return Err(if why.is_empty() {
            format!("slotr status failed: {}", out.status)
        } else {
            why
        });
    }
    match serde_json::from_slice::<Value>(&out.stdout) {
        Ok(v) if v.is_object() => Ok(v),
        _ => Err("slotr status printed no JSON object".into()),
    }
}

/// `root_id` and `root_name` on every holder and queue row whose task is a ledger id.
fn roots(v: &mut Value) {
    let mut db = None;
    let rows = v["pools"]
        .as_object_mut()
        .into_iter()
        .flat_map(|pools| pools.values_mut())
        .flat_map(|pool| {
            let pool = pool.as_object_mut();
            pool.into_iter()
                .flat_map(|p| p.iter_mut())
                .filter(|(k, _)| matches!(k.as_str(), "holders" | "queue"))
                .filter_map(|(_, rows)| rows.as_array_mut())
                .flatten()
        });
    for row in rows {
        let Some(task) = row["task"].as_str().and_then(|t| t.parse::<i64>().ok()) else {
            continue;
        };
        // ponytail: one walk per row; the pools hold a handful of rows.
        let db = db.get_or_insert_with(|| open().ok());
        let Some(db) = db.as_ref() else { return };
        let root: Option<(i64, String)> = db
            .query_row(
                "with recursive up(id,parent_id,name) as (select id,parent_id,name from tasks where id=? \
                 union all select t.id,t.parent_id,t.name from tasks t join up on t.id=up.parent_id) \
                 select id,name from up where parent_id is null",
                [task],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ok()
            .flatten();
        if let (Some((id, name)), Some(row)) = (root, row.as_object_mut()) {
            row.insert("root_id".into(), json!(id));
            row.insert("root_name".into(), json!(name));
        }
    }
}

pub(super) fn run(f: &FlagSet) -> Result<()> {
    let mut v = match status() {
        Ok(mut v) => {
            roots(&mut v);
            v["available"] = json!(true);
            v
        }
        Err(error) => json!({"available": false, "error": error}),
    };
    // Rows are on the hub: an empty host, as the glance's hub rows.
    v["host"] = json!("");
    v["now"] = json!(queries::now());
    cli::emit(f.json(), &v, false);
    Ok(())
}
