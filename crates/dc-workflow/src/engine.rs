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
    // Ground only on the upstream artifacts this phase actually needs — not the whole
    // chain. Stuffing every approved artifact into every phase overflows a small model
    // by the late phases (the WorkDecomposition call carried specs+arch+layout+stage-
    // breakdown and returned empty → no subtasks → nothing built). See
    // `Phase::needs_upstream`.
    let needed = phase.needs_upstream();
    for a in state.approved() {
        if needed.contains(&a.phase) {
            user.push_str(&format!(
                "\n=== Approved {} ===\n{}\n",
                a.phase.title(),
                a.content
            ));
        }
    }
    // A send-back carried feedback for this phase — surface it so the regeneration
    // addresses what the human flagged (spec 09).
    if let Some(notes) = state.feedback(phase) {
        user.push_str(&format!(
            "\n=== Reviewer feedback (address this) ===\n{notes}\n"
        ));
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
    // A transient backend error (a 503 "Loading model" while a Docker container
    // reloads, or a network blip) needs the retries to span a few SECONDS — three
    // back-to-back calls all land inside the same reload and all fail (observed live
    // 2026-06-15: the Specs phase died because the model was mid-reload). So when the
    // call itself errors, back off before retrying; an empty-but-successful reply
    // (thinking-budget exhaustion) retries immediately with /no_think as before.
    const BACKOFF_MS: [u64; 4] = [0, 1000, 2000, 4000];
    for attempt in 0..4 {
        // After a first weak result, drop thinking for this phase: the likeliest cause
        // is the budget vanishing into reasoning_content (the model narrates the task
        // instead of answering, and runs out before emitting the JSON).
        let effective = if attempt == 0 {
            think
        } else {
            think.with(phase, true)
        };
        let mut req = GenerateRequest::new(phase_messages(phase, state, effective));
        // A complex task's decomposition / coverage plan is long structured JSON; the
        // old 1536 cap truncated it mid-array (observed live 2026-06-14: a restaurant
        // site decomposition ran out of budget while still reasoning → no JSON → empty
        // board → nothing built). Give the phases real room; the JSON phases get more.
        req.max_tokens = if phase.produces_json() { 4096 } else { 2048 };
        match orchestrator.generate(&req) {
            Ok(resp) => {
                let content = resp.content.trim().to_string();
                // A JSON phase that came back as prose-only (no parseable array) is a
                // FAILED attempt, not a usable artifact — retry with thinking suppressed
                // rather than chaining an empty board downstream and building nothing.
                let usable = !content.is_empty()
                    && (!phase.produces_json() || contains_json_array(&content));
                if usable {
                    return Artifact::draft(phase, content);
                }
                // Empty/unusable but the backend answered: retry now (no backoff).
            }
            Err(_) => {
                // The backend errored — likely transient (reload/blip). Wait so the
                // remaining attempts outlast a multi-second model load.
                let delay = BACKOFF_MS[(attempt + 1).min(BACKOFF_MS.len() - 1)];
                if delay > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                }
            }
        }
    }
    Artifact::draft(phase, String::new())
}

/// Whether `text` contains a non-empty JSON array (tolerating surrounding prose/fences)
/// — the gate for a JSON phase's output being usable rather than just reasoning.
fn contains_json_array(text: &str) -> bool {
    let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) else {
        return false;
    };
    if start >= end {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&text[start..=end])
        .ok()
        .and_then(|v| v.as_array().map(|a| !a.is_empty()))
        .unwrap_or(false)
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
            "You slice the implementation into subtasks — ONE per source file — each \
             writing a single file to pass its own test, sized for a tiny worker model."
        }
    };
    format!(
        "You are the orchestrator (architect) in a staged coding workflow. {role} \
        Ground everything in the approved artifacts you are given. Be concise and concrete. \
        {STACK_CONSTRAINT}"
    )
}

