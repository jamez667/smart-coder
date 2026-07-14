//! Per-turn prompt assembly: turn the current loop state into the zoned [`Segment`] list
//! the [`ContextBuilder`](dc_context::ContextBuilder) budgets, plus the set of files whose
//! full contents this turn pins (so a redundant `read_file` of one can be short-circuited).
//!
//! This is a pure read of the inputs — it mutates nothing — which is why it lives outside
//! the loop body: the assembly rules (spec 05 zoning, the focused-vs-whole-task split, the
//! sacred recent window) are involved enough to read on their own.

use std::path::Path;

use dc_context::{summarize_history, Segment, TurnRecord, Zone};
use dc_model::Message;

use super::prompt::{
    imported_files, render_context_files, render_focus_files, render_other_files_map,
    render_progress_ledger,
};
use super::window::seg_from_message;
use super::AgentConfig;

/// Build this turn's zoned segments and the list of files pinned in full.
///
/// Mirrors the loop's needs exactly: the compacted history summary rides in the
/// `HistorySummary` zone, the whole recent window is tagged `RecentObservation` (sacred, so
/// an earlier read survives budget eviction), and the retrieved zone carries the plan, the
/// repo map / progress ledger (whole-task) or the focused file plus its imported bodies and a
/// signature map of the rest (focused). Returns `(segments, pinned_full_files)`.
// Each argument is a distinct read the assembly needs (config, workspace, the task text, the
// system preamble, the repo map, the rendered plan, history, and the recent window); a bag
// struct would only move the noise, so keep it flat like the loop's other helpers.
#[allow(clippy::too_many_arguments)]
pub(super) fn assemble_segments(
    cfg: &AgentConfig,
    workspace: &Path,
    instruction: &str,
    system: &str,
    repo_map: &str,
    plan_render: String,
    history: &[TurnRecord],
    recent: &[Message],
) -> (Vec<Segment>, Vec<String>) {
    // Compact older turns; keep the recent ones verbatim.
    let (older, _recent_records) = dc_context::split_for_compaction(history, cfg.keep_recent_turns);
    let summary = summarize_history(older);

    // Assemble the budgeted, zoned prompt (spec 05). The plan rides in the
    // retrieved zone as compact structured state (spec 05).
    let mut segments = vec![
        Segment::system(Zone::System, system.to_string()),
        Segment::user(Zone::TaskAnchor, instruction.to_string()),
    ];
    if !plan_render.is_empty() {
        segments.push(Segment::user(Zone::Retrieved, plan_render));
    }
    // Files whose full current contents are pinned in this turn's prompt (set in the
    // focused branch). A `read_file` of one is redundant and gets short-circuited.
    let mut pinned_full_files: Vec<String> = Vec::new();
    if cfg.focus_files.is_empty() {
        // WHOLE-TASK path: the repo map helps navigation; the progress ledger lists the
        // files that exist (so it doesn't re-create/forget them).
        if !repo_map.is_empty() {
            segments.push(Segment::user(Zone::Retrieved, repo_map.to_string()));
        }
        let ledger = render_progress_ledger(workspace);
        if !ledger.is_empty() {
            segments.push(Segment::user(Zone::Retrieved, ledger));
        }
    } else {
        // FOCUSED path (per-file step): the model needs the CODE of the files its file
        // IMPORTS FROM — signatures alone weren't enough (it re-read them to see args /
        // behavior). So pin the FULL bodies of the imported files (read-only context),
        // bounded to the few it actually imports, and use the cheap signature map only for
        // the DISTANT rest. The focused file's own body is pinned just below. All re-read
        // FRESH each turn so the view never goes stale after an edit.
        let imports = imported_files(workspace, &cfg.focus_files);
        if !imports.is_empty() {
            let ctx = render_context_files(workspace, &imports);
            if !ctx.is_empty() {
                segments.push(Segment::user(Zone::Retrieved, ctx));
            }
        }
        // Signature map of everything that's neither the focused file nor an imported one.
        let mut exclude = cfg.focus_files.clone();
        exclude.extend(imports.iter().cloned());
        let others = render_other_files_map(workspace, &exclude, cfg.repo_map_top_k);
        if !others.is_empty() {
            segments.push(Segment::user(Zone::Retrieved, others));
        }
        // The files whose CURRENT contents are pinned IN FULL this turn (focus + imported).
        // A `read_file` of any of these is pure waste — the content is already shown — so
        // the dispatch short-circuits it below (the model re-reads pinned files reflexively,
        // even its own focus file; the immediate-repeat guard misses interleaved re-reads).
        pinned_full_files = cfg.focus_files.clone();
        pinned_full_files.extend(imports);
    }
    // Pin the current contents of the focused files, re-read fresh every turn so
    // the view never goes stale after an edit (the failure mode that traps a
    // tiny model into re-applying its own first edit). This is the live anchor
    // the model copies `old_str` from.
    let focus = render_focus_files(workspace, &cfg.focus_files);
    if !focus.is_empty() {
        segments.push(Segment::user(Zone::Retrieved, focus));
    }
    if !summary.is_empty() {
        segments.push(Segment::user(Zone::HistorySummary, summary));
    }
    // The whole `keep_recent_turns` window is verbatim recent context and must
    // SURVIVE eviction — that's what keep_recent_turns promises. Tagging only the last
    // message `RecentObservation` (sacred) and the rest `HistorySummary` meant the
    // earlier recent turns were evicted first under budget pressure: a file the model
    // had just read evaporated one turn later, so it re-read it and stalled. Tag the
    // entire recent window `RecentObservation` so it's all sacred. Older turns are
    // already compacted into the `summary` above (split_for_compaction), so this only
    // protects the genuinely-recent window, not unbounded history.
    for m in recent.iter() {
        segments.push(seg_from_message(Zone::RecentObservation, m));
    }

    (segments, pinned_full_files)
}
