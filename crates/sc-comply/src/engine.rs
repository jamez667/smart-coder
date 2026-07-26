//! The audit driver: pack + workspace in, evidence pack out.
//!
//! The flow is deliberately flat. Scan once, then for each control: evaluate
//! `not_applicable_if`, run its checks through the registry, map each raw
//! observation through the check's outcome policy, and aggregate. All the
//! judgment lives in `aggregate.rs` and `status.rs`; this module only
//! sequences it.
//!
//! See `docs/specs/13-compliance-evidence.md`.

use std::path::Path;

use sc_proto::Result;

use crate::aggregate::aggregate;
use crate::collector::{AuditContext, ComplyOptions, Observation, Registry};
use crate::evidence::{now_rfc3339, CheckResult, ControlResult, EvidencePack, FrameworkMeta};
use crate::pack::{Check, CheckKind, Control, Pack};
use crate::scan::scan_workspace;
use crate::status::{ControlStatus, Outcome};

/// Audit `workspace` against `pack`, using the built-in collectors and a
/// wall-clock timestamp.
pub fn audit(workspace: &Path, pack: &Pack, options: &ComplyOptions) -> Result<EvidencePack> {
    audit_with(
        workspace,
        pack,
        options,
        &Registry::builtin(),
        now_rfc3339(),
    )
}

/// Audit with an explicit registry and timestamp.
///
/// The injected `generated_at` is what makes report output deterministic in
/// tests — a renderer that samples a live clock cannot be asserted on. The
/// explicit registry is the seam for a future retrieval collector.
pub fn audit_with(
    workspace: &Path,
    pack: &Pack,
    options: &ComplyOptions,
    registry: &Registry,
    generated_at: String,
) -> Result<EvidencePack> {
    // Scan once for the whole run, not once per check.
    let files = scan_workspace(workspace);
    let ctx = AuditContext::new(workspace, &files, options);

    let controls = pack
        .controls
        .iter()
        .map(|c| evaluate_control(c, registry, &ctx))
        .collect();

    Ok(EvidencePack::new(
        FrameworkMeta {
            id: pack.framework.id.clone(),
            name: pack.framework.name.clone(),
            version: pack.framework.version.clone(),
            authority: pack.framework.authority.clone(),
        },
        workspace.to_string_lossy().replace('\\', "/"),
        generated_at,
        pack.framework.scope_note.trim().to_string(),
        controls,
        options.disabled_capabilities(),
    ))
}

/// Evaluate one control end to end.
fn evaluate_control(
    control: &Control,
    registry: &Registry,
    ctx: &AuditContext<'_>,
) -> ControlResult {
    // `not_applicable_if` runs first; when it fires, no check runs at all.
    if let Some(kind) = &control.not_applicable_if {
        if let Some(reason) = not_applicable_reason(control, kind, registry, ctx) {
            return ControlResult {
                id: control.id.clone(),
                title: control.title.clone(),
                section: control.section,
                clause: control.clause.clone(),
                intent: control.intent.trim().to_string(),
                severity: control.severity,
                status: ControlStatus::NotApplicable,
                checks: vec![],
                rationale: reason,
                remediation: control.remediation.clone(),
            };
        }
    }

    let checks: Vec<CheckResult> = control
        .checks
        .iter()
        .map(|k| evaluate_check(control, k, registry, ctx))
        .collect();

    let (status, rationale) = aggregate(control.aggregate, &checks, &control.weight_cfg());

    ControlResult {
        id: control.id.clone(),
        title: control.title.clone(),
        section: control.section,
        clause: control.clause.clone(),
        intent: control.intent.trim().to_string(),
        severity: control.severity,
        status,
        checks,
        rationale,
        remediation: control.remediation.clone(),
    }
}

