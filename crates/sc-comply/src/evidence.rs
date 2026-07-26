//! Evidence, results, and the evidence pack itself.
//!
//! Everything here derives both `Serialize` and `Deserialize`: the JSON report
//! must round-trip, because an auditor diffs this quarter's pack against last
//! quarter's. (`sc-eval` gets away with `Deserialize`-only on its config types
//! because it never reloads its reports; this crate does.)
//!
//! See `docs/specs/13-compliance-evidence.md`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::section::Section;
use crate::status::{ControlStatus, Severity};

/// Cited excerpts longer than this are truncated. An evidence pack is read by a
/// human; a 400-character minified-JS line helps nobody.
pub const EXCERPT_MAX_CHARS: usize = 200;

/// One citation: where we looked, what we saw, and who saw it.
///
/// `produced_by` is not decoration. Once the retrieval/LLM collector lands, a
/// reader must be able to tell a deterministic regex hit from a model inference
/// at a glance — those carry very different evidentiary weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Workspace-relative, forward-slashed. Normalized so a pack generated on
    /// Windows is byte-identical to one generated on Linux — required for
    /// quarter-over-quarter diffing.
    pub file: String,
    /// 1-based line number. `None` for whole-file facts (a file existing or
    /// being absent has no meaningful line).
    pub line: Option<u32>,
    /// The cited text, trimmed and truncated to [`EXCERPT_MAX_CHARS`].
    pub excerpt: String,
    /// `"<control_id>/<check_id>"` — traces every citation back to the check
    /// that produced it.
    pub check_id: String,
    /// The [`Collector::name`](crate::collector::Collector::name) that produced
    /// this.
    pub produced_by: String,
    /// The cited path is matched by a `.gitignore` rule.
    ///
    /// Such a file is present on disk but not in version control. That is still
    /// a real exposure — it lives on developer machines and in backups — but it
    /// is not the same claim as "committed to source", and a report that
    /// conflates the two overstates its findings.
    #[serde(default)]
    pub untracked: bool,
}

impl Evidence {
    /// Build a citation, normalizing the path and truncating the excerpt.
    pub fn new(
        file: impl Into<String>,
        line: Option<u32>,
        excerpt: impl AsRef<str>,
        check_id: impl Into<String>,
        produced_by: impl Into<String>,
    ) -> Self {
        Evidence {
            file: normalize_path(&file.into()),
            line,
            excerpt: truncate_excerpt(excerpt.as_ref()),
            check_id: check_id.into(),
            produced_by: produced_by.into(),
            untracked: false,
        }
    }

    /// Mark this citation as coming from a gitignored path.
    pub fn untracked(mut self, untracked: bool) -> Self {
        self.untracked = untracked;
        self
    }

    /// `path/to/file.rs:42`, or just the path for whole-file facts.
    pub fn locator(&self) -> String {
        match self.line {
            Some(l) => format!("{}:{}", self.file, l),
            None => self.file.clone(),
        }
    }

    /// The locator plus an `[untracked]` marker where applicable — what the
    /// report renders.
    pub fn cite(&self) -> String {
        if self.untracked {
            format!("{} [untracked]", self.locator())
        } else {
            self.locator()
        }
    }
}

/// Forward-slash a path, matching `sc_index::collect_sources`' normalization.
pub fn normalize_path(p: &str) -> String {
    p.replace('\\', "/")
}

/// Trim and cap an excerpt, appending an ellipsis when cut.
///
/// Truncates on a char boundary — evidence excerpts come from arbitrary source
/// files and slicing mid-codepoint would panic.
pub fn truncate_excerpt(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() <= EXCERPT_MAX_CHARS {
        return t.to_string();
    }
    let cut: String = t.chars().take(EXCERPT_MAX_CHARS).collect();
    format!("{cut}…")
}

/// The result of evaluating one check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub check_id: String,
    /// The rendered check kind, for the report manifest.
    pub kind: String,
    pub status: ControlStatus,
    pub weight: f64,
    pub evidence: Vec<Evidence>,
    /// Always populated when the check could not be determined.
    pub note: Option<String>,
    /// The pack author's stated reason this check is evidence for the control.
    pub rationale: String,
}

