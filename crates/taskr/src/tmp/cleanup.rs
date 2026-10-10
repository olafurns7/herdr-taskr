//! Host-local cleanup; ledger authority never supplies filesystem paths.
use anyhow::{Context, Result, bail};
use rustix::fs::{self as fd, AtFlags, FileType, Mode, OFlags, Stat};
use serde::{Deserialize, Serialize};
use std::{os::fd::OwnedFd, path::Path};
use taskr_core::{db::Connection, store};

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

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
fn checked_base(base: &Path) -> Result<Option<OwnedFd>> {
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
/// Independent errors leave the next campaign/lane eligible; failed ledger reads never delete.
pub fn sweep(
    mut lookup: impl FnMut(i64) -> Result<State>,
    now: time::OffsetDateTime,
    mut report: impl FnMut(String),
) {
    let base = super::base();
    sweep_under(&base, &mut lookup, now, &mut report);
}
fn sweep_under(
    base: &Path,
    lookup: &mut impl FnMut(i64) -> Result<State>,
    now: time::OffsetDateTime,
    report: &mut impl FnMut(String),
) {
    let scan = (|| -> Result<()> {
        let Some(base_fd) = checked_base(base)? else {
            return Ok(());
        };
        let device = fd::fstat(&base_fd)?.st_dev;
        for root in ids(&base_fd)? {
            let campaign = (|| -> Result<()> {
                let state = lookup(root)?;
                if state.root_id != root || state.task_id != root {
                    return Ok(());
                }
                if state.campaign_due(now) {
                    return remove_under(base, root, None);
                }
                if state.policy != "on-close" {
                    return Ok(());
                }
                let dir = open_dir(&base_fd, &name(root)?, device)?;
                for task in ids(&dir)? {
                    let lane = (|| -> Result<()> {
                        let state = lookup(task)?;
                        if state.root_id == root && state.task_id == task && state.lane_due(now) {
                            remove_under(base, root, Some(task))?;
                        }
                        Ok(())
                    })();
                    if let Err(e) = lane {
                        report(format!("tmp sweep {root}/{task}: {e:#}"));
                    }
                }
                Ok(())
            })();
            if let Err(e) = campaign {
                report(format!("tmp sweep {root}: {e:#}"));
            }
        }
        Ok(())
    })();
    if let Err(e) = scan {
        report(format!("tmp sweep {}: {e:#}", base.display()));
    }
}

#[cfg(test)]
mod tests;