/// Evaluate a single check, mapping the raw observation through its policy.
fn evaluate_check(
    control: &Control,
    check: &Check,
    registry: &Registry,
    ctx: &AuditContext<'_>,
) -> CheckResult {
    let qualified = format!("{}/{}", control.id, check.id);

    let Some(collector) = registry.resolve(&check.kind) else {
        // No collector claims this kind. That is a tool-configuration failure,
        // not a compliance judgment, so it surfaces as Error and dominates the
        // control rather than quietly disappearing.
        return CheckResult {
            check_id: qualified,
            kind: check.kind.label().to_string(),
            status: ControlStatus::Error,
            weight: check.weight,
            evidence: vec![],
            note: Some(format!(
                "no collector handles check kind {}",
                check.kind.label()
            )),
            rationale: check.rationale.trim().to_string(),
        };
    };

    match collector.collect(check, ctx) {
        Ok(obs) => {
            let status = check.policy().resolve(obs.matched);
            CheckResult {
                check_id: qualified.clone(),
                kind: check.kind.label().to_string(),
                status,
                weight: check.weight,
                // Re-anchor citations to the fully-qualified id so a reader can
                // trace any line in the report back to its control.
                evidence: obs
                    .evidence
                    .into_iter()
                    .map(|mut e| {
                        e.check_id = qualified.clone();
                        e
                    })
                    .collect(),
                note: obs.note,
                rationale: check.rationale.trim().to_string(),
            }
        }
        Err(e) => CheckResult {
            check_id: qualified,
            kind: check.kind.label().to_string(),
            status: ControlStatus::Error,
            weight: check.weight,
            evidence: vec![],
            note: Some(format!("collector {} failed: {e}", collector.name())),
            rationale: check.rationale.trim().to_string(),
        },
    }
}