/// The result of evaluating one control.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlResult {
    pub id: String,
    pub title: String,
    /// The evidence domain this control belongs to, carried from the pack.
    ///
    /// Scoring reads this rather than the pack, so a report stays scoreable
    /// after serialization without needing the pack that produced it.
    pub section: Section,
    pub clause: String,
    pub intent: String,
    pub severity: Severity,
    pub status: ControlStatus,
    pub checks: Vec<CheckResult>,
    /// Explains *how* the status was derived, e.g.
    /// `"weighted: 7.0/11.0 observable = 64% (pass>=75%, gap<40%)"`.
    /// Auditors ask this question every time; the tool should answer it before
    /// being asked.
    pub rationale: String,
    /// Remediation guidance from the pack, if any.
    pub remediation: Option<String>,
}

impl ControlResult {
    /// Every citation across every check of this control.
    pub fn all_evidence(&self) -> Vec<&Evidence> {
        self.checks.iter().flat_map(|c| c.evidence.iter()).collect()
    }

    /// The checks that did not pass, for the gap detail sections.
    pub fn failing_checks(&self) -> Vec<&CheckResult> {
        self.checks
            .iter()
            .filter(|c| c.status != ControlStatus::Pass && c.status != ControlStatus::NotApplicable)
            .collect()
    }
}

/// A control that did not cleanly pass.
///
/// Shaped for two consumers: the report's gap section, and a future agent
/// remediation loop. [`Finding::anchor`] gives a coder-tier model a concrete
/// `(file, line)` target in the same way `sc_verify::CompileError` does for
/// `sc-workflow`'s compile-driven loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub control_id: String,
    pub control_title: String,
    pub clause: String,
    pub status: ControlStatus,
    pub severity: Severity,
    /// One line stating what is wrong.
    pub summary: String,
    /// What to do about it. `None` is common for `Unknown` findings, which
    /// often have no code-side fix at all — the remedy is to go get a document.
    pub remediation: Option<String>,
    pub evidence: Vec<Evidence>,
}

impl Finding {
    /// The primary code anchor, if this finding has one.
    ///
    /// `None` for process and organizational findings, which is why SARIF
    /// output covers only a subset of the pack.
    pub fn anchor(&self) -> Option<(&str, u32)> {
        self.evidence
            .iter()
            .find_map(|e| e.line.map(|l| (e.file.as_str(), l)))
    }
}

/// Framework identity, reproduced in the report header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameworkMeta {
    pub id: String,
    pub name: String,
    pub version: String,
    pub authority: String,
}

/// Control counts for a run.
///
/// Deliberately *not* a single headline percentage. "78% SOC 2 compliant" is
/// exactly the misreading this crate exists to prevent, so callers get counts
/// plus two ratios that must be read together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score {
    pub total: usize,
    pub passed: usize,
    pub gaps: usize,
    pub unknown: usize,
    pub errors: usize,
    pub not_applicable: usize,
}

impl Score {
    /// Tally a run's control results.
    pub fn tally(controls: &[ControlResult]) -> Self {
        let mut s = Score::default();
        for c in controls {
            s.count(c.status);
        }
        s
    }

    /// Tally each evidence domain separately.
    ///
    /// This is the answer to a scoring problem that gets *worse* as a pack gets
    /// more honest. [`coverage`](Score::coverage) and
    /// [`determinacy`](Score::determinacy) both count `Unknown` in the
    /// denominator — correct within a domain, since an unobserved control must
    /// dilute the result. But completing a framework means adding mostly
    /// organizational controls, which can never be anything but `Unknown`, so a
    /// single blended figure falls the more of the framework is declared.
    ///
    /// Per-section scores answer questions that have answers: *what fraction of
    /// what a repository can evidence, does it evidence?* An organizational
    /// section reading "0 of 43 determinable" is a statement of scope, not a
    /// failing grade, and it sits beside a Code figure it cannot drag down.
    ///
    /// Sections **partition**: the per-section totals always sum to the flat
    /// [`tally`](Score::tally). Only sections that have controls appear.
    ///
    /// There is deliberately no blended figure derived from this — see the
    /// type-level note on why a single headline number is the misreading this
    /// crate exists to prevent.
    pub fn by_section(controls: &[ControlResult]) -> BTreeMap<Section, Score> {
        let mut out: BTreeMap<Section, Score> = BTreeMap::new();
        for c in controls {
            out.entry(c.section).or_default().count(c.status);
        }
        out
    }

