//! The model-backed drafting path.
//!
//! Everything here is authoring-time. A draft is never usable until a human has
//! read it, and the loop below exists to make sure what the human reads is at
//! least *valid* — parsed into the closed vocabulary, rendered to TOML that
//! actually loads, and run past the deterministic lints.
//!
//! The retry budget is deliberately small. A model that cannot satisfy a closed
//! eight-kind vocabulary in three attempts will not manage it on the fourth, and
//! spending Pro-tier tokens on a loop is the wrong failure mode. After the budget
//! is spent the best attempt is emitted **marked as rejected**, never silently
//! dropped — a drafting tool that quietly produces nothing has wasted the
//! author's time without telling them.

pub mod parse;
pub mod prompt;
pub mod render;

use sc_model::{GenerateRequest, ModelBackend};
use sc_proto::Result;

use crate::lint::{lint_pack, LintFinding};
use crate::sample::Sample;

pub use parse::{DraftCheck, ParseOutcome};
pub use render::Provenance;

/// How many corrective attempts follow the first try.
pub const MAX_RETRIES: usize = 2;

/// Tokens for one drafting reply. Generous — a control with five checks and
/// rationales runs long, and a truncated JSON array is a wasted round trip.
const DRAFT_MAX_TOKENS: usize = 4096;

/// Low, because this is a structured-extraction task, not a creative one.
const DRAFT_TEMPERATURE: f32 = 0.1;

/// One control to draft checks for.
#[derive(Debug, Clone)]
pub struct DraftRequest {
    pub framework: String,
    pub control_id: String,
    pub control_title: String,
    pub clause: String,
    pub intent: String,
    pub severity: String,
    /// The control's text from the framework catalog.
    pub control_text: String,
}

/// What the drafting loop produced.
#[derive(Debug, Clone)]
pub struct DraftResult {
    /// The rendered `[[controls]]` block.
    pub toml: String,
    /// Whether it parsed, validated and linted cleanly.
    pub accepted: bool,
    /// Attempts made, including the first.
    pub attempts: usize,
    /// Why it was rejected, if it was.
    pub errors: Vec<String>,
    /// Lint findings against the drafted control.
    pub lints: Vec<LintFinding>,
}

impl DraftResult {
    /// A banner for a rejected draft, so a human cannot mistake it for usable.
    pub fn rejection_banner(&self) -> Option<String> {
        if self.accepted {
            return None;
        }
        let mut s = String::from(
            "# ============================================================\n\
             # REJECTED DRAFT — DO NOT USE AS-IS\n\
             # This did not pass validation after every retry. It is included\n\
             # only so the work is not lost. Fix the problems below by hand.\n",
        );
        for e in &self.errors {
            s.push_str(&format!("#   - {e}\n"));
        }
        s.push_str("# ============================================================\n");
        Some(s)
    }
}

