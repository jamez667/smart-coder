//! Persisted divider (split) positions, keyed by a stable id.
//!
//! Every resizable divider in the UI stores its position here by id, so it restores to wherever
//! the user left it on the next launch. This is the ONE place split positions live: add a divider,
//! give it an id, call [`SplitStore::get`]/[`SplitStore::set`] — no new fields, no bespoke
//! persistence per divider.
//!
//! Backed by a small JSON map (`%APPDATA%\smart-coder\splits.json`) next to the other state files.
//! Positions are fractions in `0.0..=1.0`. Kept separate from [`crate::persist::UiState`] because
//! that type derives `Eq` (it can't hold an `f32`), and a dedicated store reads as exactly what it
//! is: id → position.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Stable ids for the app's dividers. Using consts (not bare strings at the call sites) keeps the
/// persisted keys from drifting and documents every split in one list.
pub mod id {
    /// The main chat | code split (fraction = chat's share of the chat+code region).
    pub const CHAT_CODE: &str = "chat|code";
    /// The explorer column's git | files split (fraction = git's share of the column height).
    pub const EXPLORER_GIT_FILES: &str = "explorer:git|files";
}

/// An id → fraction map of divider positions, loaded once at startup and re-saved (best-effort)
/// whenever a divider drag settles.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SplitStore {
    positions: BTreeMap<String, f32>,
}

impl SplitStore {
    /// The saved fraction for `id`, or `default` if none is stored. `default` is also returned for
    /// a stored value that is NaN/out of range, so a corrupt entry can never wedge a divider.
    pub fn get(&self, id: &str, default: f32) -> f32 {
        match self.positions.get(id) {
            Some(f) if f.is_finite() && *f > 0.0 && *f < 1.0 => *f,
            _ => default,
        }
    }

    /// Record `frac` for `id` (in memory). Call [`save`](Self::save) to persist — the app does
    /// that when a drag ends, not on every mouse-move, so the disk write isn't spammed.
    pub fn set(&mut self, id: &str, frac: f32) {
        if frac.is_finite() {
            self.positions.insert(id.to_string(), frac);
        }
    }

    /// Load the saved positions, or an empty store if the file is missing/unreadable/corrupt.
    pub fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(splits_file()) else {
            return Self::default();
        };
        Self {
            positions: parse(&text),
        }
    }

    /// Persist the positions (best-effort — a write failure is silently ignored, like the other
    /// state files; a lost divider position is not worth interrupting the user).
    pub fn save(&self) {
        let _ = std::fs::create_dir_all(state_dir());
        let _ = std::fs::write(splits_file(), serialize(&self.positions));
    }
}

/// The state directory: `%APPDATA%\smart-coder` (temp-dir fallback), shared with the other state
/// files (see [`crate::persist`]).
fn state_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("smart-coder")
}

fn splits_file() -> PathBuf {
    state_dir().join("splits.json")
}

/// Serialize the id → fraction map to JSON (a flat object). Pure/host-testable.
fn serialize(positions: &BTreeMap<String, f32>) -> String {
    let obj: serde_json::Map<String, serde_json::Value> = positions
        .iter()
        .filter(|(_, f)| f.is_finite())
        .filter_map(|(k, f)| {
            serde_json::Number::from_f64(*f as f64)
                .map(|n| (k.clone(), serde_json::Value::Number(n)))
        })
        .collect();
    serde_json::Value::Object(obj).to_string()
}

/// Parse the JSON object back into the map, skipping non-numeric/garbage entries. A malformed
/// file yields an empty map (every divider then falls back to its default).
fn parse(text: &str) -> BTreeMap<String, f32> {
    let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(text) else {
        return BTreeMap::new();
    };
    obj.into_iter()
        .filter_map(|(k, v)| v.as_f64().map(|f| (k, f as f32)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_returns_default_when_absent_or_out_of_range() {
        let mut s = SplitStore::default();
        assert_eq!(s.get(id::CHAT_CODE, 0.5), 0.5, "absent → default");
        s.set(id::CHAT_CODE, 0.3);
        assert_eq!(s.get(id::CHAT_CODE, 0.5), 0.3, "stored value wins");
        // Out-of-range / non-finite stored values fall back to the default (never wedge a divider).
        s.set(id::CHAT_CODE, 1.5);
        assert_eq!(s.get(id::CHAT_CODE, 0.5), 0.5, "1.5 rejected");
        s.set(id::CHAT_CODE, f32::NAN);
        assert_eq!(s.get(id::CHAT_CODE, 0.5), 0.5, "NaN not stored");
    }

    #[test]
    fn round_trips_through_serialize_parse() {
        let mut s = SplitStore::default();
        s.set(id::CHAT_CODE, 0.62);
        s.set(id::EXPLORER_GIT_FILES, 0.33);
        let json = serialize(&s.positions);
        let back = parse(&json);
        assert_eq!(back.get(id::CHAT_CODE).copied(), Some(0.62));
        assert_eq!(back.get(id::EXPLORER_GIT_FILES).copied(), Some(0.33));
    }

    #[test]
    fn parse_tolerates_garbage() {
        assert!(parse("not json").is_empty());
        assert!(parse("[1,2,3]").is_empty());
        // A non-numeric value for a key is skipped, others survive.
        let m = parse(r#"{"chat|code":0.4,"bad":"x"}"#);
        assert_eq!(m.get("chat|code").copied(), Some(0.4));
        assert!(!m.contains_key("bad"));
    }
}
