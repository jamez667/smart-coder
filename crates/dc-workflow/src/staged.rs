//! Staged build for editing an EXISTING project (the "build" half of the Execute-plan flow).
//!
//! The plan-only workflow produces an ordered stage breakdown (`04-stage-breakdown.md`) — a
//! numbered list of small stages, each naming the file(s) it touches and what to build there.
//! A weak model can't implement a whole coupled feature in one flat agent loop (it rewrites
//! whole files, drops functions, thrashes). The decomposition is the point: this driver feeds
//! the model ONE stage at a time, scoped to that stage's file(s), keeping the project
//! compiling between stages — so each step is small enough for the model to actually land.
//!
//! Unlike [`crate::sequential`] (which CREATES one Python file per subtask against frozen
//! tests), this EDITS existing files in place and gates each stage on the project's own build
//! command (e.g. `cargo check`). No test-writing, no swarm — just decompose → scoped edit per
//! stage → verify → next.

use std::path::Path;

use dc_core::{default_registry, run_agent_observed, select_strategy, AgentConfig, EventSink};
use dc_model::ModelBackend;
use dc_proto::Result;

/// One stage of the build: a short title, the file(s) it edits, and what to do there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub title: String,
    /// Workspace-relative file(s) this stage edits (pinned in full for the step).
    pub files: Vec<String>,
    /// The prose instruction for the stage (from the breakdown).
    pub instruction: String,
}

/// The outcome of one stage's scoped agent run.
#[derive(Debug, Clone)]
pub struct StageResult {
    pub title: String,
    /// Whether the project's verify command was green after this stage.
    pub verified: bool,
    pub steps: usize,
    /// Whether the stage actually CHANGED its target file(s). A stage that finishes green
    /// without touching anything is a no-op — the build already compiled, so a bare
    /// verify-passes gate rubber-stamps it. Surfaced so the caller can flag/retry it.
    pub changed: bool,
}

/// What a staged build did.
#[derive(Debug, Clone)]
pub struct StagedReport {
    pub stages: Vec<StageResult>,
    /// Whether the final verify (after the last stage / oracle) was green.
    pub verified: bool,
    /// Whether the final BEHAVIORAL oracle passed (`None` if no oracle was configured). This is
    /// the real "does the feature work?" signal — distinct from the per-stage compile gate.
    pub oracle_passed: Option<bool>,
}

/// Max turns for one stage's scoped edit. A stage is small — add a method, a struct, a match
/// arm — so the budget is tight; a confused stage shouldn't burn the whole run.
const STAGE_MAX_STEPS: usize = 24;

/// Parse the plan-only stage breakdown (`04-stage-breakdown.md`) into ordered stages.
///
/// The format the orchestrator emits (non-Python stack) is a numbered list, each item:
/// ```text
/// 1. **Stage title**
///    - `path/to/file.rs`
///    - Description of what to build here.
/// ```
/// We tolerate variations: the file line may be a bullet with or without backticks; the
/// description is the remaining prose. A stage with no recognizable file still parses (its
/// `files` is empty) so the driver can fall back to an unscoped edit for it.
pub fn parse_stages(breakdown: &str) -> Vec<Stage> {
    let mut stages: Vec<Stage> = Vec::new();
    let mut cur: Option<Stage> = None;

    for raw in breakdown.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // A new numbered stage: "1. **Title**" or "1. Title".
        if let Some(title) = numbered_title(line) {
            if let Some(s) = cur.take() {
                stages.push(s);
            }
            cur = Some(Stage {
                title,
                files: Vec::new(),
                instruction: String::new(),
            });
            continue;
        }
        let Some(stage) = cur.as_mut() else {
            continue; // preamble before the first numbered item (e.g. the "# ... Stages" header)
        };
        // A sub-bullet: either a file path or a description sentence.
        let bullet = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .unwrap_or(line)
            .trim();
        if let Some(file) = looks_like_file(bullet) {
            stage.files.push(file);
        } else {
            if !stage.instruction.is_empty() {
                stage.instruction.push(' ');
            }
            stage.instruction.push_str(bullet);
        }
    }
    if let Some(s) = cur.take() {
        stages.push(s);
    }
    stages
}