/// Draft checks for one control, validating and retrying as needed.
pub fn draft_control(
    backend: &dyn ModelBackend,
    req: &DraftRequest,
    prov: &Provenance,
    sample: Option<&Sample>,
) -> Result<DraftResult> {
    let mut messages = prompt::draft_messages(&req.framework, &req.control_id, &req.control_text);
    let mut errors: Vec<String> = Vec::new();

    for attempt in 1..=(MAX_RETRIES + 1) {
        let mut gen = GenerateRequest::new(messages.clone());
        gen.max_tokens = DRAFT_MAX_TOKENS;
        gen.temperature = DRAFT_TEMPERATURE;

        // A backend error is fatal here, unlike the planner's degrade-to-generic
        // behaviour: there is no useful fallback draft, and silently returning
        // nothing would look like the model declined the control.
        let reply = backend.generate(&gen)?;
        // Replayed verbatim into the retry so the model sees its own output.
        let last_reply = reply.content.clone();

        let parsed = parse::parse_drafts(&reply.content);
        errors = parsed.errors.clone();

        if parsed.checks.is_empty() {
            messages = prompt::retry_messages(
                &req.framework,
                &req.control_id,
                &req.control_text,
                &last_reply,
                &errors,
            );
            continue;
        }

        let toml = render::render_control(
            &req.control_id,
            &req.control_title,
            &req.clause,
            &req.intent,
            &req.severity,
            &parsed.checks,
            prov,
        );

        // Does it actually load? This catches everything `Pack::validate` knows
        // about — bad regex, look-around, duplicate ids, crossed thresholds.
        match validate_control_block(&toml) {
            Ok(pack) => {
                let report = lint_pack(&pack, sample);
                let blocking: Vec<LintFinding> = report.blocking().into_iter().cloned().collect();
                if errors.is_empty() && blocking.is_empty() {
                    return Ok(DraftResult {
                        toml,
                        accepted: true,
                        attempts: attempt,
                        errors: vec![],
                        lints: report.findings,
                    });
                }
                // Feed the lint findings back — they are phrased as instructions
                // precisely so a model can act on them.
                let mut feedback = errors.clone();
                feedback.extend(
                    blocking
                        .iter()
                        .map(|f| format!("{}: {} — {}", f.locus, f.summary, f.suggestion)),
                );
                if attempt > MAX_RETRIES {
                    return Ok(DraftResult {
                        toml,
                        accepted: false,
                        attempts: attempt,
                        errors: feedback,
                        lints: report.findings,
                    });
                }
                errors = feedback;
            }
            Err(e) => {
                errors.push(format!("the rendered TOML did not load: {e}"));
                if attempt > MAX_RETRIES {
                    return Ok(DraftResult {
                        toml,
                        accepted: false,
                        attempts: attempt,
                        errors,
                        lints: vec![],
                    });
                }
            }
        }

        messages = prompt::retry_messages(
            &req.framework,
            &req.control_id,
            &req.control_text,
            &last_reply,
            &errors,
        );
    }

    // Budget spent without a usable parse.
    Ok(DraftResult {
        toml: String::new(),
        accepted: false,
        attempts: MAX_RETRIES + 1,
        errors,
        lints: vec![],
    })
}

