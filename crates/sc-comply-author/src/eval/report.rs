//! Rendering an eval run, including side-by-side model comparison.
//!
//! The headline is deliberately **not** a single score. A model with one
//! dishonest draft is unusable for pack authoring regardless of how well it did
//! elsewhere, so the dishonest count is reported first and separately — blending
//! it into an average would let a strong aggregate hide the one result that
//! disqualifies the model.

use std::fmt::Write as _;

use super::score::{ModelScore, Verdict};
use super::suite::EvalSuite;

/// Render one model's run.
pub fn markdown(suite: &EvalSuite, score: &ModelScore) -> String {
    let mut s = String::with_capacity(4096);
    let _ = writeln!(s, "# Compliance drafting eval — {}", score.model);
    let _ = writeln!(s);
    header(&mut s, suite);
    summary_line(&mut s, score);
    per_control_table(&mut s, std::slice::from_ref(score));
    failure_detail(&mut s, score);
    s
}

/// Render two or more models side by side.
pub fn comparison(suite: &EvalSuite, scores: &[ModelScore]) -> String {
    let mut s = String::with_capacity(8192);
    let _ = writeln!(s, "# Compliance drafting eval — model comparison");
    let _ = writeln!(s);
    header(&mut s, suite);

    let _ = writeln!(s, "## Headline");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "| Model | Dishonest | Good | Unhelpful | Broken | Score | Calls |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|");
    for m in scores {
        let _ = writeln!(
            s,
            "| `{}` | **{}** | {} | {} | {} | {:.0}% | {} |",
            m.model,
            m.dishonest_count(),
            m.count(&Verdict::Good),
            m.count(&Verdict::Unhelpful),
            m.count(&Verdict::Broken),
            m.total() * 100.0,
            m.total_attempts(),
        );
    }
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "> **Read the dishonest column first.** A single dishonest draft disqualifies a model \
         for pack authoring: it would put a green control in front of an auditor that nothing \
         verified. The percentage is secondary and must never be read on its own."
    );
    let _ = writeln!(s);

    per_control_table(&mut s, scores);

    for m in scores {
        failure_detail(&mut s, m);
    }
    s
}

fn header(s: &mut String, suite: &EvalSuite) {
    let (org, prov, tech) = suite.category_counts();
    let _ = writeln!(
        s,
        "{} controls — {org} organizational (must be undeterminable), {prov} provider-side, \
         {tech} technical (should yield real checks).",
        suite.controls.len()
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "Grading is deterministic: the authoring lints plus hand-written labels. No judge \
         model is involved — an eval for a reproducibility tool has to be reproducible itself."
    );
    let _ = writeln!(s);
}

fn summary_line(s: &mut String, m: &ModelScore) {
    let _ = writeln!(
        s,
        "**{} dishonest · {} good · {} unhelpful · {} broken — {:.0}% over {} call(s)**",
        m.dishonest_count(),
        m.count(&Verdict::Good),
        m.count(&Verdict::Unhelpful),
        m.count(&Verdict::Broken),
        m.total() * 100.0,
        m.total_attempts(),
    );
    let _ = writeln!(s);
}

fn per_control_table(s: &mut String, scores: &[ModelScore]) {
    let _ = writeln!(s, "## Per control");
    let _ = writeln!(s);

    let mut head = String::from("| Control | Category |");
    let mut sep = String::from("|---|---|");
    for m in scores {
        let _ = write!(head, " {} |", m.model);
        sep.push_str("---|");
    }
    let _ = writeln!(s, "{head}");
    let _ = writeln!(s, "{sep}");

    let Some(first) = scores.first() else {
        return;
    };
    for (i, c) in first.scores.iter().enumerate() {
        let _ = write!(s, "| {} | {} |", c.control_id, c.category);
        for m in scores {
            let cell = m
                .scores
                .get(i)
                .map(|x| match x.verdict {
                    Verdict::Good => "✅ good".to_string(),
                    Verdict::Dishonest => "❌ **DISHONEST**".to_string(),
                    Verdict::Unhelpful => "⚠️ unhelpful".to_string(),
                    Verdict::Broken => "💥 broken".to_string(),
                })
                .unwrap_or_else(|| "—".to_string());
            let _ = write!(s, " {cell} |");
        }
        let _ = writeln!(s);
    }
    let _ = writeln!(s);
}

