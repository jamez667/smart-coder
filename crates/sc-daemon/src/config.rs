//! The daemon's configuration: **which repositories it serves**.
//!
//! This is the security-relevant part of the whole intake path, and it is a
//! *closed set* by design (spec 18):
//!
//! > **A repo is chosen, never typed.** An arbitrary path in a request body is
//! > rejected outright rather than canonicalised-and-checked.
//!
//! That is the difference between path traversal being *mitigated* and being
//! *unreachable*. The surface offers the names in this file and nothing else; a
//! request naming anything absent is refused before any path handling happens, so
//! there is no normalisation logic to get subtly wrong.
//!
//! Note the daemon serves **any** repository the developer nominates — it is not
//! tied to the workspace it was built in, and nothing here may assume one.

use std::path::{Path, PathBuf};

use sc_proto::{DcError, Result};
use serde::{Deserialize, Serialize};

/// One repository the daemon will draft specs for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    /// The short name a request refers to it by: `smart-coder`, `city`.
    ///
    /// Requests carry this, never a path. It is also what a phone shows in a
    /// picker, so it should read as a project name rather than a directory.
    pub name: String,
    /// Absolute path to the working tree.
    pub path: PathBuf,
}

/// The daemon's configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// The repositories this daemon serves. Empty means the daemon has nothing to
    /// do, which is a legitimate (if useless) state rather than an error.
    #[serde(default)]
    pub repos: Vec<Repo>,
}

impl DaemonConfig {
    /// Look up a repo by the name a request used.
    ///
    /// Case-sensitive and exact: a fuzzy match here would reintroduce the
    /// ambiguity the closed set exists to remove.
    pub fn repo(&self, name: &str) -> Option<&Repo> {
        self.repos.iter().find(|r| r.name == name)
    }

    /// Resolve the repo a request named, or say what was actually on offer.
    ///
    /// The error lists the configured names because the alternative — "unknown
    /// repo" — leaves a user on a phone with no way to discover the right one.
    pub fn require_repo(&self, name: &str) -> Result<&Repo> {
        self.repo(name).ok_or_else(|| {
            let known: Vec<&str> = self.repos.iter().map(|r| r.name.as_str()).collect();
            DcError::Eval(if known.is_empty() {
                format!(
                    "no repository named {name:?} — this daemon has none configured. \
                     Add one to {}.",
                    config_file().display()
                )
            } else {
                format!(
                    "no repository named {name:?}. This daemon serves: {}",
                    known.join(", ")
                )
            })
        })
    }

    /// Every configured name, for a picker.
    pub fn names(&self) -> Vec<&str> {
        self.repos.iter().map(|r| r.name.as_str()).collect()
    }

    /// Add or replace a repo, canonicalising its path.
    ///
    /// Canonicalisation happens *here*, at configuration time by the developer at
    /// their own keyboard — never at request time. That ordering is the point: by
    /// the time a network request is handled there is no path to resolve, only a
    /// name to look up.
    pub fn add(&mut self, name: &str, path: &Path) -> Result<()> {
        if name.trim().is_empty() {
            return Err(DcError::Eval("a repo needs a name".to_string()));
        }
        if !path.is_dir() {
            return Err(DcError::Eval(format!(
                "{} is not a directory",
                path.display()
            )));
        }
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.repos.retain(|r| r.name != name);
        self.repos.push(Repo {
            name: name.to_string(),
            path,
        });
        self.repos.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }

    /// Forget a repo. `true` if one was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.repos.len();
        self.repos.retain(|r| r.name != name);
        self.repos.len() != before
    }
}

