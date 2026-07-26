//! What a sequential build produced, and the budgets/registry its steps run under.

use std::path::Path;

use sc_core::{default_registry, AgentReport, ToolRegistry};

/// What a sequential build did, for reporting/inspection.
pub struct SequentialReport {
    /// The decomposition board, rendered (for logs).
    pub board_rendered: String,
    /// True when the board was degenerate (empty / single file-less subtask) and we fell
    /// back to the whole-task behavior instead of a per-file walk.
    pub fell_back_whole_task: bool,
    /// Per-file step outcomes, in execution order: (subtask id, its agent report).
    pub per_file: Vec<(String, AgentReport)>,
    /// Incremental integration steps in order: (label e.g. "slice:author or book", report).
    /// Empty when slicing wasn't applicable (single-file app / no keyworded tests → the single
    /// full pass below was used instead).
    pub incremental: Vec<(String, AgentReport)>,
    /// The final whole-suite integration pass (the one place cross-file glue is fixed).
    pub final_pass: AgentReport,
    /// Whether the final whole-suite verification was green.
    pub verified: bool,
}

/// Per-file step budget — TINY: the step's only job is to write one file, which a capable
/// model does in turn 1. With a verify-less registry (no run_verification to dead-end on),
/// it then calls `finish`. A small cap keeps a confused step from burning budget if it
/// doesn't finish promptly — the file is already written, so we move on.
pub(super) const PER_FILE_MAX_STEPS: usize = 5;

/// The integration pass gets the lion's share of the budget: it's the convergence loop that
/// must run the suite, read failures, and fix cross-file glue until green.
pub(super) const INTEGRATION_MAX_STEPS: usize = 60;

/// The per-file registry: write/edit/finish, but deliberately NO `run_verification` or
/// `run_command`. Per-file steps run with `verify_command = None`, so a `run_verification`
/// call would return "no verification configured" and the model would dead-end on it instead
/// of finishing (observed live: every per-file step wrote its file in turn 1, then stalled
/// ~15 turns calling run_verification/run_command). Removing the tool removes the trap —
/// after writing the file the only sensible move left is `finish`.
pub(super) fn per_file_registry() -> ToolRegistry {
    let specs = default_registry()
        .specs()
        .iter()
        .filter(|s| s.name != "run_verification" && s.name != "run_command")
        .cloned()
        .collect();
    ToolRegistry::new(specs)
}

/// Read the frozen test contract the per-file steps must satisfy — the asserts that pin what
/// each file must return / which status codes / which routes. WITHOUT this a per-file step
/// only gets a vague decomposition goal ("implement save and resolve") and writes STUBS
/// (`def save(url): pass`), which the integration pass can't rescue (observed live via a
/// prompt dump). Prefer the explicit `frozen_paths` (the human/workflow-approved contract
/// tests); fall back to a shallow `test_*.py` glob of the workspace root. "" if none found.
pub(super) fn read_frozen_contract(workspace: &Path, frozen_paths: &[String]) -> String {
    let mut parts: Vec<String> = frozen_paths
        .iter()
        .filter_map(|rel| std::fs::read_to_string(workspace.join(rel)).ok())
        .collect();
    if parts.is_empty() {
        if let Ok(entries) = std::fs::read_dir(workspace) {
            let mut hits: Vec<(String, String)> = Vec::new();
            for e in entries.flatten() {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                let Some(n) = p.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if n.starts_with("test_") && n.ends_with(".py") {
                    if let Ok(s) = std::fs::read_to_string(&p) {
                        hits.push((n.to_string(), s));
                    }
                }
            }
            hits.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic order
            parts = hits.into_iter().map(|(_, s)| s).collect();
        }
    }
    parts.join("\n\n")
}
