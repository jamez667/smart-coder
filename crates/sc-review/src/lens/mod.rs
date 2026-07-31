//! One lens = one question = one model call (spec 16).
//!
//! [`prompt`] builds what the reviewer is asked; [`parse`] turns what it says
//! back into anchored findings; [`run_lens`] is the single call joining them.

pub mod parse;
pub mod prompt;

use sc_model::{GenerateRequest, Message, ModelBackend};

use crate::diff::IntegratedDiff;
use crate::finding::{Finding, Lens, ModelId};
use crate::ground::Grounding;

/// Enough room for a grounded prompt's answer; findings are short, but a reviewer
/// listing several needs more than a one-liner.
const MAX_TOKENS: usize = 1024;

/// Ask one reviewer one lens's question.
///
/// A backend error yields **no findings**, never an error: a reviewer that cannot
/// be reached is a skipped reviewer, not a failed review (spec 16). The caller
/// distinguishes the two by counting — see [`crate::engine`], which records the
/// skip rather than inferring it from a shorter `considered_by`.
pub fn run_lens(
    backend: &dyn ModelBackend,
    reviewer: &ModelId,
    lens: Lens,
    diff: &IntegratedDiff,
    grounding: &Grounding,
    goal: &str,
) -> Result<Vec<Finding>, sc_proto::DcError> {
    let p = prompt::build(lens, diff, grounding, goal);
    let mut req = GenerateRequest::new(vec![Message::system(p.system), Message::user(p.user)]);
    req.max_tokens = MAX_TOKENS;
    // Review is a judgement, not a generation: pin it as low as the seam allows so
    // two runs over the same diff say the same thing (spec 03 — determinism).
    req.temperature = 0.0;
    let reply = backend.generate(&req)?.content;
    Ok(parse::parse_findings(lens, &reply, diff, reviewer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{failing_backend, scripted};

    fn diff() -> IntegratedDiff {
        IntegratedDiff::from_changes([("a.rs", Some("fn a() {}\n"), Some("fn a() { b(); }\n"))])
    }

    #[test]
    fn a_lens_call_returns_its_parsed_findings() {
        let backend = scripted(vec![(
            "QUESTION: Does this change swallow",
            r#"[{"hunk":"H0","file":"a.rs","symbol":"a","severity":"high","summary":"swallowed"}]"#,
        )]);
        let out = run_lens(
            &backend,
            &ModelId::new("qwen"),
            Lens::ErrorHandling,
            &diff(),
            &Grounding::default(),
            "goal",
        )
        .expect("a reachable backend");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lens, Lens::ErrorHandling);
        assert_eq!(out[0].summary, "swallowed");
    }

    #[test]
    fn an_unreachable_reviewer_surfaces_as_an_error_for_the_caller_to_skip() {
        let err = run_lens(
            &failing_backend(),
            &ModelId::new("gone"),
            Lens::Duplication,
            &diff(),
            &Grounding::default(),
            "goal",
        );
        assert!(
            err.is_err(),
            "the caller decides; the lens does not swallow"
        );
    }
}
