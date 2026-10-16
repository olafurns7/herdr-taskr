//! Per-user Glance choices. Private repository results never go here.
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub const REPOS_HINT: &str = "set \"repos\" in ~/.config/taskr/tui.json";
pub const ISSUES_HINT: &str = "set \"issues_repo\" in ~/.config/taskr/tui.json";
pub const HEALTH_HINT: &str = "set \"health_repo\" and \"health_root\" in ~/.config/taskr/tui.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(deserialize_with = "panels")]
    /// Asks, checks, campaigns, PRs, Shipped, Build, Decisions, Fleet, Decision log, Issues,
    /// Health.
    pub panels: [bool; 11],
    pub repos: Vec<String>,
    pub issues_repo: Option<String>,
    pub health_repo: Option<String>,
    /// The hub root whose HEALTH notes Health reads; Health needs it and `health_repo`.
    pub health_root: Option<i64>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            panels: DEFAULT_PANELS,
            repos: vec![],
            issues_repo: None,
            health_repo: None,
            health_root: None,
        }
    }
}
/// Ledger overview on; repository panels and the optional ledger panels start hidden.
pub const DEFAULT_PANELS: [bool; 11] = [
    true, true, true, false, false, false, false, false, false, false, false,
];
/// An older file's choices are kept; the panels it predates take their defaults.
fn panels<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[bool; 11], D::Error> {
    let values = Vec::<bool>::deserialize(d)?;
    if ![4, 6, 9, 10, 11].contains(&values.len()) {
        return Err(serde::de::Error::custom(
            "expected four, six, nine, ten or eleven panel booleans",
        ));
    }
    let mut out = DEFAULT_PANELS;
    out[3..6].fill(true);
    out[..values.len()].copy_from_slice(&values);
    Ok(out)
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
        self.repos.len() <= 8
            && self.repos.iter().enumerate().all(|(i, r)| {
                repo_valid(r)
                    && !self.repos[..i]
                        .iter()
                        .any(|previous| previous.eq_ignore_ascii_case(r))
            })
            && self.issues_repo.as_deref().is_none_or(repo_valid)
            && self.health_repo.as_deref().is_none_or(repo_valid)
            && self.health_root.is_none_or(|root| root > 0)
    }
    /// Health reads only with both its repository and its hub root set.
    pub fn health(&self) -> Option<(String, i64)> {
        self.health_repo.clone().zip(self.health_root)
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
            Ok(bytes) if bytes.len() <= 16_384 => {
                match serde_json::from_slice::<serde_json::Value>(&bytes).and_then(|v| {
                    Ok((
                        v.get("panels").is_none(),
                        serde_json::from_value::<Self>(v)?,
                    ))
                }) {
                    Ok((no_panels, mut c)) if c.valid() => {
                        // Configured repos with no saved panel choice show their panels (4-6).
                        if no_panels && !c.repos.is_empty() {
                            c.panels[3..6].fill(true);
                        }
                        (c, None)
                    }
                    _ => (
                        Self::default(),
                        Some("TUI config invalid; using defaults".into()),
                    ),
                }
            }
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
    fn four_panel_migration_preserves_choices_and_repos() {
        let c: Config =
            serde_json::from_str(r#"{"panels":[false,true,false,true],"repos":["kept/private"]}"#)
                .unwrap();
        assert_eq!(
            c.panels,
            [
                false, true, false, true, true, true, false, false, false, false, false
            ]
        );
        assert_eq!(c.repos, ["kept/private"]);
        assert!(c.issues_repo.is_none() && c.health_repo.is_none() && c.health_root.is_none());
        let read: Config = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert_eq!(read.panels, c.panels);
        assert_eq!(
            Config::default().panels,
            [
                true, true, true, false, false, false, false, false, false, false, false
            ]
        );
        let six: Config = serde_json::from_str(
            r#"{"panels":[false,true,false,true,false,true],"repos":["kept/six"]}"#,
        )
        .unwrap();
        assert_eq!(
            (six.panels, six.repos),
            (
                [
                    false, true, false, true, false, true, false, false, false, false, false
                ],
                vec!["kept/six".to_string()]
            )
        );
        let nine = [true, false, true, false, true, false, true, true, false];
        let c: Config = serde_json::from_value(serde_json::json!({"panels": nine})).unwrap();
        let read: Config = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        // A nine-boolean file predates Issues (key 0) and Health (H): they start hidden.
        let mut all = [false; 11];
        all[..9].copy_from_slice(&nine);
        assert_eq!((c.panels, read.panels), (all, all));
        // Ten booleans predate Health only: Issues keeps its choice, Health starts hidden.
        all[9] = true;
        let c: Config = serde_json::from_value(serde_json::json!({"panels": &all[..10]})).unwrap();
        let read: Config = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert_eq!((c.panels, read.panels), (all, all));
        // Formerly malformed, eleven booleans are now the current shape and keep the last one.
        all[10] = true;
        let c: Config = serde_json::from_value(serde_json::json!({"panels": all})).unwrap();
        let read: Config = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert_eq!((c.panels, read.panels), (all, all));
        assert_eq!(
            serde_json::to_value(&c).unwrap()["panels"]
                .as_array()
                .unwrap()
                .len(),
            11
        );
        for values in [
            "[]",
            "[true]",
            "[true,true,true,true,true]",
            "[true,true,true,true,true,true,true]",
            "[true,true,true,true,true,true,true,true]",
            "[true,true,true,true,true,true,true,true,true,true,true,true]",
            "[true,true,true,1]",
            "[true,true,true,true,true,true,true,true,1]",
            "[true,true,true,true,true,true,true,true,true,1]",
            "[true,true,true,true,true,true,true,true,true,true,1]",
        ] {
            assert!(serde_json::from_str::<Config>(&format!(r#"{{"panels":{values}}}"#)).is_err());
        }
    }
    #[test]
    fn empty_and_optional_repo_settings_are_validated() {
        let dir = env::temp_dir().join(format!("taskr-tui-repos-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tui.json");
        for json in [
            r#"{}"#,
            r#"{"repos":[],"issues_repo":null,"health_repo":null}"#,
            r#"{"repos":["example-org/app"],"issues_repo":"other/issues","health_repo":"other/health","health_root":4242}"#,
            r#"{"health_root":null}"#,
        ] {
            fs::write(&path, json).unwrap();
            assert!(Config::load(&path).1.is_none(), "{json}");
        }
        // Health needs both its repository and its root.
        for (json, health) in [
            (r#"{"health_repo":"other/health"}"#, None),
            (r#"{"health_root":4242}"#, None),
            (
                r#"{"health_repo":"other/health","health_root":4242}"#,
                Some(("other/health".to_string(), 4242)),
            ),
        ] {
            fs::write(&path, json).unwrap();
            assert_eq!(Config::load(&path).0.health(), health, "{json}");
        }
        for bad in ["0", "-1", "1.5", "\"4242\"", "true", "9223372036854775808"] {
            fs::write(&path, format!(r#"{{"health_root":{bad}}}"#)).unwrap();
            let (config, warning) = Config::load(&path);
            assert!(warning.is_some() && config.health_root.is_none(), "{bad}");
        }
        // Repos with no saved panel choice show 4-6; an explicit choice or no repos does not.
        for (json, shown) in [
            (r#"{"repos":["example-org/app"]}"#, true),
            (r#"{"repos":[]}"#, false),
            (r#"{}"#, false),
            (
                r#"{"repos":["example-org/app"],"panels":[true,true,true,false,false,false]}"#,
                false,
            ),
        ] {
            fs::write(&path, json).unwrap();
            let panels = Config::load(&path).0.panels;
            assert_eq!(panels[3..6], [shown; 3], "{json}");
            assert_eq!(panels[..3], [true; 3], "{json}");
        }
        for key in ["repos", "issues_repo", "health_repo"] {
            for bad in ["", "--bad/repo", "a/b/c", "a/..", "a/b\n"] {
                let value = if key == "repos" {
                    serde_json::json!([bad])
                } else {
                    serde_json::json!(bad)
                };
                fs::write(
                    &path,
                    serde_json::to_vec(&serde_json::json!({key: value})).unwrap(),
                )
                .unwrap();
                let (config, warning) = Config::load(&path);
                assert!(warning.is_some(), "{key}: {bad:?}");
                assert!(
                    config.repos.is_empty()
                        && config.issues_repo.is_none()
                        && config.health_repo.is_none()
                );
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn roundtrip_defaults_bad_and_failed_paths() {
        let dir = env::temp_dir().join(format!("taskr-tui-config-{}", std::process::id()));
        let path = dir.join("tui.json");
        let _ = fs::remove_dir_all(&dir);
        assert!(Config::load(&path).1.is_none());
        let c = Config {
            panels: [false; 11],
            repos: vec!["owner/other.repo".into()],
            issues_repo: Some("example-org/issues".into()),
            health_repo: Some("example-org/health".into()),
            health_root: Some(4242),
        };
        c.save(&path).unwrap();
        let (read, warning) = Config::load(&path);
        assert!(warning.is_none());
        assert_eq!(read.panels, [false; 11]);
        assert_eq!(read.repos, c.repos);
        assert_eq!(read.issues_repo, c.issues_repo);
        assert_eq!(read.health_repo, c.health_repo);
        assert_eq!(read.health_root, c.health_root);
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
