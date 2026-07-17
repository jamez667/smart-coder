//! Shared test helpers for the `agent` submodule tests.

/// A unique temp workspace directory for a test, tagged for readability. The caller is
/// responsible for cleanup (`std::fs::remove_dir_all`).
pub(super) fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-core-agent-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}