    /// Fold one control's status into the tally.
    fn count(&mut self, status: ControlStatus) {
        self.total += 1;
        match status {
            ControlStatus::Pass => self.passed += 1,
            ControlStatus::Gap => self.gaps += 1,
            ControlStatus::Unknown => self.unknown += 1,
            ControlStatus::Error => self.errors += 1,
            ControlStatus::NotApplicable => self.not_applicable += 1,
        }
    }

    /// Controls actually in scope: everything except N/A.
    pub fn in_scope(&self) -> usize {
        self.total - self.not_applicable
    }

    /// Fraction of in-scope controls that passed.
    ///
    /// N/A is excluded from the denominator because it isn't a win; `Unknown`
    /// is *included* because it isn't one either. This is the conservative
    /// reading and the only defensible one.
    pub fn coverage(&self) -> f64 {
        let d = self.in_scope();
        if d == 0 {
            return 0.0;
        }
        self.passed as f64 / d as f64
    }

    /// Fraction of in-scope controls we could actually determine either way.
    ///
    /// This is the credibility of [`Score::coverage`] and must be printed next
    /// to it. A 100% coverage figure at 12% determinacy means we verified
    /// almost nothing and everything we verified passed — a very different
    /// claim from "we are compliant".
    pub fn determinacy(&self) -> f64 {
        let d = self.in_scope();
        if d == 0 {
            return 0.0;
        }
        (self.passed + self.gaps) as f64 / d as f64
    }

    /// A one-line count summary for the report header.
    pub fn summary_line(&self) -> String {
        format!(
            "{} pass · {} gap · {} unknown · {} error · {} n/a",
            self.passed, self.gaps, self.unknown, self.errors, self.not_applicable
        )
    }
}

/// The deliverable: one framework evaluated against one workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidencePack {
    /// Bumped when the JSON shape changes, so downstream consumers can migrate.
    pub schema_version: u32,
    pub framework: FrameworkMeta,
    pub workspace: String,
    /// RFC3339 UTC. Injected rather than sampled internally so report tests are
    /// deterministic.
    pub generated_at: String,
    /// The pack's own scope disclaimer, reproduced verbatim and rendered
    /// *before* the numbers.
    pub scope_note: String,
    pub controls: Vec<ControlResult>,
    pub score: Score,
    /// Capabilities switched off for this run (e.g. `"command-exit-code"`), so
    /// a reader knows why some controls came back `Unknown`.
    pub disabled_capabilities: Vec<String>,
}

impl EvidencePack {
    pub fn new(
        framework: FrameworkMeta,
        workspace: String,
        generated_at: String,
        scope_note: String,
        controls: Vec<ControlResult>,
        disabled_capabilities: Vec<String>,
    ) -> Self {
        let score = Score::tally(&controls);
        EvidencePack {
            schema_version: 1,
            framework,
            workspace,
            generated_at,
            scope_note,
            controls,
            score,
            disabled_capabilities,
        }
    }

    /// Every non-passing, non-N/A control as a [`Finding`].
    ///
    /// This is the seam a future agent remediation loop consumes.
    pub fn findings(&self) -> Vec<Finding> {
        self.controls
            .iter()
            .filter(|c| c.status != ControlStatus::Pass && c.status != ControlStatus::NotApplicable)
            .map(|c| Finding {
                control_id: c.id.clone(),
                control_title: c.title.clone(),
                clause: c.clause.clone(),
                status: c.status,
                severity: c.severity,
                summary: format!("{} [{}]: {}", c.id, c.status.label(), c.rationale),
                remediation: c.remediation.clone(),
                evidence: c
                    .checks
                    .iter()
                    .flat_map(|k| k.evidence.iter().cloned())
                    .collect(),
            })
            .collect()
    }

