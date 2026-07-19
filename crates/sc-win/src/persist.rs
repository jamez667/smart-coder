//! Tiny cross-session state for the GUI: remember the last project folder the user
//! opened, so it re-opens on the next launch. Dependency-free (just `serde_json`, already
//! a dep) — a single small JSON file under the OS config dir.
//!
//! The path resolution and (de)serialization are pure/host-testable; the app calls
//! [`load`] at startup and [`save`] when the picked project changes.

use std::path::{Path, PathBuf};

/// How many recent projects to remember (most-recent first).
const MAX_RECENTS: usize = 12;

/// The persisted UI state. Kept deliberately small — just what's worth surviving a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiState {
    /// The last project folder the user opened (the one that makes runs iterate in place),
    /// or `None` if they never opened one / cleared it.
    pub last_project: Option<PathBuf>,
    /// Recently-opened project folders, most-recent first (deduped, capped, existing dirs
    /// only). Drives the remote project picker and any future "recent projects" menu.
    pub recents: Vec<PathBuf>,
}

impl UiState {
    /// Promote `path` to the front of the recents list (dedup, cap) and set it as the last
    /// project. Call whenever a project is opened.
    pub fn record_project(&mut self, path: &Path) {
        let path = path.to_path_buf();
        self.recents.retain(|p| p != &path);
        self.recents.insert(0, path.clone());
        self.recents.truncate(MAX_RECENTS);
        self.last_project = Some(path);
    }
}

/// The directory the state file lives in: `%APPDATA%\smart-coder` on Windows (always set),
/// falling back to the system temp dir so we never fail to have *somewhere*.
fn state_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("smart-coder")
}

/// The full path to the state file.
fn state_file() -> PathBuf {
    state_dir().join("sc-win-state.json")
}

/// Load persisted state, or a default (empty) state if none exists / it's unreadable. A
/// last-project path that no longer exists on disk is dropped, so a deleted/renamed folder
/// doesn't leave the app pointing at nothing.
pub fn load() -> UiState {
    let path = state_file();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return UiState::default();
    };
    let mut state = parse(&text);
    if let Some(p) = &state.last_project {
        if !p.is_dir() {
            state.last_project = None;
        }
    }
    // Drop recents whose folder no longer exists (renamed/deleted), so the picker only
    // ever offers openable projects.
    state.recents.retain(|p| p.is_dir());
    state
}

/// Persist `state` (best-effort — a write failure is silently ignored; losing the
/// remembered folder is not worth interrupting the user).
pub fn save(state: &UiState) {
    let _ = std::fs::create_dir_all(state_dir());
    let _ = std::fs::write(state_file(), serialize(state));
}

/// Serialize state to JSON. Manual (one field) to avoid deriving Serialize across a
/// `PathBuf` — keeps this tiny and explicit.
fn serialize(state: &UiState) -> String {
    let last = state
        .last_project
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let recents: Vec<String> = state
        .recents
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    serde_json::json!({ "last_project": last, "recents": recents }).to_string()
}

/// Parse the JSON produced by [`serialize`]. A missing/blank/`null` field ⇒ no project.
fn parse(text: &str) -> UiState {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return UiState::default(),
    };
    let last_project = v
        .get("last_project")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let recents = v
        .get("recents")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    UiState {
        last_project,
        recents,
    }
}

/// Convenience: does `p` look like a directory we can open? (Used by callers before
/// adopting a remembered path.)
pub fn is_openable(p: &Path) -> bool {
    p.is_dir()
}

// --- Remote-session history --------------------------------------------------------------
// Each remote-mirror launch appends one JSON line here: the connection URL (token included),
// port, PID, and a unix timestamp. A session is "active" if its PID is still alive. This lets
// the user find the CURRENT url (the token rotates per launch) and see recent ones.

/// The history file: one JSON object per line (JSONL).
fn sessions_file() -> PathBuf {
    state_dir().join("remote-sessions.jsonl")
}

/// One recorded remote-mirror session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSession {
    pub url: String,
    pub port: u16,
    pub pid: u32,
    /// Unix seconds when the session started.
    pub started: u64,
}

/// Append a session record (best-effort — a write failure is ignored).
pub fn record_session(url: &str, port: u16, pid: u32, started: u64) {
    let _ = std::fs::create_dir_all(state_dir());
    let line = serde_json::json!({
        "url": url, "port": port, "pid": pid, "started": started,
    })
    .to_string();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sessions_file())
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Read all recorded sessions, most-recent first. Malformed lines are skipped.
pub fn load_sessions() -> Vec<RemoteSession> {
    let Ok(text) = std::fs::read_to_string(sessions_file()) else {
        return Vec::new();
    };
    let mut out: Vec<RemoteSession> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            Some(RemoteSession {
                url: v.get("url")?.as_str()?.to_string(),
                port: v.get("port")?.as_u64()? as u16,
                pid: v.get("pid")?.as_u64()? as u32,
                started: v.get("started")?.as_u64()?,
            })
        })
        .collect();
    out.reverse(); // newest first
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_project_path() {
        let state = UiState {
            last_project: Some(PathBuf::from(r"C:\Users\x\game")),
            recents: vec![PathBuf::from(r"C:\Users\x\game")],
        };
        let json = serialize(&state);
        let back = parse(&json);
        assert_eq!(back, state);
    }

    #[test]
    fn record_project_promotes_dedups_and_caps() {
        let mut s = UiState::default();
        s.record_project(Path::new("/a"));
        s.record_project(Path::new("/b"));
        s.record_project(Path::new("/a")); // re-open a → moves to front, no dup
        assert_eq!(
            s.recents,
            vec![PathBuf::from("/a"), PathBuf::from("/b")],
            "most-recent first, deduped"
        );
        assert_eq!(s.last_project, Some(PathBuf::from("/a")));
        // Cap holds.
        for i in 0..20 {
            s.record_project(Path::new(&format!("/p{i}")));
        }
        assert!(s.recents.len() <= MAX_RECENTS);
    }

    #[test]
    fn empty_or_null_yields_no_project() {
        assert_eq!(parse("{}"), UiState::default());
        assert_eq!(parse(r#"{"last_project": null}"#), UiState::default());
        assert_eq!(parse(r#"{"last_project": ""}"#), UiState::default());
        assert_eq!(parse("not json at all"), UiState::default());
    }

    #[test]
    fn serialize_omits_nothing_when_absent() {
        let json = serialize(&UiState::default());
        // Round-trips to the same empty state regardless of exact null encoding.
        assert_eq!(parse(&json), UiState::default());
    }
}
