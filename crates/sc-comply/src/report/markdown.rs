//! The Markdown evidence pack — the primary deliverable.
//!
//! Section order is a deliberate honesty decision: **scope and limitations come
//! before the numbers.** A reader who sees a score first and the caveats last
//! has been misled by layout even if every individual sentence is true.
//!
//! The summary table is sorted by status (gaps first), never by control id. An
//! auditor opening this wants the problems, then the worklist, then the
//! evidence for what passed.

use std::fmt::Write as _;

use crate::evidence::{ControlResult, EvidencePack, Score};
use crate::status::ControlStatus;

/// Render the full evidence pack as Markdown.
pub fn render(pack: &EvidencePack) -> String {
    let mut s = String::with_capacity(8 * 1024);

    header(&mut s, pack);
    scope_and_limitations(&mut s, pack);
    summary(&mut s, pack);
    gaps(&mut s, pack);
    manual_evidence(&mut s, pack);
    errors(&mut s, pack);
    passing(&mut s, pack);
    not_applicable(&mut s, pack);
    manifest(&mut s, pack);

    s
}

fn header(s: &mut String, pack: &EvidencePack) {
    let _ = writeln!(s, "# {} — evidence pack", pack.framework.name);
    let _ = writeln!(s);
    let _ = writeln!(s, "| | |");
    let _ = writeln!(s, "|---|---|");
    let _ = writeln!(
        s,
        "| Framework | {} ({}) |",
        pack.framework.id, pack.framework.version
    );
    let _ = writeln!(s, "| Authority | {} |", pack.framework.authority);
    let _ = writeln!(s, "| Workspace | `{}` |", pack.workspace);
    let _ = writeln!(s, "| Generated | {} |", pack.generated_at);
    let _ = writeln!(s, "| Tool | sc-comply {} |", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(s);
}

/// Rendered BEFORE the score, deliberately.
fn scope_and_limitations(s: &mut String, pack: &EvidencePack) {
    let _ = writeln!(s, "## 1. Scope and limitations");
    let _ = writeln!(s);
    let _ = writeln!(s, "> **Read this before the numbers.**");
    let _ = writeln!(s, ">");
    let _ = writeln!(
        s,
        "> This is an *argument*, not a verdict. Every result below is evidence toward a"
    );
    let _ = writeln!(
        s,
        "> control, never an attestation of compliance. A passing check means the tool found"
    );
    let _ = writeln!(
        s,
        "> supporting evidence in source; it does not mean the control operates."
    );
    let _ = writeln!(s);

    if !pack.scope_note.is_empty() {
        for line in pack.scope_note.lines() {
            let _ = writeln!(s, "{line}");
        }
        let _ = writeln!(s);
    }

    if !pack.disabled_capabilities.is_empty() {
        let _ = writeln!(s, "**Capabilities disabled for this run:**");
        let _ = writeln!(s);
        for c in &pack.disabled_capabilities {
            let _ = writeln!(
                s,
                "- `{c}` — checks of this kind were not executed and are reported Unknown."
            );
        }
        let _ = writeln!(s);
    }

    let unknown = pack.score.unknown;
    if unknown > 0 {
        let _ = writeln!(
            s,
            "**{unknown} control(s) could not be determined from source** and require manual \
             evidence. They are listed in §4."
        );
        let _ = writeln!(s);
    }
}

fn summary(s: &mut String, pack: &EvidencePack) {
    let sc = &pack.score;
    let _ = writeln!(s, "## 2. Summary");
    let _ = writeln!(s);
    let _ = writeln!(s, "**{}**", sc.summary_line());
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "- **Coverage** — {:.0}% ({} of {} in-scope controls passed)",
        sc.coverage() * 100.0,
        sc.passed,
        sc.in_scope()
    );
    let _ = writeln!(
        s,
        "- **Determinacy** — {:.0}% ({} of {} in-scope controls could be determined either way)",
        sc.determinacy() * 100.0,
        sc.passed + sc.gaps,
        sc.in_scope()
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "> Coverage without determinacy is meaningless: a high coverage figure at low \
         determinacy means almost nothing was verified and everything verified happened to pass. \
         Not-applicable controls are excluded from both denominators; Unknown controls are \
         included, because an unverified control is not a passing one."
    );
    let _ = writeln!(s);

    by_section(s, pack);

    let _ = writeln!(
        s,
        "| Control | Section | Title | Status | Severity | Checks |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|");
    for c in pack.controls_for_report() {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} |",
            c.id,
            c.section.label(),
            escape_pipes(&c.title),
            status_badge(c.status),
            c.severity.label(),
            c.checks.len()
        );
    }
    let _ = writeln!(s);
}

