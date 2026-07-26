//! The executive summary — a model-written narrative over deterministic facts.
//!
//! This is the one place in the compliance system where a model writes something
//! a reader sees. It exists because the alternative is worse: an executive
//! reading a compliance page does not want the numbers rearranged into another
//! table, they want *what is wrong, what it costs, and what to decide*. That is
//! a judgment about salience, and arithmetic cannot produce it.
//!
//! # What keeps this safe
//!
//! **The model never supplies facts.** It is handed a
//! [`Rollup`](sc_comply::rollup::Rollup) computed deterministically from the
//! audit and asked to write prose *about* it. Every number in the output must
//! already be in the input.
//!
//! **The output is validated before it is used.** [`validate`] rejects a
//! narrative that makes an attestation ("compliant", "certified"), invents a
//! percentage the rollup does not contain, or exceeds a length that suggests the
//! model started reasoning rather than summarising. A rejected narrative is
//! *dropped*, not patched — the page falls back to its deterministic form, which
//! is complete on its own.
//!
//! **It is optional.** No API key, no narrative, no error. Most people running
//! this against their repo will not have configured a model, and the page must
//! be good without one.
//!
//! **It is generated at export time, not audit time.** The audit stays
//! reproducible; the narrative is authored once and committed, so a human sees
//! it before any reader does.

use sc_comply::rollup::Rollup;
use sc_comply::status::Severity;
use sc_model::{GenerateRequest, Message, ModelBackend};
use sc_proto::Result;

/// Words that would turn a scan result into a compliance claim.
///
/// This is the highest-risk failure for this feature: a summary saying "the
/// organization is compliant" on a page a customer reads is a liability, not a
/// wording nit. Any narrative containing one is discarded entirely.
const FORBIDDEN: &[&str] = &[
    "is compliant",
    "are compliant",
    "fully compliant",
    "compliance is achieved",
    "certified",
    "certification is",
    "attests",
    "attestation that",
    "we guarantee",
    "guarantees compliance",
    "audit-ready",
    "passes the audit",
    "meets all",
    "no risk",
    "fully secure",
];

/// Upper bound on the narrative, in characters.
///
/// An executive summary that runs long has stopped being one. This also bounds
/// the blast radius of a model that ignores the brief and starts essaying.
const MAX_CHARS: usize = 1800;

/// Why a narrative was rejected. Surfaced to the operator, never to a reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// Contains language that would read as a compliance claim.
    AttestationLanguage(String),
    /// Cites a number that is not in the rollup.
    InventedFigure(String),
    TooLong(usize),
    /// Ends mid-sentence — almost always a token budget cut short.
    Truncated,
    Empty,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejection::AttestationLanguage(w) => {
                write!(f, "contains attestation language ({w:?})")
            }
            Rejection::InventedFigure(n) => {
                write!(f, "cites {n} which is not in the audit data")
            }
            Rejection::TooLong(n) => write!(f, "{n} chars, over the {MAX_CHARS} limit"),
            Rejection::Truncated => write!(f, "ends mid-sentence (token budget too low?)"),
            Rejection::Empty => write!(f, "empty"),
        }
    }
}

