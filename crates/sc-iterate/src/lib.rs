//! The **iterate flavor**: the pure logic behind smart-coder's in-place "Iterate"
//! run mode (the daily driver — read the relevant files, edit them, run the verify
//! command until green, finish), lifted out of the desktop GUI so the remote server
//! can drive the exact same behavior.
//!
//! This crate has **no UI dependencies** (no iced) and does not depend on `sc-win`
//! or `sc-web` — both of those depend on *it*. It is a leaf over the core crates.
//!
//! What lives here:
//! - [`iterate_instruction`] — frame the user's change as an in-place edit + a repo overview.
//! - [`iterate_verify_command`] — pick a language-appropriate verify gate (Cargo/npm/Unity).
//! - [`apply_iterate_overrides`] — the `AgentConfig` tweaks the iterate mode needs.
//! - [`git_dirty_files`] / [`git_revert_files`] — the safe-revert bookkeeping.
//! - [`is_comment_only_change`] / [`files_diff`] — accept a comment-only edit without a green verify.
//! - [`finish_summary`] — the accept-or-revert decision + the human closing line.
//!
//! The desktop's `run_iterate` (sc-win) and the remote `serve_iterate` (sc-web) both
//! call these, so the two front-ends stay behavior-identical.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use sc_core::{AgentConfig, AgentReport};

mod proc;
mod verify;

pub use verify::iterate_verify_command;

/// The instruction for an iterate run: the user's change, framed as an in-place edit
/// of an existing project, with an overview of the files present so the agent edits
/// them rather than recreating from scratch.
pub fn iterate_instruction(task: &str, workspace: &Path) -> String {
    let overview = repo_overview(workspace);
    let overview_block = if overview.is_empty() {
        String::new()
    } else {
        format!("\n\n{overview}")
    };
    format!(
        "You are editing an EXISTING project in place. Make this change:\n\n{task}\n\n\
         Work by reading the files that are relevant to the change, then editing them with \
         edit_file (or write_file for a new file). Do NOT recreate the project from scratch \
         and do NOT rewrite unrelated files. When you believe the change is complete, use \
         run_verification to confirm it still compiles/passes; keep editing until it is \
         green, then finish.{overview_block}"
    )
}

/// Apply the `AgentConfig` overrides the iterate mode needs on top of the caller's
/// base config: no planning ceremony, host sandbox, no frozen paths, a language-aware
/// verify command, a raised step floor, and live streaming. The caller supplies the
/// confirmer/cancel/backend; this only sets the iterate-specific knobs.
pub fn apply_iterate_overrides(
    cfg: &mut AgentConfig,
    configured_verify: &Option<String>,
    workspace: &Path,
) {
    cfg.plan_first = false;
    cfg.sandbox = sc_verify::Sandbox::Host;
    cfg.permission.frozen_paths.clear();
    cfg.verify_command = iterate_verify_command(configured_verify, workspace);
    // A real cross-file change (add an enum variant, then fix every exhaustive match on it) can
    // legitimately need many turns: locate each site, edit, verify, repeat. 40 left the model
    // budget-exhausted mid-change on a 3-site edit (observed live on void-claim's ShipRole). 70
    // gives room to finish a multi-site change without inviting unbounded thrash.
    cfg.max_steps = cfg.max_steps.max(70);
    cfg.stream = true;
}

/// The set of files with uncommitted changes in `workspace` (workspace-relative,
/// `/`-separated), per `git status --porcelain`. Empty if the tree is clean or this
/// isn't a git repo. Captured at run start so we know which files we must NOT
/// auto-revert (reverting a file that was already dirty would destroy the user's work).
pub fn git_dirty_files(workspace: &Path) -> BTreeSet<String> {
    let out = git(workspace)
        .arg("status")
        .arg("--porcelain")
        .output();
    let mut set = BTreeSet::new();
    if let Ok(o) = out {
        if o.status.success() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                // Porcelain: "XY <path>" (path starts at column 3); handle rename "-> ".
                let path = line.get(3..).unwrap_or("").trim();
                let path = path.rsplit(" -> ").next().unwrap_or(path);
                if !path.is_empty() {
                    set.insert(path.trim_matches('"').replace('\\', "/"));
                }
            }
        }
    }
    set
}

/// Revert `files` (workspace-relative) to their committed state via `git checkout --`.
/// Returns true if git ran and reverted; false if this isn't a git repo or it failed.
/// No-op (true) for an empty list.
pub fn git_revert_files(workspace: &Path, files: &[String]) -> bool {
    if files.is_empty() {
        return true;
    }
    match git(workspace)
        .arg("checkout")
        .arg("--")
        .args(files)
        .output()
    {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

/// `git diff --no-color` over `files` in `workspace`. Empty if the list is empty, git
/// isn't present, or nothing changed.
pub fn files_diff(workspace: &Path, files: &[String]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let out = git(workspace)
        .args(["diff", "--no-color", "--"])
        .args(files)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(),
    }
}

/// Whether a unified diff touches only comments / blank lines (so it can't break the
/// build and is safe to accept without a green verify).
pub fn is_comment_only_change(diff: &str) -> bool {
    let mut saw_content = false;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
            continue;
        }
        let content = match line.as_bytes().first() {
            Some(b'+') | Some(b'-') => &line[1..],
            _ => continue,
        };
        saw_content = true;
        if !is_comment_or_blank(content) {
            return false;
        }
    }
    saw_content
}

fn is_comment_or_blank(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*')
}

/// The outcome of an iterate run: whether to accept the change, and the human line.
pub struct IterateOutcome {
    pub ok: bool,
    pub summary: String,
}