fn failure_detail(s: &mut String, m: &ModelScore) {
    let failures: Vec<_> = m
        .scores
        .iter()
        .filter(|c| c.verdict != Verdict::Good)
        .collect();
    if failures.is_empty() {
        return;
    }

    let _ = writeln!(s, "## Failures — `{}`", m.model);
    let _ = writeln!(s);

    // Dishonest first: they are the results that decide whether the model is
    // usable at all.
    let mut sorted = failures;
    sorted.sort_by_key(|c| match c.verdict {
        Verdict::Dishonest => 0,
        Verdict::Broken => 1,
        Verdict::Unhelpful => 2,
        Verdict::Good => 3,
    });

    for c in sorted {
        let _ = writeln!(
            s,
            "### {} — {} ({})",
            c.control_id,
            c.verdict.label(),
            c.category
        );
        let _ = writeln!(s);
        for r in &c.reasons {
            let _ = writeln!(s, "- {r}");
        }
        for f in &c.blocking_lints {
            let _ = writeln!(s, "- lint `{}`: {}", f.lint, f.summary);
        }
        let _ = writeln!(s);
        if !c.toml.trim().is_empty() {
            let _ = writeln!(s, "<details><summary>drafted TOML</summary>");
            let _ = writeln!(s);
            let _ = writeln!(s, "```toml");
            let _ = writeln!(s, "{}", c.toml.trim());
            let _ = writeln!(s, "```");
            let _ = writeln!(s);
            let _ = writeln!(s, "</details>");
            let _ = writeln!(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::score::ControlScore;
    use super::*;

    fn suite() -> EvalSuite {
        EvalSuite {
            controls: vec![super::super::suite::EvalControl {
                id: "T1".into(),
                framework: "F".into(),
                title: "t".into(),
                clause: String::new(),
                severity: "medium".into(),
                intent: "i".into(),
                text: "x".into(),
                must_be_undeterminable: true,
                expect_provider_side_care: false,
                expect_real_checks: false,
                note: "n".into(),
            }],
        }
    }

    fn score(model: &str, verdict: Verdict) -> ModelScore {
        ModelScore {
            model: model.into(),
            scores: vec![ControlScore {
                control_id: "T1".into(),
                category: "organizational",
                verdict,
                attempts: 1,
                blocking_lints: vec![],
                reasons: vec!["because".into()],
                toml: "[[controls]]\nid = \"T1\"".into(),
            }],
        }
    }

    #[test]
    fn the_comparison_leads_with_dishonesty() {
        let md = comparison(
            &suite(),
            &[score("a", Verdict::Good), score("b", Verdict::Dishonest)],
        );
        let dishonest_col = md.find("| Model | Dishonest").expect("headline table");
        let caveat = md.find("Read the dishonest column first").expect("caveat");
        let per_control = md.find("## Per control").expect("per-control");
        assert!(dishonest_col < caveat);
        assert!(caveat < per_control, "the caveat must precede the detail");
    }

    #[test]
    fn a_dishonest_result_is_visually_loud() {
        let md = markdown(&suite(), &score("m", Verdict::Dishonest));
        assert!(md.contains("**DISHONEST**"), "{md}");
    }

    #[test]
    fn failures_render_their_reasons_and_draft() {
        let md = markdown(&suite(), &score("m", Verdict::Unhelpful));
        assert!(md.contains("## Failures"));
        assert!(md.contains("- because"));
        assert!(md.contains("drafted TOML"));
    }

    #[test]
    fn a_clean_run_has_no_failure_section() {
        let md = markdown(&suite(), &score("m", Verdict::Good));
        assert!(!md.contains("## Failures"));
    }

    #[test]
    fn the_header_states_the_grading_is_model_free() {
        let md = markdown(&suite(), &score("m", Verdict::Good));
        assert!(md.contains("No judge model"), "{md}");
    }

    #[test]
    fn comparison_puts_every_model_in_the_per_control_table() {
        let md = comparison(
            &suite(),
            &[
                score("alpha", Verdict::Good),
                score("beta", Verdict::Broken),
            ],
        );
        assert!(md.contains("alpha"));
        assert!(md.contains("beta"));
        assert!(md.contains("💥 broken"));
    }
}