/// If `line` opens a numbered stage (`3. **Foo**` / `3. Foo` / `3) Foo`), return its title
/// (markdown bold stripped). `None` otherwise.
fn numbered_title(line: &str) -> Option<String> {
    let mut chars = line.char_indices();
    // Require a leading run of ASCII digits.
    let mut end = 0;
    let mut saw_digit = false;
    for (i, c) in chars.by_ref() {
        if c.is_ascii_digit() {
            saw_digit = true;
            end = i + 1;
        } else {
            break;
        }
    }
    if !saw_digit {
        return None;
    }
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    let title = rest.trim().trim_matches('*').trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// If `bullet` looks like a source file path (has a code extension, no spaces after unwrapping
/// backticks), return the cleaned relative path. Recognizes the common breakdown convention of
/// naming the file on its own bullet, optionally in backticks.
fn looks_like_file(bullet: &str) -> Option<String> {
    let t = bullet.trim().trim_matches('`').trim();
    // A path token: no whitespace, contains a '.', ends in a known code extension.
    if t.is_empty() || t.contains(char::is_whitespace) {
        return None;
    }
    const EXTS: [&str; 8] = [".rs", ".py", ".js", ".ts", ".go", ".java", ".css", ".html"];
    if EXTS.iter().any(|e| t.to_ascii_lowercase().ends_with(e)) {
        Some(t.to_string())
    } else {
        None
    }
}

/// Drive a staged build: run each stage's scoped edit in order, gating every stage on
/// `verify_command` so the project keeps compiling. `base_cfg` supplies the backend posture
/// (permissions, sandbox, suffix); this overrides focus/verify/steps per stage. Every stage's
/// events flow to `sink` (prefix them by stage in the caller if desired). `on_stage` is called
/// as each stage starts, for progress display.
#[allow(clippy::too_many_arguments)]
pub fn staged_build(
    backend: &dyn ModelBackend,
    stages: &[Stage],
    workspace: &Path,
    verify_command: &str,
    oracle_command: Option<&str>,
    base_cfg: &AgentConfig,
    on_stage: &dyn Fn(usize, &Stage),
    sink: &dyn EventSink,
) -> Result<StagedReport> {
    let registry = default_registry();
    let strategy = select_strategy(&backend.capabilities());
    let mut results: Vec<StageResult> = Vec::new();
    let mut last_verified = false;
    // If the oracle passes partway through the stage list, the feature already WORKS — the
    // remaining stages can only churn already-correct code (observed live: stage 1 landed the
    // whole render change to satisfy the compiler, then stage 2 re-edited the same match and
    // broke a brace, burning its budget un-breaking its own damage). Short-circuit once green.
    let mut oracle_short_circuited = false;

    // The union of every file the feature touches across all stages — pinned in full each stage
    // so an INTEGRATION stage (edit terrain.rs to wire in the new lake.rs) can SEE the new
    // module's real API without burning its budget re-reading it (observed live: stage 2 spent
    // its whole budget re-reading lake.rs turn after turn because only terrain.rs was pinned).
    let feature_files: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        for s in stages {
            for f in &s.files {
                if !v.contains(f) {
                    v.push(f.clone());
                }
            }
        }
        v
    };

    for (i, stage) in stages.iter().enumerate() {
        on_stage(i, stage);
        let cfg = stage_cfg(base_cfg, verify_command, stage, &feature_files);

        // Snapshot the stage's target files up front. This is BOTH the no-op check (did the
        // stage change anything?) AND the edit-safety anchor: the pre-stage files compile (the
        // previous stage left them green), so if a stage ends with the build BROKEN we can
        // restore this known-good state and retry — instead of letting a stage that corrupted
        // the file poison every stage after it (observed live: the model breaks a string
        // literal, digs deeper trying to fix it, stalls, and the file is left uncompilable).
        let before = snapshot(workspace, &stage.files);
        let mut steps = 0usize;
        let mut changed = false;
        // Up to 3 attempts: retry on a no-op (nudge) OR on a build-breaking result (restore the
        // clean snapshot first so the retry starts from compiling code, not the mess).
        // Is this stage's target a LARGE existing file (edit surgically) vs. a new/small file
        // (create it whole)? Drives the instruction so the model doesn't try to add a big block
        // of logic into a 500+ line file — that's the coherence wall; new logic goes in a new file.
        let big_edit = stage.files.iter().any(|f| is_large_existing(workspace, f));
        let new_or_small = stage.files.iter().any(|f| !is_large_existing(workspace, f));
        for attempt in 0..3 {
            let instruction =
                stage_instruction(stage, i, stages.len(), attempt > 0, big_edit, new_or_small);
            let report = run_agent_observed(
                backend,
                None,
                &registry,
                strategy.as_ref(),
                &instruction,
                workspace,
                &cfg,
                sink,
            )?;
            steps += report.steps;
            last_verified = report.verified == Some(true);
            changed = snapshot(workspace, &stage.files) != before || stage.files.is_empty();

            if last_verified && changed {
                break; // real, compiling change — accept the stage
            }
            if attempt < 2 {
                // Not done. If the stage left the build broken, REVERT its target files to the
                // last-green snapshot so the next attempt edits compiling code (edit-safety).
                // A no-op (green but unchanged) needs no revert — just the firmer retry.
                if !last_verified {
                    restore(workspace, &before);
                    changed = false;
                }
            }
        }
        results.push(StageResult {
            title: stage.title.clone(),
            verified: last_verified,
            steps,
            changed,
        });

        // After a stage that compiled, check the behavioral oracle. If it already passes, the
        // feature works — stop before later stages can churn (and break) the code that just
        // satisfied it. The final convergence loop below is then a no-op confirm.
        if last_verified && i + 1 < stages.len() {
            if let Some(oracle) = oracle_command {
                let rep = dc_verify::run_verification_in(&base_cfg.sandbox, workspace, oracle);
                if rep.command_ok {
                    oracle_short_circuited = true;
                    break;
                }
            }
        }
    }

    // FINAL BEHAVIORAL ORACLE. The per-stage `cargo check` gate only proves the code compiles —
    // a model can (and did) satisfy it with stubs that generate/render nothing. If an oracle is
    // configured (a frozen behavioral test that a stub CANNOT pass), run a final, unfocused
    // convergence loop gated on it: the model sees the whole project and the oracle's failure
    // message, and must make the feature actually WORK, not just compile.
    let mut oracle_ok = None;
    if oracle_short_circuited {
        // A mid-list stage already turned the oracle green; the remaining stages were skipped.
        // No convergence loop needed — the feature works. Record the pass directly.
        oracle_ok = Some(true);
        last_verified = true;
    } else if let Some(oracle) = oracle_command {
        let mut cfg = base_cfg.clone();
        cfg.plan_first = false;
        // Pin the feature's files so the convergence loop edits them directly instead of wandering
        // the tree. Observed live: with focus cleared (whole-project), the model spent its whole
        // oracle budget on list_dir/read_file trying to REDISCOVER where render_sample lives —
        // reading water.rs/mod.rs/rivers.rs over and over, never editing — and stalled. The feature
        // files ARE where the fix goes; pinning them keeps them in the window so the model acts.
        cfg.focus_files = feature_files.clone();
        cfg.verify_command = Some(oracle.to_string());
        cfg.max_steps = ORACLE_MAX_STEPS;
        let report = run_agent_observed(
            backend,
            None,
            &registry,
            strategy.as_ref(),
            &oracle_instruction(),
            workspace,
            &cfg,
            sink,
        )?;
        oracle_ok = Some(report.verified == Some(true));
        last_verified = report.verified == Some(true);
    }

    Ok(StagedReport {
        stages: results,
        verified: last_verified,
        oracle_passed: oracle_ok,
    })
}

