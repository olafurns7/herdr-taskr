//! Host-local cleanup; ledger authority never supplies filesystem paths.
use anyhow::{Context, Result, bail};
use rustix::fs::{self as fd, AtFlags, FileType, Mode, OFlags, Stat};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::OwnedFd,
    path::Path,
};
use taskr_core::{db::Connection, store};

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const MARKER: &str = ".taskr-tmp";
const MARKER_CONTENT: &[u8] = b"taskr tmp base v1\n";
mod size;
pub use size::measure;

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub task_id: i64,
    pub root_id: i64,
    pub policy: String,
    pub closed: bool,
    pub closed_at: Option<String>,
    pub root_closed_at: Option<String>,
}
impl State {
    pub fn read(db: &Connection, task: i64) -> store::Result<Self> {
        let root = store::root(db, task)?;
        let close_state = |id| {
            db.query_row(
                "select status='closed', case when status='closed' then closed_at end from tasks where id=?",
                [id],
                |r| Ok((r.get::<_, bool>(0)?, r.get::<_, Option<String>>(1)?)),
            )
        };
        let policy: Option<String> = db.query_row("select (select json_extract(data,'$.value') from events where task_id=? and kind='ref' and json_extract(data,'$.key')='tmp.cleanup' order by id desc limit 1)", [root], |r| r.get(0))?;
        let (closed, closed_at) = close_state(task)?;
        let root_closed_at = close_state(root)?.1;
        Ok(Self {
            task_id: task,
            root_id: root,
            policy: policy
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "on-close".into()),
            closed,
            closed_at,
            root_closed_at,
        })
    }
    fn campaign_due(&self, now: time::OffsetDateTime) -> bool {
        self.closed
            && matches!(self.policy.as_str(), "on-close" | "root-close")
            && old(&self.root_closed_at, now)
    }
    fn lane_due(&self, now: time::OffsetDateTime) -> bool {
        self.closed && self.policy == "on-close" && old(&self.closed_at, now)
    }
}
fn old(at: &Option<String>, now: time::OffsetDateTime) -> bool {
    at.as_deref()
        .and_then(store::parse_time)
        .is_some_and(|at| now - at >= time::Duration::minutes(10))
}
fn check_base_stat(s: &Stat) -> Result<()> {
    if FileType::from_raw_mode(s.st_mode) != FileType::Directory {
        bail!("base is not a directory");
    }
    let uid = rustix::process::getuid().as_raw();
    if s.st_uid != uid {
        bail!("base is owned by uid {}, not {uid}", s.st_uid);
    }
    if s.st_mode & 0o022 != 0 {
        bail!("base is group- or world-writable");
    }
    Ok(())
}
fn open_base(base: &Path) -> Result<Option<OwnedFd>> {
    if !base.is_absolute()
        || base
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        bail!("TASKR_TMP_BASE must be absolute and contain no '..'");
    }
    let m = match fd::statat(fd::CWD, base, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(m) => m,
        Err(e) if e == rustix::io::Errno::NOENT => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if FileType::from_raw_mode(m.st_mode) == FileType::Symlink {
        bail!("base is a symlink");
    }
    let base_fd = fd::open(base, DIR, Mode::empty())?;
    let s = fd::fstat(&base_fd)?;
    check_base_stat(&s)?;
    if s.st_dev != m.st_dev || s.st_ino != m.st_ino {
        bail!("base changed while opening");
    }
    Ok(Some(base_fd))
}
// Numeric names alone cannot establish that an existing base belongs to taskr.
pub(super) fn init_base(base: &Path, lookup: &mut impl FnMut(i64) -> Result<State>) -> Result<()> {
    let dir = open_base(base)?.context("tmp base is missing")?;
    let identity = fd::fstat(&dir)?;
    dedicated(
        &dir,
        None,
        identity.st_dev,
        lookup,
        &mut size::Walk {
            left: 4096,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
        },
    )
    .context("not a dedicated taskr tmp base")?;
    let current = open_base(base)?.context("tmp base disappeared")?;
    let current = fd::fstat(&current)?;
    if current.st_dev != identity.st_dev || current.st_ino != identity.st_ino {
        bail!("tmp base changed during initialization");
    }
    match fd::openat(
        &dir,
        MARKER,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(marker) => File::from(marker).write_all(MARKER_CONTENT)?,
        Err(e) if e == rustix::io::Errno::EXIST => {}
        Err(e) => return Err(e.into()),
    }
    check_marker(&dir)
}
fn dedicated(
    dir: &OwnedFd,
    root: Option<i64>,
    device: fd::Dev,
    lookup: &mut impl FnMut(i64) -> Result<State>,
    limit: &mut size::Walk,
) -> Result<()> {
    let mut entries = fd::Dir::read_from(dir)?;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let child = entry.file_name();
        if matches!(child.to_bytes(), b"." | b"..") {
            continue;
        }
        limit.entry()?;
        if root.is_none() && child.to_bytes() == MARKER.as_bytes() {
            check_marker(dir)?;
            continue;
        }
        let text = child.to_str()?;
        let id = text.parse::<i64>()?;
        if id <= 0 || id.to_string() != text {
            bail!("noncanonical tmp directory id");
        }
        let state = lookup(id)?;
        if state.task_id != id || state.root_id != root.unwrap_or(id) {
            bail!("tmp directory membership mismatch");
        }
        let before = fd::statat(dir, child, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(before.st_mode) == FileType::Symlink {
            bail!("tmp directory candidate is a symlink");
        }
        let candidate = open_dir(dir, child, device)?;
        let identity = fd::fstat(&candidate)?;
        check_base_stat(&identity)?;
        if root.is_none() {
            dedicated(&candidate, Some(id), device, lookup, limit)?;
        }
        size::linked(dir, child, &identity)?;
    }
    limit.entry()?;
    Ok(())
}
fn check_marker(base: &OwnedFd) -> Result<()> {
    let before = fd::statat(base, MARKER, AtFlags::SYMLINK_NOFOLLOW)
        .context("missing or unreadable taskr tmp base marker; initialize a dedicated base with tmp ID --mkdir")?;
    if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile
        || before.st_uid != rustix::process::getuid().as_raw()
        || before.st_mode & 0o7777 != 0o600
    {
        bail!("invalid taskr tmp base marker type, owner or mode");
    }
    let marker = fd::openat(
        base,
        MARKER,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let after = fd::fstat(&marker)?;
    if after.st_dev != before.st_dev
        || after.st_ino != before.st_ino
        || after.st_mode != before.st_mode
        || after.st_uid != before.st_uid
    {
        bail!("taskr tmp base marker changed while opening");
    }
    let mut contents = Vec::new();
    File::from(marker)
        .take(MARKER_CONTENT.len() as u64 + 1)
        .read_to_end(&mut contents)?;
    if contents != MARKER_CONTENT {
        bail!("invalid taskr tmp base marker content");
    }
    Ok(())
}
fn checked_base(base: &Path) -> Result<Option<OwnedFd>> {
    let dir = open_base(base)?;
    if let Some(dir) = &dir {
        check_marker(dir)?;
    }
    Ok(dir)
}
fn same_device(s: &Stat, device: fd::Dev) -> Result<()> {
    if s.st_dev != device {
        bail!("refusing directory on another device");
    }
    Ok(())
}
fn open_dir(parent: &OwnedFd, name: &std::ffi::CStr, device: fd::Dev) -> Result<OwnedFd> {
    let before = fd::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    let dir = fd::openat(parent, name, DIR, Mode::empty())?;
    let after = fd::fstat(&dir)?;
    same_device(&after, device)?;
    if before.st_dev != after.st_dev || before.st_ino != after.st_ino {
        bail!("directory changed while opening");
    }
    Ok(dir)
}
fn name(id: i64) -> Result<std::ffi::CString> {
    if id <= 0 {
        bail!("tmp ids must be positive integers");
    }
    Ok(std::ffi::CString::new(id.to_string())?)
}
/// Deletes a lane, or a whole campaign for a root. Missing dirs are already clean.
pub fn remove(root: i64, task: Option<i64>) -> Result<()> {
    remove_under(&super::base(), root, task)
}
fn remove_under(base: &Path, root: i64, task: Option<i64>) -> Result<()> {
    let root = name(root)?;
    let task = task.map(name).transpose()?;
    let Some(base_fd) =
        checked_base(base).with_context(|| format!("refusing tmp base {}", base.display()))?
    else {
        return Ok(());
    };
    let device = fd::fstat(&base_fd)?.st_dev;
    let result = (|| {
        if let Some(task) = task {
            let campaign = match open_dir(&base_fd, &root, device) {
                Err(e)
                    if e.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) =>
                {
                    return Ok(());
                }
                result => result?,
            };
            erase(&campaign, &task, device, 0)
        } else {
            erase(&base_fd, &root, device, 0)
        }
    })();
    result.with_context(|| format!("remove tmp campaign {}", root.to_string_lossy()))
}
fn erase(parent: &OwnedFd, name: &std::ffi::CStr, device: fd::Dev, depth: usize) -> Result<()> {
    if depth >= 128 {
        bail!("tmp directory nesting exceeds 128");
    }
    let dir = match open_dir(parent, name, device) {
        Err(e) if e.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => {
            return Ok(());
        }
        result => result?,
    };
    let identity = fd::fstat(&dir)?;
    let mut entries = fd::Dir::read_from(&dir)?;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let child = entry.file_name();
        if matches!(child.to_bytes(), b"." | b"..") {
            continue;
        }
        // A replaced directory must not be followed through its old name.
        let linked = fd::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if linked.st_dev != identity.st_dev || linked.st_ino != identity.st_ino {
            bail!("tmp directory was replaced");
        }
        let s = fd::statat(&dir, child, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(s.st_mode) == FileType::Directory {
            erase(&dir, child, device, depth + 1)?;
        } else {
            fd::unlinkat(&dir, child, AtFlags::empty())?;
        }
    }
    let linked = fd::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if linked.st_dev != identity.st_dev || linked.st_ino != identity.st_ino {
        bail!("tmp directory was replaced");
    }
    fd::unlinkat(parent, name, AtFlags::REMOVEDIR)?;
    Ok(())
}
fn ids(dir: &OwnedFd) -> Result<Vec<i64>> {
    let mut entries = fd::Dir::read_from(dir)?;
    let mut ids = Vec::new();
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let Ok(s) = entry.file_name().to_str() else {
            continue;
        };
        if let Ok(id) = s.parse::<i64>()
            && id > 0
            && id.to_string() == s
        {
            ids.push(id);
        }
    }
    Ok(ids)
}
/// Independent errors leave the next candidate eligible. The report's bool marks lookup
/// errors for rate limiting; filesystem errors remain individually visible.
pub fn sweep(
    mut lookup: impl FnMut(i64) -> Result<State>,
    now: time::OffsetDateTime,
    mut report: impl FnMut(String, bool),
) {
    let base = super::base();
    sweep_under(&base, &mut lookup, now, &mut report);
}
fn sweep_under(
    base: &Path,
    lookup: &mut impl FnMut(i64) -> Result<State>,
    now: time::OffsetDateTime,
    report: &mut impl FnMut(String, bool),
) {
    let scan = (|| -> Result<()> {
        let Some(base_fd) = checked_base(base)? else {
            return Ok(());
        };
        let device = fd::fstat(&base_fd)?.st_dev;
        for root in ids(&base_fd)? {
            let state = match lookup(root) {
                Ok(state) => state,
                Err(e) => {
                    report(format!("tmp sweep {root}: {e:#}"), true);
                    continue;
                }
            };
            let campaign = (|| -> Result<()> {
                if state.root_id != root || state.task_id != root {
                    return Ok(());
                }
                let mut whole = state.campaign_due(now);
                if !whole && state.policy != "on-close" {
                    return Ok(());
                }
                let dir = open_dir(&base_fd, &name(root)?, device)?;
                let mut lanes = Vec::new();
                for task in ids(&dir)? {
                    match lookup(task) {
                        Ok(lane) => {
                            let confirmed = lane.root_id == root
                                && lane.task_id == task
                                && lane.policy == state.policy
                                && lane.closed
                                && old(&lane.closed_at, now);
                            whole &= confirmed;
                            if confirmed && state.policy == "on-close" && lane.lane_due(now) {
                                lanes.push(task);
                            }
                        }
                        Err(e) => {
                            whole = false;
                            report(format!("tmp sweep {root}/{task}: {e:#}"), true);
                        }
                    }
                }
                if whole {
                    return remove_under(base, root, None);
                }
                for task in lanes {
                    if let Err(e) = remove_under(base, root, Some(task)) {
                        report(format!("tmp sweep {root}/{task}: {e:#}"), false);
                    }
                }
                Ok(())
            })();
            if let Err(e) = campaign {
                report(format!("tmp sweep {root}: {e:#}"), false);
            }
        }
        Ok(())
    })();
    if let Err(e) = scan {
        report(format!("tmp sweep {}: {e:#}", base.display()), false);
    }
}

#[cfg(test)]
mod tests;