/// The locked technology stack, woven into every phase prompt. The small models are
/// most reliable on Python + plain web, and a fixed stack lets the harness match the
/// verify command (pytest / vitest) and reject off-stack files. No TypeScript, no React
/// build tooling, no other backend languages.
const STACK_CONSTRAINT: &str = "STACK: backend in Python with Flask; a frontend, ONLY IF the \
    task needs a user interface, in plain JavaScript, HTML, and CSS. Build ONLY what the task \
    asks for — if it is a backend/JSON API with no UI, create NO frontend files (no index.html, \
    script.js, or styles.css) and write NO frontend tests; app.py alone is the whole project. \
    Do NOT use TypeScript, React, Vue, a build step, or any other backend language (no \
    Node.js/Express, no Java, no Go). Every source file must be a .py, .js, .html, or .css file. \
    LIBRARIES: the installed Python packages you may import are flask, flask_sqlalchemy, \
    flask_restful, flask_cors, marshmallow, requests, pytest, and the standard library. \
    Do NOT use any package outside that list (no FastAPI, no Django) — it is not installed \
    and the tests will fail to import. Frontend uses only the browser's built-in fetch and \
    DOM APIs (no npm packages). Write Flask route handlers as plain `def`, never `async def`.";

fn phase_instruction(phase: Phase) -> String {
    match phase {
        Phase::StageBreakdown => {
            // ONE test file per SOURCE file in the layout. This 1:1 alignment is what
            // makes the swarm work: each source file gets its own test, so each becomes a
            // single-file subtask judged by a single test the worker can actually satisfy.
            // A test that spans multiple source files (a route test that needs both the
            // .py and its template) can't be satisfied by any one single-file worker, and
            // every subtask reverts (observed live 2026-06-14). Two runners (spec 08):
            // pytest for `.py` (test_<name>.py), vitest for frontend (.<name>.test.js).
            "Output the tests that pin the task's required BEHAVIOR — nothing more. JSON array \
             of coverage items; each item: \
             {\"file\":\"<test file>\",\"covers\":\"one specific behavior the test must check\",\
             \"expect\":<the exact JSON the route returns for this case>}. The `expect` value is \
             the EXACT response body as a JSON literal, with EVERY field the spec states — e.g. \
             for a counter that returns name+value: \"expect\":{\"name\":\"x\",\"value\":1}; for \
             an error: \"expect\":{\"error\":\"not found\"}. Omit `expect` only when the behavior \
             has no JSON body. For a Flask backend/API, ALL route behavior is tested via the test \
             client in ONE `test_app.py` (pytest) — one item per route/behavior (happy path and \
             important edge cases, e.g. invalid input returning the right error code). Add a \
             frontend test (`<name>.test.js`, vitest) ONLY if the task asked for a UI file; if \
             the task is a backend/JSON API with no UI, output NO frontend tests. Do NOT invent \
             tests for files the task didn't ask for. No prose, just the JSON array."
                .to_string()
        }
        Phase::WorkDecomposition => {
            // ONE SUBTASK PER SOURCE FILE. The tests are 1:1 with source files (one test
            // per file), so each subtask is a single source file gated by its single test
            // — which a single-file worker can actually write and pass. (A subtask owning
            // multiple files breaks: the single-shot worker returns one file's content and
            // mashes the rest into it — observed live 2026-06-14, HTML pasted into app.py.)
            "Create ONE subtask per IMPLEMENTATION source file the layout defines. Each \
             subtask writes exactly ONE source file to make that file's own test pass. The \
             goal must be an IMPLEMENT instruction (what to build), e.g. \"Implement the \
             Flask root route in app.py that serves the page\" — NOT a restatement of what \
             the test verifies. Do NOT include any subtask that writes, edits, or runs \
             tests (the tests are frozen). Output ONLY a JSON array of subtasks; each item: \
             {\"id\":\"t1\",\"goal\":\"Implement ...\",\"files\":[\"one_source_file\"],\"deps\":[\"id\"]}. \
             Each `files` list has exactly ONE non-test source file. Use deps only when one \
             file must exist before another (e.g. a template before the route that renders \
             it). No prose, just the JSON array."
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
    fn late_phases_ground_only_on_needed_upstream_not_the_whole_chain() {
        // The overflow fix: WorkDecomposition needs the layout (files) + stage-breakdown
        // (tests), but NOT the prose specs/architecture — feeding everything overflows
        // the small model and it returns empty (observed live: restaurant site).
        let mut s = WorkflowState::new("build it");
        for (p, body) in [
            (Phase::Specs, "SPECS_PROSE"),
            (Phase::Architecture, "ARCH_PROSE"),
            (Phase::Layout, "LAYOUT_FILES"),
            (Phase::StageBreakdown, "STAGE_TESTS"),
            (Phase::ImplementationPlan, "IMPL_PLAN"),
        ] {
            s.set(Artifact::draft(p, body));
            s.approve(p);
        }
        let msgs = phase_messages(Phase::WorkDecomposition, &s, ThinkPolicy::default());
        let joined: String = msgs.iter().map(|m| m.content.clone()).collect();
        assert!(joined.contains("LAYOUT_FILES"), "needs the layout");
        assert!(joined.contains("STAGE_TESTS"), "needs the stage breakdown");
        assert!(!joined.contains("SPECS_PROSE"), "must drop the specs prose");
        assert!(
            !joined.contains("ARCH_PROSE"),
            "must drop the architecture prose"
        );
        assert!(
            !joined.contains("IMPL_PLAN"),
            "must drop the impl-plan prose"
        );
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

    #[test]
    fn generate_phase_recovers_from_a_transient_backend_error() {
        // A 503 "Loading model" while the Docker container reloads makes generate()
        // return Err. The engine must back off and retry, not give up — otherwise a
        // momentary blip kills the whole workflow (observed live 2026-06-15). Here the
        // first call errors, the second succeeds.
        use dc_model::{Capabilities, GenerateResponse, ToolCalling};
        use std::cell::Cell;

        struct FlakyBackend {
            calls: Cell<usize>,
        }
        impl ModelBackend for FlakyBackend {
            fn name(&self) -> &str {
                "flaky"
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities {
                    max_context_tokens: 8_192,
                    tool_calling: ToolCalling::None,
                    on_device: false,
                }
            }
            fn generate(&self, _req: &GenerateRequest) -> dc_proto::Result<GenerateResponse> {
                let n = self.calls.get();
                self.calls.set(n + 1);
                if n == 0 {
                    Err(dc_proto::DcError::Backend("Loading model".to_string()))
                } else {
                    Ok(GenerateResponse {
                        content: "# Specs\nrecovered after the blip".to_string(),
                    })
                }
            }
        }

        let backend = FlakyBackend {
            calls: Cell::new(0),
        };
        let s = WorkflowState::new("t");
        let a = generate_phase(&backend, Phase::Specs, &s, ThinkPolicy::default());
        assert!(
            a.content.contains("recovered after the blip"),
            "must recover from a transient error, got: {:?}",
            a.content
        );
        assert_eq!(backend.calls.get(), 2, "errored once, then succeeded");
    }

    #[test]
    fn json_phase_rejects_prose_only_and_retries_for_the_array() {
        // The restaurant-site bug: the decomposition model narrates the task in prose
        // and never emits JSON. That's NOT a usable artifact for a JSON phase — the
        // engine must reject it and retry until it gets a parseable array.
        let backend = MockBackend::new([
            "The user wants me to act as an orchestrator. Constraint Checklist: ...",
            r#"[{"id":"t1","goal":"do a","files":["a.py"]}]"#,
        ]);
        let s = WorkflowState::new("build a thing");
        let a = generate_phase(
            &backend,
            Phase::WorkDecomposition,
            &s,
            ThinkPolicy::default(),
        );
        assert!(
            a.content.contains("\"id\""),
            "a JSON phase must yield the array, not the prose: {:?}",
            a.content
        );
    }

    #[test]
    fn prose_phase_accepts_prose_as_usual() {
        // A non-JSON phase (specs) is happy with prose — the JSON gate must not apply.
        let backend = MockBackend::new(["## Goals\nship a great thing"]);
        let s = WorkflowState::new("t");
        let a = generate_phase(&backend, Phase::Specs, &s, ThinkPolicy::default());
        assert!(a.content.contains("Goals"));
    }

    #[test]
    fn contains_json_array_detects_array_in_prose() {
        assert!(contains_json_array("blah [\n{\"id\":\"t1\"}\n] done"));
        assert!(!contains_json_array("no json here, just prose"));
        assert!(!contains_json_array("[]"), "empty array is not usable");
    }
}