/// Per-evidence-domain scores.
///
/// The blended figures above answer "how much of this framework did we settle?".
/// These answer the question a reader can act on: *of the things a repository
/// can evidence, how many does it?* An Organizational row reading 0% is a
/// statement of scope — the evidence is in an HR system — and it sits beside a
/// Code figure it cannot drag down.
fn by_section(s: &mut String, pack: &EvidencePack) {
    let sections = Score::by_section(&pack.controls);
    if sections.len() < 2 {
        return; // One domain: the blended figures above already say it.
    }

    let _ = writeln!(s, "### By evidence domain");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "| Domain | Evidence lives in | Controls | Pass | Gap | Unknown | Coverage | Determinacy |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|");
    for (section, sc) in &sections {
        let _ = writeln!(
            s,
            "| **{}** | {} | {} | {} | {} | {} | {:.0}% | {:.0}% |",
            section.label(),
            section.evidence_lives_in(),
            sc.total,
            sc.passed,
            sc.gaps,
            sc.unknown,
            sc.coverage() * 100.0,
            sc.determinacy() * 100.0,
        );
    }
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "> These are deliberately **not** combined into one figure. A framework is completed \
         mostly by declaring organizational controls, which a repository can never settle — \
         blending them in would make an honest pack look worse than a selective one, and would \
         let a large governance section hide a poor Code result."
    );
    let _ = writeln!(s);
}

fn gaps(s: &mut String, pack: &EvidencePack) {
    let items: Vec<&ControlResult> = pack
        .controls
        .iter()
        .filter(|c| c.status == ControlStatus::Gap)
        .collect();

    let _ = writeln!(s, "## 3. Gaps");
    let _ = writeln!(s);
    if items.is_empty() {
        let _ = writeln!(
            s,
            "_No gaps identified among the controls this pack can evaluate._"
        );
        let _ = writeln!(s);
        return;
    }

    let mut sorted = items;
    sorted.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.id.cmp(&b.id)));

    for c in sorted {
        control_detail(s, c);
    }
}

/// The Unknowns, phrased as a worklist. This is the section that makes the tool
/// useful rather than annoying.
fn manual_evidence(s: &mut String, pack: &EvidencePack) {
    let items: Vec<&ControlResult> = pack
        .controls
        .iter()
        .filter(|c| c.status == ControlStatus::Unknown)
        .collect();

    let _ = writeln!(s, "## 4. Manual evidence required");
    let _ = writeln!(s);
    if items.is_empty() {
        let _ = writeln!(s, "_Every in-scope control was determinable from source._");
        let _ = writeln!(s);
        return;
    }

    let _ = writeln!(
        s,
        "These controls could not be decided from source-controlled artifacts. Each is a task \
         for the auditor, not a finding against the codebase."
    );
    let _ = writeln!(s);

    let mut sorted = items;
    sorted.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.id.cmp(&b.id)));

    for c in sorted {
        let _ = writeln!(s, "### {} — {}", c.id, c.title);
        let _ = writeln!(s);
        let _ = writeln!(s, "- **Why undetermined:** {}", c.rationale);
        for k in &c.checks {
            if k.status == ControlStatus::Unknown {
                let reason = k
                    .note
                    .clone()
                    .unwrap_or_else(|| "not determinable".to_string());
                let _ = writeln!(s, "- `{}` — {}", k.check_id, reason);
            }
        }
        if let Some(r) = &c.remediation {
            let _ = writeln!(s, "- **Obtain:** {}", one_line(r));
        }
        let _ = writeln!(s);
    }
}

