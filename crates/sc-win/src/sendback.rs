//! Turning line comments into workflow send-back: the code-review path to feedback.
//!
//! A human opens a gating phase's `.md` in the code view, drops line comments on the
//! parts they want changed, and clicks "Send back"; those comments BECOME the notes the
//! workflow re-plans from.
//!
//! Split from [`sc_craft_ui::comments`] when the editor became its own crate (spec 21).
//! Storing and rendering comments is an editor feature both products have; only this
//! half — which reads `sc_workflow::{Phase, ReviewNote}` — is agent-only.

use sc_craft_ui::comments::Comment;

/// Format the pending comments on a phase's artifact file into send-back notes — the
/// code-review path to workflow feedback: a human opens a gating phase's `.md` in the code
/// view, drops line comments on the parts they want changed, and clicks "Send back"; those
/// comments BECOME the notes the workflow re-plans from.
///
/// The formatting itself is the engine's ([`sc_workflow::format_sendback_notes`]) so the CLI
/// can produce identical feedback; this only projects the GUI's richer [`Comment`] (which also
/// carries resolution + undo state) onto the engine's [`sc_workflow::ReviewNote`]. `comments`
/// should already be filtered to the phase's file.
pub fn format_sendback_notes(comments: &[&Comment]) -> Option<String> {
    let notes: Vec<sc_workflow::ReviewNote<'_>> = comments
        .iter()
        .map(|c| sc_workflow::ReviewNote::new(c.start, c.end, &c.text))
        .collect();
    sc_workflow::format_sendback_notes(&notes)
}

/// A phase's artifact file plus the comments left on it — one entry of a send-back harvest.
/// `phase` is the workflow phase the file belongs to; `file` is its workspace-relative path.
pub struct PhaseComments<'a> {
    pub phase: sc_workflow::Phase,
    pub file: &'a str,
    pub notes: Vec<&'a Comment>,
}

