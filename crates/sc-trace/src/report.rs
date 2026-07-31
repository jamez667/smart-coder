//! Rendering a [`TraceReport`] for a human, or for a machine.
//!
//! Both renderers are pure `fn(&TraceReport) -> String` with no I/O, matching
//! `sc-comply`'s `report` module — the caller decides where the bytes go.
//!
//! Two properties the layout must hold (spec 13/17):
//!
//! * **Problems first.** A reader wants the drift at the top, not a table sorted
//!   by spec id.
//! * **No headline score.** Counts, never a blended percentage — "94% traceable"
//!   lets a reader stop reading, and the missing 6% is the entire point.

use crate::engine::TraceReport;
use crate::status::ClaimStatus;

/// The human-readable report.
pub fn text(report: &TraceReport) -> String {
    let mut out = String::new();
    out.push_str("spec traceability\n");
    out.push_str("=================\n\n");

    let problems = report.problems();
    if problems.is_empty() {
        out.push_str("Every anchored claim resolved.\n");
    } else {
        for claim in problems {
            out.push_str(&format!(
                "{:<10} {}:{}\n             {}\n",
                claim.status.label().to_uppercase(),
                claim.spec,
                claim.line,
                claim.target
            ));
            if let Some(detail) = &claim.detail {
                out.push_str(&format!("             {detail}\n"));
            }
            if let Some(location) = &claim.location {
                out.push_str(&format!("             → {location}\n"));
            }
            out.push('\n');
        }
    }

    if !report.ungoverned.is_empty() {
        out.push_str(&format!(
            "\n{} crate(s) no spec describes (warning — this never fails the check):\n",
            report.ungoverned.len()
        ));
        for u in &report.ungoverned {
            out.push_str(&format!("  · {}\n", u.krate));
        }
    }

    out.push_str(&format!("\n{}\n", report.tally.summary_line()));
    // Say plainly what `unknown` means, so nobody reads it as a pass. This is
    // the same commitment sc-comply makes by printing its caveats next to its
    // numbers rather than below them.
    if report.tally.unknown > 0 {
        out.push_str(
            "unknown = the checker could not look (unsupported language, ambiguous \
             name, malformed anchor) — not a pass.\n",
        );
    }
    out
}

/// The machine-readable report.
pub fn json(report: &TraceReport) -> String {
    serde_json::to_string_pretty(report).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

/// The closing line for a caller that gates on this.
pub fn check_summary(report: &TraceReport) -> String {
    let blocking = report.blocking();
    if blocking == 0 {
        format!("spec traceability: {}", report.tally.summary_line())
    } else {
        format!(
            "spec traceability: {blocking} claim(s) need a human — {}",
            report.tally.summary_line()
        )
    }
}

/// The statuses that appear in a report, for a caller building its own view.
pub fn statuses(report: &TraceReport) -> Vec<ClaimStatus> {
    report.claims.iter().map(|c| c.status).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{trace, Claim, Ungoverned};
    use crate::status::Tally;
    use crate::test_support::repo_root;

    fn sample() -> TraceReport {
        TraceReport {
            claims: vec![
                Claim {
                    spec: "docs/specs/09.md".into(),
                    line: 42,
                    target: "sc_workflow::Phase::ALL len=6".into(),
                    status: ClaimStatus::Stale,
                    detail: Some("spec says len=6, code has 5 (elements)".into()),
                    location: Some("crates/sc-workflow/src/phase.rs:30".into()),
                },
                Claim {
                    spec: "docs/specs/18.md".into(),
                    line: 83,
                    target: "sc_web::mint_token".into(),
                    status: ClaimStatus::Ok,
                    detail: None,
                    location: Some("crates/sc-web/src/lib.rs:29".into()),
                },
            ],
            ungoverned: vec![Ungoverned {
                krate: "sc-lonely".into(),
            }],
            tally: Tally {
                ok: 1,
                stale: 1,
                ungoverned: 1,
                ..Default::default()
            },
        }
    }

    #[test]
    fn the_text_report_leads_with_the_problem_and_shows_its_evidence() {
        let out = text(&sample());
        let stale_at = out.find("STALE").expect("the finding is shown");
        let summary_at = out.find("1 ok").expect("counts are shown");
        assert!(stale_at < summary_at, "problems come before the tally");

        // Everything a human needs to act: where the claim is, what it said,
        // why it is wrong, and where the code is.
        assert!(out.contains("docs/specs/09.md:42"), "{out}");
        assert!(out.contains("len=6"), "{out}");
        assert!(out.contains("code has 5"), "{out}");
        assert!(out.contains("crates/sc-workflow/src/phase.rs:30"), "{out}");
    }

    #[test]
    fn ungoverned_crates_are_shown_as_a_warning() {
        let out = text(&sample());
        assert!(out.contains("sc-lonely"), "{out}");
        assert!(
            out.contains("never fails the check"),
            "the reader must not mistake a warning for a gate: {out}"
        );
    }

    #[test]
    fn the_report_carries_no_headline_score() {
        let out = text(&sample());
        assert!(!out.contains('%'), "{out}");
        assert!(out.contains("1 ok"), "counts, not ratios: {out}");
    }

    #[test]
    fn unknown_is_explained_so_nobody_reads_it_as_a_pass() {
        let mut r = sample();
        r.tally.unknown = 2;
        let out = text(&r);
        assert!(out.contains("not a pass"), "{out}");
    }

    #[test]
    fn a_clean_report_says_so() {
        let clean = TraceReport {
            claims: vec![Claim {
                spec: "docs/specs/01.md".into(),
                line: 1,
                target: "sc_a::x".into(),
                status: ClaimStatus::Ok,
                detail: None,
                location: None,
            }],
            ungoverned: vec![],
            tally: Tally {
                ok: 1,
                ..Default::default()
            },
        };
        let out = text(&clean);
        assert!(out.contains("Every anchored claim resolved"), "{out}");
    }

    #[test]
    fn json_is_parseable_and_pretty() {
        let out = json(&sample());
        let back: TraceReport = serde_json::from_str(&out).unwrap();
        assert_eq!(back, sample());
        assert!(out.contains('\n'), "pretty-printed for a human piping it");
    }

    #[test]
    fn the_check_summary_names_the_blocking_count() {
        assert!(check_summary(&sample()).contains("1 claim(s) need a human"));
        let clean = TraceReport::default();
        assert!(!check_summary(&clean).contains("need a human"));
    }

    #[test]
    fn renders_the_real_repo_without_panicking() {
        let out = text(&trace(&repo_root()));
        assert!(out.contains("spec traceability"), "{out}");
    }
}