fn errors(s: &mut String, pack: &EvidencePack) {
    let items: Vec<&ControlResult> = pack
        .controls
        .iter()
        .filter(|c| c.status == ControlStatus::Error)
        .collect();

    let _ = writeln!(s, "## 5. Tool errors");
    let _ = writeln!(s);
    if items.is_empty() {
        let _ = writeln!(s, "_None._");
        let _ = writeln!(s);
        return;
    }

    let _ = writeln!(
        s,
        "A collector failed on these controls. **These are tool failures, not compliance \
         findings** — the control status is genuinely unknown, and the run should be repeated \
         after the cause is fixed."
    );
    let _ = writeln!(s);
    for c in items {
        let _ = writeln!(s, "### {} — {}", c.id, c.title);
        let _ = writeln!(s);
        for k in &c.checks {
            if k.status == ControlStatus::Error {
                let note = k
                    .note
                    .clone()
                    .unwrap_or_else(|| "unknown failure".to_string());
                let _ = writeln!(s, "- `{}` — {}", k.check_id, note);
            }
        }
        let _ = writeln!(s);
    }
}

/// Passing controls, WITH their evidence. A pass an auditor cannot verify is
/// worthless, so the citations are not omitted here.
fn passing(s: &mut String, pack: &EvidencePack) {
    let items: Vec<&ControlResult> = pack
        .controls
        .iter()
        .filter(|c| c.status == ControlStatus::Pass)
        .collect();

    let _ = writeln!(s, "## 6. Passing controls");
    let _ = writeln!(s);
    if items.is_empty() {
        let _ = writeln!(s, "_None._");
        let _ = writeln!(s);
        return;
    }

    for c in items {
        let _ = writeln!(s, "<details>");
        let _ = writeln!(
            s,
            "<summary><strong>{}</strong> — {}</summary>",
            c.id, c.title
        );
        let _ = writeln!(s);
        let _ = writeln!(s, "{}", c.rationale);
        let _ = writeln!(s);
        for k in &c.checks {
            let _ = writeln!(s, "- `{}` ({}) — {}", k.check_id, k.kind, k.status.label());
            for e in &k.evidence {
                let _ = writeln!(s, "  - `{}` — {}", e.cite(), escape_pipes(&e.excerpt));
            }
        }
        let _ = writeln!(s);
        let _ = writeln!(s, "</details>");
        let _ = writeln!(s);
    }
}

fn not_applicable(s: &mut String, pack: &EvidencePack) {
    let items: Vec<&ControlResult> = pack
        .controls
        .iter()
        .filter(|c| c.status == ControlStatus::NotApplicable)
        .collect();

    let _ = writeln!(s, "## 7. Not applicable");
    let _ = writeln!(s);
    if items.is_empty() {
        let _ = writeln!(s, "_None._");
        let _ = writeln!(s);
        return;
    }
    for c in items {
        let _ = writeln!(s, "- **{}** — {} ({})", c.id, c.title, c.rationale);
    }
    let _ = writeln!(s);
}

/// Every check, so a reader can re-derive any conclusion in the report.
fn manifest(s: &mut String, pack: &EvidencePack) {
    let _ = writeln!(s, "## 8. Appendix — pack manifest");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "Every check this run evaluated, so any conclusion above can be independently \
         reproduced."
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "| Check | Kind | Status | Weight |");
    let _ = writeln!(s, "|---|---|---|---|");
    for c in &pack.controls {
        for k in &c.checks {
            let _ = writeln!(
                s,
                "| `{}` | {} | {} | {:.1} |",
                k.check_id,
                k.kind,
                k.status.label(),
                k.weight
            );
        }
    }
    let _ = writeln!(s);
}

