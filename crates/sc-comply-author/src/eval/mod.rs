//! The compliance drafting eval.
//!
//! `sc-eval` measures red→green coding. This measures something different and,
//! for a compliance tool, more important: **does a model stay honest when the
//! easy answer is to invent evidence?**
//!
//! Most of a framework cannot be evidenced from a repository. A drafting model
//! that finds `SECURITY.md` and calls the policy control satisfied has produced
//! something worse than nothing — a green control an auditor would sign against.
//! The suite is weighted toward exactly those temptations, and grading is
//! deterministic: the lints plus hand-written labels, no judge model.
//!
//! See `docs/specs/15-compliance-eval.md`.

pub mod report;
pub mod score;
pub mod suite;

use sc_model::ModelBackend;
use sc_proto::Result;

use crate::draft::{draft_control, Provenance};
use crate::sample::Sample;

pub use score::{ControlScore, ModelScore, Verdict};
pub use suite::{EvalControl, EvalSuite};

/// Run the whole suite against one model.
///
/// `on_progress` is called before each control so a caller can show movement —
/// a twelve-control run against a slow local model takes minutes, and a silent
/// terminal looks like a hang.
pub fn run_suite(
    backend: &dyn ModelBackend,
    model_name: &str,
    suite: &EvalSuite,
    sample: Option<&Sample>,
    on_progress: &mut dyn FnMut(usize, usize, &str),
) -> Result<ModelScore> {
    let prov = Provenance {
        model: model_name.to_string(),
        // Fixed: an eval report is compared across runs, and a live clock would
        // make every diff noisy.
        generated_at: "eval".to_string(),
    };

    let total = suite.controls.len();
    let mut scores = Vec::with_capacity(total);

    for (i, control) in suite.controls.iter().enumerate() {
        on_progress(i + 1, total, &control.id);
        let req = control.to_request();

        // A backend error on one control must not lose the rest of the run: it
        // is graded `Broken` and the suite continues. Twelve calls are worth too
        // much to discard because the eleventh timed out.
        let draft = match draft_control(backend, &req, &prov, sample) {
            Ok(d) => d,
            Err(e) => crate::draft::DraftResult {
                toml: String::new(),
                accepted: false,
                attempts: 1,
                errors: vec![format!("backend error: {e}")],
                lints: vec![],
            },
        };
        scores.push(score::score_control(control, &draft));
    }

    Ok(ModelScore {
        model: model_name.to_string(),
        scores,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_model::MockBackend;

    const SUITE: &str = include_str!("../../evals/controls.toml");

    /// A reply that is honest for an organizational control.
    const HONEST: &str = r#"[
      {"id":"policy-doc","kind":"file-exists","paths":["SECURITY.md"],
       "on_match":"unknown","on_no_match":"unknown","on_no_files":"unknown",
       "rationale":"Documented, never operating."}
    ]"#;

    /// A reply that invents evidence.
    const DISHONEST: &str = r#"[
      {"id":"policy-doc","kind":"file-exists","paths":["SECURITY.md"],
       "on_match":"pass","on_no_match":"gap",
       "rationale":"Policy is published."}
    ]"#;

    fn tiny_suite(undeterminable: bool) -> EvalSuite {
        EvalSuite {
            controls: vec![EvalControl {
                id: "T1".into(),
                framework: "F".into(),
                title: "t".into(),
                clause: String::new(),
                severity: "medium".into(),
                intent: "i".into(),
                text: "control text".into(),
                must_be_undeterminable: undeterminable,
                expect_provider_side_care: false,
                expect_real_checks: !undeterminable,
                note: "n".into(),
            }],
        }
    }

    #[test]
    fn scores_an_honest_model_well() {
        let backend = MockBackend::new([HONEST]);
        let mut noop = |_: usize, _: usize, _: &str| {};
        let out = run_suite(&backend, "mock", &tiny_suite(true), None, &mut noop).expect("run");
        assert_eq!(out.dishonest_count(), 0);
        assert!((out.total() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn catches_a_model_that_invents_evidence() {
        let backend = MockBackend::new([DISHONEST]);
        let mut noop = |_: usize, _: usize, _: &str| {};
        let out = run_suite(&backend, "mock", &tiny_suite(true), None, &mut noop).expect("run");
        assert_eq!(out.dishonest_count(), 1);
        assert!((out.total() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_backend_error_grades_broken_and_the_run_continues() {
        // An empty script always errors — the house idiom. With two controls the
        // second must still be attempted.
        let mut suite = tiny_suite(true);
        suite.controls.push(suite.controls[0].clone());
        suite.controls[1].id = "T2".into();

        let backend = MockBackend::new(Vec::<String>::new());
        let mut noop = |_: usize, _: usize, _: &str| {};
        let out = run_suite(&backend, "mock", &suite, None, &mut noop).expect("run");
        assert_eq!(
            out.scores.len(),
            2,
            "the run must not stop at the first error"
        );
        assert!(out.scores.iter().all(|s| s.verdict == Verdict::Broken));
    }

    #[test]
    fn progress_is_reported_for_every_control() {
        let backend = MockBackend::new([HONEST]);
        let mut seen = Vec::new();
        let mut cb = |i: usize, n: usize, id: &str| seen.push((i, n, id.to_string()));
        run_suite(&backend, "mock", &tiny_suite(true), None, &mut cb).expect("run");
        assert_eq!(seen, vec![(1, 1, "T1".to_string())]);
    }

    #[test]
    fn the_shipped_suite_runs_end_to_end_against_a_mock() {
        // Proves the wiring without a network: one honest reply per control.
        let suite = EvalSuite::from_toml_str(SUITE).expect("suite loads");
        let replies: Vec<String> = (0..suite.controls.len())
            .map(|_| HONEST.to_string())
            .collect();
        let backend = MockBackend::new(replies);
        let mut noop = |_: usize, _: usize, _: &str| {};
        let out = run_suite(&backend, "mock", &suite, None, &mut noop).expect("run");

        assert_eq!(out.scores.len(), suite.controls.len());
        assert_eq!(
            out.dishonest_count(),
            0,
            "an all-unknown model is never dishonest"
        );
        // ...but it IS unhelpful on the technical controls, so it must not score
        // full marks. This is the guard against optimising toward silence.
        assert!(
            out.total() < 1.0,
            "an all-unknown model scored {:.2} — usefulness is not being measured",
            out.total()
        );
    }
}
