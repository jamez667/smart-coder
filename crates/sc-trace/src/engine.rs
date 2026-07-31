//! The one entry point: read the specs, resolve every anchor, check coverage.
//!
//! Deterministic and model-free. Same walk → declare → resolve → report shape as
//! `sc-comply`, turned inward: the target is this repository's own documentation
//! rather than a regulatory framework (spec 17).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::anchor::{parse_anchors, AnchorKind};
use crate::coverage::{coverage, CrateCoverage};
use crate::manifest::Workspace;
use crate::resolve::{resolve, Resolution};
use crate::scan::read_specs;
use crate::status::{ClaimStatus, Tally};

/// One anchored claim, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// The spec that made the claim, workspace-relative.
    pub spec: String,
    /// 1-based line in that spec.
    pub line: usize,
    /// What the anchor pointed at.
    pub target: String,
    pub status: ClaimStatus,
    /// Why, whenever the status is not plain `Ok`. Mandatory for anything a
    /// human has to act on.
    pub detail: Option<String>,
    /// Where it landed: `path:line`.
    pub location: Option<String>,
}

/// A crate no spec describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ungoverned {
    pub krate: String,
}

/// The whole result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceReport {
    /// Anchored claims, worst first.
    pub claims: Vec<Claim>,
    /// Crates no spec mentions. Warns; never fails the check.
    pub ungoverned: Vec<Ungoverned>,
    /// Counts per status. No headline score, by design.
    pub tally: Tally,
}

impl TraceReport {
    /// How many findings would fail `trace --check` — `Broken` and `Stale` only.
    pub fn blocking(&self) -> usize {
        self.tally.blocking()
    }

    /// The claims a human needs to act on, worst first.
    pub fn problems(&self) -> Vec<&Claim> {
        self.claims
            .iter()
            .filter(|c| c.status != ClaimStatus::Ok)
            .collect()
    }
}

/// Check every anchored claim in `root`'s specs against `root`'s code.
pub fn trace(root: &Path) -> TraceReport {
    let ws = Workspace::load(root);
    let specs = read_specs(root);

    let mut claims = Vec::new();
    // Which crate each resolvable anchor pointed at, so an anchor governs its
    // crate even when the prose never spells the name.
    let mut anchor_targets: Vec<(String, String, usize)> = Vec::new();

    for spec in &specs {
        for anchor in parse_anchors(&spec.path, &spec.contents) {
            if let Some(lib) = anchored_crate(&anchor.kind, &ws) {
                anchor_targets.push((lib, spec.path.clone(), anchor.line));
            }
            let Resolution {
                status,
                targets,
                note,
            } = resolve(&anchor, root, &ws);
            claims.push(Claim {
                spec: anchor.spec.clone(),
                line: anchor.line,
                target: anchor.target(),
                status,
                detail: note,
                location: targets.first().map(|t| t.display()),
            });
        }
    }

    // Problems first — a reader wants the drift at the top, never a table sorted
    // by spec id. Ties break deterministically so two runs render identically.
    claims.sort_by(|a, b| {
        a.status
            .report_order()
            .cmp(&b.status.report_order())
            .then(a.spec.cmp(&b.spec))
            .then(a.line.cmp(&b.line))
    });

    let cov = coverage(&ws, &specs, &anchor_targets);
    let ungoverned: Vec<Ungoverned> = cov
        .iter()
        .filter(|c| c.status == ClaimStatus::Ungoverned)
        .map(|c| Ungoverned {
            krate: c.name.clone(),
        })
        .collect();

    let mut tally = Tally::of(claims.iter().map(|c| c.status));
    tally.ungoverned = ungoverned.len();

    TraceReport {
        claims,
        ungoverned,
        tally,
    }
}

/// The crate an anchor targets, if it names one this workspace has.
fn anchored_crate(kind: &AnchorKind, ws: &Workspace) -> Option<String> {
    match kind {
        AnchorKind::Symbol { sym } | AnchorKind::SymbolLen { sym, .. } => {
            ws.by_lib_name(&sym.crate_seg).map(|c| c.lib_name.clone())
        }
        AnchorKind::Path { path } => ws.owning(path).map(|c| c.lib_name.clone()),
        AnchorKind::Malformed { .. } => None,
    }
}

