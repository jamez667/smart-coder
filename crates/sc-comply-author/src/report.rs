//! Rendering a lint report.
//!
//! Same discipline as the evidence pack it critiques: state the limitations
//! before the findings, never imply coverage the run did not have, and give the
//! reader something actionable rather than a score.

use std::fmt::Write as _;

use sc_comply::status::Severity;

use crate::lint::LintReport;

/// Render a lint report as Markdown.
pub fn markdown(report: &LintReport) -> String {
    let mut s = String::with_capacity(4 * 1024);

    let _ = writeln!(s, "# Pack critique — {}", report.framework);
    let _ = writeln!(s);

    // Limitations first, for the same reason the evidence pack puts scope before
    // the score: a reader who sees "0 findings" first has been told the pack is
    // fine before being told what was not checked.
    if !report.had_sample {
        let _ = writeln!(
            s,
            "> **No sample workspace was supplied.** Lints that need real files — glob \
             reachability, self-referential patterns, absent targets — did not run. A clean \
             result here does not mean the pack is clean."
        );
        let _ = writeln!(s);
    }

    if report.is_clean() {
        let _ = writeln!(s, "No findings.");
        let _ = writeln!(s);
        return s;
    }

    let _ = writeln!(s, "**{}**", report.summary_line());
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "These are authoring defects in the pack, not findings about any audited \
         codebase. Nothing here is fixed automatically — every change is the author's call."
    );
    let _ = writeln!(s);

    for sev in [
        Severity::Critical,
        Severity::High,
        Severity::Medium,
        Severity::Low,
    ] {
        let at_sev: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.severity == sev)
            .collect();
        if at_sev.is_empty() {
            continue;
        }
        let _ = writeln!(s, "## {}", sev.label());
        let _ = writeln!(s);
        for f in at_sev {
            let _ = writeln!(s, "### `{}` — {}", f.locus, f.lint);
            let _ = writeln!(s);
            let _ = writeln!(s, "{}", f.summary);
            let _ = writeln!(s);
            let _ = writeln!(s, "- **Why it matters:** {}", f.consequence);
            let _ = writeln!(s, "- **Fix:** {}", f.suggestion);
            let _ = writeln!(s);
        }
    }

    s
}

/// Render a lint report as JSON, for tooling.
pub fn json(report: &LintReport) -> String {
    let findings: Vec<serde_json::Value> = report
        .findings
        .iter()
        .map(|f| {
            serde_json::json!({
                "lint": f.lint,
                "severity": f.severity.label(),
                "locus": f.locus,
                "summary": f.summary,
                "consequence": f.consequence,
                "suggestion": f.suggestion,
            })
        })
        .collect();

    let doc = serde_json::json!({
        "schema_version": 1,
        "framework": report.framework,
        "had_sample": report.had_sample,
        "counts": {
            "critical": report.count(Severity::Critical),
            "high": report.count(Severity::High),
            "medium": report.count(Severity::Medium),
            "low": report.count(Severity::Low),
        },
        "findings": findings,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lint::LintFinding;

    fn report_with(findings: Vec<LintFinding>, had_sample: bool) -> LintReport {
        LintReport {
            findings,
            framework: "Test 1.0".into(),
            had_sample,
        }
    }

    fn finding(sev: Severity) -> LintFinding {
        LintFinding::new(
            "some-lint",
            sev,
            "T1/check",
            "the summary",
            "the consequence",
            "the fix",
        )
    }

    #[test]
    fn warns_loudly_when_no_sample_was_used() {
        let md = markdown(&report_with(vec![], false));
        assert!(md.contains("No sample workspace"), "{md}");
        // And that warning precedes the "no findings" reassurance.
        let warn = md.find("No sample workspace").expect("warning");
        let clean = md.find("No findings").expect("clean line");
        assert!(warn < clean);
    }

    #[test]
    fn a_clean_report_with_a_sample_has_no_caveat() {
        let md = markdown(&report_with(vec![], true));
        assert!(!md.contains("No sample workspace"));
        assert!(md.contains("No findings"));
    }

    #[test]
    fn groups_findings_by_severity_worst_first() {
        let md = markdown(&report_with(
            vec![finding(Severity::Low), finding(Severity::Critical)],
            true,
        ));
        let crit = md.find("## critical").expect("critical section");
        let low = md.find("## low").expect("low section");
        assert!(crit < low);
    }

    #[test]
    fn renders_the_fix_for_each_finding() {
        let md = markdown(&report_with(vec![finding(Severity::High)], true));
        assert!(md.contains("**Fix:** the fix"));
        assert!(md.contains("**Why it matters:** the consequence"));
        assert!(md.contains("`T1/check`"));
    }

    #[test]
    fn states_that_nothing_is_auto_fixed() {
        let md = markdown(&report_with(vec![finding(Severity::High)], true));
        assert!(md.contains("author's call"), "{md}");
    }

    #[test]
    fn json_is_valid_and_carries_counts() {
        let out = json(&report_with(
            vec![finding(Severity::High), finding(Severity::Low)],
            true,
        ));
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["counts"]["high"], 1);
        assert_eq!(v["counts"]["low"], 1);
        assert_eq!(v["findings"].as_array().expect("array").len(), 2);
        assert_eq!(v["had_sample"], true);
    }
}
