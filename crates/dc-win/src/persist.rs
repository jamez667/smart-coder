//! Tiny cross-session state for the GUI: remember the last project folder the user
//! opened, so it re-opens on the next launch. Dependency-free (just `serde_json`, already
//! a dep) — a single small JSON file under the OS config dir.
//!
//! The path resolution and (de)serialization are pure/host-testable; the app calls
//! [`load`] at startup and [`save`] when the picked project changes.

use std::path::{Path, PathBuf};

/// The persisted UI state. Kept deliberately small — just what's worth surviving a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiState {
    /// The last project folder the user opened (the one that makes runs iterate in place),
    /// or `None` if they never opened one / cleared it.
    pub last_project: Option<PathBuf>,
}

/// The directory the state file lives in: `%APPDATA%\dumb-coder` on Windows (always set),
/// falling back to the system temp dir so we never fail to have *somewhere*.
fn state_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("dumb-coder")
}

/// The full path to the state file.
fn state_file() -> PathBuf {
    state_dir().join("dc-win-state.json")
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
    serde_json::json!({ "last_project": last }).to_string()
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
    UiState { last_project }
}

/// Convenience: does `p` look like a directory we can open? (Used by callers before
/// adopting a remembered path.)
pub fn is_openable(p: &Path) -> bool {
    p.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_project_path() {
        let state = UiState {
            last_project: Some(PathBuf::from(r"C:\Users\x\game")),
        };
        let json = serialize(&state);
        let back = parse(&json);
        assert_eq!(back, state);
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