/// Build the prompt. Public so the exact brief is reviewable.
pub fn narrative_messages(rollup: &Rollup, project: &str) -> Vec<Message> {
    let system = "You write the executive summary at the top of a software compliance report.\n\
         \n\
         Your reader is a security-conscious buyer, a prospect's compliance team, or an \
         auditor in a first meeting. They have two minutes. They want to know where this \
         project stands and what remains outstanding — not a restatement of the table below \
         your text.\n\
         \n\
         HARD RULES:\n\
         1. Use ONLY the figures given to you. Never compute a new percentage, never \
            estimate, never infer a number that is not present.\n\
         2. NEVER state or imply that the project is compliant, certified, audit-ready or \
            secure. This is an automated source-code scan. Certification requires an \
            accredited auditor and evidence this tool cannot see.\n\
         3. 'Unknown' means the tool could not determine the answer from source code — \
            most of any framework is organizational (policies, training, vendor contracts, \
            incident records) and is invisible to a code scan. It does NOT mean a failure, \
            and must never be presented as one.\n\
         4. Lead with what is verified and what is outstanding. If a finding appears in \
            several frameworks, say so — that is one fix with several times the leverage, \
            and it is the most useful thing on the page.\n\
         5. Controls are grouped by EVIDENCE DOMAIN — where the evidence physically \
            lives. Lead with the domains a repository can actually settle (code, \
            infrastructure), because those are the numbers the reader can act on. A low \
            organizational figure is a statement about where that evidence lives — HR \
            systems, contracts, board minutes — not a shortfall. Say so plainly. NEVER \
            average or combine the domains into a single figure: doing so would let a \
            large organizational section hide a poor result in code, and would make a \
            more honest report look worse than a selective one.\n\
         6. Be plain. No marketing language, no hedging padding, no bullet-point soup.\n\
         \n\
         FORMAT: two or three short paragraphs, under 250 words total. Plain prose, no \
         headings, no markdown, no lists. Write as the project team reporting to an outside \
         reader."
        .to_string();

    let mut facts = String::with_capacity(1024);
    facts.push_str(&format!("Project: {project}\n\n"));
    facts.push_str(&format!(
        "Assessed {} controls across {} compliance frameworks.\n\
         - {} passed (evidence found in source)\n\
         - {} are gaps (evidence absent or contradicted)\n\
         - {} are unknown (not determinable from source code)\n\
         - {} tool errors\n\
         Pass rate {:.0}% of assessed controls. Determinacy {:.0}% — the share the tool \
         could decide either way.\n\n",
        rollup.controls,
        rollup.frameworks,
        rollup.passed,
        rollup.gaps,
        rollup.unknown,
        rollup.errors,
        rollup.pass_rate() * 100.0,
        rollup.determinacy() * 100.0,
    ));

    if !rollup.by_section.is_empty() {
        facts.push_str(
            "By evidence domain — where the evidence physically lives. These are separate \
             scores and must NOT be averaged together:\n",
        );
        for (section, sc) in &rollup.by_section {
            facts.push_str(&format!(
                "- {} (evidence in {}; owned by {}): {} controls, {} passed, {} gaps, \
                 {} unknown. Determinacy {:.0}%.\n",
                section.label(),
                section.evidence_lives_in(),
                section.owner(),
                sc.total,
                sc.passed,
                sc.gaps,
                sc.unknown,
                sc.determinacy() * 100.0,
            ));
        }
        facts.push('\n');
    }

    let shared = rollup.shared_findings();
    if shared.is_empty() {
        facts.push_str("No single finding recurs across multiple frameworks.\n\n");
    } else {
        facts.push_str("Findings appearing in MORE THAN ONE framework (highest leverage first):\n");
        for f in shared.iter().take(5) {
            facts.push_str(&format!(
                "- \"{}\" — a gap in {} frameworks ({}), severity {}. {}\n",
                f.check,
                f.reach(),
                f.frameworks.join(", "),
                f.severity.label(),
                f.rationale
            ));
        }
        facts.push('\n');
    }

    if let Some((name, det)) = rollup.weakest_coverage.first() {
        facts.push_str(&format!(
            "Least source-verifiable framework: {name} at {:.0}% determinacy — largely \
             governance controls requiring documentary evidence.\n\n",
            det * 100.0
        ));
    }

    if !rollup.disabled_capabilities.is_empty() {
        facts.push_str(&format!(
            "Capabilities disabled for this scan: {}.\n\n",
            rollup.disabled_capabilities.join(", ")
        ));
    }

    facts.push_str("Write the executive summary.");

    vec![Message::system(system), Message::user(facts)]
}

