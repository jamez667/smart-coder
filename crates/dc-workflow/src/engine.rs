//! The phase engine (spec 09): produce one phase's artifact by reasoning over the
//! task plus the prior *approved* artifacts.
//!
//! Each phase is a single orchestrator (T1) call that emits a Markdown document.
//! A small model never holds the whole problem — only the approved artifacts it
//! needs. The last phase (work decomposition) emits the JSON subtask array the
//! swarm consumes ([`dc_swarm`]).

use dc_model::{GenerateRequest, Message, ModelBackend};

use crate::phase::Phase;
use crate::policy::ThinkPolicy;
use crate::state::{Artifact, WorkflowState};

/// Build the messages for producing `phase`: a phase-specific system prompt, the
/// original task, and every approved upstream artifact as grounding context. When
/// `think` suppresses this phase, a `/no_think` suffix is appended to the system
/// prompt (a thinking model then skips its chain-of-thought).
pub fn phase_messages(phase: Phase, state: &WorkflowState, think: ThinkPolicy) -> Vec<Message> {
    let mut user = format!("Task: {}\n", state.task);
    for a in state.approved() {
        if a.phase.index() < phase.index() {
            user.push_str(&format!(
                "\n=== Approved {} ===\n{}\n",
                a.phase.title(),
                a.content
            ));
        }
    }
    user.push_str(&format!("\n{}", phase_instruction(phase)));

    let mut system = system_for(phase);
    if think.suppress(phase) {
        system.push_str(" /no_think");
    }
    vec![Message::system(system), Message::user(user)]
}

/// Produce `phase`'s artifact via the orchestrator. The returned [`Artifact`] is a
/// draft; the runner/checkpoint decides whether to approve it.
///
/// Robustness (spec 00 — degrade, don't silently corrupt): a thinking model
/// occasionally spends its whole budget in the reasoning block and returns empty
/// visible content, and a backend can blip. So we retry an empty/failed
/// generation a couple of times — and, after the first empty try, force
/// `/no_think` for this phase so the model spends tokens on the answer, not
/// deliberation. A persistently empty artifact is left empty for the runner to
/// reject loudly rather than chaining a broken plan downstream.
pub fn generate_phase(
    orchestrator: &dyn ModelBackend,
    phase: Phase,
    state: &WorkflowState,
    think: ThinkPolicy,
) -> Artifact {
    for attempt in 0..3 {
        // After a first empty result, drop thinking for this phase: the likeliest
        // cause is the budget vanishing into reasoning_content.
        let effective = if attempt == 0 {
            think
        } else {
            think.with(phase, true)
        };
        let mut req = GenerateRequest::new(phase_messages(phase, state, effective));
        req.max_tokens = 1536;
        if let Ok(resp) = orchestrator.generate(&req) {
            let content = resp.content.trim().to_string();
            if !content.is_empty() {
                return Artifact::draft(phase, content);
            }
        }
    }
    Artifact::draft(phase, String::new())
}

fn system_for(phase: Phase) -> String {
    let role = match phase {
        Phase::Specs => "You write a crisp spec: goals, non-goals, and constraints.",
        Phase::Architecture => {
            "You design the high-level architecture: components, boundaries, data flow, key choices."
        }
        Phase::Layout => {
            "You define the concrete project layout: directories, modules/files, and each one's responsibility."
        }
        Phase::StageBreakdown => {
            "You plan the TESTS first (TDD). You don't write test code yourself — you list the \
             coverage each test file must hit; small worker models will write the actual tests \
             from your coverage list. The stages are 'make these tests pass'."
        }
        Phase::ImplementationPlan => {
            "For each stage, you write the concrete, ordered plan to make its tests pass (red → green)."
        }
        Phase::WorkDecomposition => {
            "You slice the plan into small INDEPENDENT subtasks sized for a tiny worker model."
        }
    };
    format!(
        "You are the orchestrator (architect) in a staged coding workflow. {role} \
        Ground everything in the approved artifacts you are given. Be concise and concrete."
    )
}