/// Run a control's `not_applicable_if` probe.
///
/// Returns `Some(reason)` when the control should be skipped. A probe that
/// errors or cannot determine an answer does **not** skip the control: when in
/// doubt, evaluate it. Silently marking a control N/A on a broken probe would
/// remove it from the denominator and inflate the score.
fn not_applicable_reason(
    control: &Control,
    kind: &CheckKind,
    registry: &Registry,
    ctx: &AuditContext<'_>,
) -> Option<String> {
    let probe = Check {
        id: "not-applicable-if".to_string(),
        kind: kind.clone(),
        on_match: Outcome::NotApplicable,
        on_no_match: Outcome::Pass,
        on_no_files: None,
        weight: 1.0,
        exclude_globs: vec![],
        tracked_only: false,
        rationale: String::new(),
    };
    let collector = registry.resolve(kind)?;
    match collector.collect(&probe, ctx) {
        Ok(Observation {
            matched: Some(true),
            ..
        }) => Some(format!(
            "not applicable to this codebase: {} held",
            kind.describe()
        )),
        _ => {
            let _ = control;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Severity;
    use crate::test_support::{temp_repo, write};

    fn soc2() -> Pack {
        // Assembled from the pack directory: shipped packs are split by section.
        crate::registry::load_shipped("soc2").expect("shipped soc2 loads")
    }

    fn run(root: &Path) -> EvidencePack {
        audit_with(
            root,
            &soc2(),
            &ComplyOptions::default(),
            &Registry::builtin(),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .expect("audit")
    }

    fn control<'a>(pack: &'a EvidencePack, id: &str) -> &'a ControlResult {
        pack.controls
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no control {id}"))
    }

    /// A workspace with one planted gap, one satisfied control, and one
    /// genuinely unobservable control. This is the test that proves the whole
    /// pipeline end to end.
    fn synthetic_repo(tag: &str) -> std::path::PathBuf {
        let root = temp_repo(tag);

        // CC6.1: a planted private key AND a tracked .env -> definite gaps.
        write(
            &root,
            "deploy/id_rsa",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIE...\n",
        );
        write(&root, ".env", "API_TOKEN=hunter2\n");
        write(&root, ".gitignore", "target/\n");

        // CC8.1: full change-management evidence -> should pass.
        write(
            &root,
            ".github/workflows/ci.yml",
            "on: push\njobs:\n  t:\n    steps:\n      - run: cargo test\n",
        );
        write(&root, "CONTRIBUTING.md", "Open a PR.\n");
        write(&root, "CODEOWNERS", "* @team\n");

        // Clean source, so CC6.6's must-not-match checks pass.
        write(&root, "src/lib.rs", "pub fn init_tracing() {}\n");

        root
    }

    #[test]
    fn end_to_end_audit_produces_expected_statuses() {
        let root = synthetic_repo("engine-e2e");
        let pack = run(&root);

        // Every control in the pack is evaluated; none silently vanish.
        assert_eq!(pack.score.total, soc2().controls.len());
        assert_eq!(pack.score.total, pack.controls.len());

        // CC6.1: the planted key and tracked .env are found.
        let cc61 = control(&pack, "CC6.1");
        assert_eq!(cc61.status, ControlStatus::Gap, "{}", cc61.rationale);
        let cited: Vec<String> = cc61.all_evidence().iter().map(|e| e.locator()).collect();
        assert!(
            cited.iter().any(|l| l.starts_with("deploy/id_rsa")),
            "{cited:?}"
        );
        assert!(cited.iter().any(|l| l.starts_with(".env")), "{cited:?}");

        // CC8.1: CI + tests + CONTRIBUTING + CODEOWNERS is enough weight.
        let cc81 = control(&pack, "CC8.1");
        assert_eq!(cc81.status, ControlStatus::Pass, "{}", cc81.rationale);

        // CC9.2 is unobservable from source by construction.
        assert_eq!(control(&pack, "CC9.2").status, ControlStatus::Unknown);

        // No tool failures anywhere.
        assert_eq!(pack.score.errors, 0, "unexpected collector errors");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_clean_repo_does_not_report_the_planted_gaps() {
        let root = temp_repo("engine-clean");
        write(&root, ".gitignore", ".env\n*.key\n*.pem\n");
        write(&root, ".gitleaks.toml", "[allowlist]\n");
        write(&root, "src/lib.rs", "pub fn init_tracing() {}\n");
        write(
            &root,
            ".github/workflows/ci.yml",
            "jobs:\n  t:\n    steps:\n      - run: cargo test\n",
        );
        write(&root, "CONTRIBUTING.md", "PRs welcome\n");

        let pack = run(&root);
        let cc61 = control(&pack, "CC6.1");
        assert_eq!(cc61.status, ControlStatus::Pass, "{}", cc61.rationale);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_workspace_yields_unknowns_not_a_clean_bill_of_health() {
        // The most dangerous failure mode: an engine that finds nothing and
        // reports everything as fine.
        let root = temp_repo("engine-empty");
        let pack = run(&root);

        assert_eq!(pack.score.errors, 0);
        assert!(
            pack.score.unknown > 0,
            "an empty workspace must produce unknowns, got {}",
            pack.score.summary_line()
        );
        // Determinacy should be poor, and that must be visible.
        assert!(
            pack.score.determinacy() < 1.0,
            "determinacy {} implies we saw everything in an empty repo",
            pack.score.determinacy()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn command_checks_are_reported_as_disabled() {
        let root = temp_repo("engine-cmd");
        let pack = run(&root);
        assert_eq!(
            pack.disabled_capabilities,
            vec!["command-exit-code".to_string()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn evidence_is_traceable_to_its_control_and_check() {
        let root = synthetic_repo("engine-trace");
        let pack = run(&root);

        for c in &pack.controls {
            for e in c.all_evidence() {
                assert!(
                    e.check_id.starts_with(&format!("{}/", c.id)),
                    "evidence {:?} is not traceable to control {}",
                    e.check_id,
                    c.id
                );
                assert!(!e.produced_by.is_empty());
            }
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A control that does not apply to containerless projects.
    ///
    /// Note the sense of `file-absent`: it "matches" when the path EXISTS, so
    /// this fires — and skips the control — for a repo that has no Dockerfile
    /// only if we invert it. Here we use `file-exists` on a marker that means
    /// "this project opted out", which is the natural way to write it.
    const NA_PACK: &str = r#"
[framework]
id = "t"
name = "T"
version = "1"
authority = "A"

[[controls]]
id = "R1"
title = "Container hardening"
not_applicable_if = { kind = "file-exists", paths = [".no-containers"] }
checks = [
  { id = "c1", kind = "file-exists", paths = ["never-exists"], on_match = "pass", on_no_match = "gap" },
]
"#;

    #[test]
    fn not_applicable_if_skips_a_control_without_running_its_checks() {
        let pack_def = Pack::from_toml_str(NA_PACK).expect("parse");
        let root = temp_repo("engine-na");
        // The opt-out marker is present, so the control is skipped entirely.
        write(&root, ".no-containers", "");

        let out = audit_with(
            &root,
            &pack_def,
            &ComplyOptions::default(),
            &Registry::builtin(),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .expect("audit");

        let c = &out.controls[0];
        assert_eq!(c.status, ControlStatus::NotApplicable);
        assert!(c.checks.is_empty(), "checks must not run when N/A fires");
        assert_eq!(out.score.in_scope(), 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn not_applicable_if_that_does_not_fire_leaves_the_control_in_scope() {
        let pack_def = Pack::from_toml_str(NA_PACK).expect("parse");
        let root = temp_repo("engine-na-off");
        // No opt-out marker, so the control is evaluated normally.
        write(&root, "Cargo.toml", "[package]\nname = \"x\"\n");

        let out = audit_with(
            &root,
            &pack_def,
            &ComplyOptions::default(),
            &Registry::builtin(),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .expect("audit");

        assert_eq!(out.controls[0].status, ControlStatus::Gap);
        assert_eq!(out.score.in_scope(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn not_applicable_if_uses_file_absent_match_sense_consistently() {
        // `file-absent` "matches" when the path EXISTS. Pinning it here because
        // the inversion is genuinely easy to get backwards when authoring a
        // pack, and getting it backwards silently removes a control from the
        // scored denominator.
        let src = r#"
[framework]
id = "t"
name = "T"
version = "1"
authority = "A"

[[controls]]
id = "R1"
title = "Skipped when .env exists"
not_applicable_if = { kind = "file-absent", path = ".env" }
checks = [
  { id = "c1", kind = "file-exists", paths = ["never-exists"], on_match = "pass", on_no_match = "gap" },
]
"#;
        let pack_def = Pack::from_toml_str(src).expect("parse");

        let with_env = temp_repo("engine-na-sense-a");
        write(&with_env, ".env", "X=1\n");
        let out = audit_with(
            &with_env,
            &pack_def,
            &ComplyOptions::default(),
            &Registry::builtin(),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .expect("audit");
        assert_eq!(
            out.controls[0].status,
            ControlStatus::NotApplicable,
            "file-absent matches when the file is PRESENT"
        );
        let _ = std::fs::remove_dir_all(&with_env);

        let without_env = temp_repo("engine-na-sense-b");
        write(&without_env, "README.md", "hi\n");
        let out2 = audit_with(
            &without_env,
            &pack_def,
            &ComplyOptions::default(),
            &Registry::builtin(),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .expect("audit");
        assert_eq!(out2.controls[0].status, ControlStatus::Gap);
        let _ = std::fs::remove_dir_all(&without_env);
    }

    #[test]
    fn an_unhandled_check_kind_is_an_error_not_a_silent_pass() {
        // A registry with no collectors: every check must surface as Error.
        let root = temp_repo("engine-nocollectors");
        let out = audit_with(
            &root,
            &soc2(),
            &ComplyOptions::default(),
            &Registry::new(vec![]),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .expect("audit");

        assert_eq!(out.score.passed, 0);
        assert!(out.score.errors > 0, "{}", out.score.summary_line());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn severity_and_intent_survive_into_the_result() {
        let root = temp_repo("engine-meta");
        let pack = run(&root);
        let cc61 = control(&pack, "CC6.1");
        assert_eq!(cc61.severity, Severity::Critical);
        assert!(!cc61.intent.is_empty());
        assert!(cc61.remediation.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Run the shipped pack against this repository.
    ///
    /// Asserts only that the engine survives a real tree without erroring —
    /// deliberately not the statuses, which change as the repo changes and
    /// would rot the test. This catches the class of bug that synthetic
    /// fixtures never do.
    #[test]
    fn audits_its_own_workspace() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf();

        let pack = run(&root);
        assert_eq!(pack.score.errors, 0, "collector errors on the real repo");
        assert_eq!(pack.score.total, soc2().controls.len());
        assert!(pack.score.total > 0);
    }
}
