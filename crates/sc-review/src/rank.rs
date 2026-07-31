//! Vote merging, ranking, and the retry prompt (spec 16).
//!
//! Three jobs, all pure logic over findings:
//!
//! * **[`merge_votes`]** — two models describing the same problem are one finding
//!   with two votes. Two models describing *different* problems in the same hunk
//!   are two findings. Over-merging is the failure mode: it silently discards a
//!   finding while inflating another's vote count, corrupting exactly the signal
//!   a panel exists to produce. When in doubt, keep them separate.
//! * **[`rank`]** — corroborated → several reviewers → one reviewer, severity
//!   breaking ties within a band.
//! * **[`retry_feedback`]** — the evidence, never the prose summary.

use crate::finding::{Finding, ModelId, Severity};

/// Merge findings from several reviewers into one list with vote counts.
///
/// Two findings merge only when they share a **lens** and their anchors point at
/// the same place — the same hunk, or the same symbol. Wording is ignored
/// entirely: two models describing one duplicated helper differently are one
/// finding with two votes. Line proximity is never consulted.
///
/// The bar is set by what merging *costs when wrong*. A wrongly-merged pair loses
/// a real finding and overstates another's agreement; a wrongly-separated pair
/// shows the same problem twice, which a human reading the report notices and
/// forgives. So the tie goes to keeping them apart, and same-hunk-different-claim
/// is explicitly not a match.
pub fn merge_votes(findings: Vec<Finding>, considered_by: &[ModelId]) -> Vec<Finding> {
    let mut merged: Vec<Finding> = Vec::new();
    for f in findings {
        match merged.iter_mut().find(|m| same_finding(m, &f)) {
            Some(existing) => {
                for who in f.raised_by {
                    if !existing.raised_by.contains(&who) {
                        existing.raised_by.push(who);
                    }
                }
                // Keep the strongest claim: the highest severity anyone assigned,
                // and any corroboration either carried.
                existing.severity = existing.severity.max(f.severity);
                if !existing.corroborated && f.corroborated {
                    existing.corroborated = true;
                    existing.evidence = f.evidence;
                }
                // Prefer the more specific anchor — a symbol beats no symbol.
                if existing.anchor.symbol.is_none() {
                    existing.anchor.symbol = f.anchor.symbol;
                }
                if existing.anchor.hunk.is_none() {
                    existing.anchor.hunk = f.anchor.hunk;
                    existing.anchor.line = f.anchor.line;
                }
            }
            None => merged.push(f),
        }
    }
    for f in &mut merged {
        f.considered_by = considered_by.to_vec();
        f.raised_by.sort();
    }
    merged
}

/// Do two findings describe the same problem? Same lens, anchors pointing at the
/// same place, and — the guard against over-merging — claims that are not plainly
/// about different things.
fn same_finding(a: &Finding, b: &Finding) -> bool {
    if a.lens != b.lens {
        return false;
    }
    if !a.anchor.points_at_same_place(&b.anchor) {
        return false;
    }
    // Within one hunk, two findings naming DIFFERENT symbols are two problems.
    // Without this, every finding in a hunk collapses into the first one.
    match (&a.anchor.symbol, &b.anchor.symbol) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => true,
    }
}

/// Order findings for display and for the decision that reads the top of the list.
///
/// The order the spec sets, and the reason for each step:
///
/// 1. **Corroborated first.** A deterministic check outranks any number of
///    opinions.
/// 2. **Then by votes.** Agreement between reviewers is real evidence, one notch
///    weaker — it ranks, and only ranks.
/// 3. **Then severity**, breaking ties within a band.
/// 4. **Then: a finding whose named symbol did not resolve drops.** A model that
///    cited a symbol `sc-index` cannot find in that file got the anchor wrong,
///    which is a cheap hallucination signal.
///
/// Ties beyond that break on lens then file then summary, so the order is
/// deterministic — two runs over the same findings render identically (spec 03).
pub fn rank(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        b.corroborated
            .cmp(&a.corroborated)
            .then(b.votes().cmp(&a.votes()))
            .then(b.severity.cmp(&a.severity))
            .then(a.anchor_unresolved.cmp(&b.anchor_unresolved))
            .then(a.lens.cmp(&b.lens))
            .then(a.anchor.file.cmp(&b.anchor.file))
            .then(a.summary.cmp(&b.summary))
    });
}

/// The findings that met the bar to stop the run: corroborated **and** at or
/// above `gate_at`. Carried as a count on the finish event rather than left for
/// each renderer to recompute, so every surface agrees on whether a review
/// stopped anything.
pub fn blocking<'a>(
    findings: impl IntoIterator<Item = &'a Finding>,
    gate_at: Severity,
) -> Vec<&'a Finding> {
    findings
        .into_iter()
        .filter(|f| f.is_blocking(gate_at))
        .collect()
}