/// Budget for the final oracle-convergence loop — larger than a stage, since making a feature
/// truly behave (across the files a stub left empty) is the real work.
const ORACLE_MAX_STEPS: usize = 40;

/// The instruction for the final oracle pass: the feature must actually WORK, not just compile.
fn oracle_instruction() -> String {
    "The feature's code has been added across the project and it compiles, but a BEHAVIORAL test \
     (run_verification) is FAILING — which means the feature does not actually work yet (e.g. a \
     function was stubbed out, or the pieces aren't wired together so nothing actually runs). \
     Read the failing test's message CAREFULLY: it names the exact function whose output is wrong \
     (e.g. \"render_sample() does not know about them\"). That named function is what you must \
     edit — it is in the files already pinned in your context, so DO NOT go hunting through the \
     tree with list_dir; open the pinned file, find that function, and add the missing logic so it \
     returns/routes the value the test expects. Edit surgically (edit_file / edit_lines — do not \
     rewrite a large file whole). You may NOT edit the test/oracle file itself. Run \
     run_verification and keep going until it PASSES, then finish."
        .to_string()
}

/// The per-stage agent config: pins the whole feature's file set (so an integration stage sees
/// the new module it wires in without re-reading it), gated on the project build, tight budget.
/// The stage's own file is what it edits; the rest are read-only context. Order the stage's own
/// files first so they're the primary anchor.
fn stage_cfg(
    base: &AgentConfig,
    verify_command: &str,
    stage: &Stage,
    feature_files: &[String],
) -> AgentConfig {
    let mut cfg = base.clone();
    cfg.plan_first = false;
    // Stage's own files first, then the rest of the feature's files as context.
    let mut focus = stage.files.clone();
    for f in feature_files {
        if !focus.contains(f) {
            focus.push(f.clone());
        }
    }
    cfg.focus_files = focus;
    cfg.verify_command = Some(verify_command.to_string());
    cfg.max_steps = STAGE_MAX_STEPS;
    cfg
}