/// Resolve a send-back from the line comments across ALL phase artifacts — the GUI's
/// upstream send-back.
///
/// The reviewer reads a phase's `.md` and drops comments on it; the FILE a comment sits on
/// says which phase it's about, so a comment on `architecture.md` while the layout is gating
/// is unambiguously a request to change the architecture. Returns the target phase and the
/// formatted notes:
///
/// * The **target is the earliest commented phase** — [`sc_workflow::WorkflowState::invalidate_from`]
///   drops it *and everything downstream*, so any later commented phase is regenerating anyway
///   and bouncing to the earliest is what makes all the feedback apply.
/// * Notes from every commented phase are included, grouped under a `## <Phase>` header so the
///   model knows which artifact each bullet is about. Downstream comments are the *consequences*
///   the reviewer spotted while reading — the regeneration should see them.
/// * `None` when nothing is commented, so the caller falls back to the free-text box and the
///   gating phase.
///
/// `by_phase` should list every phase artifact with its comments, in pipeline order. Pure — no
/// I/O, so the targeting rule is host-testable.
pub fn resolve_sendback(
    by_phase: &[PhaseComments<'_>],
) -> Option<(sc_workflow::Phase, Option<String>)> {
    // Pipeline order, so "first commented" is "earliest phase".
    let mut commented: Vec<&PhaseComments<'_>> =
        by_phase.iter().filter(|p| !p.notes.is_empty()).collect();
    commented.sort_by_key(|p| p.phase.index());
    let target = commented.first()?.phase;

    // One section per commented phase. With a single phase commented (the common case) we skip
    // the header entirely — the notes read exactly as they did before upstream send-back existed.
    let notes = if commented.len() == 1 {
        format_sendback_notes(&commented[0].notes)
    } else {
        let sections: Vec<String> = commented
            .iter()
            .filter_map(|p| {
                format_sendback_notes(&p.notes).map(|b| format!("## {}\n{b}", p.phase.title()))
            })
            .collect();
        (!sections.is_empty()).then(|| sections.join("\n\n"))
    };
    Some((target, notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_sendback_notes_bullets_each_comment_with_its_range() {
        let a = Comment::new("specs/x/spec.md", 3, 3, "tighten the goal");
        let b = Comment::new("specs/x/spec.md", 10, 14, "this section is out of scope");
        let notes = format_sendback_notes(&[&a, &b]).unwrap();
        assert_eq!(
            notes,
            "- [line 3] tighten the goal\n- [lines 10-14] this section is out of scope"
        );
    }

    #[test]
    fn format_sendback_notes_none_when_no_comments() {
        assert_eq!(format_sendback_notes(&[]), None);
    }

    use sc_workflow::Phase;

    /// Build a `PhaseComments` list from (phase, file, comments) triples.
    fn by_phase<'a>(rows: &'a [(Phase, &'a str, Vec<&'a Comment>)]) -> Vec<PhaseComments<'a>> {
        rows.iter()
            .map(|(phase, file, notes)| PhaseComments {
                phase: *phase,
                file,
                notes: notes.clone(),
            })
            .collect()
    }

    #[test]
    fn resolve_sendback_targets_the_gating_phase_when_only_it_is_commented() {
        // The pre-existing behavior: comments on the phase being reviewed bounce it to itself,
        // and the notes are the plain bullet list with no phase header.
        let c = Comment::new("specs/x/layout.md", 4, 4, "split this module");
        let rows = [
            (Phase::Architecture, "specs/x/architecture.md", vec![]),
            (Phase::Layout, "specs/x/layout.md", vec![&c]),
        ];
        let (target, notes) = resolve_sendback(&by_phase(&rows)).unwrap();
        assert_eq!(target, Phase::Layout);
        assert_eq!(notes.as_deref(), Some("- [line 4] split this module"));
    }

    #[test]
    fn resolve_sendback_targets_the_earliest_commented_phase() {
        // The upstream case: reading the layout, the reviewer realises the ARCHITECTURE is
        // wrong. A comment on architecture.md must bounce to Architecture — invalidate_from
        // then drops layout too, so it regenerates from the corrected architecture.
        let arch = Comment::new("specs/x/architecture.md", 7, 9, "events, not polling");
        let layout = Comment::new("specs/x/layout.md", 2, 2, "follows from the above");
        let rows = [
            (Phase::Architecture, "specs/x/architecture.md", vec![&arch]),
            (Phase::Layout, "specs/x/layout.md", vec![&layout]),
        ];
        let (target, notes) = resolve_sendback(&by_phase(&rows)).unwrap();
        assert_eq!(target, Phase::Architecture, "earliest commented phase wins");

        // Both phases' notes ride along, grouped so the model knows which artifact each is
        // about — the downstream comment is the consequence the reviewer already spotted.
        let notes = notes.unwrap();
        assert_eq!(
            notes,
            "## Architecture\n- [lines 7-9] events, not polling\n\n## Layout\n- [line 2] follows from the above"
        );
    }

    #[test]
    fn resolve_sendback_is_none_when_nothing_is_commented() {
        // No comments anywhere → the caller falls back to the free-text note box and the
        // gating phase, exactly as before.
        let rows = [
            (Phase::Specs, "specs/x/spec.md", vec![]),
            (Phase::Layout, "specs/x/layout.md", vec![]),
        ];
        assert!(resolve_sendback(&by_phase(&rows)).is_none());
        assert!(resolve_sendback(&[]).is_none());
    }

    #[test]
    fn resolve_sendback_orders_by_pipeline_not_by_input_order() {
        // The caller may hand rows in any order; targeting must follow the PIPELINE order.
        let spec = Comment::new("specs/x/spec.md", 1, 1, "wrong goal");
        let layout = Comment::new("specs/x/layout.md", 5, 5, "and so this is wrong");
        let rows = [
            (Phase::Layout, "specs/x/layout.md", vec![&layout]),
            (Phase::Specs, "specs/x/spec.md", vec![&spec]),
        ];
        let (target, notes) = resolve_sendback(&by_phase(&rows)).unwrap();
        assert_eq!(target, Phase::Specs);
        // Sections are in pipeline order too, so the model reads cause before consequence.
        let notes = notes.unwrap();
        assert!(
            notes.find("## Specs").unwrap() < notes.find("## Layout").unwrap(),
            "sections in pipeline order: {notes}"
        );
    }
}
