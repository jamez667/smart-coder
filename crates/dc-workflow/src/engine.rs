//! The phase engine (spec 09): produce one phase's artifact by reasoning over the
//! task plus the prior *approved* artifacts.
//!
//! Each phase is a single orchestrator (T1) call that emits a Markdown document.
//! A small model never holds the whole problem — only the approved artifacts it
//! needs. The last phase (work decomposition) emits the JSON subtask array the
//! swarm consumes ([`dc_swarm`]).

use dc_model::{GenerateRequest, Message, ModelBackend};

use crate::phase::Phase;
use crate::state::{Artifact, WorkflowState};

/// Build the messages for producing `phase`: a phase-specific system prompt, the
/// original task, and every approved upstream artifact as grounding context.
pub fn phase_messages(phase: Phase, state: &WorkflowState) -> Vec<Message> {
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
    vec![Message::system(system_for(phase)), Message::user(user)]
}

/// Produce `phase`'s artifact via the orchestrator. The returned [`Artifact`] is a
/// draft; the runner/checkpoint decides whether to approve it.
pub fn generate_phase(
    orchestrator: &dyn ModelBackend,
    phase: Phase,
    state: &WorkflowState,
) -> Artifact {
    let mut req = GenerateRequest::new(phase_messages(phase, state));
    // Planning documents need more room than a single tool call.
    req.max_tokens = 1536;
    let content = orchestrator
        .generate(&req)
        .map(|r| r.content.trim().to_string())
        .unwrap_or_default();
    Artifact::draft(phase, content)
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
            "You break the work into small incremental stages. For EACH stage, write the unit tests \
             FIRST that define 'done' — the stage is complete when those tests pass."
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
        Phase::WorkDecomposition => {
            // The swarm's decomposition parser expects exactly this JSON shape.
            "Output ONLY a JSON array of subtasks; each item: \
             {\"id\":\"t1\",\"goal\":\"...\",\"files\":[\"path\"],\"deps\":[\"id\"]}. \
             Each subtask owns a DISJOINT set of files; use deps only when one must \
             finish before another. No prose, just the JSON array."
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
        let msgs = phase_messages(Phase::Architecture, &s);
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
        let msgs = phase_messages(Phase::Specs, &s);
        let joined: String = msgs.iter().map(|m| m.content.clone()).collect();
        assert!(!joined.contains("ARCH_DRAFT"));
    }

    #[test]
    fn decomposition_phase_asks_for_json() {
        let s = WorkflowState::new("t");
        let msgs = phase_messages(Phase::WorkDecomposition, &s);
        let joined: String = msgs.iter().map(|m| m.content.clone()).collect();
        assert!(joined.contains("JSON array"));
        assert!(joined.contains("\"files\""));
    }

    #[test]
    fn generate_phase_returns_a_draft() {
        let backend = MockBackend::new(["# Specs\nGoals: ship it"]);
        let s = WorkflowState::new("ship it");
        let a = generate_phase(&backend, Phase::Specs, &s);
        assert_eq!(a.phase, Phase::Specs);
        assert!(a.content.contains("Goals"));
        assert!(!a.is_approved());
    }
}
