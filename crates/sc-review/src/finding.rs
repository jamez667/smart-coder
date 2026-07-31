//! What a review produces: a [`Finding`], anchored to a hunk and a symbol, with
//! its provenance attached (spec 16).
//!
//! The two invariants this module exists to hold:
//!
//! * **A finding is evidence handed to a decision, never an edit.** There is no
//!   patch field here and there must never be one.
//! * **Only a corroborated finding may block or feed a retry.** That is
//!   [`Finding::may_act`], and it is the *only* place the rule is expressed, so
//!   there is one thing to be right about rather than four call sites to keep in
//!   agreement.

use serde::{Deserialize, Serialize};

use crate::diff::HunkId;

/// Which question a finding came from (spec 16 — "Lenses, not one reviewer").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lens {
    /// Does this reimplement something the repo already has?
    Duplication,
    /// Is a failure swallowed, or an error path untested?
    ErrorHandling,
    /// Does this match how the surrounding code solves this?
    AbstractionFit,
    /// Does the diff touch things the subtask didn't ask about?
    UnrelatedChanges,
}

impl Lens {
    /// Every lens, in the spec's order — which is also the order they are dropped
    /// in if review proves too expensive.
    pub const ALL: [Lens; 4] = [
        Lens::Duplication,
        Lens::ErrorHandling,
        Lens::AbstractionFit,
        Lens::UnrelatedChanges,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            Lens::Duplication => "duplication",
            Lens::ErrorHandling => "error-handling",
            Lens::AbstractionFit => "abstraction-fit",
            Lens::UnrelatedChanges => "unrelated-changes",
        }
    }
}

impl std::fmt::Display for Lens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

/// How serious a finding is, as the *reviewer* judged it. Severity ranks within a
/// confidence band; it never promotes a finding across one (spec 16 — "Ranking").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn parse(s: &str) -> Option<Severity> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" | "minor" | "nit" => Some(Severity::Low),
            "medium" | "med" | "moderate" => Some(Severity::Medium),
            "high" | "major" | "critical" => Some(Severity::High),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

/// A reviewer's identity: a connection + model name, exactly like the coder and
/// planner stages (spec 16 — "Reaching the models"). A newtype rather than an
/// enum, because the panel is a *list* of these and a closed set is the thing
/// that stops a second endpoint being added.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelId(pub String);

impl ModelId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a finding points.
///
/// Deliberately *not* keyed on `line`: models cite line numbers badly, so the
/// line is carried as a render hint and is never part of a finding's identity
/// (spec 16 — "line numbers are the weakest identifier available"). The hunk is a
/// choice from a list the reviewer was shown; the symbol is verifiable against
/// `sc-index`, which doubles as a cheap hallucination check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub file: String,
    /// Which hunk — `None` when the reviewer named none, in which case the
    /// finding still attaches to the file (degrading gracefully, per spec).
    pub hunk: Option<HunkId>,
    /// The enclosing fn/type the reviewer named.
    pub symbol: Option<String>,
    /// A hint for rendering. Never an identity.
    pub line: Option<usize>,
}

impl Anchor {
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            file: path.into(),
            hunk: None,
            symbol: None,
            line: None,
        }
    }

    pub fn with_hunk(mut self, hunk: HunkId) -> Self {
        self.hunk = Some(hunk);
        self
    }

    pub fn with_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.symbol = Some(symbol.into());
        self
    }

    pub fn with_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }

    /// Do two anchors point at the same place? File must match; then a shared
    /// hunk *or* a shared symbol is enough. Line is not consulted at all.
    ///
    /// Note what this deliberately does NOT do: two anchors in the same file with
    /// no hunk and no symbol on either do not match. Nothing identifies them
    /// beyond the file, and merging on a filename alone is exactly the
    /// over-merging the spec names as the failure mode.
    pub fn points_at_same_place(&self, other: &Anchor) -> bool {
        if self.file != other.file {
            return false;
        }
        let same_hunk = matches!((self.hunk, other.hunk), (Some(a), Some(b)) if a == b);
        let same_symbol = match (&self.symbol, &other.symbol) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        };
        same_hunk || same_symbol
    }
}

/// One thing a reviewer noticed, with everything needed to decide what to do
/// about it — and nothing that would let it act on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub lens: Lens,
    pub severity: Severity,
    pub anchor: Anchor,
    /// For a human reading the report. Never injected into a retry prompt — a
    /// worker handed prose thrashes (spec 16 — "carries evidence, not a verdict").
    pub summary: String,
    /// A deterministic check agreed. The single gate on acting.
    pub corroborated: bool,
    /// What that check actually found — the text a retry prompt injects. `Some`
    /// exactly when `corroborated`; an uncorroborated finding has nothing to say
    /// by definition.
    pub evidence: Option<String>,
    /// Who raised it.
    pub raised_by: Vec<ModelId>,
    /// Who reviewed this diff at all. Makes a lone finding interpretable: raised
    /// by one of four reviewers is *contested*; raised by one of one is merely
    /// unreviewed, and collapsing the two would be the dishonest shortcut.
    pub considered_by: Vec<ModelId>,
    /// The reviewer named a symbol that `sc-index` could not resolve in that file.
    /// Not fatal — it drops the finding in rank as a cheap hallucination check.
    pub anchor_unresolved: bool,
}

impl Finding {
    /// A new, uncorroborated finding from one reviewer.
    pub fn new(
        lens: Lens,
        severity: Severity,
        anchor: Anchor,
        summary: impl Into<String>,
        raised_by: ModelId,
    ) -> Self {
        Self {
            lens,
            severity,
            anchor,
            summary: summary.into(),
            corroborated: false,
            evidence: None,
            raised_by: vec![raised_by],
            considered_by: Vec::new(),
            anchor_unresolved: false,
        }
    }