    /// Controls sorted for the report summary table: problems first, then by
    /// severity descending, then by id for stability.
    pub fn controls_for_report(&self) -> Vec<&ControlResult> {
        let mut v: Vec<&ControlResult> = self.controls.iter().collect();
        v.sort_by(|a, b| {
            a.status
                .report_order()
                .cmp(&b.status.report_order())
                .then(b.severity.cmp(&a.severity))
                .then(a.id.cmp(&b.id))
        });
        v
    }
}

/// Current UTC time as an RFC3339 string.
///
/// Hand-rolled rather than pulling in a date crate for one field. Callers
/// should treat this as an *input* to [`EvidencePack::new`] rather than having
/// the renderer sample a clock — a report that embeds a live timestamp has
/// untestable output.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_rfc3339(secs)
}

/// Format Unix seconds as RFC3339 UTC.
///
/// Uses Howard Hinnant's `civil_from_days` algorithm, which is exact for all
/// dates in the proleptic Gregorian calendar and needs no lookup tables.
pub fn format_rfc3339(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let rem = unix_secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // civil_from_days: shift the epoch to 0000-03-01 so leap day lands last.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl(id: &str, status: ControlStatus) -> ControlResult {
        sectioned(id, status, Section::Code)
    }

    fn sectioned(id: &str, status: ControlStatus, section: Section) -> ControlResult {
        ControlResult {
            id: id.to_string(),
            title: "t".into(),
            section,
            clause: "c".into(),
            intent: "i".into(),
            severity: Severity::Medium,
            status,
            checks: vec![],
            rationale: "r".into(),
            remediation: None,
        }
    }

    /// **The property the whole sectioning design rests on.**
    ///
    /// Completing a framework means adding organizational controls, which can
    /// never be anything but `Unknown`. If that could move the Code score, then
    /// every honest addition to a pack would make its most trustworthy number
    /// look worse — and the rational response would be to stop declaring
    /// controls the tool cannot see, which is precisely the dishonesty the
    /// declarations exist to prevent.
    #[test]
    fn organizational_controls_cannot_move_the_code_score() {
        let code = vec![
            sectioned("A", ControlStatus::Pass, Section::Code),
            sectioned("B", ControlStatus::Pass, Section::Code),
            sectioned("C", ControlStatus::Gap, Section::Code),
        ];
        let before = Score::by_section(&code);
        let code_before = before[&Section::Code];

        // Now declare forty organizational controls, all undeterminable — the
        // realistic shape of completing SOC 2 or ISO 27001.
        let mut after_controls = code.clone();
        for i in 0..40 {
            after_controls.push(sectioned(
                &format!("ORG{i}"),
                ControlStatus::Unknown,
                Section::Organizational,
            ));
        }
        let after = Score::by_section(&after_controls);

        assert_eq!(
            after[&Section::Code],
            code_before,
            "declaring organizational controls changed the Code tally"
        );
        assert!(
            (after[&Section::Code].determinacy() - 1.0).abs() < f64::EPSILON,
            "Code determinacy must stay at 100%, got {}",
            after[&Section::Code].determinacy()
        );

        // The blended figure, for contrast: this is what a single score does.
        let blended = Score::tally(&after_controls);
        assert!(
            blended.determinacy() < 0.08,
            "the blended figure should have collapsed, got {}",
            blended.determinacy()
        );
    }

    #[test]
    fn sections_partition_the_controls() {
        // Per-section totals must sum to the flat tally: a section split that
        // dropped or duplicated a control would silently misreport every score.
        let controls = vec![
            sectioned("A", ControlStatus::Pass, Section::Code),
            sectioned("B", ControlStatus::Gap, Section::Infrastructure),
            sectioned("C", ControlStatus::Unknown, Section::Documentation),
            sectioned("D", ControlStatus::Unknown, Section::Organizational),
            sectioned("E", ControlStatus::NotApplicable, Section::Code),
        ];
        let flat = Score::tally(&controls);
        let by = Score::by_section(&controls);

        let sum = |f: fn(&Score) -> usize| by.values().map(f).sum::<usize>();
        assert_eq!(sum(|s| s.total), flat.total);
        assert_eq!(sum(|s| s.passed), flat.passed);
        assert_eq!(sum(|s| s.gaps), flat.gaps);
        assert_eq!(sum(|s| s.unknown), flat.unknown);
        assert_eq!(sum(|s| s.errors), flat.errors);
        assert_eq!(sum(|s| s.not_applicable), flat.not_applicable);
    }

    #[test]
    fn an_absent_section_is_omitted_rather_than_reported_as_zero() {
        // A framework with no infrastructure controls should not render an
        // "Infrastructure — 0%" heading, which reads as a failure rather than
        // as an absence.
        let by = Score::by_section(&[sectioned("A", ControlStatus::Pass, Section::Code)]);
        assert_eq!(by.len(), 1);
        assert!(!by.contains_key(&Section::Infrastructure));
    }

    #[test]
    fn sections_are_ordered_most_source_visible_first() {
        // BTreeMap iteration order is the report's reading order, so the
        // sections a reader can act on come first.
        let controls = vec![
            sectioned("D", ControlStatus::Unknown, Section::Organizational),
            sectioned("A", ControlStatus::Pass, Section::Code),
            sectioned("C", ControlStatus::Unknown, Section::Documentation),
            sectioned("B", ControlStatus::Gap, Section::Infrastructure),
        ];
        let order: Vec<Section> = Score::by_section(&controls).into_keys().collect();
        assert_eq!(order, Section::ALL);
    }

    #[test]
    fn score_excludes_not_applicable_from_denominator() {
        let controls = vec![
            ctrl("A", ControlStatus::Pass),
            ctrl("B", ControlStatus::Gap),
            ctrl("C", ControlStatus::NotApplicable),
            ctrl("D", ControlStatus::NotApplicable),
        ];
        let s = Score::tally(&controls);
        assert_eq!(s.total, 4);
        assert_eq!(s.not_applicable, 2);
        assert_eq!(s.in_scope(), 2);
        // 1 pass of 2 in scope, NOT 1 of 4 and NOT 3 of 4.
        assert!((s.coverage() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_counts_against_coverage_but_not_determinacy() {
        let controls = vec![
            ctrl("A", ControlStatus::Pass),
            ctrl("B", ControlStatus::Unknown),
            ctrl("C", ControlStatus::Unknown),
            ctrl("D", ControlStatus::Unknown),
        ];
        let s = Score::tally(&controls);
        // Coverage is low because unknowns are not wins.
        assert!((s.coverage() - 0.25).abs() < f64::EPSILON);
        // Determinacy says: we only had an opinion on 1 of 4.
        assert!((s.determinacy() - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn determinacy_falls_when_unknowns_rise() {
        let mostly_known = Score::tally(&[
            ctrl("A", ControlStatus::Pass),
            ctrl("B", ControlStatus::Gap),
            ctrl("C", ControlStatus::Unknown),
        ]);
        let mostly_unknown = Score::tally(&[
            ctrl("A", ControlStatus::Pass),
            ctrl("B", ControlStatus::Unknown),
            ctrl("C", ControlStatus::Unknown),
        ]);
        assert!(mostly_known.determinacy() > mostly_unknown.determinacy());
    }

    #[test]
    fn empty_score_does_not_divide_by_zero() {
        let s = Score::tally(&[]);
        assert!((s.coverage() - 0.0).abs() < f64::EPSILON);
        assert!((s.determinacy() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn all_not_applicable_does_not_divide_by_zero() {
        let s = Score::tally(&[ctrl("A", ControlStatus::NotApplicable)]);
        assert_eq!(s.in_scope(), 0);
        assert!((s.coverage() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn findings_exclude_pass_and_not_applicable() {
        let pack = EvidencePack::new(
            FrameworkMeta {
                id: "f".into(),
                name: "F".into(),
                version: "1".into(),
                authority: "A".into(),
            },
            "/ws".into(),
            "2026-01-01T00:00:00Z".into(),
            "scope".into(),
            vec![
                ctrl("A", ControlStatus::Pass),
                ctrl("B", ControlStatus::Gap),
                ctrl("C", ControlStatus::Unknown),
                ctrl("D", ControlStatus::NotApplicable),
                ctrl("E", ControlStatus::Error),
            ],
            vec![],
        );
        let ids: Vec<_> = pack.findings().into_iter().map(|f| f.control_id).collect();
        assert_eq!(ids, vec!["B", "C", "E"]);
    }

    #[test]
    fn report_ordering_puts_gaps_first_then_severity() {
        let mut gap_low = ctrl("Z", ControlStatus::Gap);
        gap_low.severity = Severity::Low;
        let mut gap_crit = ctrl("Y", ControlStatus::Gap);
        gap_crit.severity = Severity::Critical;
        let pack = EvidencePack::new(
            FrameworkMeta {
                id: "f".into(),
                name: "F".into(),
                version: "1".into(),
                authority: "A".into(),
            },
            "/ws".into(),
            "2026-01-01T00:00:00Z".into(),
            "scope".into(),
            vec![ctrl("A", ControlStatus::Pass), gap_low, gap_crit],
            vec![],
        );
        let ids: Vec<_> = pack
            .controls_for_report()
            .iter()
            .map(|c| c.id.clone())
            .collect();
        // Critical gap, then low gap, then the pass.
        assert_eq!(ids, vec!["Y", "Z", "A"]);
    }

    #[test]
    fn evidence_normalizes_paths_and_truncates() {
        let e = Evidence::new("src\\a\\b.rs", Some(4), "  hello  ", "CC1/x", "regex");
        assert_eq!(e.file, "src/a/b.rs");
        assert_eq!(e.excerpt, "hello");
        assert_eq!(e.locator(), "src/a/b.rs:4");

        let long = "x".repeat(EXCERPT_MAX_CHARS + 50);
        let e2 = Evidence::new("f", None, long, "CC1/x", "regex");
        assert_eq!(e2.excerpt.chars().count(), EXCERPT_MAX_CHARS + 1); // + ellipsis
        assert_eq!(e2.locator(), "f");
    }

    #[test]
    fn truncate_excerpt_respects_char_boundaries() {
        // Multi-byte input must not panic or split a codepoint.
        let s = "é".repeat(EXCERPT_MAX_CHARS + 10);
        let out = truncate_excerpt(&s);
        assert_eq!(out.chars().count(), EXCERPT_MAX_CHARS + 1);
    }

    #[test]
    fn finding_anchor_finds_first_line_bearing_evidence() {
        let f = Finding {
            control_id: "CC6.1".into(),
            control_title: "t".into(),
            clause: "c".into(),
            status: ControlStatus::Gap,
            severity: Severity::Critical,
            summary: "s".into(),
            remediation: None,
            evidence: vec![
                Evidence::new("whole-file.txt", None, "", "CC6.1/a", "file"),
                Evidence::new("src/x.rs", Some(12), "bad", "CC6.1/b", "regex"),
            ],
        };
        assert_eq!(f.anchor(), Some(("src/x.rs", 12)));
    }

    #[test]
    fn finding_without_line_evidence_has_no_anchor() {
        let f = Finding {
            control_id: "CC1.1".into(),
            control_title: "t".into(),
            clause: "c".into(),
            status: ControlStatus::Unknown,
            severity: Severity::Low,
            summary: "s".into(),
            remediation: None,
            evidence: vec![Evidence::new("f.txt", None, "", "CC1.1/a", "file")],
        };
        assert_eq!(f.anchor(), None);
    }

    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day, to exercise civil_from_days.
        assert_eq!(format_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(format_rfc3339(1_735_689_599), "2024-12-31T23:59:59Z");
    }

    #[test]
    fn evidence_pack_round_trips_as_json() {
        // An auditor diffs packs across quarters, so this must survive serde
        // in both directions.
        let pack = EvidencePack::new(
            FrameworkMeta {
                id: "soc2".into(),
                name: "SOC 2".into(),
                version: "1.0.0".into(),
                authority: "AICPA".into(),
            },
            "/ws".into(),
            "2026-01-01T00:00:00Z".into(),
            "scope".into(),
            vec![ctrl("CC6.1", ControlStatus::Gap)],
            vec!["command-exit-code".into()],
        );
        let json = serde_json::to_string(&pack).expect("serialize");
        let back: EvidencePack = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(pack, back);
    }
}