fn phase_instruction(phase: Phase) -> String {
    match phase {
        Phase::StageBreakdown => {
            // A coverage test-plan, not test code. Each item names a test file and
            // the behavior it must cover; a worker writes the actual test from this.
            "Output ONLY a JSON array of coverage items; each item: \
             {\"file\":\"test_x.py\",\"covers\":\"one specific behavior the test must check\"}. \
             Group related behaviors under the same test file. Cover the happy path and the \
             important edge cases. No prose, just the JSON array."
                .to_string()
        }
        Phase::WorkDecomposition => {
            // The swarm's decomposition parser expects exactly this JSON shape.
            // Crucially: the test files are ALREADY WRITTEN and frozen — decompose
            // only the IMPLEMENTATION work that makes them pass, never test-writing
            // or a 'run the tests' step (the harness verifies; tests aren't a task).
            "The test files already exist and are FROZEN — do not include any subtask \
             that writes, edits, or runs tests. Decompose only the IMPLEMENTATION work \
             that makes the existing tests pass. Output ONLY a JSON array of subtasks; \
             each item: {\"id\":\"t1\",\"goal\":\"...\",\"files\":[\"path\"],\"deps\":[\"id\"]}. \
             Every `files` entry is a non-test source file, and each subtask owns a \
             DISJOINT set of files; use deps only when one must finish before another. \
             No prose, just the JSON array."
                .to_string()
        }
        _ => format!(
            "Write the {} as a short Markdown document. Output only the document.",
            phase.title()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Artifact;
    use dc_model::MockBackend;

    #[test]
    fn messages_include_task_and_approved_upstream() {
        let mut s = WorkflowState::new("build a CLI");
        s.set(Artifact::draft(Phase::Specs, "the spec text"));
        s.approve(Phase::Specs);
        let msgs = phase_messages(Phase::Architecture, &s, ThinkPolicy::default());
        let joined: String = msgs.iter().map(|m| m.content.clone()).collect();
        assert!(joined.contains("build a CLI"));
        assert!(joined.contains("the spec text"));
        assert!(joined.contains("Approved Specs"));
    }

    #[test]
    fn messages_exclude_downstream_and_unapproved() {
        let mut s = WorkflowState::new("t");
        // A later-phase artifact and an unapproved one must not leak into an
        // earlier phase's context.
        s.set(Artifact::draft(Phase::Architecture, "ARCH_DRAFT")); // unapproved
        let msgs = phase_messages(Phase::Specs, &s, ThinkPolicy::default());
        let joined: String = msgs.iter().map(|m| m.content.clone()).collect();
        assert!(!joined.contains("ARCH_DRAFT"));
    }

    #[test]
    fn decomposition_phase_asks_for_json() {
        let s = WorkflowState::new("t");
        let msgs = phase_messages(Phase::WorkDecomposition, &s, ThinkPolicy::default());
        let joined: String = msgs.iter().map(|m| m.content.clone()).collect();
        assert!(joined.contains("JSON array"));
        assert!(joined.contains("\"files\""));
    }

    #[test]
    fn think_policy_appends_no_think_per_phase() {
        let s = WorkflowState::new("t");
        // Default: a doc phase gets /no_think; a JSON reasoning phase doesn't.
        let spec_sys = phase_messages(Phase::Specs, &s, ThinkPolicy::default())[0]
            .content
            .clone();
        assert!(spec_sys.contains("/no_think"), "{spec_sys}");
        let cov_sys = phase_messages(Phase::StageBreakdown, &s, ThinkPolicy::default())[0]
            .content
            .clone();
        assert!(!cov_sys.contains("/no_think"), "{cov_sys}");
        // A per-step override flips just that phase.
        let forced = ThinkPolicy::always_think().with(Phase::Specs, true);
        let spec2 = phase_messages(Phase::Specs, &s, forced)[0].content.clone();
        assert!(spec2.contains("/no_think"));
    }

    #[test]
    fn generate_phase_returns_a_draft() {
        let backend = MockBackend::new(["# Specs\nGoals: ship it"]);
        let s = WorkflowState::new("ship it");
        let a = generate_phase(&backend, Phase::Specs, &s, ThinkPolicy::default());
        assert_eq!(a.phase, Phase::Specs);
        assert!(a.content.contains("Goals"));
        assert!(!a.is_approved());
    }

    #[test]
    fn generate_phase_retries_past_an_empty_reply() {
        // A thinking model can return empty visible content (budget spent in
        // reasoning); the engine retries and recovers.
        let backend = MockBackend::new(["", "  ", "# Specs\nrecovered"]);
        let s = WorkflowState::new("t");
        let a = generate_phase(&backend, Phase::Specs, &s, ThinkPolicy::default());
        assert!(a.content.contains("recovered"), "got: {:?}", a.content);
    }

    #[test]
    fn generate_phase_gives_up_empty_after_retries() {
        // Persistently empty (e.g. dead backend) → empty artifact; the runner turns
        // that into a loud error.
        let backend = MockBackend::new(["", "", "", ""]);
        let s = WorkflowState::new("t");
        let a = generate_phase(&backend, Phase::Specs, &s, ThinkPolicy::default());
        assert!(a.content.is_empty());
    }
}