/// Wrap a rendered control block in a minimal pack and load it.
fn validate_control_block(control_toml: &str) -> Result<sc_comply::Pack> {
    let src = format!(
        "[framework]\n\
         id = \"draft\"\n\
         name = \"Draft\"\n\
         version = \"0.0.0\"\n\
         authority = \"draft\"\n\n\
         {control_toml}"
    );
    sc_comply::Pack::from_toml_str(&src)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_model::MockBackend;

    fn prov() -> Provenance {
        Provenance {
            model: "gemini-test".into(),
            generated_at: "2026-07-25T00:00:00Z".into(),
        }
    }

    fn request() -> DraftRequest {
        DraftRequest {
            framework: "ISO 27001".into(),
            control_id: "A.8.28".into(),
            control_title: "Secure coding".into(),
            clause: "ISO 27001 A.8.28".into(),
            intent: "Secure coding principles shall be applied.".into(),
            severity: "high".into(),
            control_text: "Secure coding principles shall be applied to software development."
                .into(),
        }
    }

    const GOOD_REPLY: &str = r#"[
      {"id":"ci-runs-tests","kind":"regex-match-in-glob",
       "glob":".github/workflows/*.yml","pattern":"cargo test",
       "on_match":"pass","on_no_match":"gap","on_no_files":"unknown",
       "rationale":"An automated test gate is evidenced in the pipeline."}
    ]"#;

    #[test]
    fn accepts_a_valid_first_attempt() {
        let backend = MockBackend::new([GOOD_REPLY]);
        let out = draft_control(&backend, &request(), &prov(), None).expect("draft");
        assert!(out.accepted, "{:?}", out.errors);
        assert_eq!(out.attempts, 1);
        assert!(out.toml.contains("# DRAFT (gemini-test"));
        assert!(out.toml.contains("A.8.28"));
    }

    #[test]
    fn retries_after_an_invented_check_kind_and_then_succeeds() {
        let bad = r#"[{"id":"x","kind":"grep-the-repo","on_match":"pass","on_no_match":"gap"}]"#;
        let backend = MockBackend::new([bad, GOOD_REPLY]);
        let out = draft_control(&backend, &request(), &prov(), None).expect("draft");
        assert!(out.accepted, "{:?}", out.errors);
        assert_eq!(out.attempts, 2, "should have taken exactly one retry");
    }

    #[test]
    fn gives_up_after_the_retry_budget_and_marks_the_draft_rejected() {
        let bad = r#"[{"id":"x","kind":"nonsense","on_match":"pass","on_no_match":"gap"}]"#;
        let backend = MockBackend::new([bad, bad, bad]);
        let out = draft_control(&backend, &request(), &prov(), None).expect("draft");
        assert!(!out.accepted);
        assert_eq!(out.attempts, MAX_RETRIES + 1);
        assert!(!out.errors.is_empty());
        let banner = out
            .rejection_banner()
            .expect("a rejected draft needs a banner");
        assert!(banner.contains("DO NOT USE AS-IS"));
    }

    #[test]
    fn a_draft_that_maps_indeterminate_to_pass_is_rejected_by_the_lints() {
        // The safety property: the loop will not hand a human a draft whose
        // unobservable case reports pass, even though it parses fine.
        let unsafe_reply = r#"[
          {"id":"tls","kind":"regex-match-in-glob","glob":"**/*.yml",
           "pattern":"min_tls","on_match":"pass","on_no_match":"gap",
           "on_no_files":"pass","rationale":"TLS floor."}
        ]"#;
        let backend = MockBackend::new([unsafe_reply, unsafe_reply, unsafe_reply]);
        let out = draft_control(&backend, &request(), &prov(), None).expect("draft");
        assert!(
            !out.accepted,
            "a pass-on-unobservable draft must not be accepted"
        );
        assert!(
            out.errors
                .iter()
                .any(|e| e.contains("indeterminate-maps-to-pass") || e.contains("never observed")),
            "{:?}",
            out.errors
        );
    }

    #[test]
    fn a_backend_error_propagates_rather_than_looking_like_an_empty_draft() {
        // MockBackend with an empty script always errors — the house idiom.
        let backend = MockBackend::new(Vec::<String>::new());
        assert!(draft_control(&backend, &request(), &prov(), None).is_err());
    }

    #[test]
    fn an_accepted_draft_has_no_rejection_banner() {
        let backend = MockBackend::new([GOOD_REPLY]);
        let out = draft_control(&backend, &request(), &prov(), None).expect("draft");
        assert!(out.rejection_banner().is_none());
    }

    #[test]
    fn tolerates_a_fenced_reply() {
        let fenced = format!("Here you go:\n```json\n{GOOD_REPLY}\n```");
        let backend = MockBackend::new([fenced]);
        let out = draft_control(&backend, &request(), &prov(), None).expect("draft");
        assert!(out.accepted, "{:?}", out.errors);
    }

    #[test]
    fn an_all_unknown_draft_for_an_org_control_is_accepted() {
        // The behaviour the prompt teaches: declaring a control undeterminable
        // is a correct answer and must survive the loop.
        let reply = r#"[
          {"id":"policy-published","kind":"file-exists","paths":["SECURITY.md"],
           "on_match":"unknown","on_no_match":"unknown","on_no_files":"unknown",
           "rationale":"Evidences that the policy is DOCUMENTED, never that it operates."}
        ]"#;
        let backend = MockBackend::new([reply]);
        let mut req = request();
        req.control_id = "A.5.1".into();
        req.control_title = "Policies for information security".into();
        req.intent = "Management shall define policies with board oversight.".into();
        let out = draft_control(&backend, &req, &prov(), None).expect("draft");
        assert!(out.accepted, "{:?}", out.errors);
    }
}