/// The per-stage instruction: do ONLY this stage, surgically, keep the project compiling.
/// The focused file(s) are already pinned in full by the agent loop (focus_files), so this
/// tells the model to edit them in place — never rewrite them wholesale.
fn stage_instruction(
    stage: &Stage,
    idx: usize,
    total: usize,
    firmer: bool,
    big_edit: bool,
    new_or_small: bool,
) -> String {
    let files = if stage.files.is_empty() {
        "the relevant existing file(s)".to_string()
    } else {
        stage.files.join(", ")
    };
    // On a retry (the first attempt changed nothing), lead with a blunt correction.
    let retry_note = if firmer {
        "YOUR PREVIOUS ATTEMPT CHANGED NOTHING. You must actually EDIT the file this time — add \
         real code with edit_file/write_file; do not just read and finish.\n\n"
    } else {
        ""
    };
    // Size-aware guidance: a NEW/small file is written WHOLE (the model can hold ~100 lines); a
    // LARGE existing file gets only a tiny surgical edit (a big block would exceed the model's
    // structural coherence and break the file). This is file-level decomposition — new logic
    // lives in a new module, the big file only gets the hook.
    let size_note = if big_edit && !new_or_small {
        "This stage's file is a LARGE existing file. Make ONLY A SMALL, surgical change — add a \
         module declaration, a struct field, a single method CALL, or a match arm (a handful of \
         lines). Do NOT add a large block of logic here; that logic belongs in the feature's own \
         new module. Use `edit_lines` (address by the line numbers shown) for the edit.\n\n"
    } else if new_or_small {
        "This stage's file is NEW or small. WRITE IT WHOLE in one shot with `write_file` (for a \
         new file) or a few `append_file`/`edit_lines` calls — put the feature's real logic here \
         so the large existing files only need tiny hooks.\n\n"
    } else {
        ""
    };
    format!(
        "{retry_note}{size_note}You are editing an EXISTING project in place. This is stage {} of {} of a larger \
         feature; do ONLY this stage, nothing more:\n\n\
         STAGE: {}\n{}\n\n\
         Edit {} with SURGICAL edit_file/edit_lines changes — add the methods/types/arms this \
         stage needs and wire them in. Do NOT rewrite whole large files (you will drop existing \
         code and break the build), and do NOT start other stages.\n\n\
         CRITICAL — USE WHAT THE EARLIER STAGES ACTUALLY BUILT. The stage description came from \
         an up-front design and may name a type/function that doesn't match what was really \
         created (e.g. it says integrate a `LakeGenerator` type, but an earlier stage's new \
         module actually exposes a `generate_lakes()` function and a `Lake` struct). ALL the \
         feature's files — including the new module an earlier stage created — are shown IN FULL \
         above; that is the source of truth. Call the functions / use the types that ACTUALLY \
         exist in those pinned files. Do NOT invent a `LakeGenerator` (or any symbol) just \
         because the design named it — implement the same INTENT with the real API that's there. \
         A `use` or reference to a non-existent symbol fails to compile and you will get stuck.\n\n\
         This stage must make a REAL change to {} — do not finish having changed nothing. When \
         your change is in and the project still compiles, run_verification; keep editing until \
         it is green, then finish. If verification shows compiler errors, read them and fix \
         exactly those.",
        idx + 1,
        total,
        stage.title,
        stage.instruction,
        files,
        files,
    )
}