fn control_detail(s: &mut String, c: &ControlResult) {
    let _ = writeln!(s, "### {} — {} ({})", c.id, c.title, c.severity.label());
    let _ = writeln!(s);
    if !c.clause.is_empty() {
        let _ = writeln!(s, "**Clause:** {}", c.clause);
        let _ = writeln!(s);
    }
    if !c.intent.is_empty() {
        let _ = writeln!(s, "**Intent:** {}", one_line(&c.intent));
        let _ = writeln!(s);
    }
    let _ = writeln!(s, "**Determination:** {}", c.rationale);
    let _ = writeln!(s);

    for k in c.failing_checks() {
        let _ = writeln!(
            s,
            "**`{}`** ({}) — {}",
            k.check_id,
            k.kind,
            k.status.label()
        );
        let _ = writeln!(s);
        if !k.rationale.is_empty() {
            let _ = writeln!(s, "{}", one_line(&k.rationale));
            let _ = writeln!(s);
        }
        if let Some(n) = &k.note {
            let _ = writeln!(s, "_{}_", one_line(n));
            let _ = writeln!(s);
        }
        for e in &k.evidence {
            let _ = writeln!(s, "- `{}`", e.cite());
            if !e.excerpt.is_empty() {
                let _ = writeln!(s, "  ```");
                let _ = writeln!(s, "  {}", e.excerpt);
                let _ = writeln!(s, "  ```");
            }
        }

        // Explain the marker where it appears, rather than leaving the reader
        // to guess what "[untracked]" means.
        if k.evidence.iter().any(|e| e.untracked) {
            let _ = writeln!(
                s,
                "_`[untracked]` — matched by a `.gitignore` rule: present on disk but not in \
                 version control. Still an exposure on developer machines and backups, but not \
                 a commit to source._"
            );
        }
        let _ = writeln!(s);
    }

    if let Some(r) = &c.remediation {
        let _ = writeln!(s, "**Remediation:** {}", one_line(r));
        let _ = writeln!(s);
    }
}

fn status_badge(s: ControlStatus) -> &'static str {
    match s {
        ControlStatus::Pass => "✅ pass",
        ControlStatus::Gap => "❌ gap",
        ControlStatus::Unknown => "❓ unknown",
        ControlStatus::Error => "⚠️ error",
        ControlStatus::NotApplicable => "— n/a",
    }
}

