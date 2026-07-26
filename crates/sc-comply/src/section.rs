//! Evidence domains — where a control's evidence physically lives.
//!
//! # Why this exists
//!
//! A framework's controls are not the same *kind* of thing. "Dependencies are
//! scanned for known vulnerabilities" is answerable from a repository. "The
//! board exercises oversight of the security programme" is not, and no amount of
//! scanning will ever make it so.
//!
//! Scoring both in one denominator conflates two different questions — *can a
//! repository evidence this?* and *does a repository have anything to do with
//! this?* — and the blend gets worse as a pack gets more complete. The engine
//! counts `Unknown` in the denominator of both
//! [`coverage`](crate::evidence::Score::coverage) and
//! [`determinacy`](crate::evidence::Score::determinacy), deliberately: an
//! unobserved control must dilute the result, or a pack could inflate its score
//! by declaring things it cannot see.
//!
//! That is right within a domain and misleading across domains. Completing a
//! framework means adding mostly organizational controls, so a single blended
//! figure *falls* the more honest the pack becomes:
//!
//! | controls | can `pass` | blended determinacy |
//! |---|---|---|
//! | 111 (today) | 68 | 61% |
//! | 400 | 68 | 17% |
//! | 700 | 68 | 10% |
//!
//! Sections split the denominator, so each score answers a question that has an
//! answer. Code stays at ~86% no matter how many governance controls are
//! declared alongside it — and that property is asserted directly in
//! `Score::by_section`'s tests.
//!
//! # Why it sits on the control, not the check
//!
//! A control aggregates its checks into one status, so a control split across
//! sections would have an aggregate belonging to neither. Real packs mix freely:
//! `CIS-16.1` pairs a `CONTRIBUTING.md` presence check with a CI-pipeline check.
//! The control is the smallest unit that still means something.

use serde::{Deserialize, Serialize};

/// Where a control's evidence lives, and therefore who can act on it.
///
/// Ordered from most to least source-visible, which is also the order a reader
/// should scan: the sections at the top are the ones a repository can settle.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Section {
    /// Evidence lives in source files. Owned by engineers.
    ///
    /// The default, because a pack that forgets to classify a control should
    /// land it where the scrutiny is highest rather than quietly parking it
    /// somewhere nothing is expected.
    #[default]
    Code,
    /// Evidence lives in infrastructure-as-code, CI configuration and cloud
    /// settings. Owned by the platform team.
    ///
    /// Source-visible only to the extent the infrastructure is committed. What a
    /// cloud console holds that the repository does not is out of reach, and
    /// checks here should say so rather than reporting a gap.
    Infrastructure,
    /// Evidence is a document that lives in the repository — a policy, a
    /// procedure, a threat model. Owned by the security lead.
    ///
    /// The section where the *document is not the practice* distinction bites
    /// hardest. A committed policy evidences that the policy exists, never that
    /// it is followed, reviewed or acknowledged.
    Documentation,
    /// Evidence lives outside the repository entirely: HR systems, signed
    /// contracts, board minutes, training records, a provider's own audit
    /// report. Owned by HR, Legal or an executive.
    ///
    /// Controls here resolve to `Unknown` by construction. That is not a
    /// deficiency in the pack — it is the honest answer, and the reason these
    /// are declared rather than omitted.
    Organizational,
}

impl Section {
    /// Every section, in reading order.
    pub const ALL: &'static [Section] = &[
        Section::Code,
        Section::Infrastructure,
        Section::Documentation,
        Section::Organizational,
    ];

    /// The kebab-case name, matching the TOML value and the pack filename.
    pub fn slug(self) -> &'static str {
        match self {
            Section::Code => "code",
            Section::Infrastructure => "infrastructure",
            Section::Documentation => "documentation",
            Section::Organizational => "organizational",
        }
    }

    /// Title-case name for a report heading.
    pub fn label(self) -> &'static str {
        match self {
            Section::Code => "Code",
            Section::Infrastructure => "Infrastructure",
            Section::Documentation => "Documentation",
            Section::Organizational => "Organizational",
        }
    }

    /// Where this section's evidence lives — shown under the heading so a low
    /// score reads as a statement of scope rather than a failure.
    pub fn evidence_lives_in(self) -> &'static str {
        match self {
            Section::Code => "source files in this repository",
            Section::Infrastructure => {
                "infrastructure-as-code, CI configuration and cloud settings"
            }
            Section::Documentation => "policies and procedures committed to this repository",
            Section::Organizational => {
                "systems outside this repository — HR records, contracts, board minutes"
            }
        }
    }

    /// Who is able to act on a finding here.
    pub fn owner(self) -> &'static str {
        match self {
            Section::Code => "engineers",
            Section::Infrastructure => "the platform team",
            Section::Documentation => "the security lead",
            Section::Organizational => "HR, Legal or an executive sponsor",
        }
    }

    /// Whether a repository scan can, in principle, settle controls here.
    ///
    /// `false` for [`Section::Organizational`], which is why an `Unknown` there
    /// is reported as expected rather than as a shortfall.
    pub fn is_source_evidenceable(self) -> bool {
        !matches!(self, Section::Organizational)
    }

    /// Parse a slug — used by the pack loader to derive a section from the
    /// filename, so the directory layout and the score cannot disagree.
    pub fn from_slug(s: &str) -> Option<Section> {
        Section::ALL
            .iter()
            .copied()
            .find(|x| x.slug() == s.trim().to_lowercase())
    }
}

impl std::fmt::Display for Section {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_control_with_no_section_is_code() {
        // The serde default. Every pack written before sections existed loads
        // unchanged, with its controls landing where scrutiny is highest.
        #[derive(Deserialize)]
        struct C {
            #[serde(default)]
            section: Section,
        }
        let c: C = toml::from_str("").expect("empty parses");
        assert_eq!(c.section, Section::Code);
    }

    #[test]
    fn slugs_round_trip() {
        for s in Section::ALL {
            assert_eq!(Section::from_slug(s.slug()), Some(*s));
        }
    }

    #[test]
    fn an_unknown_slug_is_rejected_rather_than_defaulted() {
        // Silently defaulting a typo to `code` would put an organizational
        // control into the section a reader trusts most.
        assert_eq!(Section::from_slug("infra"), None);
        assert_eq!(Section::from_slug("governance"), None);
    }

    #[test]
    fn only_organizational_is_beyond_a_source_scan() {
        assert!(Section::Code.is_source_evidenceable());
        assert!(Section::Infrastructure.is_source_evidenceable());
        assert!(Section::Documentation.is_source_evidenceable());
        assert!(!Section::Organizational.is_source_evidenceable());
    }

    #[test]
    fn all_lists_every_variant_in_reading_order() {
        // A missed variant would silently vanish from every report.
        assert_eq!(Section::ALL.len(), 4);
        assert_eq!(Section::ALL[0], Section::Code);
        assert_eq!(Section::ALL[3], Section::Organizational);
        let mut sorted = Section::ALL.to_vec();
        sorted.sort();
        assert_eq!(sorted, Section::ALL, "Ord must match reading order");
    }
}
