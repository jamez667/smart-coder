//! The phase engine (spec 09): produce one phase's artifact by reasoning over the
//! task plus the prior *approved* artifacts.
//!
//! Each phase is a single orchestrator (T1) call that emits a Markdown document.
//! A small model never holds the whole problem — only the approved artifacts it
//! needs. The last phase (work decomposition) emits the JSON subtask array the
//! swarm consumes ([`sc_swarm`]).

use sc_model::{GenerateRequest, Message, ModelBackend};

use crate::phase::Phase;
use crate::policy::ThinkPolicy;
use crate::stack::ProjectStack;
use crate::state::{Artifact, WorkflowState};

/// Build the messages for producing `phase`: a phase-specific system prompt, the
/// original task, and every approved upstream artifact as grounding context. When
/// `think` suppresses this phase, a `/no_think` suffix is appended to the system
/// prompt (a thinking model then skips its chain-of-thought).
pub fn phase_messages(
    phase: Phase,
    state: &WorkflowState,
    think: ThinkPolicy,
    stack: ProjectStack,
) -> Vec<Message> {
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
    user.push_str(&format!(
        "\n{}",
        phase_instruction(phase, stack, &state.task)
    ));

    let mut system = system_for(phase, stack);
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
/// `on_token` is invoked with each streamed content delta of the *successful* attempt, so the
/// caller can render the reply live (token by token) instead of the run looking frozen while a
/// slow phase generates. On a RETRY the earlier attempt's partial tokens were already streamed —
/// the reply visibly restarts; the caller resets its per-phase buffer at each phase start.
pub fn generate_phase(
    orchestrator: &dyn ModelBackend,
    phase: Phase,
    state: &WorkflowState,
    think: ThinkPolicy,
    stack: ProjectStack,
    on_token: &mut dyn FnMut(&str),
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
        let mut req = GenerateRequest::new(phase_messages(phase, state, effective, stack));
        // A complex task's decomposition / coverage plan is long structured JSON; the
        // old 1536 cap truncated it mid-array (observed live 2026-06-14: a restaurant
        // site decomposition ran out of budget while still reasoning → no JSON → empty
        // board → nothing built). Give the phases real room; the JSON phases get more.
        req.max_tokens = if phase.produces_json() { 4096 } else { 2048 };
        // Stream the reply so the chat panel watches it type (a slow phase used to sit frozen).
        // Tokens flow to `on_token` during THIS attempt; the retry/empty/JSON-gate logic below is
        // unchanged — `generate_streaming` returns the same GenerateResponse with the full content.
        match orchestrator.generate_streaming(&req, on_token) {
            Ok(resp) => {
                let content = resp.content.trim().to_string();
                // A JSON phase that came back as prose-only (no parseable array) is a
                // FAILED attempt, not a usable artifact — retry with thinking suppressed
                // rather than chaining an empty board downstream and building nothing.
                // The non-Python stage breakdown intentionally emits a Markdown design doc, not
                // a JSON coverage array, so the JSON gate must not apply to it.
                let wants_json = phase.produces_json()
                    && (phase != Phase::StageBreakdown || matches!(stack, ProjectStack::Python));
                let usable = !content.is_empty() && (!wants_json || contains_json_array(&content));
                if usable {
                    // Guardrail: the layout + (non-Python) breakdown are Markdown docs where a weak
                    // planner degenerates into repeating the same file across dozens of near-duplicate
                    // sections (observed live 2026-07-21: a 25-stage breakdown touching 24 files, many
                    // dupes, for a small feature). Collapse repeated file sections and cap the count so
                    // the artifact — and the decomposition derived from it — stays sane regardless of
                    // model discipline. JSON phases are untouched (their shape is validated elsewhere).
                    let content = if wants_json {
                        content
                    } else {
                        dedup_file_sections(&content)
                    };
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

/// Collapse a Markdown file-list document (layout / breakdown) so each source FILE appears at most
/// once, and cap the number of sections. A weak planner repeats the same file across many
/// near-duplicate `##` sections, ballooning the doc (and, downstream, the decomposition into
/// duplicate subtasks → duplicated edits). We keep the FIRST section that references a given file
/// and drop later ones; sections with no detectable file path are kept as-is (headers, intros).
///
/// Pure/host-testable. Preserves the leading preamble before the first `##` heading verbatim.
fn dedup_file_sections(doc: &str) -> String {
    /// Hard cap on `##` sections kept — a single feature's breakdown/layout well under this; the
    /// cap only catches a runaway.
    const MAX_SECTIONS: usize = 12;

    let lines: Vec<&str> = doc.lines().collect();
    // Find the section boundaries: indices of lines starting a `## ` heading.
    let heads: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim_start().starts_with("## "))
        .map(|(i, _)| i)
        .collect();
    if heads.len() < 2 {
        return doc.to_string(); // nothing to dedup
    }

    // Preamble = everything before the first heading.
    let mut out: Vec<&str> = lines[..heads[0]].to_vec();
    let mut seen_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut kept = 0usize;
    for (h, &start) in heads.iter().enumerate() {
        let end = heads.get(h + 1).copied().unwrap_or(lines.len());
        let section = &lines[start..end];
        // The file this section is about: the first path-looking token in the heading or a
        // `**File**:`/`File:` line within the section.
        let file = section.iter().find_map(|l| extract_path_token(l));
        // Keep a section with no file (intro/summary), and the first per file, up to the cap.
        let keep = match &file {
            Some(f) => kept < MAX_SECTIONS && seen_files.insert(f.clone()),
            None => kept < MAX_SECTIONS,
        };
        if keep {
            out.extend_from_slice(section);
            kept += 1;
        }
    }
    out.join("\n")
}

/// Pull the first source-file-looking path out of a line: a `foo/bar.ext` token (has a `/` and a
/// file extension). `None` if the line has no such token. Tolerant of surrounding backticks,
/// `**File**:` labels, and trailing punctuation.
fn extract_path_token(line: &str) -> Option<String> {
    line.split(|c: char| c.is_whitespace() || matches!(c, '`' | '*' | '(' | ')' | ',' | ':'))
        .map(|t| t.trim_matches(|c| c == '`' || c == '.'))
        .find(|t| {
            t.contains('/')
                && t.rsplit('/').next().is_some_and(|name| {
                    name.rsplit_once('.')
                        .is_some_and(|(stem, ext)| !stem.is_empty() && (1..=5).contains(&ext.len()))
                })
        })
        .map(|t| t.to_string())
}

fn system_for(phase: Phase, stack: ProjectStack) -> String {
    let role = match phase {
        Phase::Specs => "You write a crisp spec: goals, non-goals, and constraints.",
        Phase::Architecture => {
            "You design the APPROACH, grounded in the REAL existing files surveyed in the task: \
             the patterns to use, the specific existing code/modules to reuse (named), what shared \
             helpers/abstractions to introduce, the key decisions, data flow, and boundaries — you \
             do NOT list the concrete files to create or edit (that's Layout)."
        }
        Phase::Layout => {
            "You turn the spec + architecture into the concrete per-file change list: exactly \
             which files to create or edit and each one's responsibility, applying the \
             architecture's patterns and reuse decisions."
        }
        Phase::StageBreakdown if matches!(stack, ProjectStack::Python) => {
            "You plan the TESTS first (TDD). You don't write test code yourself — you list the \
             coverage each test file must hit; small worker models will write the actual tests \
             from your coverage list. The stages are 'make these tests pass'."
        }
        Phase::StageBreakdown => {
            // This role now subsumes the old ImplementationPlan phase: the breakdown gives BOTH
            // the ordered build sequence AND the concrete per-stage steps, so no separate
            // implementation-planning pass is needed for a small worker model.
            "You break the work into an ordered set of small implementation stages — the concrete \
             build sequence, foundations first — AND, for each stage, the concrete steps to \
             implement it: the file(s) it touches and the specific edits (functions/types to add \
             or change, call sites to update), in order. A design breakdown to review, not tests \
             or code."
        }
        Phase::WorkDecomposition => {
            "You slice the implementation into subtasks — ONE per source file — each \
             writing a single file to pass its own test, sized for a tiny worker model."
        }
    };
    format!(
        "You are the orchestrator (architect) in a staged coding workflow. {role} \
        Ground everything in the approved artifacts you are given. Be concise and concrete. \
        {}",
        stack.constraint()
    )
}

/// Does this task read like an IN-PLACE edit of existing code (thread a parameter through
/// an existing function and its call sites, add a struct field, change a signature) rather
/// than a NEW feature/subsystem? In-place edits should NOT be decomposed into a new module —
/// that invents a file the task never asked for and stalls the weak model on the extra seam.
/// Conservative: only true on strong in-place signals AND absent new-thing signals, so a
/// genuine "add feature X" still gets the module-extraction guidance.
fn is_in_place_edit(task: &str) -> bool {
    let t = task.to_ascii_lowercase();
    // Strong signals that the change threads through EXISTING code.
    let in_place = [
        "call site",
        "call sites",
        "callers",
        "every caller",
        "signature",
        "parameter",
        "add a field",
        "add a param",
        "thread ",
        "wire it through",
        "wire through",
        "forward it",
        "pass &[]",
        "pass `&[]`",
    ]
    .iter()
    .any(|s| t.contains(s));
    // Signals that a genuinely new subsystem/module is wanted — these VETO in-place so the
    // module-extraction guidance still applies to real new features.
    let new_thing = [
        "new module",
        "new subsystem",
        "new system",
        "implement a ",
        "build a new",
        "add a new screen",
        "add a new page",
    ]
    .iter()
    .any(|s| t.contains(s));
    in_place && !new_thing
}

fn phase_instruction(phase: Phase, stack: ProjectStack, task: &str) -> String {
    // The Python eval-ladder instructions (test_app.py, pytest, Flask, one-subtask-per-file) are
    // guarded on `ProjectStack::Python`; a real Rust/JS project gets the non-Python branches
    // (an ordered design breakdown, and small atomic change-chunks) so it's never told to write
    // `test_app.py` or decompose per-file.
    match phase {
        Phase::StageBreakdown => {
            // Non-Python stacks (the plan-only Execute flow on a real Rust/JS project) don't do
            // the frozen-test TDD dance — they want a readable, ordered implementation breakdown
            // to review, not a pytest coverage array. Only the Python eval ladder gets the
            // test-first JSON below (kept byte-identical so the ladder is unaffected).
            if !matches!(stack, ProjectStack::Python) {
                // This breakdown now SUBSUMES the old implementation-plan phase: each stage carries
                // its concrete per-stage steps (file(s) + the ordered edits within), so a small
                // worker model can act on it without a separate implementation-planning pass.
                let head = "Break the work into an ORDERED set of small implementation stages — the \
                     concrete build sequence for this change. For each stage give: a short title, \
                     the file(s) it touches (from the layout), and an ORDERED list of the concrete \
                     steps to implement it — the specific functions/types to add or change and the \
                     call sites to update — precise enough to act on without a separate planning \
                     pass. Order stages so each builds on the last (foundations first, then what \
                     depends on them).\n\n";
                // The module-extraction directive is right for a NEW feature/subsystem (its core
                // logic belongs in a new small file so big files get only a tiny hook — this is what
                // landed the idle-city-sim lakes). But for an IN-PLACE edit (thread a parameter
                // through an existing fn and its call sites, add a field, change a signature) a new
                // module is pure over-engineering: it invents a file the task never asked for, adds
                // coupling, and the weak model then stalls wiring the extra seam (observed live on
                // void-claim: the plan invented collide_invited.rs and never reached character.rs).
                // Pick the guidance by task shape.
                let core = if is_in_place_edit(task) {
                    "CRITICAL — THIS IS AN IN-PLACE EDIT, NOT A NEW MODULE. Do NOT create any new \
                     file or module. Edit the EXISTING files named in the task directly: change the \
                     signature/struct where it already lives, then update its call sites. Each stage \
                     is one existing file's edit (or a signature stage + a call-sites stage). Keep \
                     the change minimal and local — inventing a new `<feature>.rs` module here is \
                     WRONG and will be rejected.\n\n"
                } else {
                    "CRITICAL — KEEP EDITS TO LARGE EXISTING FILES TINY. Put the feature's CORE \
                     LOGIC in a NEW, small module file (e.g. a new `<feature>.rs`), which the first \
                     stage CREATES whole. The existing large files should get only MINIMAL edits: \
                     one stage adds the module declaration (`mod <feature>;`) and a field, and one \
                     or two stages add a few lines to wire it in (a call, a match arm). Do NOT plan \
                     a stage that adds a large block of logic INTO an existing 500+ line file — that \
                     logic belongs in the new module; the existing file only gets the small hook. \
                     This keeps every edit to a big file down to a handful of lines.\n\n"
                };
                let tail = "This is a DESIGN breakdown to review, NOT tests and NOT code — do not \
                     write test files or source code. Output a short Markdown document with a \
                     numbered list of stages, each with its file(s) and its ordered steps.\n\n\
                     SCOPE DISCIPLINE — this is critical:\n\
                     • Touch the FEWEST files that actually deliver the change. A small feature is a \
                     handful of files (the new type + where it's stored + one place it's used), NOT \
                     every file that mentions a related word. Do NOT add a stage for a file just \
                     because it renders/handles something nearby — only files the change genuinely \
                     requires.\n\
                     • List each file AT MOST ONCE across the whole breakdown. Never create two \
                     stages that edit the same file with reworded steps, and never restate the \
                     list. If you're about to name a file you already used, STOP — you are done.\n\
                     • Prefer FEWER, well-scoped stages. If your breakdown exceeds ~8 stages for a \
                     single feature, you are over-scoping — collapse or drop the speculative ones.";
                return format!("{head}{core}{tail}");
            }
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
        // Non-Python (the Execute-plan flow on a real Rust/JS/etc project): decompose into
        // SMALL, ATOMIC change-chunks grounded in the REAL files — one self-contained edit each,
        // in dependency order. This is the "super small chunks" the chunked executor walks and
        // cargo-checks one at a time. Deliberately finer than one-per-file: a change that spans
        // several sites (add an enum variant, then fix each match on it) becomes SEPARATE chunks,
        // the foundational one first (the variant) so the dependents (the match arms) build on it.
        Phase::WorkDecomposition if !matches!(stack, ProjectStack::Python) => {
            "Break the change into the SMALLEST sensible ATOMIC steps — one self-contained edit \
             per subtask — grounded in the REAL files from the layout/breakdown above. Order them \
             so each builds on the last (foundations first). Rules:\n\
             • Each subtask is ONE concrete edit to ONE real file (use the exact paths above; do \
             NOT invent files). Split a multi-site change into one subtask PER site — e.g. \
             'add the enum variant' is one subtask, and adding the arm to EACH match that switches \
             on it is a SEPARATE subtask, each depending on the variant subtask.\n\
             • The goal must say exactly WHAT to change and WHERE (file + the function/match/type), \
             concrete enough to do without searching.\n\
             • Use deps to force order: the foundational change (new type/variant/signature) has \
             no deps; every dependent edit deps on it.\n\
             Output ONLY a JSON array; each item: \
             {\"id\":\"t1\",\"goal\":\"Add ... to ... in <file>\",\"files\":[\"<real file>\"],\"deps\":[]}. \
             No prose, just the JSON array."
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
        Phase::Architecture => {
            "Describe the DESIGN APPROACH for this change. A SURVEY of this project's REAL \
             existing files (and, for files the spec names, their full contents) is provided \
             ABOVE in the task — you MUST mine it. Ground every reuse claim in that survey: \
             name the SPECIFIC existing modules, files, types, and functions from THIS project \
             (by their real path/name) that this feature should REUSE or extend. Do NOT hand-wave \
             with generic categories like \"ECS patterns\" or \"the rendering system\" — if you \
             cite a pattern, point at the exact place in THIS codebase where it already lives. \
             Cover: the PATTERNS and abstractions to use (and where they already live here), the \
             existing code to REUSE (named), any shared HELPERS worth introducing, the KEY design \
             decisions and the DATA FLOW, and the BOUNDARIES between the parts. Naming existing \
             files to REUSE is required; but do NOT enumerate the NEW files to create or edit and \
             do NOT produce a file-change list — that is the next phase's (Layout's) job. Output a \
             short Markdown document. Output only the document."
                .to_string()
        }
        Phase::Layout => {
            "Take the spec and the approved architecture and produce the CONCRETE file-change \
             list: exactly which files to CREATE or EDIT (real paths where known), each with one \
             line on its responsibility. APPLY the architecture's patterns and reuse decisions — \
             reference them, don't re-explain the reasoning. This IS the per-file breakdown. \
             Output a short Markdown document — e.g. a list of files, each with its \
             responsibility.\n\
             CRITICAL: list each file EXACTLY ONCE. Do NOT repeat a file with reworded change \
             descriptions, and do NOT restate the list. One `## <path>` heading per file, one line \
             under it, then STOP. A small feature touches only a handful of files — if you find \
             yourself writing the same path again, you are done. Output only the document."
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
    use crate::stack::ProjectStack;
    use crate::state::Artifact;
    use sc_model::MockBackend;

    #[test]
    fn dedup_file_sections_collapses_repeated_files() {
        // The live sprawl: the same file appears in multiple reworded sections. Dedup keeps the
        // FIRST section per file and drops the rest; the preamble and no-file sections survive.
        let doc = "# Implementation Plan\n\n\
                   ## 1. Add enum in seat_types.rs\n\
                   - File: crates/sim/src/seat_types.rs\n\
                   - Add SeatType enum\n\n\
                   ## 2. Field in character.rs\n\
                   - File: crates/sim/src/character.rs\n\
                   - Add seat_type field\n\n\
                   ## 3. Rework seat_types.rs again\n\
                   - File: crates/sim/src/seat_types.rs\n\
                   - Add Display impl\n\n\
                   ## 4. More character.rs\n\
                   - File: crates/sim/src/character.rs\n\
                   - Add Clone\n";
        let out = dedup_file_sections(doc);
        assert!(out.contains("# Implementation Plan"), "preamble kept");
        // The FILE line for each path appears once (the first section's), not once per dup section.
        assert_eq!(
            out.matches("File: crates/sim/src/seat_types.rs").count(),
            1,
            "seat_types File line once"
        );
        assert_eq!(
            out.matches("File: crates/sim/src/character.rs").count(),
            1,
            "character File line once"
        );
        assert!(
            out.contains("Add SeatType enum"),
            "first seat_types section kept"
        );
        assert!(
            !out.contains("Add Display impl"),
            "duplicate seat_types section dropped"
        );
        assert!(
            !out.contains("Add Clone"),
            "duplicate character section dropped"
        );
    }

    #[test]
    fn dedup_file_sections_caps_runaway_and_keeps_no_file_sections() {
        // A no-file section (a summary) is kept; and a doc with more sections than the cap is
        // truncated to the cap (a runaway can't produce a 25-stage artifact).
        let mut doc = String::from("intro\n\n## Summary\n- overview, no file path\n\n");
        for i in 0..30 {
            doc.push_str(&format!(
                "## Stage {i}\n- File: crates/x/f{i}.rs\n- do it\n\n"
            ));
        }
        let out = dedup_file_sections(&doc);
        assert!(out.contains("## Summary"), "no-file section survives");
        let sections = out.matches("\n## ").count() + out.starts_with("## ") as usize;
        assert!(sections <= 12, "capped at MAX_SECTIONS, got {sections}");
    }

    #[test]
    fn extract_path_token_finds_real_paths_only() {
        assert_eq!(
            extract_path_token("- File: `crates/sim/src/seat_types.rs`"),
            Some("crates/sim/src/seat_types.rs".to_string())
        );
        assert_eq!(extract_path_token("## 1. Add the enum"), None); // no path
        assert_eq!(extract_path_token("just prose about seat_type"), None); // no slash+ext
    }

    #[test]
    fn rust_stack_prompt_names_cargo_not_flask() {
        // The language-aware fix: a Rust project's phase prompts must speak Rust/cargo, never
        // the Python/Flask default — else the orchestrator designs a Flask app.py for a Rust repo.
        let s = WorkflowState::new("add lakes to the terrain");
        let sys = phase_messages(
            Phase::Architecture,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Rust,
        )[0]
        .content
        .clone();
        assert!(sys.contains("cargo"), "Rust prompt names cargo: {sys}");
        assert!(sys.to_lowercase().contains("rust"));
        assert!(
            !sys.to_lowercase().contains("flask"),
            "no Flask in a Rust prompt: {sys}"
        );
        assert!(!sys.contains("app.py"), "no app.py in a Rust prompt");
    }

    #[test]
    fn non_python_stage_breakdown_asks_for_an_ordered_design_not_tests() {
        // The plan-only Execute flow: on a Rust project the stage breakdown must produce a
        // readable ordered breakdown, not a pytest coverage JSON array.
        let s = WorkflowState::new("add lakes");
        let msgs = phase_messages(
            Phase::StageBreakdown,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Rust,
        );
        let joined: String = msgs.iter().map(|m| m.content.clone()).collect();
        assert!(
            joined.to_lowercase().contains("ordered"),
            "ordered stages: {joined}"
        );
        assert!(
            !joined.contains("test_app.py"),
            "no pytest file for a Rust breakdown"
        );
        assert!(
            !joined.contains("JSON array"),
            "not the coverage JSON: {joined}"
        );
    }

    #[test]
    fn in_place_edit_task_forbids_a_new_module() {
        // Threading a param through existing fns + call sites must NOT be told to extract a
        // new module (that invented collide_invited.rs on void-claim and stalled at 2/3 files).
        let s = WorkflowState::new(
            "Add `invited_slots: &[u32]` to the tile_collide signature and pass &[] at every call site.",
        );
        let joined: String = phase_messages(
            Phase::StageBreakdown,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Rust,
        )
        .iter()
        .map(|m| m.content.clone())
        .collect();
        assert!(
            joined.contains("IN-PLACE EDIT"),
            "in-place directive present: {joined}"
        );
        assert!(
            !joined.contains("NEW, small module file"),
            "must NOT tell an in-place edit to extract a module"
        );
    }

    #[test]
    fn new_feature_task_keeps_the_module_extraction_guidance() {
        // A genuine new subsystem still gets the module-extraction directive (the lakes pattern).
        let s = WorkflowState::new("Add lakes to the terrain generator.");
        let joined: String = phase_messages(
            Phase::StageBreakdown,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Rust,
        )
        .iter()
        .map(|m| m.content.clone())
        .collect();
        assert!(
            joined.contains("NEW, small module file"),
            "new feature keeps module guidance"
        );
        assert!(!joined.contains("IN-PLACE EDIT"));
    }

    #[test]
    fn is_in_place_edit_heuristic() {
        assert!(is_in_place_edit("update every caller to pass &[]"));
        assert!(is_in_place_edit(
            "add a parameter to the integrate signature"
        ));
        assert!(!is_in_place_edit("add lakes to the terrain")); // new feature
                                                                // A "new module" ask vetoes even if it mentions call sites.
        assert!(!is_in_place_edit(
            "implement a new module and update its call sites"
        ));
    }

    #[test]
    fn python_stage_breakdown_still_asks_for_the_coverage_json() {
        let s = WorkflowState::new("build an API");
        let msgs = phase_messages(
            Phase::StageBreakdown,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
        );
        let joined: String = msgs.iter().map(|m| m.content.clone()).collect();
        assert!(
            joined.contains("JSON array"),
            "Python ladder keeps coverage JSON"
        );
        assert!(joined.contains("test_app.py"));
    }

    #[test]
    fn python_stack_keeps_the_original_flask_constraint() {
        let s = WorkflowState::new("build an API");
        let sys = phase_messages(
            Phase::Specs,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
        )[0]
        .content
        .clone();
        assert!(sys.contains("Flask"), "Python default preserved");
    }

    #[test]
    fn messages_include_task_and_approved_upstream() {
        let mut s = WorkflowState::new("build a CLI");
        s.set(Artifact::draft(Phase::Specs, "the spec text"));
        s.approve(Phase::Specs);
        let msgs = phase_messages(
            Phase::Architecture,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
        );
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
        let msgs = phase_messages(
            Phase::Specs,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
        );
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
        ] {
            s.set(Artifact::draft(p, body));
            s.approve(p);
        }
        let msgs = phase_messages(
            Phase::WorkDecomposition,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
        );
        let joined: String = msgs.iter().map(|m| m.content.clone()).collect();
        assert!(joined.contains("LAYOUT_FILES"), "needs the layout");
        assert!(joined.contains("STAGE_TESTS"), "needs the stage breakdown");
        assert!(!joined.contains("SPECS_PROSE"), "must drop the specs prose");
        assert!(
            !joined.contains("ARCH_PROSE"),
            "must drop the architecture prose"
        );
    }

    #[test]
    fn decomposition_phase_asks_for_json() {
        let s = WorkflowState::new("t");
        let msgs = phase_messages(
            Phase::WorkDecomposition,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
        );
        let joined: String = msgs.iter().map(|m| m.content.clone()).collect();
        assert!(joined.contains("JSON array"));
        assert!(joined.contains("\"files\""));
    }

    #[test]
    fn think_policy_appends_no_think_per_phase() {
        let s = WorkflowState::new("t");
        // Default: a doc phase gets /no_think; a JSON reasoning phase doesn't.
        let spec_sys = phase_messages(
            Phase::Specs,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
        )[0]
        .content
        .clone();
        assert!(spec_sys.contains("/no_think"), "{spec_sys}");
        let cov_sys = phase_messages(
            Phase::StageBreakdown,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
        )[0]
        .content
        .clone();
        assert!(!cov_sys.contains("/no_think"), "{cov_sys}");
        // A per-step override flips just that phase.
        let forced = ThinkPolicy::always_think().with(Phase::Specs, true);
        let spec2 = phase_messages(Phase::Specs, &s, forced, ProjectStack::Python)[0]
            .content
            .clone();
        assert!(spec2.contains("/no_think"));
    }

    #[test]
    fn generate_phase_returns_a_draft() {
        let backend = MockBackend::new(["# Specs\nGoals: ship it"]);
        let s = WorkflowState::new("ship it");
        let a = generate_phase(
            &backend,
            Phase::Specs,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
            &mut |_d| {},
        );
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
        let a = generate_phase(
            &backend,
            Phase::Specs,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
            &mut |_d| {},
        );
        assert!(a.content.contains("recovered"), "got: {:?}", a.content);
    }

    #[test]
    fn generate_phase_gives_up_empty_after_retries() {
        // Persistently empty (e.g. dead backend) → empty artifact; the runner turns
        // that into a loud error.
        let backend = MockBackend::new(["", "", "", ""]);
        let s = WorkflowState::new("t");
        let a = generate_phase(
            &backend,
            Phase::Specs,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
            &mut |_d| {},
        );
        assert!(a.content.is_empty());
    }

    #[test]
    fn generate_phase_recovers_from_a_transient_backend_error() {
        // A 503 "Loading model" while the Docker container reloads makes generate()
        // return Err. The engine must back off and retry, not give up — otherwise a
        // momentary blip kills the whole workflow (observed live 2026-06-15). Here the
        // first call errors, the second succeeds.
        use sc_model::{Capabilities, GenerateResponse, ToolCalling};
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
            fn generate(&self, _req: &GenerateRequest) -> sc_proto::Result<GenerateResponse> {
                let n = self.calls.get();
                self.calls.set(n + 1);
                if n == 0 {
                    Err(sc_proto::DcError::Backend("Loading model".to_string()))
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
        let a = generate_phase(
            &backend,
            Phase::Specs,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
            &mut |_d| {},
        );
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
            ProjectStack::Python,
            &mut |_d| {},
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
        let a = generate_phase(
            &backend,
            Phase::Specs,
            &s,
            ThinkPolicy::default(),
            ProjectStack::Python,
            &mut |_d| {},
        );
        assert!(a.content.contains("Goals"));
    }

    #[test]
    fn contains_json_array_detects_array_in_prose() {
        assert!(contains_json_array("blah [\n{\"id\":\"t1\"}\n] done"));
        assert!(!contains_json_array("no json here, just prose"));
        assert!(!contains_json_array("[]"), "empty array is not usable");
    }
}