/// Collapse a multi-line pack string into one line for inline rendering.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Keep table cells from breaking on embedded pipes.
fn escape_pipes(s: &str) -> String {
    s.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{CheckResult, Evidence, FrameworkMeta};
    use crate::status::Severity;
    use crate::Section;

    fn evidence(file: &str, line: Option<u32>, excerpt: &str, check: &str) -> Evidence {
        Evidence::new(file, line, excerpt, check, "regex")
    }

    fn check(
        id: &str,
        status: ControlStatus,
        note: Option<&str>,
        ev: Vec<Evidence>,
    ) -> CheckResult {
        CheckResult {
            check_id: id.to_string(),
            kind: "regex-must-not-match".to_string(),
            status,
            weight: 1.0,
            evidence: ev,
            note: note.map(|n| n.to_string()),
            rationale: "why this is evidence".to_string(),
        }
    }

    fn control(
        id: &str,
        status: ControlStatus,
        severity: Severity,
        checks: Vec<CheckResult>,
    ) -> ControlResult {
        ControlResult {
            id: id.to_string(),
            title: format!("{id} title"),
            section: Default::default(),
            clause: format!("TSC {id}"),
            intent: "the intent".to_string(),
            severity,
            status,
            checks,
            rationale: format!("{} determined", id),
            remediation: Some("do the thing".to_string()),
        }
    }

    /// A pack with one of everything, at a fixed timestamp.
    fn sample() -> EvidencePack {
        EvidencePack::new(
            FrameworkMeta {
                id: "soc2-tsc-2017".into(),
                name: "SOC 2 Trust Services Criteria".into(),
                version: "1.0.0".into(),
                authority: "AICPA".into(),
            },
            "/ws".into(),
            "2026-01-01T00:00:00Z".into(),
            "Source-controlled artifacts only.".into(),
            vec![
                control(
                    "CC6.1",
                    ControlStatus::Gap,
                    Severity::Critical,
                    vec![check(
                        "CC6.1/no-keys",
                        ControlStatus::Gap,
                        None,
                        vec![evidence(
                            "deploy/id_rsa",
                            Some(2),
                            "-----BEGIN RSA PRIVATE KEY-----",
                            "CC6.1/no-keys",
                        )],
                    )],
                ),
                control(
                    "CC8.1",
                    ControlStatus::Unknown,
                    Severity::High,
                    vec![check(
                        "CC8.1/pr-review",
                        ControlStatus::Unknown,
                        Some("branch protection lives in the provider API"),
                        vec![],
                    )],
                ),
                control(
                    "CC7.2",
                    ControlStatus::Pass,
                    Severity::Medium,
                    vec![check(
                        "CC7.2/logging",
                        ControlStatus::Pass,
                        None,
                        vec![evidence(
                            "src/lib.rs",
                            Some(4),
                            "tracing::info!",
                            "CC7.2/logging",
                        )],
                    )],
                ),
                control("CC9.9", ControlStatus::NotApplicable, Severity::Low, vec![]),
            ],
            vec!["command-exit-code".into()],
        )
    }

    #[test]
    fn scope_and_limitations_precede_the_score() {
        // Layout is an honesty property: caveats before numbers.
        let md = render(&sample());
        let scope = md
            .find("## 1. Scope and limitations")
            .expect("scope section");
        let summary = md.find("## 2. Summary").expect("summary section");
        assert!(scope < summary, "the score must not precede the caveats");
    }

    #[test]
    fn header_carries_provenance() {
        let md = render(&sample());
        assert!(md.contains("SOC 2 Trust Services Criteria"));
        assert!(md.contains("2026-01-01T00:00:00Z"));
        assert!(md.contains("`/ws`"));
        assert!(md.contains("AICPA"));
    }

    #[test]
    fn disabled_capabilities_are_named() {
        let md = render(&sample());
        assert!(
            md.contains("command-exit-code"),
            "a disabled capability must be disclosed"
        );
    }

    #[test]
    fn gap_cites_file_and_line_with_the_excerpt() {
        let md = render(&sample());
        assert!(md.contains("deploy/id_rsa:2"), "gap must cite file:line");
        assert!(
            md.contains("-----BEGIN RSA PRIVATE KEY-----"),
            "gap must show the excerpt"
        );
    }

    #[test]
    fn unknown_appears_in_the_manual_evidence_worklist_not_in_gaps() {
        let md = render(&sample());
        let gaps = md.find("## 3. Gaps").expect("gaps");
        let manual = md.find("## 4. Manual evidence required").expect("manual");
        let errors = md.find("## 5. Tool errors").expect("errors");

        let gaps_body = &md[gaps..manual];
        let manual_body = &md[manual..errors];

        assert!(
            !gaps_body.contains("CC8.1"),
            "an Unknown must not be listed as a gap"
        );
        assert!(
            manual_body.contains("CC8.1"),
            "an Unknown must appear in the worklist"
        );
        assert!(manual_body.contains("branch protection lives in the provider API"));
    }

    #[test]
    fn passing_controls_still_show_their_evidence() {
        // A pass an auditor cannot verify is worthless.
        let md = render(&sample());
        let passing = md.find("## 6. Passing controls").expect("passing");
        let body = &md[passing..];
        assert!(body.contains("CC7.2"));
        assert!(body.contains("src/lib.rs:4"), "a pass must be citable too");
    }

    #[test]
    fn summary_reports_counts_and_both_ratios_but_no_single_grade() {
        let md = render(&sample());
        assert!(md.contains("1 pass · 1 gap · 1 unknown · 0 error · 1 n/a"));
        assert!(md.contains("**Coverage**"));
        assert!(md.contains("**Determinacy**"));
        // No letter grade or single headline "compliant" percentage.
        assert!(
            !md.contains("compliant"),
            "the report must not imply an attestation"
        );
    }

    #[test]
    fn not_applicable_is_listed_separately() {
        let md = render(&sample());
        let na = md.find("## 7. Not applicable").expect("na");
        assert!(md[na..].contains("CC9.9"));
    }

    #[test]
    fn manifest_lists_every_check_for_reproducibility() {
        let md = render(&sample());
        let man = md.find("## 8. Appendix").expect("manifest");
        let body = &md[man..];
        assert!(body.contains("CC6.1/no-keys"));
        assert!(body.contains("CC8.1/pr-review"));
        assert!(body.contains("CC7.2/logging"));
    }

    #[test]
    fn summary_table_is_sorted_gaps_first() {
        let md = render(&sample());
        let table = md.find("| Control | Section | Title |").expect("table");
        let after = &md[table..];
        let gap_at = after.find("CC6.1").expect("gap row");
        let unknown_at = after.find("CC8.1").expect("unknown row");
        let pass_at = after.find("CC7.2").expect("pass row");
        assert!(gap_at < unknown_at, "gaps sort before unknowns");
        assert!(unknown_at < pass_at, "unknowns sort before passes");
    }

    /// A mixed pack reports each evidence domain separately.
    #[test]
    fn the_summary_breaks_the_score_down_by_evidence_domain() {
        let mut controls = sample().controls;
        controls[0].section = Section::Code;
        controls[1].section = Section::Organizational;
        let pack = EvidencePack::new(
            sample().framework,
            "w".into(),
            "t".into(),
            String::new(),
            controls,
            vec![],
        );
        let md = render(&pack);

        assert!(md.contains("### By evidence domain"), "{md}");
        assert!(md.contains("**Code**"), "{md}");
        assert!(md.contains("**Organizational**"), "{md}");
        // The reader is told where the evidence lives, so a 0% row reads as
        // scope rather than as a failure.
        assert!(md.contains("HR records"), "{md}");
        // And is told explicitly not to blend them.
        assert!(md.contains("not** combined into one figure"), "{md}");
    }

    /// A single-domain pack gets no breakdown — one row restating the blended
    /// figures is noise.
    #[test]
    fn a_single_domain_pack_omits_the_breakdown() {
        let mut controls = sample().controls;
        for c in &mut controls {
            c.section = Section::Code;
        }
        let pack = EvidencePack::new(
            sample().framework,
            "w".into(),
            "t".into(),
            String::new(),
            controls,
            vec![],
        );
        assert!(!render(&pack).contains("### By evidence domain"));
    }

    #[test]
    fn renders_an_empty_pack_without_panicking() {
        let empty = EvidencePack::new(
            FrameworkMeta {
                id: "x".into(),
                name: "X".into(),
                version: "1".into(),
                authority: "A".into(),
            },
            "/ws".into(),
            "2026-01-01T00:00:00Z".into(),
            String::new(),
            vec![],
            vec![],
        );
        let md = render(&empty);
        assert!(md.contains("## 2. Summary"));
        assert!(md.contains("_No gaps identified"));
    }

    #[test]
    fn pipes_in_content_do_not_break_tables() {
        let mut pack = sample();
        pack.controls[0].title = "a | b".to_string();
        let md = render(&pack);
        assert!(md.contains("a \\| b"));
    }
}
