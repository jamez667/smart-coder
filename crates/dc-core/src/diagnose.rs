//! Root-cause diagnosis — a focused debugger pass when the agent is stuck on a test failure
//! (spec 03 — recovery, the evolution of the shelved advisor in [`crate::advisor`]).
//!
//! A small model debugs blind: the failing-test observation it reacts to is a downstream
//! SYMPTOM, not the cause. A 404 from a misregistered route shows up as
//! `'NoneType' object is not subscriptable` at the assert line; a missing import shows up
//! three frames away. So the model edits the file the traceback points at — often the wrong
//! one — and loops. The per-symptom hints in `dc-verify`'s parser ([`crate`] doc) cover the
//! shapes we've hand-coded; this is the GENERAL fallback for the rest.
//!
//! Unlike the advisor (a separate model fed only a thin summary), this runs the SAME worker
//! model but feeds it the REAL inputs — the FULL untruncated test output and the FULL
//! contents of every source file — and asks for the single most likely root cause (file +
//! line + why + fix direction, not the solution). One model call, no loop. The harness
//! injects the report as the next observation so the model fixes the right file.

use dc_model::{GenerateRequest, Message, ModelBackend};

/// One source file handed to the diagnostician, verbatim.
pub struct SourceFile {
    pub path: String,
    pub contents: String,
}

/// Run one root-cause diagnosis. Returns the model's terse report, or `None` if it errored
/// or came back empty (the caller then falls through to the generic recovery path).
pub fn diagnose_failure(
    backend: &dyn ModelBackend,
    task: &str,
    verify_output: &str,
    sources: &[SourceFile],
) -> Option<String> {
    let system = "You are a debugger doing ROOT-CAUSE analysis. You are given a failing test \
        run and the FULL source of a small app. The traceback points at a SYMPTOM, not the \
        cause — a 404 from a misregistered route shows up as `'NoneType' object is not \
        subscriptable` at the assert line; a missing import shows up several frames away. Read \
        ALL the source and the WHOLE output, then name the SINGLE most likely root cause. Do \
        NOT write the fix. Do NOT restate the test. Reply EXACTLY in this shape:\n\
        FILE: <one source path>\n\
        LINE: <line number or symbol, best effort>\n\
        CAUSE: <one sentence — the actual defect>\n\
        FIX: <one short imperative — the direction, not the code>";

    let mut user = format!("TASK: {task}\n\nFULL TEST OUTPUT (untruncated):\n{verify_output}\n\nSOURCE FILES:\n");
    for f in sources {
        user.push_str(&format!("\n=== {} ===\n{}\n", f.path, f.contents));
    }

    let mut req = GenerateRequest::new(vec![Message::system(system.to_string()), Message::user(user)]);
    // Room to reason then answer (a thinking model spends tokens internally first — the same
    // rationale as the advisor's cap); terseness comes from the prompt, not the cap.
    req.max_tokens = 800;
    match backend.generate(&req) {
        Ok(resp) => {
            let report = resp.content.trim();
            if report.is_empty() {
                None
            } else {
                Some(report.to_string())
            }
        }
        Err(_) => None,
    }
}

/// Wrap a diagnosis as the observation injected into the stuck model's loop, framed so the
/// model trusts it over the raw symptom and fixes the file it names.
pub fn diagnosis_observation(report: &str) -> String {
    format!(
        "ROOT-CAUSE DIAGNOSIS (a deep read of the FULL test output + ALL source files by a \
         focused debugger — trust this over the raw symptom above; fix the file it names, not \
         the one the traceback points at):\n{report}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_model::{Capabilities, GenerateResponse, MockBackend, ToolCalling};
    use dc_proto::Result;

    fn sources() -> Vec<SourceFile> {
        vec![
            SourceFile {
                path: "app.py".into(),
                contents: "register_blueprint(tasks, url_prefix='/tasks')".into(),
            },
            SourceFile {
                path: "store.py".into(),
                contents: "def add(t): ...".into(),
            },
        ]
    }

    #[test]
    fn returns_a_diagnosis_from_the_backend() {
        let backend = MockBackend::new([
            "FILE: app.py\nLINE: 5\nCAUSE: url_prefix doubles the route to /tasks/tasks\nFIX: drop the prefix",
        ]);
        let report = diagnose_failure(&backend, "todo board", "404 ...", &sources()).unwrap();
        assert!(report.contains("app.py") && report.contains("url_prefix"));
        let obs = diagnosis_observation(&report);
        assert!(obs.contains("ROOT-CAUSE DIAGNOSIS") && obs.contains("app.py"));
    }

    #[test]
    fn the_diagnostician_sees_the_full_output_and_sources() {
        // An Echo backend returns the last user message, so we can assert the FULL output and
        // each source (filename + contents) actually reached the prompt — the difference from
        // the advisor, which only ever saw a summary.
        struct Echo;
        impl ModelBackend for Echo {
            fn name(&self) -> &str {
                "echo"
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities {
                    max_context_tokens: 8192,
                    tool_calling: ToolCalling::None,
                    on_device: false,
                }
            }
            fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
                Ok(GenerateResponse {
                    content: req.messages.last().unwrap().content.clone(),
                })
            }
        }
        let echoed = diagnose_failure(&Echo, "todo board", "E TypeError NoneType at test_app.py:12", &sources())
            .unwrap();
        assert!(echoed.contains("NoneType at test_app.py:12"), "full output missing");
        assert!(echoed.contains("=== app.py ==="), "source filename missing");
        assert!(echoed.contains("url_prefix='/tasks'"), "source contents missing");
    }

    #[test]
    fn empty_or_errored_diagnosis_is_none() {
        assert!(diagnose_failure(&MockBackend::new(["   "]), "t", "o", &sources()).is_none());
        // An exhausted MockBackend errors on generate.
        assert!(
            diagnose_failure(&MockBackend::new(Vec::<String>::new()), "t", "o", &sources()).is_none()
        );
    }
}