/// Check a narrative before it is allowed onto a page.
pub fn validate(text: &str, rollup: &Rollup) -> std::result::Result<String, Rejection> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(Rejection::Empty);
    }
    if trimmed.chars().count() > MAX_CHARS {
        return Err(Rejection::TooLong(trimmed.chars().count()));
    }

    // A summary cut off by the token budget reads as a complete thought that
    // simply stops. That is worse than no summary at all on a compliance page,
    // and nothing else here would catch it.
    if !trimmed.ends_with(['.', '!', '?', '"', ')']) {
        return Err(Rejection::Truncated);
    }

    let lower = trimmed.to_lowercase();
    for word in FORBIDDEN {
        if lower.contains(word) {
            return Err(Rejection::AttestationLanguage((*word).to_string()));
        }
    }

    // Every percentage must be one the rollup actually contains. A model that
    // computes its own figure is exactly the failure this feature must not have
    // on a customer's compliance page.
    let allowed = allowed_figures(rollup);
    for cap in find_percentages(trimmed) {
        if !allowed.contains(&cap) {
            return Err(Rejection::InventedFigure(format!("{cap}%")));
        }
    }

    Ok(trimmed.to_string())
}

/// Percentages the model is permitted to state.
///
/// Includes a ±1 band because a model rounding 43.4% to 43% is correct, not an
/// invention.
fn allowed_figures(rollup: &Rollup) -> Vec<u32> {
    let mut out = Vec::new();
    let mut allow = |ratio: f64| {
        // A ±1 band: the prompt hands over pre-rounded figures, and a model that
        // re-renders "42%" as "43%" is rounding, not inventing.
        let r = (ratio * 100.0).round() as i64;
        for d in [-1, 0, 1] {
            let v = r + d;
            if (0..=100).contains(&v) {
                out.push(v as u32);
            }
        }
    };

    allow(rollup.pass_rate());
    allow(rollup.determinacy());
    for (_, det) in &rollup.weakest_coverage {
        allow(*det);
    }
    // Per-domain figures are handed to the model too, so they must be allowed —
    // otherwise the summary would be rejected precisely when it does what the
    // brief asks and leads with the domain a reader can act on.
    for score in rollup.by_section.values() {
        allow(score.determinacy());
        allow(score.coverage());
    }
    out
}

