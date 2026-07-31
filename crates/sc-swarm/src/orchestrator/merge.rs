//! Turning a worker's *text* proposal into files on disk, gated by verification
//! (spec 08 — parallel reasoning, serialized & reviewed writes).
//!
//! The tiny worker hands back its fix as prose or a rewritten file; the smarter
//! orchestrator does the mechanical exactness the worker is bad at. Everything
//! here is reversible: the pre-merge snapshot is what lets a rejected merge be
//! reverted, and it doubles as the "before" side of the integrated diff that
//! post-integration review (spec 16) reads.

use std::path::Path;

use sc_model::ModelBackend;

use super::scope::{badness, is_frozen};
use super::{Integration, SwarmConfig};
use crate::worker::{ProposedChange, WorkerResult};

/// Merge a worker's *text* proposal into the real workspace, then verify (spec 08
/// — parallel reasoning, serialized & reviewed writes).
///
/// The tiny worker handed back its fix as text; the smarter `orchestrator` turns
/// that into the actual file. For each focused file it asks the orchestrator to
/// produce the complete corrected file (reviewing the worker's proposal against
/// the real current contents), writes it, then runs the whole suite. A merge that
/// breaks the suite is reverted and rejected — the mainline stays coherent.
pub(super) fn integrate(
    orchestrator: &dyn ModelBackend,
    workspace: &Path,
    result: &WorkerResult,
    cfg: &SwarmConfig,
) -> Integration {
    if !result.has_proposal() {
        return Integration::Rejected("no proposal from worker".to_string());
    }
    if result.files.is_empty() {
        return Integration::Rejected("proposal has no target file".to_string());
    }
    // A subtask that targets ONLY frozen contract tests has nothing to do — workers
    // make the tests pass, they don't rewrite them (spec 11).
    if result.files.iter().all(|f| is_frozen(f, &cfg.frozen_paths)) {
        return Integration::Rejected("subtask targets only frozen contract tests".to_string());
    }

    // Ask the orchestrator to turn the proposal into the corrected file(s). Frozen
    // contract tests are skipped — the merge may never overwrite them.
    let mut changes = Vec::new();
    for file in &result.files {
        if is_frozen(file, &cfg.frozen_paths) {
            continue;
        }
        let current = std::fs::read_to_string(workspace.join(file))
            .unwrap_or_default()
            .replace("\r\n", "\n");
        match merge_file(orchestrator, file, &current, &result.proposal) {
            Some(merged) if merged != current => changes.push(ProposedChange {
                path: file.clone(),
                after: Some(merged),
            }),
            _ => {}
        }
    }
    if changes.is_empty() {
        return Integration::Rejected("orchestrator produced no change".to_string());
    }

    // Snapshot the files we're about to touch so we can revert on rejection.
    let backup: Vec<(String, Option<String>)> = changes
        .iter()
        .map(|c| {
            let p = workspace.join(&c.path);
            (c.path.clone(), std::fs::read_to_string(&p).ok())
        })
        .collect();

    // No verify command: nothing to gate on, just apply.
    let Some(cmd) = &cfg.verify_command else {
        apply_changes(workspace, &changes);
        return Integration::Accepted(
            changes.iter().map(|c| c.path.clone()).collect(),
            integrated_diff(&backup, &changes),
        );
    };

    // Baseline failure count BEFORE applying, so a multi-file task can land its
    // pieces cumulatively. A subtask that fixes only its own file leaves the whole
    // suite red (other files still broken) — but it must not be reverted for that.
    // The gate is "didn't make things worse": accept if the failing-test count goes
    // down or stays equal; reject only a change that increases failures. The run is
    // "done" only when every subtask lands and the board is all-done — by which
    // point, for genuine fixes, the suite is actually green.
    let before = badness(&sc_verify::run_verification_in(
        &cfg.sandbox,
        workspace,
        cmd,
    ));
    apply_changes(workspace, &changes);
    let after = badness(&sc_verify::run_verification_in(
        &cfg.sandbox,
        workspace,
        cmd,
    ));

    if after <= before {
        Integration::Accepted(
            changes.iter().map(|c| c.path.clone()).collect(),
            integrated_diff(&backup, &changes),
        )
    } else {
        revert(workspace, &backup);
        Integration::Rejected(format!(
            "broke the suite at integration ({before} -> {after} failing)"
        ))
    }
}

/// The diff that actually landed, from the pre-merge snapshot and the applied
/// changes — the two things `integrate` already holds.
///
/// Built here rather than by re-reading the workspace afterwards, because by then
/// a *later* subtask's merge may have touched the same file and the "integrated
/// diff" would silently include someone else's work.
pub(super) fn integrated_diff(
    backup: &[(String, Option<String>)],
    changes: &[ProposedChange],
) -> sc_review::IntegratedDiff {
    sc_review::IntegratedDiff::from_changes(changes.iter().map(|c| {
        let before = backup
            .iter()
            .find(|(path, _)| path == &c.path)
            .and_then(|(_, content)| content.as_deref());
        (c.path.as_str(), before, c.after.as_deref())
    }))
}

/// Ask the orchestrator to apply `proposal` to `current`, returning the complete
/// corrected file. A single call (the fastest merge) — the capable model handles
/// the exact reproduction the tiny worker couldn't. `None` if it errored.
pub(super) fn merge_file(
    orchestrator: &dyn ModelBackend,
    path: &str,
    current: &str,
    proposal: &str,
) -> Option<String> {
    use sc_model::{GenerateRequest, Message};
    // `/no_think` for the same reason as the proposer (worker.rs): a Qwen3-class
    // orchestrator otherwise writes its reasoning into the merged file. Merge only
    // ever wants the final file bytes.
    let system = "You apply a proposed fix to a file. You are given the CURRENT file \
        and a worker's proposed corrected version. Output the complete, final file \
        contents only — no markdown fences, no commentary. Keep everything the fix \
        doesn't change; apply the fix exactly. /no_think";
    let user = format!(
        "File: {path}\n\n--- CURRENT ---\n{current}\n\n--- PROPOSED FIX ---\n{proposal}\n\n\
         Output the complete corrected {path}:"
    );
    let req = GenerateRequest::new(vec![Message::system(system), Message::user(user)]);
    let raw = orchestrator.generate(&req).ok()?.content;
    Some(unfence(&raw))
}

/// Strip a surrounding ``` fence (optional language tag) the model may add, then
/// ensure exactly one trailing newline (normal for a source file). Without a fence
/// the body is preserved as-is (aside from the trailing newline).
pub(super) fn unfence(s: &str) -> String {
    let trimmed = s.trim_start();
    let body = if let Some(rest) = trimmed.strip_prefix("```") {
        // Drop the ``` (or ```lang) line and a trailing ``` fence.
        let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        rest.trim_end()
            .strip_suffix("```")
            .unwrap_or(rest)
            .trim_end()
            .to_string()
    } else {
        s.trim_end().to_string()
    };
    format!("{body}\n")
}

pub(super) fn apply_changes(workspace: &Path, changes: &[ProposedChange]) {
    for c in changes {
        let p = workspace.join(&c.path);
        match &c.after {
            Some(content) => {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&p, content);
            }
            None => {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

pub(super) fn revert(workspace: &Path, backup: &[(String, Option<String>)]) {
    for (rel, content) in backup {
        let p = workspace.join(rel);
        match content {
            Some(c) => {
                let _ = std::fs::write(&p, c);
            }
            None => {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}
