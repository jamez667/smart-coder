//! Authoring-time assistant for `sc-comply` framework packs.
//!
//! Two jobs, in descending order of value:
//!
//! 1. **Critique** an existing pack with deterministic lints — catching the
//!    `on_no_files` mistakes, unreachable patterns and over-claiming controls
//!    that turn an evidence pack into a confident lie.
//! 2. **Draft** checks for a new framework from control-catalog text, via a
//!    model. Every draft is validated and linted before a human sees it, and
//!    carries a provenance marker.
//!
//! **This crate never runs during an audit.** That is enforced structurally:
//! `sc-comply` has no dependency on `sc-model`, so the audit path cannot reach a
//! model even by accident. An evidence pack has to be reproducible, and that
//! property is the whole reason anyone should trust it.
//!
//! See `docs/specs/14-pack-authoring.md`.

pub mod draft;
pub mod eval;
pub mod lint;
pub mod report;
pub mod sample;

#[cfg(test)]
mod test_support;

pub use draft::{draft_control, DraftRequest, DraftResult, Provenance};
pub use eval::{run_suite, EvalSuite, ModelScore, Verdict};
pub use lint::{lint_pack, LintCtx, LintFinding, LintReport};
pub use sample::Sample;

#[cfg(test)]
mod self_critique {
    //! The test that audits the shipped SOC 2 pack.
    //!
    //! It runs every lint against `crates/sc-comply/packs/soc2-tsc.toml` with
    //! this repository as the sample workspace. The shipped pack encodes real
    //! authoring judgment, and this is what keeps that judgment honest.

    use super::*;
    use std::path::{Path, PathBuf};

    const SOC2: &str = include_str!("../../sc-comply/packs/soc2-tsc.toml");

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf()
    }

    #[test]
    fn the_shipped_soc2_pack_has_no_blocking_findings() {
        let pack = sc_comply::Pack::from_toml_str(SOC2).expect("shipped pack parses");
        let sample = Sample::load(&repo_root());
        let report = lint_pack(&pack, Some(&sample));

        let blocking = report.blocking();
        assert!(
            blocking.is_empty(),
            "the shipped SOC 2 pack has {} blocking finding(s):\n{}",
            blocking.len(),
            blocking
                .iter()
                .map(|f| format!(
                    "  [{}] {} — {}\n      fix: {}",
                    f.severity.label(),
                    f.locus,
                    f.summary,
                    f.suggestion
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn linting_the_shipped_pack_needs_no_model_and_does_not_panic() {
        // Also the smoke test that every lint survives a real 4k-file tree.
        let pack = sc_comply::Pack::from_toml_str(SOC2).expect("parses");
        let sample = Sample::load(&repo_root());
        let report = lint_pack(&pack, Some(&sample));
        assert!(report.had_sample);
        assert!(!report.framework.is_empty());
    }
}