/// The feedback block for a retry prompt.
///
/// **Evidence, not verdicts.** This is the easiest thing in the spec to get
/// wrong: "you duplicated something" is unactionable, and a worker handed it will
/// either thrash or reword the code until the reviewer stops complaining. So what
/// goes in is what the deterministic check found — a named symbol at a named
/// location — and the model's prose summary is left in the report where a human
/// reads it.
///
/// Only corroborated findings appear, which is not a filter so much as a
/// consequence: an uncorroborated finding has no evidence to inject by
/// definition. Returns `None` when nothing qualifies, so the caller can tell
/// "no review feedback" from "an empty block".
pub fn retry_feedback(findings: &[Finding]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for f in findings.iter().filter(|f| f.may_act()) {
        if let Some(ev) = &f.evidence {
            lines.push(format!("✗ review ({}): {}", f.lens, ev));
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::HunkId;
    use crate::finding::{Anchor, Lens};

    fn m(s: &str) -> ModelId {
        ModelId::new(s)
    }

    fn f(lens: Lens, sev: Severity, anchor: Anchor, summary: &str, by: &str) -> Finding {
        Finding::new(lens, sev, anchor, summary, m(by))
    }

    #[test]
    fn two_models_describing_one_problem_differently_are_one_finding_with_two_votes() {
        // Wording is ignored: what matters is what they point at.
        let a = f(
            Lens::Duplication,
            Severity::Medium,
            Anchor::file("a.rs")
                .with_hunk(HunkId(0))
                .with_symbol("format_date"),
            "reimplements the date helper",
            "qwen",
        );
        let b = f(
            Lens::Duplication,
            Severity::High,
            Anchor::file("a.rs")
                .with_hunk(HunkId(0))
                .with_symbol("format_date"),
            "this already exists in utils",
            "gemini",
        );
        let merged = merge_votes(vec![a, b], &[m("qwen"), m("gemini")]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].votes(), 2);
        assert_eq!(merged[0].raised_by, vec![m("gemini"), m("qwen")]);
        // The strongest claim survives the merge.
        assert_eq!(merged[0].severity, Severity::High);
        // Agreement never promotes an opinion to a fact.
        assert!(!merged[0].may_act(), "two votes is still two opinions");
    }

    #[test]
    fn two_models_flagging_different_problems_in_one_hunk_stay_two_findings() {
        // The named failure mode. Over-merging silently discards a finding AND
        // inflates another's votes — corrupting the exact signal a panel produces.
        let a = f(
            Lens::ErrorHandling,
            Severity::High,
            Anchor::file("a.rs")
                .with_hunk(HunkId(0))
                .with_symbol("load"),
            "swallows the parse error",
            "qwen",
        );
        let b = f(
            Lens::ErrorHandling,
            Severity::Medium,
            Anchor::file("a.rs")
                .with_hunk(HunkId(0))
                .with_symbol("save"),
            "ignores the write result",
            "gemini",
        );
        let merged = merge_votes(vec![a, b], &[m("qwen"), m("gemini")]);

        assert_eq!(merged.len(), 2, "different symbols = different problems");
        assert!(merged.iter().all(|x| x.votes() == 1));
    }

    #[test]
    fn findings_from_different_lenses_never_merge() {
        let anchor = Anchor::file("a.rs").with_hunk(HunkId(0)).with_symbol("f");
        let merged = merge_votes(
            vec![
                f(
                    Lens::Duplication,
                    Severity::Low,
                    anchor.clone(),
                    "dup",
                    "qwen",
                ),
                f(Lens::AbstractionFit, Severity::Low, anchor, "fit", "qwen"),
            ],
            &[m("qwen")],
        );
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn a_symbol_only_and_a_hunk_only_finding_merge_into_the_more_specific_anchor() {
        let a = f(
            Lens::Duplication,
            Severity::Low,
            Anchor::file("a.rs").with_symbol("format_date"),
            "dup",
            "qwen",
        );
        let b = f(
            Lens::Duplication,
            Severity::Low,
            Anchor::file("a.rs")
                .with_hunk(HunkId(3))
                .with_symbol("format_date"),
            "dup again",
            "gemini",
        );
        let merged = merge_votes(vec![a, b], &[m("qwen"), m("gemini")]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].anchor.hunk, Some(HunkId(3)), "gained the hunk");
        assert_eq!(merged[0].votes(), 2);
    }

    #[test]
    fn every_merged_finding_records_who_reviewed_the_diff_at_all() {
        // `considered_by` is what makes a lone finding interpretable — contested
        // (others looked and didn't raise it) vs merely unreviewed.
        let panel = [m("qwen"), m("gemini"), m("gpt")];
        let merged = merge_votes(
            vec![f(
                Lens::AbstractionFit,
                Severity::Low,
                Anchor::file("a.rs").with_hunk(HunkId(0)),
                "lone opinion",
                "qwen",
            )],
            &panel,
        );
        assert_eq!(merged[0].considered_by, panel.to_vec());
        assert!(merged[0].is_contested());
    }

    #[test]
    fn ranking_puts_a_deterministic_check_above_any_number_of_opinions() {
        let mut unanimous = f(
            Lens::Duplication,
            Severity::High,
            Anchor::file("b.rs").with_hunk(HunkId(0)).with_symbol("x"),
            "three models agree",
            "qwen",
        );
        unanimous.raised_by = vec![m("qwen"), m("gemini"), m("gpt")];

        let mut corroborated = f(
            Lens::ErrorHandling,
            Severity::Low,
            Anchor::file("a.rs").with_hunk(HunkId(0)).with_symbol("y"),
            "one model, but checked",
            "qwen",
        );
        corroborated.corroborate("`except: pass` at a.rs:3");

        let mut findings = vec![unanimous, corroborated];
        rank(&mut findings);

        assert_eq!(
            findings[0].summary, "one model, but checked",
            "corroboration outranks unanimity, even at lower severity"
        );
    }

    #[test]
    fn a_finding_whose_symbol_does_not_resolve_drops_in_rank() {
        let mut resolved = f(
            Lens::AbstractionFit,
            Severity::Medium,
            Anchor::file("a.rs")
                .with_hunk(HunkId(0))
                .with_symbol("real"),
            "anchored",
            "qwen",
        );
        resolved.anchor_unresolved = false;
        let mut hallucinated = f(
            Lens::AbstractionFit,
            Severity::Medium,
            Anchor::file("a.rs")
                .with_hunk(HunkId(1))
                .with_symbol("imaginary"),
            "anchored",
            "qwen",
        );
        hallucinated.anchor_unresolved = true;

        let mut findings = vec![hallucinated, resolved];
        rank(&mut findings);
        assert_eq!(findings[0].anchor.symbol.as_deref(), Some("real"));
    }

    #[test]
    fn ranking_is_deterministic_for_otherwise_equal_findings() {
        let mk = |file: &str, summary: &str| {
            f(
                Lens::Duplication,
                Severity::Low,
                Anchor::file(file).with_hunk(HunkId(0)),
                summary,
                "qwen",
            )
        };
        let mut one = vec![mk("b.rs", "z"), mk("a.rs", "y"), mk("a.rs", "x")];
        let mut two = vec![mk("a.rs", "x"), mk("b.rs", "z"), mk("a.rs", "y")];
        rank(&mut one);
        rank(&mut two);
        assert_eq!(one, two);
    }

    #[test]
    fn blocking_counts_only_corroborated_findings_at_or_above_the_bar() {
        let mut high_uncorroborated = f(
            Lens::Duplication,
            Severity::High,
            Anchor::file("a.rs").with_hunk(HunkId(0)),
            "opinion",
            "qwen",
        );
        high_uncorroborated.raised_by = vec![m("qwen"), m("gemini"), m("gpt")];

        let mut low_corroborated = f(
            Lens::ErrorHandling,
            Severity::Low,
            Anchor::file("a.rs").with_hunk(HunkId(1)),
            "checked",
            "qwen",
        );
        low_corroborated.corroborate("`except: pass`");

        let mut high_corroborated = f(
            Lens::Duplication,
            Severity::High,
            Anchor::file("b.rs").with_hunk(HunkId(0)),
            "checked and serious",
            "qwen",
        );
        high_corroborated.corroborate("`f` already exists at c.rs:1");

        let all = vec![high_uncorroborated, low_corroborated, high_corroborated];
        let blocked = blocking(&all, Severity::High);
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].summary, "checked and serious");
    }

    #[test]
    fn a_retry_prompt_carries_the_evidence_never_the_prose() {
        let mut corroborated = f(
            Lens::Duplication,
            Severity::High,
            Anchor::file("src/report/render.rs")
                .with_hunk(HunkId(0))
                .with_symbol("format_date"),
            "this smells like a duplicate to me",
            "qwen",
        );
        corroborated.corroborate(
            "You added `format_date` in src/report/render.rs. An equivalent already \
             exists: `format_date` already exists at src/utils/date.rs:41. Import and \
             use it instead of reimplementing it.",
        );
        let opinion = f(
            Lens::AbstractionFit,
            Severity::High,
            Anchor::file("a.rs").with_hunk(HunkId(0)),
            "I would have written this differently",
            "qwen",
        );

        let text = retry_feedback(&[corroborated, opinion]).expect("something to say");
        // The symbol AND its location — the bar `feedback_text` sets for tests.
        assert!(text.contains("format_date"), "{text}");
        assert!(text.contains("src/utils/date.rs:41"), "{text}");
        // The model's prose never reaches a worker.
        assert!(!text.contains("smells like"), "{text}");
        assert!(!text.contains("written this differently"), "{text}");
    }

    #[test]
    fn nothing_corroborated_means_no_retry_feedback_at_all() {
        // An uncorroborated finding has no evidence to inject by definition, so it
        // could only ever produce the vague prompt the spec forbids.
        let opinion = f(
            Lens::AbstractionFit,
            Severity::High,
            Anchor::file("a.rs").with_hunk(HunkId(0)),
            "taste",
            "qwen",
        );
        assert!(retry_feedback(&[opinion]).is_none());
        assert!(retry_feedback(&[]).is_none());
    }
}
