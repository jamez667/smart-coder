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
pub mod narrative;
pub mod report;
pub mod sample;
pub mod worklist;

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

    /// Every pack shipped in `crates/sc-comply/packs/` must load and lint clean.
    ///
    /// This is the collection-wide version of the SOC 2 self-critique: as packs
    /// multiply, the risk is that quality quietly degrades with volume. Adding a
    /// pack automatically enrolls it here — there is no list to forget to update.
    #[test]
    fn every_shipped_pack_loads_and_has_no_blocking_findings() {
        let dir = repo_root().join("crates/sc-comply/packs");
        let sample = Sample::load(&repo_root());

        let mut packs: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("packs directory")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        packs.sort();

        assert!(
            packs.len() >= 5,
            "expected the shipped pack collection, found {}",
            packs.len()
        );

        let mut failures = Vec::new();
        for path in &packs {
            let name = path.file_stem().unwrap_or_default().to_string_lossy();
            let pack = match sc_comply::Pack::load(path) {
                Ok(p) => p,
                Err(e) => {
                    failures.push(format!("{name}: failed to load: {e}"));
                    continue;
                }
            };
            let report = lint_pack(&pack, Some(&sample));
            for f in report.blocking() {
                failures.push(format!(
                    "{name} [{}] {} — {}",
                    f.severity.label(),
                    f.locus,
                    f.summary
                ));
            }
        }

        assert!(
            failures.is_empty(),
            "{} blocking finding(s) across {} shipped pack(s):\n{}",
            failures.len(),
            packs.len(),
            failures.join("\n")
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
