//! `taskr tmp ID [--mkdir]`: a lane's own tmp dir, `<base>/<root-id>/<task-id>`. The ledger
//! names the root (a read a client forwards to the hub); the directory work is always on the
//! caller's host. Paths are built from integer ids only.
pub mod cleanup;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};
use taskr_core::{ExitCode, compact_json, goflag::FlagSet};

pub struct Args {
    pub task: i64,
    pub mkdir: bool,
    pub json: bool,
}

/// `$TASKR_TMP_BASE`, else `/tmp/taskr-<uid>`. The override is rebuilt from its components,
/// which drops trailing separators and non-leading `.` components so `link/` and `link/.`
/// cannot carry a symlink past the lstat check below. Symlinks are not resolved.
pub fn base() -> PathBuf {
    std::env::var_os("TASKR_TMP_BASE")
        .filter(|v| !v.is_empty())
        .map(|v| PathBuf::from(v).components().collect::<PathBuf>())
        .unwrap_or_else(|| {
            PathBuf::from(format!("/tmp/taskr-{}", rustix::process::getuid().as_raw()))
        })
}

/// The lane dir of `task` in campaign `root` (a root's own dir is `<root>/<root>`).
pub fn lane(root: i64, task: i64) -> PathBuf {
    base().join(root.to_string()).join(task.to_string())
}

/// The parsed arguments, or the exit code once usage, help or an error is printed.
pub fn parse(json: bool, args: &[String]) -> Result<Args, ExitCode> {
    let mut f = FlagSet::new("tmp", json);
    f.bool(
        "mkdir",
        false,
        "create the base, campaign and lane dirs (0700) on this host",
    );
    if let Err(e) = f.parse(args, 1, 1) {
        return Err(crate::cli::error(f.json(), "tmp", &e, ExitCode::Usage));
    }
    if f.help() {
        print!("{}", f.usage(&crate::cli::usage_line("tmp")));
        return Err(ExitCode::Ok);
    }
    let Some(task) = f.positional[0].parse::<i64>().ok().filter(|n| *n > 0) else {
        let e = format!(
            "task id must be a positive integer, got {}",
            taskr_core::goflag::quote(&f.positional[0])
        );
        return Err(crate::cli::error(f.json(), "tmp", &e, ExitCode::Usage));
    };
    Ok(Args {
        task,
        mkdir: f.get_bool("mkdir"),
        json: f.json(),
    })
}

/// Local mode: the root from this host's ledger.
pub fn dispatch(json: bool, args: &[String]) -> Option<ExitCode> {
    if args.first()? != "tmp" {
        return None;
    }
    let a = match parse(json, &args[1..]) {
        Ok(a) => a,
        Err(code) => return Some(code),
    };
    let root = taskr_core::db::path()
        .and_then(|p| taskr_core::db::open(&p))
        .map_err(|e| (ExitCode::Database, e))
        .and_then(|db| cleanup::State::read(&db, a.task).map_err(|e| (e.code, e.message)));
    Some(match root {
        Ok(state) => finish(&a, state.root_id, Some(json!(state))),
        Err((code, e)) => crate::cli::error(a.json, "tmp", &e, code),
    })
}

/// Prints the lane dir, creating it first with `--mkdir`.
pub fn finish(a: &Args, root: i64, cleanup: Option<Value>) -> ExitCode {
    let base = base();
    let dir = lane(root, a.task);
    let made = if !base.is_absolute() {
        Err(format!(
            "TASKR_TMP_BASE must be an absolute path, got {}",
            base.display()
        ))
    } else if base.components().any(|c| c == Component::ParentDir) {
        Err(format!(
            "TASKR_TMP_BASE must not contain '..', got {}",
            base.display()
        ))
    } else if a.mkdir {
        dir_ok(&base)
            .and_then(|()| {
                cleanup::init_base(&base)
                    .map_err(|e| format!("refusing tmp base {}: {e:#}", base.display()))
            })
            .and_then(|()| {
                [root, a.task]
                    .iter()
                    .try_fold(base, |dir, id| {
                        dir_ok(&dir).map(|()| dir.join(id.to_string()))
                    })
                    .and_then(|dir| dir_ok(&dir))
            })
    } else {
        Ok(())
    };
    if let Err(e) = made {
        let v = if a.json {
            json!({"error":e,"kind":"tmp"})
        } else {
            json!({"err":e,"k":"tmp"})
        };
        if a.json {
            eprintln!("taskr tmp: {e}");
        }
        println!(
            "{}{}",
            if a.json { "" } else { "x1 1 " },
            compact_json(&v).unwrap()
        );
        return ExitCode::Watch;
    }
    if a.json {
        let v = json!({"task_id":a.task,"root_id":root,"tmpdir":dir.to_string_lossy(),"cleanup":cleanup});
        println!("{}", compact_json(&v).unwrap());
    } else {
        println!("{}", dir.display());
    }
    ExitCode::Ok
}

/// Creates `p` (0700) when missing, then refuses it unless lstat shows a real directory the
/// caller owns that neither group nor others can write.
fn dir_ok(p: &Path) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o700).create(p) {
        // The umask can only clear bits; this makes it exactly 0700.
        Ok(()) => fs::set_permissions(p, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("chmod {}: {e}", p.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("create {}: {e}", p.display())),
    }
    let m = fs::symlink_metadata(p).map_err(|e| format!("stat {}: {e}", p.display()))?;
    let uid = rustix::process::getuid().as_raw();
    let why = if m.file_type().is_symlink() {
        "is a symlink".to_string()
    } else if !m.is_dir() {
        "is not a directory".into()
    } else if m.uid() != uid {
        format!("is owned by uid {}, not {uid}", m.uid())
    } else if m.mode() & 0o022 != 0 {
        format!("is group- or world-writable ({:o})", m.mode() & 0o777)
    } else {
        return Ok(());
    };
    Err(format!("refusing tmp dir {}: it {why}", p.display()))
}