    /// Attach what a deterministic check found. This — and only this — is what
    /// lets a finding gate or feed a retry.
    pub fn corroborate(&mut self, evidence: impl Into<String>) {
        self.corroborated = true;
        self.evidence = Some(evidence.into());
    }

    /// **The rule.** A finding may stop a run or feed a retry only if a
    /// deterministic check agreed. Reviewer agreement never reaches this: three
    /// models can be confidently wrong together, so agreement ranks and nothing
    /// more (spec 16).
    pub fn may_act(&self) -> bool {
        self.corroborated
    }

    /// Does this finding meet the bar to stop the run? Corroborated *and* at or
    /// above the configured gating severity — both, never either.
    pub fn is_blocking(&self, gate_at: Severity) -> bool {
        self.may_act() && self.severity >= gate_at
    }

    /// How many reviewers raised it.
    pub fn votes(&self) -> usize {
        self.raised_by.len()
    }

    /// Raised by one reviewer while others looked at the same diff and did not —
    /// worth showing, worth ranking low.
    pub fn is_contested(&self) -> bool {
        self.considered_by.len() > 1 && self.votes() == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> ModelId {
        ModelId::new(s)
    }

    #[test]
    fn an_uncorroborated_finding_can_never_block() {
        // The load-bearing asymmetry of the whole spec. A high-severity finding
        // raised unanimously by a panel still cannot stop a run.
        let mut f = Finding::new(
            Lens::Duplication,
            Severity::High,
            Anchor::file("src/a.rs").with_symbol("format_date"),
            "looks duplicated",
            m("qwen"),
        );
        f.raised_by = vec![m("qwen"), m("gemini"), m("claude")];
        f.considered_by = f.raised_by.clone();

        assert!(!f.may_act());
        assert!(!f.is_blocking(Severity::Low));
        assert!(!f.is_blocking(Severity::High));

        // Only a deterministic check flips it.
        f.corroborate("`format_date` already exists at src/utils/date.rs:41");
        assert!(f.may_act());
        assert!(f.is_blocking(Severity::High));
    }

    #[test]
    fn gating_needs_both_corroboration_and_severity() {
        let mut low = Finding::new(
            Lens::ErrorHandling,
            Severity::Low,
            Anchor::file("a.rs"),
            "swallowed",
            m("qwen"),
        );
        low.corroborate("`except: pass` at a.rs:3");
        assert!(low.may_act(), "corroborated, so it may feed a retry");
        assert!(!low.is_blocking(Severity::High), "but not at this bar");
        assert!(low.is_blocking(Severity::Low));
    }

    #[test]
    fn anchors_match_on_hunk_or_symbol_never_on_line() {
        let a = Anchor::file("src/a.rs")
            .with_hunk(HunkId(1))
            .with_line(10)
            .with_symbol("parse");
        // Same hunk, wildly different line: still the same place.
        let same_hunk = Anchor::file("src/a.rs").with_hunk(HunkId(1)).with_line(93);
        assert!(a.points_at_same_place(&same_hunk));

        // Same symbol, no hunk named: still the same place (case-insensitively).
        let same_symbol = Anchor::file("src/a.rs").with_symbol("PARSE");
        assert!(a.points_at_same_place(&same_symbol));

        // Same line, different hunk AND different symbol: NOT the same place —
        // line proximity is never the primary key.
        let elsewhere = Anchor::file("src/a.rs")
            .with_hunk(HunkId(2))
            .with_symbol("render")
            .with_line(10);
        assert!(!a.points_at_same_place(&elsewhere));

        // Different file never matches.
        assert!(!a.points_at_same_place(&Anchor::file("src/b.rs").with_hunk(HunkId(1))));
    }

    #[test]
    fn two_file_only_anchors_do_not_match_each_other() {
        // Over-merging is the failure mode: a filename identifies nothing, so two
        // findings that name only a file stay two findings.
        let a = Anchor::file("src/a.rs");
        let b = Anchor::file("src/a.rs");
        assert!(!a.points_at_same_place(&b));
    }

    #[test]
    fn contested_means_others_looked_and_did_not_raise_it() {
        let mut f = Finding::new(
            Lens::AbstractionFit,
            Severity::Medium,
            Anchor::file("a.rs").with_hunk(HunkId(0)),
            "doesn't fit",
            m("qwen"),
        );
        // One of one: unreviewed, not contested.
        f.considered_by = vec![m("qwen")];
        assert!(!f.is_contested());
        // One of three: contested.
        f.considered_by = vec![m("qwen"), m("gemini"), m("gpt")];
        assert!(f.is_contested());
    }

    #[test]
    fn findings_round_trip_through_json() {
        // Findings ride the swarm event stream, which must survive
        // Serialize→Deserialize for `--json` and replay parity.
        let mut f = Finding::new(
            Lens::Duplication,
            Severity::High,
            Anchor::file("src/a.rs")
                .with_hunk(HunkId(2))
                .with_symbol("format_date")
                .with_line(12),
            "reimplements format_date",
            m("qwen"),
        );
        f.corroborate("`format_date` already exists at src/utils/date.rs:41");
        f.considered_by = vec![m("qwen"), m("gemini")];
        let line = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<Finding>(&line).unwrap(), f);
    }

    #[test]
    fn severity_parses_the_words_a_model_actually_uses() {
        assert_eq!(Severity::parse("HIGH"), Some(Severity::High));
        assert_eq!(Severity::parse(" critical "), Some(Severity::High));
        assert_eq!(Severity::parse("nit"), Some(Severity::Low));
        assert_eq!(Severity::parse("catastrophic"), None);
    }
}
