//! Per-turn prompt assembly: turn the current loop state into the zoned [`Segment`] list
//! the [`ContextBuilder`](sc_context::ContextBuilder) budgets, plus the set of files whose
//! full contents this turn pins (so a redundant `read_file` of one can be short-circuited).
//!
//! This is a pure read of the inputs — it mutates nothing and touches no disk — which is why
//! it lives outside the loop body: the assembly rules (spec 05 zoning, the focused-vs-whole-
//! task split, the sacred recent window) are involved enough to read on their own. Everything
//! retrieved from the workspace comes pre-rendered from the [`StableContext`], so a turn with
//! no workspace change assembles a byte-identical prefix and the backend's KV cache holds.

use sc_context::{summarize_history, Segment, TurnRecord, Zone};

use super::stable::StableContext;
use super::window::{seg_from_message, RecentWindow};
use super::AgentConfig;
use crate::plan::PlanState;

/// Build this turn's zoned segments and the list of files pinned in full.
///
/// Mirrors the loop's needs exactly: `compacted` is the history of the turns the loop has
/// EVICTED from the recent window (rendered as the `HistorySummary` zone -- so the summary
/// only changes when an eviction happens), the whole recent window is tagged
/// `RecentObservation` (sacred, so an earlier read survives budget eviction), and the
/// retrieved zone carries the plan plus the cached repo map / progress ledger (whole-task) or
/// the cached imported bodies + signature map (focused). Returns
/// `(segments, pinned_full_files)`.
pub(super) fn assemble_segments(
    cfg: &AgentConfig,
    instruction: &str,
    system: &str,
    stable: &StableContext,
    plan: &PlanState,
    compacted: &[TurnRecord],
    recent: &RecentWindow,
) -> (Vec<Segment>, Vec<String>) {
    // Assemble the budgeted, zoned prompt (spec 05). The plan rides in the
    // retrieved zone as compact structured state (spec 05).
    let mut segments = vec![
        Segment::system(Zone::System, system.to_string()),
        Segment::user(Zone::TaskAnchor, instruction.to_string()),
    ];
    // Only a plan WITH steps earns a segment; an empty plan must add nothing, or a
    // placeholder rides in the prompt every turn for no information.
    if !plan.is_empty() {
        let plan_render = plan.render();
        if !plan_render.is_empty() {
            segments.push(Segment::user(Zone::Retrieved, plan_render));
        }
    }
    stable.push_retrieved(cfg, &mut segments);
    let pinned_full_files = stable.pinned_full_files(cfg);
    // The focused files' current contents, pinned so the view never goes stale after an
    // edit (the failure mode that traps a tiny model into re-applying its own first edit).
    // This is the live anchor the model copies `old_str` from; SACRED (Zone::FocusFile) and
    // laid out just before the recent window, so an edit invalidates the cached prefix only
    // from here on.
    if let Some(focus) = stable.focus_segment() {
        segments.push(focus);
    }
    let summary = summarize_history(compacted);
    if !summary.is_empty() {
        segments.push(Segment::user(Zone::HistorySummary, summary));
    }
    // The whole recent window is verbatim recent context and must SURVIVE the builder's
    // eviction -- that's what keep_recent_turns promises. Tagging only the last message
    // `RecentObservation` (sacred) and the rest `HistorySummary` meant the earlier recent
    // turns were evicted first under budget pressure: a file the model had just read
    // evaporated one turn later, so it re-read it and stalled. Tag the entire window
    // `RecentObservation` so it's all sacred. The loop itself bounds the window by budget,
    // evicting whole turns oldest-first into `compacted`, so this protects the genuinely
    // recent turns, not unbounded history.
    for m in recent.messages() {
        segments.push(seg_from_message(Zone::RecentObservation, m));
    }

    (segments, pinned_full_files)
}

#[cfg(test)]
mod tests {
    use super::super::test_util::temp_dir;
    use super::*;

    #[test]
    fn plan_is_pinned_into_the_retrieved_zone_and_short_circuits_reads() {
        let ws = temp_dir("refplan-assemble");
        std::fs::write(ws.join("PLAN-lakes.md"), "## Plan: lakes\nstep one").unwrap();
        let cfg = AgentConfig::default();
        let registry = sc_tools::default_registry();
        let instruction = "Implement the feature plan in PLAN-lakes.md.";
        let stable = StableContext::new(&ws, &cfg, &registry, instruction, String::new());
        let (segments, pinned) = assemble_segments(
            &cfg,
            instruction,
            "system",
            &stable,
            &PlanState::default(),
            &[],
            &RecentWindow::default(),
        );
        assert!(
            pinned.contains(&"PLAN-lakes.md".to_string()),
            "plan is pinned so re-reads short-circuit"
        );
        let joined: String = segments.iter().map(|s| s.text.clone()).collect();
        assert!(
            joined.contains("## Plan: lakes"),
            "plan body is in the prompt"
        );
        assert!(
            joined.contains("do NOT re-read this"),
            "steered away from re-reading"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn an_empty_plan_adds_no_segment_and_a_stepped_one_adds_one() {
        let ws = temp_dir("plan-seg");
        let cfg = AgentConfig::default();
        let registry = sc_tools::default_registry();
        let stable = StableContext::new(&ws, &cfg, &registry, "task", String::new());
        let (without, _) = assemble_segments(
            &cfg,
            "task",
            "system",
            &stable,
            &PlanState::default(),
            &[],
            &RecentWindow::default(),
        );
        let plan = PlanState::from_descriptions(["locate", "edit"]);
        let (with, _) = assemble_segments(
            &cfg,
            "task",
            "system",
            &stable,
            &plan,
            &[],
            &RecentWindow::default(),
        );
        assert_eq!(with.len(), without.len() + 1);
        assert!(with.iter().any(|s| s.text.starts_with("plan:")));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