/// Every `NN%` in the text.
fn find_percentages(text: &str) -> Vec<u32> {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            // Consume any decimal part so "44.6%" reads as 44 — and, critically,
            // so the loop resumes AFTER it. Returning to `i` here would re-read
            // the "6" as a separate number and reject a legitimate figure.
            let digits: String = bytes[start..i].iter().collect();
            if i < bytes.len()
                && bytes[i] == '.'
                && bytes.get(i + 1).is_some_and(|c| c.is_ascii_digit())
            {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < bytes.len() && bytes[i] == '%' {
                if let Ok(n) = digits.parse::<u32>() {
                    out.push(n);
                }
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Generate the executive summary, or `None` if it cannot be trusted.
///
/// Returns `Ok(None)` rather than an error when the model declines, fails or
/// produces something invalid: the page is complete without a narrative, and a
/// failed export would be a worse outcome than a missing paragraph.
pub fn generate(
    backend: &dyn ModelBackend,
    rollup: &Rollup,
    project: &str,
    on_reject: &mut dyn FnMut(&Rejection),
) -> Result<Option<String>> {
    let mut req = GenerateRequest::new(narrative_messages(rollup, project));
    // Generous: reasoning models spend a large share of their budget before
    // emitting a token, and a truncated summary is worse than none — it reads
    // as a complete thought that stops mid-sentence. `validate` cannot catch
    // that, so the budget has to.
    req.max_tokens = 8000;
    // Low but not zero: this is writing, and a little variation reads better
    // than a template, while staying anchored to the supplied facts.
    req.temperature = 0.3;

    let reply = backend.generate(&req)?;
    match validate(&reply.content, rollup) {
        Ok(text) => Ok(Some(text)),
        Err(r) => {
            on_reject(&r);
            Ok(None)
        }
    }
}

/// The severity worth leading with, for the deterministic fallback line.
pub fn headline_severity(rollup: &Rollup) -> Option<Severity> {
    rollup.recurring.first().map(|r| r.severity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_comply::rollup::RecurringFinding;
    use sc_model::MockBackend;

    fn rollup() -> Rollup {
        Rollup {
            frameworks: 10,
            controls: 110,
            passed: 42,
            gaps: 6,
            unknown: 62,
            errors: 0,
            recurring: vec![RecurringFinding {
                check: "secret-scanning-configured".into(),
                frameworks: vec!["SOC 2".into(), "ISO 27001".into(), "PCI DSS".into()],
                severity: Severity::Critical,
                rationale: "Automated secret detection prevents recurrence.".into(),
                remediation: Some("Add gitleaks to CI.".into()),
            }],
            weakest_coverage: vec![("EU cluster".into(), 0.20), ("SOC 2".into(), 0.44)],
            disabled_capabilities: vec!["command-exit-code".into()],
            by_section: [
                (
                    sc_comply::Section::Code,
                    sc_comply::evidence::Score {
                        total: 40,
                        passed: 30,
                        gaps: 4,
                        unknown: 6,
                        ..Default::default()
                    },
                ),
                (
                    sc_comply::Section::Organizational,
                    sc_comply::evidence::Score {
                        total: 70,
                        unknown: 70,
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
        }
    }

    /// The brief must hand over the per-domain figures and forbid blending them.
    #[test]
    fn the_prompt_gives_the_evidence_domains_and_forbids_averaging_them() {
        let msgs = narrative_messages(&rollup(), "acme-api");
        let system = &msgs[0].content;
        let user = &msgs[1].content;

        assert!(user.contains("By evidence domain"), "{user}");
        assert!(user.contains("Code"), "{user}");
        assert!(user.contains("Organizational"), "{user}");
        assert!(
            system.contains("NEVER \\\n            average") || system.contains("NEVER average"),
            "the brief must forbid blending the domains: {system}"
        );
        assert!(
            system.contains("hide a poor result in code"),
            "the brief must say WHY blending is wrong: {system}"
        );
    }

    /// A per-domain percentage must survive validation.
    ///
    /// The brief now asks the model to lead with the domain figures, so those
    /// figures have to be in the allow-list. Without this the summary would be
    /// rejected exactly when it did what it was told.
    #[test]
    fn a_per_domain_percentage_is_not_treated_as_invented() {
        let r = rollup();
        // Code: 34 of 40 determinable = 85%.
        let det = r.by_section[&sc_comply::Section::Code].determinacy();
        assert_eq!((det * 100.0).round() as u32, 85);

        let text = "Evidence that lives in source is 85% determinable. Everything \
                    organizational sits outside this repository and is reported unknown.";
        assert!(
            validate(text, &r).is_ok(),
            "a figure the prompt supplied must not be rejected as invented"
        );
    }

    /// A figure from no domain at all is still rejected.
    #[test]
    fn a_figure_from_nowhere_is_still_rejected() {
        let text = "Evidence that lives in source is 73% determinable.";
        assert!(matches!(
            validate(text, &rollup()),
            Err(Rejection::InventedFigure(_))
        ));
    }

    #[test]
    fn the_prompt_supplies_the_facts_and_forbids_inventing_them() {
        let msgs = narrative_messages(&rollup(), "acme-api");
        let system = &msgs[0].content;
        let user = &msgs[1].content;

        assert!(system.contains("ONLY the figures given"));
        assert!(system.contains("NEVER state or imply"));
        assert!(
            system.contains("does NOT mean a failure"),
            "the Unknown semantics must be taught, or the summary will report them as failures"
        );
        assert!(user.contains("acme-api"));
        assert!(user.contains("110 controls"));
        assert!(user.contains("secret-scanning-configured"));
        assert!(
            user.contains("3 frameworks"),
            "leverage must be in the facts"
        );
    }

    #[test]
    fn accepts_a_good_narrative() {
        let text = "This project has been scanned against ten compliance frameworks covering \
                    110 controls. Evidence was found in source for 42 of them; 6 are \
                    outstanding gaps. The remaining controls could not be determined from \
                    source code, which is expected: most of any framework covers governance, \
                    training and vendor management that no code scan can see.\n\n\
                    The most significant outstanding item is automated secret scanning, which \
                    is absent and appears as a gap in three separate frameworks — a single \
                    change with disproportionate benefit.";
        assert!(validate(text, &rollup()).is_ok());
    }

    #[test]
    fn rejects_a_compliance_claim() {
        // The highest-risk failure. A summary saying this on a customer's page
        // is a liability, not a wording nit.
        for bad in [
            "The project is compliant with SOC 2.",
            "This system is fully compliant.",
            "The organization is certified against ISO 27001.",
            "The codebase is audit-ready.",
            "This meets all requirements.",
        ] {
            let r = validate(bad, &rollup());
            assert!(
                matches!(r, Err(Rejection::AttestationLanguage(_))),
                "accepted an attestation: {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_an_invented_percentage() {
        // 91% appears nowhere in the rollup.
        let text = "The project achieves 91% coverage across the assessed frameworks.";
        assert!(matches!(
            validate(text, &rollup()),
            Err(Rejection::InventedFigure(_))
        ));
    }

    #[test]
    fn allows_the_real_figures_and_rounding() {
        let r = rollup();
        // pass_rate = 42/110 = 38%, determinacy = 48/110 = 44%, weakest = 20%.
        for pct in [38, 44, 20] {
            let text = format!("Determinacy stands at {pct}% of assessed controls.");
            assert!(
                validate(&text, &r).is_ok(),
                "rejected a legitimate figure: {pct}%"
            );
        }
        // And a ±1 rounding is fine.
        assert!(validate("Roughly 39% passed.", &r).is_ok());
    }

    #[test]
    fn rejects_an_over_long_narrative() {
        let text = "word ".repeat(600);
        assert!(matches!(
            validate(&text, &rollup()),
            Err(Rejection::TooLong(_))
        ));
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(validate("   \n ", &rollup()), Err(Rejection::Empty));
    }

    #[test]
    fn rejects_a_truncated_narrative() {
        // Observed live: a low token budget produced "The tool verified 42
        // controls" and stopped. It reads as a complete thought, so nothing
        // else here would catch it.
        let cut = "We assessed the project against 111 controls across 10 frameworks. \
                   The tool verified 42 controls";
        assert_eq!(validate(cut, &rollup()), Err(Rejection::Truncated));
    }

    #[test]
    fn accepts_normal_sentence_endings() {
        for ending in [
            "Six gaps remain outstanding.",
            "Is that acceptable?",
            "The remainder require documentary evidence (see below).",
        ] {
            let text = format!("Ten frameworks were assessed. {ending}");
            assert!(validate(&text, &rollup()).is_ok(), "rejected: {ending}");
        }
    }

    #[test]
    fn generate_returns_the_text_on_a_good_reply() {
        let good = "Ten frameworks were assessed covering 110 controls, with evidence found \
                    for 42. Six gaps remain outstanding. The rest could not be determined from \
                    source, which is expected for governance controls.";
        let backend = MockBackend::new([good]);
        let mut seen = Vec::new();
        let mut on_reject = |r: &Rejection| seen.push(r.clone());
        let out = generate(&backend, &rollup(), "proj", &mut on_reject).expect("generate");
        assert!(out.is_some());
        assert!(seen.is_empty());
    }

    #[test]
    fn generate_drops_a_bad_narrative_rather_than_publishing_it() {
        // The safety property: an unusable narrative must not reach a page, and
        // must not fail the export either — the page is fine without one.
        let backend = MockBackend::new(["This project is fully compliant with all frameworks."]);
        let mut seen = Vec::new();
        let mut on_reject = |r: &Rejection| seen.push(r.clone());
        let out = generate(&backend, &rollup(), "proj", &mut on_reject).expect("no error");
        assert_eq!(out, None, "an attestation must never be published");
        assert_eq!(seen.len(), 1, "and the operator must be told why");
    }

    #[test]
    fn a_backend_error_propagates_so_the_caller_can_decide() {
        let backend = MockBackend::new(Vec::<String>::new());
        let mut noop = |_: &Rejection| {};
        assert!(generate(&backend, &rollup(), "proj", &mut noop).is_err());
    }

    #[test]
    fn percentage_scanner_handles_decimals_and_bare_numbers() {
        assert_eq!(find_percentages("43% and 44.6% here"), vec![43, 44]);
        assert_eq!(
            find_percentages("110 controls, no percent"),
            Vec::<u32>::new()
        );
        assert_eq!(find_percentages("100%"), vec![100]);
    }
}
