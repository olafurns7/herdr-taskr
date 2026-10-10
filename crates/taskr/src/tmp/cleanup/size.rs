//! Read-only traversal through the same checked descriptors as cleanup.
use super::*;
use std::time::{Duration, Instant};

const ENTRIES: usize = 1_000_000;
const DEPTH: usize = 128;
const SECONDS: u64 = 15;
const ROOTS: usize = 4096;

struct Walk {
    left: usize,
    deadline: Instant,
}
impl Walk {
    fn entry(&mut self) -> Result<()> {
        if self.left == 0 || Instant::now() >= self.deadline {
            bail!("tmp size walk limit reached");
        }
        self.left -= 1;
        Ok(())
    }
}
fn linked(parent: &OwnedFd, name: &std::ffi::CStr, identity: &Stat) -> Result<()> {
    let current = fd::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if current.st_dev != identity.st_dev || current.st_ino != identity.st_ino {
        bail!("tmp size directory was replaced");
    }
    Ok(())
}
fn walk(
    parent: &OwnedFd,
    name: &std::ffi::CStr,
    device: fd::Dev,
    depth: usize,
    limit: &mut Walk,
    root: i64,
    lookup: &mut impl FnMut(i64) -> Result<State>,
) -> Result<u64> {
    if depth >= DEPTH {
        bail!("tmp size nesting limit reached");
    }
    let dir = open_dir(parent, name, device)?;
    let identity = fd::fstat(&dir)?;
    let mut entries = fd::Dir::read_from(&dir)?;
    let mut total = 0_u64;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let child = entry.file_name();
        if matches!(child.to_bytes(), b"." | b"..") {
            continue;
        }
        limit.entry()?;
        linked(parent, name, &identity)?;
        let s = fd::statat(&dir, child, AtFlags::SYMLINK_NOFOLLOW)?;
        same_device(&s, device)?;
        let bytes = match FileType::from_raw_mode(s.st_mode) {
            FileType::Directory => {
                if depth == 0 {
                    let text = child.to_str()?;
                    let id = text.parse::<i64>()?;
                    if id <= 0 || id.to_string() != text {
                        bail!("noncanonical tmp lane id");
                    }
                    let lane = lookup(id)?;
                    if lane.task_id != id || lane.root_id != root {
                        bail!("tmp lane membership mismatch");
                    }
                }
                walk(&dir, child, device, depth + 1, limit, root, lookup)?
            }
            FileType::RegularFile => u64::try_from(s.st_size)?,
            _ => 0,
        };
        total = total.checked_add(bytes).context("tmp size overflow")?;
    }
    linked(parent, name, &identity)?;
    if Instant::now() >= limit.deadline {
        bail!("tmp size time limit reached");
    }
    Ok(total)
}