/// The daemon's home directory: `~/.smart-coder/`.
///
/// Deliberately the home directory rather than any workspace — the daemon is a
/// user-level service serving several repositories, and putting its queue inside
/// one of them would make that repo special and get the queue committed.
pub fn home() -> PathBuf {
    // `HOME` on Unix, `USERPROFILE` on Windows. Falling back to the temp dir keeps
    // the daemon runnable in a stripped environment rather than failing to start.
    std::env::var_os("SC_DAEMON_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
        .join(".smart-coder")
}

/// Where the config lives.
pub fn config_file() -> PathBuf {
    home().join("daemon.json")
}

/// Read the config, or a default (no repos) when there is none.
///
/// A *missing* file is not an error — a daemon nobody has configured yet should
/// say "no repositories configured" rather than fail to start. A *malformed* one
/// is an error, because silently serving an empty set would look identical to
/// having been configured with nothing, and the developer would be left wondering
/// where their repos went.
pub fn load() -> Result<DaemonConfig> {
    load_from(&config_file())
}

/// Read the config at an explicit path (the seam every test uses).
pub fn load_from(path: &Path) -> Result<DaemonConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| DcError::Eval(format!("{} is not valid JSON: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DaemonConfig::default()),
        Err(e) => Err(e.into()),
    }
}

/// Write the config.
pub fn save(cfg: &DaemonConfig) -> Result<()> {
    save_to(&config_file(), cfg)
}

/// Write the config to an explicit path.
pub fn save_to(path: &Path, cfg: &DaemonConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cfg).map_err(|e| DcError::Eval(e.to_string()))?;
    crate::atomic::write(path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

    fn cfg_with(names: &[(&str, &Path)]) -> DaemonConfig {
        let mut cfg = DaemonConfig::default();
        for (name, path) in names {
            cfg.add(name, path).unwrap();
        }
        cfg
    }

    #[test]
    fn a_request_names_a_repo_and_gets_the_configured_path() {
        let a = temp_dir("cfg-a");
        let b = temp_dir("cfg-b");
        let cfg = cfg_with(&[("alpha", &a), ("beta", &b)]);

        assert_eq!(cfg.repo("alpha").unwrap().path, a.canonicalize().unwrap());
        assert_eq!(cfg.repo("beta").unwrap().path, b.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn an_unconfigured_name_is_refused_and_the_error_says_what_is_on_offer() {
        // A user on a phone with "unknown repo" and nothing else has no way to
        // discover the right name.
        let a = temp_dir("cfg-known");
        let cfg = cfg_with(&[("alpha", &a)]);

        let err = cfg.require_repo("gamma").expect_err("not configured");
        let msg = err.to_string();
        assert!(msg.contains("gamma"), "{msg}");
        assert!(msg.contains("alpha"), "lists what IS served: {msg}");
        let _ = std::fs::remove_dir_all(&a);
    }

    #[test]
    fn a_path_is_never_accepted_as_a_repo_name() {
        // The closed set is the whole defence: there is no path-handling code at
        // request time to get subtly wrong, because a request carries no path.
        let a = temp_dir("cfg-traversal");
        let cfg = cfg_with(&[("alpha", &a)]);

        for attempt in [
            "../../etc/passwd",
            "/etc/passwd",
            "C:\\Windows\\System32",
            "alpha/../beta",
            "ALPHA",
        ] {
            assert!(
                cfg.repo(attempt).is_none(),
                "{attempt:?} must not resolve to a repo"
            );
        }
        let _ = std::fs::remove_dir_all(&a);
    }

    #[test]
    fn adding_requires_a_real_directory_and_a_name() {
        let a = temp_dir("cfg-add");
        let mut cfg = DaemonConfig::default();

        assert!(cfg.add("", &a).is_err(), "a nameless repo is unusable");
        assert!(
            cfg.add("ghost", &a.join("nope")).is_err(),
            "a path that is not there cannot be served"
        );
        assert!(cfg.add("alpha", &a).is_ok());
        let _ = std::fs::remove_dir_all(&a);
    }

    #[test]
    fn adding_the_same_name_replaces_rather_than_duplicates() {
        // Two entries under one name would make `repo()` order-dependent, which
        // is exactly the ambiguity a closed set is supposed to remove.
        let a = temp_dir("cfg-dup-a");
        let b = temp_dir("cfg-dup-b");
        let mut cfg = DaemonConfig::default();
        cfg.add("alpha", &a).unwrap();
        cfg.add("alpha", &b).unwrap();

        assert_eq!(cfg.repos.len(), 1);
        assert_eq!(cfg.repo("alpha").unwrap().path, b.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn removing_forgets_a_repo() {
        let a = temp_dir("cfg-rm");
        let mut cfg = cfg_with(&[("alpha", &a)]);
        assert!(cfg.remove("alpha"));
        assert!(!cfg.remove("alpha"), "already gone");
        assert!(cfg.repo("alpha").is_none());
        let _ = std::fs::remove_dir_all(&a);
    }

    #[test]
    fn a_missing_config_is_an_empty_daemon_not_a_failure() {
        // A daemon nobody has configured should say "no repositories" rather than
        // refuse to start.
        let dir = temp_dir("cfg-missing");
        let cfg = load_from(&dir.join("daemon.json")).expect("missing is fine");
        assert!(cfg.repos.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_config_is_an_error_not_an_empty_set() {
        // Silently serving nothing looks identical to being configured with
        // nothing — the developer would be left wondering where their repos went.
        let dir = temp_dir("cfg-bad");
        let path = dir.join("daemon.json");
        std::fs::write(&path, "{ not json").unwrap();

        let err = load_from(&path).expect_err("malformed must be loud");
        assert!(err.to_string().contains("not valid JSON"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_round_trips_through_disk() {
        let dir = temp_dir("cfg-roundtrip");
        let repo = temp_dir("cfg-roundtrip-repo");
        let path = dir.join("daemon.json");

        let cfg = cfg_with(&[("alpha", &repo)]);
        save_to(&path, &cfg).unwrap();
        assert_eq!(load_from(&path).unwrap(), cfg);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn the_daemon_home_is_not_inside_any_repository() {
        // The queue must not live in a served repo: that repo would become
        // special, and the queue would get committed.
        let home = home();
        assert!(home.ends_with(".smart-coder"), "{}", home.display());
    }
}
