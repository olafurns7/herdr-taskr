//! Per-user Glance choices. Private repository results never go here.
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub panels: [bool; 4],
    pub repos: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            panels: [true; 4],
            repos: vec!["trip-to-japan/trip".into()],
        }
    }
}
pub fn repo_valid(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && owner.len() <= 39
        && !owner.starts_with('-')
        && !name.is_empty()
        && name.len() <= 100
        && owner
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        && name != "."
        && name != ".."
}
impl Config {
    fn valid(&self) -> bool {
        !self.repos.is_empty()
            && self.repos.len() <= 8
            && self.repos.iter().enumerate().all(|(i, r)| {
                repo_valid(r)
                    && !self.repos[..i]
                        .iter()
                        .any(|previous| previous.eq_ignore_ascii_case(r))
            })
    }
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let read = || -> io::Result<Vec<u8>> {
            let file = fs::File::open(path)?;
            let mut bytes = vec![];
            file.take(16_385).read_to_end(&mut bytes)?;
            Ok(bytes)
        };
        match read() {
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Self::default(), None),
            Ok(bytes) if bytes.len() <= 16_384 => match serde_json::from_slice::<Self>(&bytes) {
                Ok(c) if c.valid() => (c, None),
                _ => (
                    Self::default(),
                    Some("TUI config invalid; using defaults".into()),
                ),
            },
            _ => (
                Self::default(),
                Some("TUI config unreadable; using defaults".into()),
            ),
        }
    }
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("missing config directory"))?;
        fs::create_dir_all(dir)?;
        // A unique sibling plus rename leaves a previous config intact on failure.
        let tmp = dir.join(format!(
            ".tui-{}-{}.tmp",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            serde_json::to_writer(&mut file, self)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    }
}
pub fn path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|p| p.join("taskr/tui.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_defaults_bad_and_failed_paths() {
        let dir = env::temp_dir().join(format!("taskr-tui-config-{}", std::process::id()));
        let path = dir.join("tui.json");
        let _ = fs::remove_dir_all(&dir);
        assert!(Config::load(&path).1.is_none());
        let c = Config {
            panels: [false; 4],
            repos: vec!["owner/other.repo".into()],
        };
        c.save(&path).unwrap();
        let (read, warning) = Config::load(&path);
        assert!(warning.is_none());
        assert_eq!(read.panels, [false; 4]);
        assert_eq!(read.repos, c.repos);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(&path, b"{bad").unwrap();
        assert!(Config::load(&path).1.is_some());
        fs::write(&path, br#"{"repos":["--bad/repo"]}"#).unwrap();
        assert!(Config::load(&path).1.is_some());
        fs::write(&path, br#"{"repos":["Owner/Repo","owner/repo"]}"#).unwrap();
        assert!(Config::load(&path).1.is_some());
        fs::write(&path, serde_json::to_vec(&serde_json::json!({"repos": (0..9).map(|i| format!("owner/repo{i}")).collect::<Vec<_>>()})).unwrap()).unwrap();
        assert!(Config::load(&path).1.is_some());
        assert!(c.save(&path.join("child")).is_err());
        assert!(Config::load(&dir).1.is_some());
        fs::remove_dir_all(dir).unwrap();
        for repo in ["a/b", "trip-to-japan/trip"] {
            assert!(repo_valid(repo));
        }
        for repo in ["a/b/c", "../x", "a/..", "a/b\n", "-a/b"] {
            assert!(!repo_valid(repo));
        }
    }
}