/// Whether `rel` is a LARGE existing file — one a mid-size model can't reliably add a big block
/// of logic to (it loses track of the structure). Above this, a stage must make only a tiny hook
/// edit; the real logic goes in a new module. New/missing files are not "large" (created whole).
const LARGE_FILE_LINES: usize = 200;

fn is_large_existing(workspace: &Path, rel: &str) -> bool {
    std::fs::read_to_string(workspace.join(rel))
        .map(|s| s.lines().count() > LARGE_FILE_LINES)
        .unwrap_or(false)
}

/// A cheap content snapshot of `files` (workspace-relative), for detecting whether a stage
/// actually changed anything. Missing files read as empty. Order-stable.
fn snapshot(workspace: &Path, files: &[String]) -> Vec<(String, String)> {
    files
        .iter()
        .map(|f| {
            let body = std::fs::read_to_string(workspace.join(f)).unwrap_or_default();
            (f.clone(), body)
        })
        .collect()
}

/// Restore files to a prior [`snapshot`] — the edit-safety revert. Writes each recorded body
/// back, so a stage that broke the build leaves the target files exactly as they were before
/// it ran (compiling). Best-effort: a write error is ignored (the retry will surface it).
fn restore(workspace: &Path, snap: &[(String, String)]) {
    for (rel, body) in snap {
        let _ = std::fs::write(workspace.join(rel), body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BREAKDOWN: &str = "# Lakes Feature Implementation Stages\n\
        \n\
        1. **Add Lake Generation**\n\
        \x20  - `crates/city/src/gen/terrain.rs`\n\
        \x20  - Introduce a Lake struct and a generate_lakes function.\n\
        \n\
        2. **Wire Lakes into Rendering**\n\
        \x20  - `crates/city/src/render.rs`\n\
        \x20  - Add a lake arm to draw_land.\n";

    #[test]
    fn parses_ordered_stages_with_files_and_instructions() {
        let stages = parse_stages(BREAKDOWN);
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].title, "Add Lake Generation");
        assert_eq!(stages[0].files, vec!["crates/city/src/gen/terrain.rs"]);
        assert!(stages[0].instruction.contains("Lake struct"));
        assert_eq!(stages[1].title, "Wire Lakes into Rendering");
        assert_eq!(stages[1].files, vec!["crates/city/src/render.rs"]);
        assert!(stages[1].instruction.contains("draw_land"));
    }

    #[test]
    fn a_stage_with_no_file_still_parses_with_empty_files() {
        let stages = parse_stages("1. **Think hard**\n   - Just reason about it.\n");
        assert_eq!(stages.len(), 1);
        assert!(stages[0].files.is_empty());
        assert!(stages[0].instruction.contains("reason"));
    }

    #[test]
    fn looks_like_file_accepts_paths_rejects_prose() {
        assert_eq!(looks_like_file("`src/a.rs`"), Some("src/a.rs".to_string()));
        assert_eq!(looks_like_file("crates/city/src/render.rs"), Some("crates/city/src/render.rs".to_string()));
        assert_eq!(looks_like_file("Add a lake arm to draw_land."), None);
        assert_eq!(looks_like_file("just some prose"), None);
    }

    #[test]
    fn numbered_title_handles_bold_and_plain_and_parens() {
        assert_eq!(numbered_title("1. **Foo**").as_deref(), Some("Foo"));
        assert_eq!(numbered_title("12. Bar").as_deref(), Some("Bar"));
        assert_eq!(numbered_title("3) Baz").as_deref(), Some("Baz"));
        assert_eq!(numbered_title("- not numbered"), None);
        assert_eq!(numbered_title("prose"), None);
    }

    #[test]
    fn stage_instruction_scopes_to_the_stage_and_forbids_rewrite() {
        let s = Stage {
            title: "Add Lake struct".to_string(),
            files: vec!["terrain.rs".to_string()],
            instruction: "Introduce a Lake struct.".to_string(),
        };
        let i = stage_instruction(&s, 0, 3, false, false, false);
        assert!(i.contains("stage 1 of 3"));
        assert!(i.contains("Add Lake struct"));
        assert!(i.contains("terrain.rs"));
        assert!(i.to_lowercase().contains("do not rewrite whole large files"));
        assert!(i.to_lowercase().contains("only this stage"));
        // The adapt-to-real-code guard (use what earlier stages actually built).
        assert!(i.contains("EARLIER STAGES ACTUALLY BUILT"), "steers to the real API: {i}");
    }

    #[test]
    fn firmer_retry_leads_with_a_no_op_correction() {
        let s = Stage {
            title: "x".into(),
            files: vec!["a.rs".into()],
            instruction: "do it".into(),
        };
        let normal = stage_instruction(&s, 0, 1, false, false, false);
        let firmer = stage_instruction(&s, 0, 1, true, false, false);
        assert!(!normal.contains("CHANGED NOTHING"));
        assert!(firmer.starts_with("YOUR PREVIOUS ATTEMPT CHANGED NOTHING"));
    }

    #[test]
    fn stage_instruction_is_size_aware() {
        let s = Stage { title: "t".into(), files: vec!["big.rs".into()], instruction: "x".into() };
        // Large existing file → tiny-hook guidance.
        let big = stage_instruction(&s, 0, 1, false, true, false);
        assert!(big.contains("LARGE existing file"), "steers to a small hook: {big}");
        assert!(big.to_lowercase().contains("small, surgical change"));
        // New/small file → write-it-whole guidance.
        let small = stage_instruction(&s, 0, 1, false, false, true);
        assert!(small.contains("NEW or small"), "steers to write-whole: {small}");
    }

    #[test]
    fn snapshot_detects_a_change() {
        let ws = std::env::temp_dir().join(format!("dc-staged-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&ws);
        std::fs::write(ws.join("a.rs"), "fn a() {}").unwrap();
        let files = vec!["a.rs".to_string()];
        let before = snapshot(&ws, &files);
        assert_eq!(snapshot(&ws, &files), before, "same content → same snapshot");
        std::fs::write(ws.join("a.rs"), "fn a() { let x = 1; }").unwrap();
        assert_ne!(snapshot(&ws, &files), before, "edit → different snapshot");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn restore_reverts_a_build_breaking_edit_to_the_snapshot() {
        // Edit-safety: a stage that corrupts the file can be rolled back to the last-green state
        // so the next attempt/stage edits compiling code, not the mess.
        let ws = std::env::temp_dir().join(format!("dc-staged-restore-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&ws);
        std::fs::write(ws.join("a.rs"), "fn a() {}\n").unwrap();
        let files = vec!["a.rs".to_string()];
        let green = snapshot(&ws, &files);
        // The model breaks it (unterminated brace).
        std::fs::write(ws.join("a.rs"), "fn a() { broken(\n").unwrap();
        assert_ne!(snapshot(&ws, &files), green);
        restore(&ws, &green);
        assert_eq!(std::fs::read_to_string(ws.join("a.rs")).unwrap(), "fn a() {}\n", "reverted");
        let _ = std::fs::remove_dir_all(&ws);
    }
}
