use super::*;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "taskr-cleanup-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        init_base(&p).unwrap();
        Self(p)
    }
    fn lane(&self, root: i64, task: i64) -> PathBuf {
        let p = self.0.join(root.to_string()).join(task.to_string());
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("file"), "scratch").unwrap();
        p
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn at() -> time::OffsetDateTime {
    store::parse_time("2026-10-10T00:20:00Z").unwrap()
}
fn state(root: i64, task: i64, policy: &str, lane: Option<&str>, campaign: Option<&str>) -> State {
    State {
        root_id: root,
        task_id: task,
        policy: policy.into(),
        closed: lane.is_some(),
        closed_at: lane.map(str::to_string),
        root_closed_at: campaign.map(str::to_string),
    }
}
#[test]
fn sweep_policies_grace_unknown_open_and_noninteger() {
    let b = Scratch::new();
    for root in 1..=7 {
        b.lane(root, root);
        b.lane(root, root + 10);
    }
    for task in [22, 23, 24, 25] {
        b.lane(2, task);
    }
    for p in ["noninteger", "001", "2/not-a-task"] {
        fs::create_dir(b.0.join(p)).unwrap();
    }
    let old = Some("2026-10-10T00:09:59Z");
    let recent = Some("2026-10-10T00:10:01Z");
    let mut errors = vec![];
    // Candidate scope before deletion: campaigns 1,4 and lanes 2/12,6/16 only.
    sweep_under(
        &b.0,
        &mut |id| {
            Ok(match id {
                1 | 11 => state(1, id, "on-close", old, old),
                2 => state(2, id, "on-close", None, None),
                12 => state(2, id, "on-close", old, None),
                22 => state(2, id, "on-close", recent, None),
                23 => state(2, id, "on-close", None, None),
                24 => state(99, id, "on-close", old, None),
                25 => state(2, id, "on-close", Some("invalid"), None),
                3 => state(3, id, "root-close", None, None),
                13 => state(3, id, "root-close", old, None),
                4 | 14 => state(4, id, "root-close", old, old),
                5 | 15 => state(5, id, "keep", old, old),
                6 => state(6, id, "on-close", recent, recent),
                16 => state(6, id, "on-close", old, recent),
                _ => bail!("unknown task {id}"),
            })
        },
        at(),
        &mut |e, _| errors.push(e),
    );
    for p in ["1", "4", "2/12", "6/16"] {
        assert!(!b.0.join(p).exists(), "{p}");
    }
    for p in [
        "2/2",
        "2/22",
        "2/23",
        "2/24",
        "2/25",
        "3/13",
        "5/15",
        "6/6",
        "7/17",
        "noninteger",
        "001",
        "2/not-a-task",
    ] {
        assert!(b.0.join(p).exists(), "{p}");
    }
    assert!(errors.iter().any(|e| e.contains("unknown task 7")));
    assert!(state(2, 22, "on-close", Some("2026-10-10T00:10:00Z"), None).lane_due(at()));
    assert!(!state(2, 22, "broken", old, old).campaign_due(at()));
}
#[test]
fn nofollow_unlinks_links_without_touching_the_target() {
    let b = Scratch::new();
    let target = Scratch::new();
    let outside = target.lane(9, 9);
    let lane = b.lane(1, 2);
    fs::create_dir(lane.join("nested")).unwrap();
    symlink(&outside, lane.join("nested/link")).unwrap();
    symlink(&outside, lane.join("link")).unwrap();
    remove_under(&b.0, 1, Some(2)).unwrap();
    assert!(!lane.exists());
    assert_eq!(fs::read_to_string(outside.join("file")).unwrap(), "scratch");
    symlink(&outside, b.0.join("1/2")).unwrap();
    assert!(remove_under(&b.0, 1, Some(2)).is_err());
    assert!(outside.join("file").exists());
}
#[test]
fn refuses_unsafe_base_and_devices() {
    let b = Scratch::new();
    let target = Scratch::new();
    target.lane(1, 2);
    let link = b.0.join("link");
    symlink(&target.0, &link).unwrap();
    assert!(remove_under(&link, 1, Some(2)).is_err());
    for mode in [0o770, 0o707] {
        fs::set_permissions(&target.0, fs::Permissions::from_mode(mode)).unwrap();
        assert!(remove_under(&target.0, 1, Some(2)).is_err());
        assert!(target.0.join("1/2/file").exists());
    }
    fs::set_permissions(&target.0, fs::Permissions::from_mode(0o700)).unwrap();
    let dir = checked_base(&target.0).unwrap().unwrap();
    let mut s = fd::fstat(&dir).unwrap();
    s.st_uid = rustix::process::getuid().as_raw().wrapping_add(1);
    assert!(
        check_base_stat(&s)
            .unwrap_err()
            .to_string()
            .contains("owned by uid")
    );
    s.st_uid = rustix::process::getuid().as_raw();
    assert!(same_device(&s, s.st_dev.wrapping_add(1)).is_err());
    assert!(open_dir(&dir, &name(1).unwrap(), s.st_dev.wrapping_add(1)).is_err());
    assert!(target.0.join("1/2/file").exists());
    assert!(remove_under(&target.0, 0, Some(2)).is_err());
    assert!(remove_under(&target.0, 1, Some(-2)).is_err());
    assert!(remove_under(&target.0.join("../other"), 1, Some(2)).is_err());
}
#[test]
fn failed_ledger_reads_and_one_unsafe_campaign_do_not_delete_other_paths() {
    let b = Scratch::new();
    let lane = b.lane(1, 2);
    let mut errors = vec![];
    sweep_under(
        &b.0,
        &mut |_| bail!("hub unreachable"),
        at(),
        &mut |e, _| errors.push(e),
    );
    assert!(lane.join("file").exists());
    assert!(errors.iter().any(|e| e.contains("hub unreachable")));
    let target = Scratch::new();
    let outside = target.lane(9, 9);
    symlink(&outside, b.0.join("3")).unwrap();
    sweep_under(
        &b.0,
        &mut |id| {
            Ok(state(
                if id == 2 { 1 } else { id },
                id,
                "on-close",
                Some("2026-10-10T00:00:00Z"),
                None,
            ))
        },
        at(),
        &mut |e, _| errors.push(e),
    );
    assert!(!lane.exists());
    assert!(outside.join("file").exists());
    assert!(errors.iter().any(|e| e.contains("tmp sweep 3")));
}
#[test]
fn marker_required_before_any_deletion() {
    let outside = Scratch::new();
    let target = outside.0.join("sentinel");
    fs::write(&target, "outside").unwrap();
    for invalid in ["missing", "symlink", "content", "mode", "directory"] {
        let b = Scratch::new();
        let lane = b.lane(1, 2);
        let marker = b.0.join(MARKER);
        fs::remove_file(&marker).unwrap();
        match invalid {
            "missing" => {}
            "symlink" => symlink(&target, &marker).unwrap(),
            "content" => fs::write(&marker, "not taskr").unwrap(),
            "mode" => fs::write(&marker, MARKER_CONTENT).unwrap(),
            "directory" => fs::create_dir(&marker).unwrap(),
            _ => unreachable!(),
        }
        if invalid == "content" {
            fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        } else if invalid == "mode" {
            fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert!(remove_under(&b.0, 1, None).is_err(), "{invalid}");
        assert!(remove_under(&b.0, 1, Some(2)).is_err(), "{invalid}");
        let mut errors = vec![];
        sweep_under(
            &b.0,
            &mut |id| {
                Ok(state(
                    1,
                    id,
                    "on-close",
                    Some("2026-10-10T00:00:00Z"),
                    Some("2026-10-10T00:00:00Z"),
                ))
            },
            at(),
            &mut |e, lookup| {
                assert!(!lookup);
                errors.push(e);
            },
        );
        assert!(!errors.is_empty(), "{invalid}");
        assert!(lane.join("file").exists(), "{invalid}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "outside");
        if invalid != "missing" {
            assert!(init_base(&b.0).is_err(), "{invalid}");
            assert_eq!(fs::read_to_string(&target).unwrap(), "outside");
        }
    }
}
#[test]
fn campaign_removal_requires_all_lane_authority() {
    let old = Some("2026-10-10T00:00:00Z");
    for policy in ["on-close", "root-close"] {
        for blocked in [
            "open",
            "unknown",
            "mismatched",
            "malformed",
            "unconfirmed",
            "recent",
            "failed",
        ] {
            let b = Scratch::new();
            b.lane(1, 1);
            b.lane(1, 2);
            let protected = b.lane(1, 3);
            fs::write(b.0.join("1/extra"), "campaign sentinel").unwrap();
            let mut errors = vec![];
            // Whole campaign is ineligible; only 1/1 and 1/2 may go under on-close.
            sweep_under(
                &b.0,
                &mut |id| {
                    let mut lane = state(1, id, policy, old, old);
                    if id == 3 {
                        match blocked {
                            "open" => {
                                lane.closed = false;
                                lane.closed_at = None;
                            }
                            "unknown" => bail!("unknown task 3"),
                            "mismatched" => lane.root_id = 9,
                            "malformed" => lane.closed_at = Some("invalid".into()),
                            "unconfirmed" => lane.closed = false,
                            "recent" => lane.closed_at = Some("2026-10-10T00:19:59Z".into()),
                            "failed" => bail!("hub unreachable"),
                            _ => unreachable!(),
                        }
                    }
                    Ok(lane)
                },
                at(),
                &mut |e, lookup| {
                    assert!(lookup);
                    errors.push(e);
                },
            );
            assert!(protected.join("file").exists(), "{policy}/{blocked}");
            assert!(b.0.join("1/extra").exists(), "{policy}/{blocked}");
            assert_eq!(b.0.join("1/2").exists(), policy == "root-close");
            assert_eq!(!errors.is_empty(), matches!(blocked, "unknown" | "failed"));
            sweep_under(
                &b.0,
                &mut |id| Ok(state(1, id, policy, old, old)),
                at(),
                &mut |e, _| panic!("{e}"),
            );
            assert!(!b.0.join("1").exists(), "all confirmed closed: {policy}");
        }
    }
}
#[test]
fn symlink_replacement_race_never_follows_target() {
    use std::sync::{Arc, atomic::AtomicBool};
    let target = Scratch::new();
    let outside = target.lane(9, 9);
    for _ in 0..20 {
        let b = Scratch::new();
        let lane = b.lane(1, 2);
        let child = lane.join("child");
        fs::create_dir(&child).unwrap();
        for n in 0..50 {
            fs::write(child.join(n.to_string()), "x").unwrap();
        }
        let parked = lane.join("parked");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let outside_copy = outside.clone();
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                if fs::rename(&child, &parked).is_ok() {
                    let _ = symlink(&outside_copy, &child);
                    let _ = fs::remove_file(&child);
                    let _ = fs::rename(&parked, &child);
                }
            }
        });
        let _ = remove_under(&b.0, 1, Some(2));
        stop.store(true, Ordering::Relaxed);
        thread.join().unwrap();
        assert_eq!(fs::read_to_string(outside.join("file")).unwrap(), "scratch");
    }
}
