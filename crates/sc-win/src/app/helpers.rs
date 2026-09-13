//! Small standalone helper functions shared across the app modules.

/// Find a README in `dir`, case-insensitively (`README.md`, `readme.md`, `Readme.md`, or a
/// bare `README`). Returns the first match, or `None`.
pub(crate) fn find_readme(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let lower = name.to_ascii_lowercase();
            if lower == "readme.md" || lower == "readme" || lower == "readme.txt" {
                return Some(path);
            }
        }
    }
    None
}

/// Find a dedicated TODO file in `dir`, case-insensitively (`TODO.md`, `todo.md`, `TODO`,
/// `TODO.txt`). Returns the first match, or `None`.
pub(crate) fn find_todo_file(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let lower = name.to_ascii_lowercase();
            if lower == "todo.md" || lower == "todo" || lower == "todo.txt" {
                return Some(path);
            }
        }
    }
    None
}

/// A computed snapshot of the workspace's tree + git state. Produced by [`compute_snapshot`]
/// (the expensive filesystem walk + git subprocess calls) so that work can run OFF the UI thread;
/// [`App::apply_snapshot`] then applies it with cheap assignments.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceSnapshot {
    pub(crate) tree: Vec<sc_win::filetree::TreeRow>,
    pub(crate) file_status: std::collections::BTreeMap<String, sc_win::gitdiff::FileStatus>,
    pub(crate) stage_states: std::collections::BTreeMap<String, sc_win::gitdiff::StageState>,
    pub(crate) unstaged_deltas: std::collections::BTreeMap<String, sc_win::gitdiff::LineDelta>,
    pub(crate) staged_deltas: std::collections::BTreeMap<String, sc_win::gitdiff::LineDelta>,
    pub(crate) branch: Option<String>,
    pub(crate) upstream: sc_win::gitdiff::UpstreamStatus,
}

/// Compute the full workspace snapshot: walk the tree and run the git status/diff/branch queries.
/// This is the BLOCKING work (filesystem + `git` subprocesses); it takes no `&self` so it can run
/// on a background thread (see `Message::SyncWorkspace`). Pure — reads the workspace, mutates
/// nothing.
pub(crate) fn compute_snapshot(root: std::path::PathBuf) -> WorkspaceSnapshot {
    let tree = sc_win::filetree::full_rows(&root);
    // ONE `git status`, both views. These were two calls running the identical command
    // and parsing it differently -- ~26ms of pure process-spawn waste every 500ms on a
    // machine where spawning git at all costs that much.
    let (file_status, stage_states) = sc_win::gitdiff::status_both(&root);
    let mut unstaged_deltas = sc_win::gitdiff::line_deltas(&root, false);
    let staged_deltas = sc_win::gitdiff::line_deltas(&root, true);
    // Untracked files don't show in `git diff --numstat`; count their lines directly as all-added
    // so the Changes row still shows a +N.
    for (path, status) in &file_status {
        if *status == sc_win::gitdiff::FileStatus::Added && !unstaged_deltas.contains_key(path) {
            if let Ok(text) = std::fs::read_to_string(root.join(path)) {
                unstaged_deltas.insert(
                    path.clone(),
                    sc_win::gitdiff::LineDelta {
                        added: text.lines().count(),
                        removed: 0,
                    },
                );
            }
        }
    }
    let branch = sc_win::gitdiff::current_branch(&root);
    let upstream = sc_win::gitdiff::upstream_status(&root);
    WorkspaceSnapshot {
        tree,
        file_status,
        stage_states,
        unstaged_deltas,
        staged_deltas,
        branch,
        upstream,
    }
}