/// Decide whether to accept the run's edits or revert them, and produce the closing
/// line — the exact logic the desktop uses. `report` is the agent's result; `touched`
/// is every file the agent wrote (workspace-relative, `/`-separated); `dirty_at_start`
/// is the pre-run dirty set (never auto-reverted). On a not-green, non-comment-only
/// result the clean-at-start touched files are `git checkout`-reverted.
pub fn finish_summary(
    report: &AgentReport,
    touched: &[String],
    dirty_at_start: &BTreeSet<String>,
    workspace: &Path,
) -> IterateOutcome {
    let clean_touched: Vec<String> = touched
        .iter()
        .filter(|f| !dirty_at_start.contains(*f))
        .cloned()
        .collect();
    let comment_only =
        !clean_touched.is_empty() && is_comment_only_change(&files_diff(workspace, &clean_touched));

    let verified_ok = report.verified != Some(false);
    let ok = report.finished && (verified_ok || comment_only);
    if ok {
        let summary = match (report.verified, comment_only) {
            (Some(true), _) => format!(
                "done — verify green in {} steps ({} file(s) changed)",
                report.steps,
                touched.len()
            ),
            (_, true) => format!(
                "done — comment-only change, skipped compile check ({} file(s))",
                touched.len()
            ),
            _ => format!("done in {} steps", report.steps),
        };
        return IterateOutcome { ok, summary };
    }

    // Failure → revert the agent's mess, but only files that were CLEAN before the run.
    let (safe, unsafe_dirty): (Vec<String>, Vec<String>) = touched
        .iter()
        .cloned()
        .partition(|f| !dirty_at_start.contains(f));
    let reverted = git_revert_files(workspace, &safe);
    let base = match (report.finished, report.verified) {
        (true, Some(false)) => "stopped — the change didn't compile".to_string(),
        _ => format!("stopped after {} steps without a clean result", report.steps),
    };
    let revert_note = if !reverted && !safe.is_empty() {
        format!(
            " ⚠ couldn't auto-revert (not a git repo?) — check: {}.",
            safe.join(", ")
        )
    } else if !safe.is_empty() {
        format!(" Reverted {} file(s) to committed state.", safe.len())
    } else {
        " Your files are unchanged.".to_string()
    };
    let dirty_note = if unsafe_dirty.is_empty() {
        String::new()
    } else {
        format!(
            " ⚠ {} file(s) had uncommitted changes and were NOT auto-reverted \
             (to protect your work) — please review: {}.",
            unsafe_dirty.len(),
            unsafe_dirty.join(", ")
        )
    };
    IterateOutcome {
        ok: false,
        summary: format!("{base}.{revert_note}{dirty_note}"),
    }
}

/// A repo-relative file overview (bounded), so the agent edits existing files rather
/// than recreating the project. Skips VCS/build/generated noise.
pub fn repo_overview(workspace: &Path) -> String {
    const MAX_FILES: usize = 200;
    let mut files: Vec<(String, u64)> = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if is_noise_dir(name) {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    let rel = path
                        .strip_prefix(workspace)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    files.push((rel, size));
                }
                _ => {}
            }
        }
    }
    if files.is_empty() {
        return String::new();
    }
    files.sort();
    let mut out = String::from("Existing files (edit these in place where the task applies):\n");
    for (rel, size) in files.iter().take(MAX_FILES) {
        out.push_str(&format!("  {rel} ({size} bytes)\n"));
    }
    if files.len() > MAX_FILES {
        out.push_str(&format!("  … and {} more\n", files.len() - MAX_FILES));
    }
    out
}

/// Directories excluded from the repo overview (VCS/build/generated noise).
pub fn is_noise_dir(name: &str) -> bool {
    matches!(
        name,
        "target"
            | ".git"
            | "node_modules"
            | "__pycache__"
            | ".smart-coder"
            | ".pytest_cache"
            | "screenshots"
            | "dist"
            | "build"
            | "Library"
            | "Temp"
            | "obj"
            | "Logs"
            | "UserSettings"
            | "Builds"
    ) || name.starts_with('.') && name != "."
}

/// A windowless `git -C <workspace>` command.
fn git(workspace: &Path) -> Command {
    let mut c = proc::command("git");
    c.arg("-C").arg(workspace);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_only_detects_pure_comment_diffs() {
        let diff = "+++ b/x\n@@ -1 +1,2 @@\n context\n+// a new comment\n+   \n";
        assert!(is_comment_only_change(diff));
    }

    #[test]
    fn comment_only_rejects_code_changes() {
        let diff = "+++ b/x\n@@ -1 +1 @@\n-let x = 1;\n+let x = 2;\n";
        assert!(!is_comment_only_change(diff));
    }

    #[test]
    fn comment_only_false_when_no_content_lines() {
        assert!(!is_comment_only_change("+++ b/x\n@@ -0,0 +0,0 @@\n"));
    }

    #[test]
    fn instruction_frames_in_place_edit() {
        let ins = iterate_instruction("add a flag", Path::new("/nonexistent-xyz"));
        assert!(ins.contains("EXISTING project in place"));
        assert!(ins.contains("add a flag"));
        assert!(ins.contains("run_verification"));
    }

    #[test]
    fn noise_dirs_excluded() {
        assert!(is_noise_dir("target"));
        assert!(is_noise_dir(".git"));
        assert!(is_noise_dir(".hidden"));
        assert!(!is_noise_dir("src"));
        assert!(!is_noise_dir("."));
    }
}