pub fn measure(
    mut lookup: impl FnMut(i64) -> Result<State>,
    mut report: impl FnMut(String),
) -> Vec<(i64, u64)> {
    measure_under(&super::super::base(), &mut lookup, &mut report)
}
fn measure_under(
    base: &Path,
    lookup: &mut impl FnMut(i64) -> Result<State>,
    report: &mut impl FnMut(String),
) -> Vec<(i64, u64)> {
    let mut measured = Vec::new();
    let scan = (|| -> Result<()> {
        let Some(base_fd) = checked_base(base)? else {
            return Ok(());
        };
        let device = fd::fstat(&base_fd)?.st_dev;
        let identity = fd::fstat(&base_fd)?;
        let mut entries = fd::Dir::read_from(&base_fd)?;
        let mut scan = Walk {
            left: ROOTS,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        while let Some(entry) = entries.read() {
            let entry = entry?;
            let child = entry.file_name();
            if matches!(child.to_bytes(), b"." | b".." | b".taskr-tmp") {
                continue;
            }
            scan.entry()?;
            let Ok(text) = child.to_str() else {
                continue;
            };
            let Ok(root) = text.parse::<i64>() else {
                continue;
            };
            if root <= 0 || root.to_string() != text {
                continue;
            }
            let campaign = (|| -> Result<u64> {
                let state = lookup(root)?;
                if state.task_id != root || state.root_id != root {
                    bail!("tmp campaign is not a real root");
                }
                let mut limit = Walk {
                    left: ENTRIES,
                    deadline: (Instant::now() + Duration::from_secs(SECONDS)).min(scan.deadline),
                };
                let bytes = walk(&base_fd, child, device, 0, &mut limit, root, lookup)?;
                let current = checked_base(base)?.context("tmp size base disappeared")?;
                let s = fd::fstat(&current)?;
                if s.st_dev != identity.st_dev || s.st_ino != identity.st_ino {
                    bail!("tmp size base was replaced");
                }
                Ok(bytes)
            })();
            match campaign {
                Ok(bytes) => measured.push((root, bytes)),
                Err(e) => report(format!("tmp size {root}: {e:#}")),
            }
        }
        Ok(())
    })();
    if let Err(e) = scan {
        report(format!("tmp size scan: {e:#}"));
    }
    measured
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Scratch, state};
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    fn lookup(id: i64) -> Result<State> {
        Ok(state(1, id, "keep", None, None))
    }
    #[test]
    fn complete_logical_size_empty_links_and_authority() {
        let b = Scratch::new();
        let lane = b.lane(1, 2);
        let outside = Scratch::new();
        fs::write(outside.0.join("large"), vec![0; 100_000]).unwrap();
        symlink(&outside.0, lane.join("link")).unwrap();
        fs::write(lane.join("other"), "12345").unwrap();
        assert_eq!(
            measure_under(&b.0, &mut lookup, &mut |e| panic!("{e}")),
            [(1, 12)]
        );
        fs::remove_file(lane.join("file")).unwrap();
        fs::remove_file(lane.join("other")).unwrap();
        assert_eq!(
            measure_under(&b.0, &mut lookup, &mut |e| panic!("{e}")),
            [(1, 0)]
        );
        for wrong in ["root", "lane", "unknown"] {
            let mut errors = vec![];
            let result = measure_under(
                &b.0,
                &mut |id| {
                    if wrong == "unknown" {
                        bail!("unknown ledger authority");
                    }
                    Ok(state(
                        if (wrong == "root" && id == 1) || (wrong == "lane" && id == 2) {
                            9
                        } else {
                            1
                        },
                        id,
                        "keep",
                        None,
                        None,
                    ))
                },
                &mut |e| errors.push(e),
            );
            assert!(result.is_empty(), "{wrong}");
            assert!(!errors.is_empty());
            assert!(lane.exists());
        }
    }
    #[test]
    fn unsafe_unmarked_and_bounded_failure_preserve_prior_report() {
        let b = Scratch::new();
        let lane = b.lane(1, 2);
        let base = checked_base(&b.0).unwrap().unwrap();
        let device = fd::fstat(&base).unwrap().st_dev;
        let mut prior = 77;
        for (left, deadline) in [
            (0, Instant::now() + Duration::from_secs(15)),
            (10, Instant::now()),
        ] {
            if let Ok(bytes) = walk(
                &base,
                &name(1).unwrap(),
                device,
                0,
                &mut Walk { left, deadline },
                1,
                &mut lookup,
            ) {
                prior = bytes;
            }
            assert_eq!(prior, 77);
        }
        assert!(
            walk(
                &base,
                &name(1).unwrap(),
                device.wrapping_add(1),
                0,
                &mut Walk {
                    left: 10,
                    deadline: Instant::now() + Duration::from_secs(15)
                },
                1,
                &mut lookup
            )
            .is_err()
        );
        assert!(
            walk(
                &base,
                &name(1).unwrap(),
                device,
                DEPTH,
                &mut Walk {
                    left: 10,
                    deadline: Instant::now() + Duration::from_secs(15)
                },
                1,
                &mut lookup
            )
            .is_err()
        );
        for mode in [0o770, 0o707] {
            fs::set_permissions(&b.0, fs::Permissions::from_mode(mode)).unwrap();
            assert!(measure_under(&b.0, &mut lookup, &mut |_| {}).is_empty());
        }
        fs::set_permissions(&b.0, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_file(b.0.join(MARKER)).unwrap();
        assert!(measure_under(&b.0, &mut lookup, &mut |_| {}).is_empty());
        assert!(lane.join("file").exists());
        assert!(!b.0.join(MARKER).exists());
    }
}
