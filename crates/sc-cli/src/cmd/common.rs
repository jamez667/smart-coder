//! Plumbing every subcommand needs: resolving the workspace, and opening the
//! session log.

/// Resolve the current directory as the workspace, printing the error and
/// yielding `None` when it can't be read. Every subcommand starts with this.
pub fn workspace() -> Option<std::path::PathBuf> {
    match std::env::current_dir() {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            None
        }
    }
}

/// Open (create/truncate) a session log file, creating the parent dir. Returns
/// `None` on failure (logging is best-effort — never break a run over it).
pub fn open_log(path: &std::path::Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("warning: cannot create log dir {}: {e}", parent.display());
            return None;
        }
    }
    match std::fs::File::create(path) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("warning: cannot open log {}: {e}", path.display());
            None
        }
    }
}