/// Full coverage detail, for a caller that wants the receipts rather than only
/// the ungoverned names.
pub fn crate_coverage(root: &Path) -> Vec<CrateCoverage> {
    let ws = Workspace::load(root);
    let specs = read_specs(root);
    let mut targets = Vec::new();
    for spec in &specs {
        for anchor in parse_anchors(&spec.path, &spec.contents) {
            if let Some(lib) = anchored_crate(&anchor.kind, &ws) {
                targets.push((lib, spec.path.clone(), anchor.line));
            }
        }
    }
    coverage(&ws, &specs, &targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::ClaimKind;
    use crate::test_support::{crate_manifest, repo_root, temp_repo, workspace_manifest, write};

    #[test]
    fn the_real_repo_has_no_broken_anchors() {
        // The highest-value test here: every anchor in docs/specs/ points at real
        // code today, so this passes now and fails loudly the moment someone
        // renames `mint_token` or moves `phase.rs`. The tool, checking itself.
        //
        // Asserts on Broken/Stale only — not exact Unknown or Ungoverned counts —
        // so adding a crate or a spec never requires editing this test.
        let report = trace(&repo_root());
        let broken: Vec<&Claim> = report
            .claims
            .iter()
            .filter(|c| c.status == ClaimStatus::Broken)
            .collect();
        assert!(broken.is_empty(), "broken anchors: {broken:#?}");

        let stale: Vec<&Claim> = report
            .claims
            .iter()
            .filter(|c| c.status == ClaimStatus::Stale)
            .collect();
        assert!(stale.is_empty(), "stale claims: {stale:#?}");

        assert_eq!(report.blocking(), 0);
        // And it actually found the anchors, rather than passing vacuously.
        assert!(report.claims.len() >= 10, "{} claims", report.claims.len());
    }

    #[test]
    fn a_broken_anchor_is_found_and_reported_worst_first() {
        let root = temp_repo("engine-broken");
        write(&root, "Cargo.toml", &workspace_manifest(&["sc-a"]));
        write(&root, "crates/sc-a/Cargo.toml", &crate_manifest("sc-a"));
        write(&root, "crates/sc-a/src/lib.rs", "pub fn real() {}\n");
        write(
            &root,
            "docs/specs/01-a.md",
            "About `sc-a`.\n\nGood <!--@ sc_a::real -->\nBad <!--@ sc_a::ghost -->\n",
        );

        let report = trace(&root);
        assert_eq!(report.claims.len(), 2);
        // Problems first.
        assert_eq!(report.claims[0].status, ClaimStatus::Broken);
        assert_eq!(report.claims[0].target, "sc_a::ghost");
        assert_eq!(report.claims[0].spec, "docs/specs/01-a.md");
        assert_eq!(report.claims[0].line, 4);
        assert!(report.claims[0].detail.is_some());
        assert_eq!(report.claims[1].status, ClaimStatus::Ok);
        assert_eq!(report.tally.broken, 1);
        assert_eq!(report.tally.ok, 1);
        assert_eq!(report.blocking(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ungoverned_crates_are_reported_but_do_not_block() {
        let root = temp_repo("engine-ungoverned");
        write(
            &root,
            "Cargo.toml",
            &workspace_manifest(&["sc-a", "sc-lonely"]),
        );
        write(&root, "crates/sc-a/Cargo.toml", &crate_manifest("sc-a"));
        write(&root, "crates/sc-a/src/lib.rs", "pub fn real() {}\n");
        write(
            &root,
            "crates/sc-lonely/Cargo.toml",
            &crate_manifest("sc-lonely"),
        );
        write(&root, "crates/sc-lonely/src/lib.rs", "pub fn x() {}\n");
        write(&root, "docs/specs/01-a.md", "About `sc-a` only.\n");

        let report = trace(&root);
        assert_eq!(
            report.ungoverned,
            vec![Ungoverned {
                krate: "sc-lonely".into()
            }]
        );
        assert_eq!(report.tally.ungoverned, 1);
        // Warns, never fails: adding a crate and its spec in one commit is good
        // practice, but a hard failure blocks legitimate work-in-progress.
        assert_eq!(report.blocking(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_anchor_governs_its_crate_even_with_no_prose_mention() {
        let root = temp_repo("engine-anchor-governs");
        write(&root, "Cargo.toml", &workspace_manifest(&["sc-a"]));
        write(&root, "crates/sc-a/Cargo.toml", &crate_manifest("sc-a"));
        write(&root, "crates/sc-a/src/lib.rs", "pub fn real() {}\n");
        // The name never appears in prose — only inside the anchor.
        write(&root, "docs/specs/01-a.md", "See <!--@ sc_a::real -->\n");

        let report = trace(&root);
        assert!(report.ungoverned.is_empty(), "{:?}", report.ungoverned);

        let cov = crate_coverage(&root);
        assert_eq!(cov[0].claimed_by.clone().unwrap().2, ClaimKind::Anchor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_repo_with_no_specs_reports_nothing_rather_than_failing() {
        let root = temp_repo("engine-nospecs");
        write(&root, "Cargo.toml", &workspace_manifest(&[]));
        let report = trace(&root);
        assert!(report.claims.is_empty());
        assert_eq!(report.blocking(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_report_round_trips_through_json() {
        let report = trace(&repo_root());
        let json = serde_json::to_string(&report).unwrap();
        let back: TraceReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
        // No blended figure anywhere in the payload.
        assert!(!json.contains("percent"), "no headline score");
        assert!(!json.contains("score"), "no headline score");
    }
}
