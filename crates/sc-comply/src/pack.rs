//! The framework pack format: controls and checks as data, never code.
//!
//! Adding a framework means authoring a TOML file, not writing Rust. Thirty
//! frameworks differ in *content*, not *mechanism*, so there is one engine and
//! many packs.
//!
//! Loading follows the `sc_eval::TaskSuite` precedent, with one deliberate
//! divergence and one addition:
//!
//! - **No pack-relative path resolution.** `TaskSuite` resolves fixture paths
//!   against the suite file because fixtures ship with the suite. A pack's
//!   globs address the *audited workspace*, which has nothing to do with where
//!   the pack lives; making them pack-relative would be an outright bug.
//! - **A validation pass.** Every regex and glob is compiled at load. A
//!   malformed pack must fail immediately, not halfway through an audit whose
//!   output an auditor will sign.
//!
//! See `docs/specs/13-compliance-evidence.md`.

use std::collections::HashSet;
use std::path::Path;

use regex::Regex;
use sc_proto::{DcError, Result};
use serde::Deserialize;

use crate::aggregate::{Aggregate, WeightCfg};
use crate::glob::Glob;
use crate::status::{Outcome, OutcomePolicy, Severity};

/// Framework identity and scope.
#[derive(Debug, Clone, Deserialize)]
pub struct FrameworkSection {
    pub id: String,
    pub name: String,
    pub version: String,
    pub authority: String,
    /// Rendered verbatim at the top of the evidence pack, *before* the numbers.
    /// This is where a pack states what it cannot see.
    #[serde(default)]
    pub scope_note: String,
}

/// A named assertion over a value extracted from a structured file.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Assertion {
    /// The key path resolves to any value.
    Exists,
    Equals {
        value: toml::Value,
    },
    NotEquals {
        value: toml::Value,
    },
    Gte {
        value: f64,
    },
    Lte {
        value: f64,
    },
    Matches {
        pattern: String,
    },
    /// A non-empty array, table or string.
    NonEmpty,
}

/// Which languages a symbol check should consider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LangSel {
    Rust,
    Python,
    #[serde(rename = "csharp")]
    CSharp,
}

/// The closed vocabulary of deterministic checks.
///
/// Everything expressible here is pure data. What is *not* expressible is
/// listed in the spec and in each pack's `scope_note`: organizational controls,
/// anything requiring a live system, and whole-program semantic properties
/// ("is authorization enforced on *every* admin route?"). Those are the future
/// retrieval collector's territory, and until then they belong in a pack as
/// explicitly-declared `Unknown`, never omitted.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CheckKind {
    /// Any of `paths` exists. Literal workspace-relative paths, not globs, so a
    /// citation names an exact artifact. Directories count.
    FileExists { paths: Vec<String> },

    /// `path` must not exist. A "match" here means the file *was* found.
    FileAbsent { path: String },

    /// At least one line in at least one glob-matched file matches `pattern`.
    RegexMatchInGlob { glob: String, pattern: String },

    /// No line in any glob-matched file matches `pattern`. A "match" is the bad
    /// outcome; the hits are the actionable findings.
    RegexMustNotMatch { glob: String, pattern: String },

    /// A symbol whose *name* matches `name_pattern` is defined somewhere.
    /// Backed by `sc-index`, so Rust/Python/C# only.
    SymbolExists {
        name_pattern: String,
        #[serde(default)]
        languages: Vec<LangSel>,
    },

    /// A dotted key path in a TOML file satisfies `assert`.
    TomlPath {
        path: String,
        key_path: String,
        assert: Assertion,
    },

    /// A dotted/indexed key path in a JSON file satisfies `assert`.
    JsonPath {
        path: String,
        key_path: String,
        assert: Assertion,
    },

    /// Run `command` in the workspace; a "match" is an exit code in
    /// `expect_codes`.
    ///
    /// **Disabled by default.** A pack that can run shell commands is an attack
    /// vector: an auditor downloads a vendor's pack, runs it against a
    /// checkout, and is owned. When disabled these evaluate to `Unknown` with a
    /// stated reason rather than being silently skipped.
    CommandExitCode {
        command: String,
        #[serde(default = "default_expect_codes")]
        expect_codes: Vec<i32>,
        #[serde(default = "default_timeout_secs")]
        timeout_secs: u64,
    },
}

fn default_expect_codes() -> Vec<i32> {
    vec![0]
}

fn default_timeout_secs() -> u64 {
    60
}

fn default_weight() -> f64 {
    1.0
}

impl CheckKind {
    /// A stable label for the report manifest.
    pub fn label(&self) -> &'static str {
        match self {
            CheckKind::FileExists { .. } => "file-exists",
            CheckKind::FileAbsent { .. } => "file-absent",
            CheckKind::RegexMatchInGlob { .. } => "regex-match-in-glob",
            CheckKind::RegexMustNotMatch { .. } => "regex-must-not-match",
            CheckKind::SymbolExists { .. } => "symbol-exists",
            CheckKind::TomlPath { .. } => "toml-path",
            CheckKind::JsonPath { .. } => "json-path",
            CheckKind::CommandExitCode { .. } => "command-exit-code",
        }
    }

    /// A one-line rendering of what this check looks for, for the appendix.
    pub fn describe(&self) -> String {
        match self {
            CheckKind::FileExists { paths } => format!("any of: {}", paths.join(", ")),
            CheckKind::FileAbsent { path } => format!("absent: {path}"),
            CheckKind::RegexMatchInGlob { glob, pattern } => format!("{glob} =~ /{pattern}/"),
            CheckKind::RegexMustNotMatch { glob, pattern } => format!("{glob} !~ /{pattern}/"),
            CheckKind::SymbolExists { name_pattern, .. } => format!("symbol =~ /{name_pattern}/"),
            CheckKind::TomlPath { path, key_path, .. } => format!("{path} :: {key_path}"),
            CheckKind::JsonPath { path, key_path, .. } => format!("{path} :: {key_path}"),
            CheckKind::CommandExitCode { command, .. } => format!("$ {command}"),
        }
    }
}

/// One check within a control.
#[derive(Debug, Clone, Deserialize)]
pub struct Check {
    /// Unique within its control. Cited as `<control_id>/<check_id>`.
    pub id: String,
    #[serde(flatten)]
    pub kind: CheckKind,
    pub on_match: Outcome,
    pub on_no_match: Outcome,
    /// What it means that we could not look at all. Defaults to `on_no_match`.
    #[serde(default)]
    pub on_no_files: Option<Outcome>,
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// Paths matching any of these globs are not searched by this check.
    ///
    /// This exists because a detector inevitably matches its own detection
    /// pattern, and a scanner that reports its own test fixtures and regexes as
    /// findings is one an auditor learns to ignore. The real finding gets
    /// buried under the noise.
    ///
    /// Exclusions are a correctness hazard in a compliance tool — they can hide
    /// genuine findings — so they are per-check rather than global, and every
    /// exclusion that actually suppressed something is disclosed in the report.
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    /// Search only files that are in version control, skipping gitignored ones.
    ///
    /// Set this on any check whose control is about what was **committed**. A
    /// secret sitting untracked in a working directory is a real exposure, but
    /// it is a *different* one: it is not in the repository, not in history, and
    /// not visible to anyone who clones. A control titled "credentials are not
    /// committed to source" that fires on a gitignored file states something
    /// false, which is exactly the overclaiming this engine exists to avoid.
    ///
    /// Pair it with a separate, honestly-worded control if on-disk exposure also
    /// matters — see `local-secret-hygiene` in the shipped SOC 2 pack.
    #[serde(default)]
    pub tracked_only: bool,
    /// Why this check is evidence for the control. Rendered in the report.
    #[serde(default)]
    pub rationale: String,
}

impl Check {
    pub fn policy(&self) -> OutcomePolicy {
        OutcomePolicy {
            on_match: self.on_match,
            on_no_match: self.on_no_match,
            on_no_files: self.on_no_files,
        }
    }
}

/// One control: what the framework requires, and how we look for evidence.
#[derive(Debug, Clone, Deserialize)]
pub struct Control {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub clause: String,
    /// What a human auditor is actually asking. Rendered verbatim.
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub aggregate: Aggregate,
    /// Required when `aggregate = "weighted"`.
    #[serde(default)]
    pub pass_at: Option<f64>,
    #[serde(default)]
    pub gap_below: Option<f64>,
    #[serde(default)]
    pub max_unknown_share: Option<f64>,
    /// What to do about a gap here. Often `None` for organizational controls.
    #[serde(default)]
    pub remediation: Option<String>,
    /// Evaluated before the checks; if it matches, the control is
    /// `NotApplicable` and no check runs.
    #[serde(default)]
    pub not_applicable_if: Option<CheckKind>,
    pub checks: Vec<Check>,
}

impl Control {
    /// Thresholds for weighted aggregation, falling back to the defaults.
    pub fn weight_cfg(&self) -> WeightCfg {
        let d = WeightCfg::default();
        WeightCfg {
            pass_at: self.pass_at.unwrap_or(d.pass_at),
            gap_below: self.gap_below.unwrap_or(d.gap_below),
            max_unknown_share: self.max_unknown_share.unwrap_or(d.max_unknown_share),
        }
    }
}

/// A framework pack.
#[derive(Debug, Clone, Deserialize)]
pub struct Pack {
    pub framework: FrameworkSection,
    pub controls: Vec<Control>,
}

impl Pack {
    /// Parse and validate a pack from TOML text.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let pack: Pack =
            toml::from_str(s).map_err(|e| DcError::Comply(format!("parsing pack: {e}")))?;
        pack.validate()?;
        Ok(pack)
    }

    /// Load and validate a pack from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| DcError::Comply(format!("reading pack {}: {e}", path.display())))?;
        Self::from_toml_str(&text)
    }

    /// Reject malformed packs at load time.
    ///
    /// Everything checked here would otherwise surface as a wrong or missing
    /// finding in an auditor's report, which is far worse than a startup error.
    pub fn validate(&self) -> Result<()> {
        if self.controls.is_empty() {
            return Err(DcError::Comply("pack declares no controls".to_string()));
        }

        let mut seen_controls: HashSet<&str> = HashSet::new();
        for c in &self.controls {
            if c.id.trim().is_empty() {
                return Err(DcError::Comply("a control has an empty id".to_string()));
            }
            if !seen_controls.insert(c.id.as_str()) {
                return Err(DcError::Comply(format!("duplicate control id {:?}", c.id)));
            }
            if c.checks.is_empty() {
                // A control with no checks would aggregate to NotApplicable and
                // silently vanish from the in-scope denominator.
                return Err(DcError::Comply(format!(
                    "control {:?} declares no checks",
                    c.id
                )));
            }

            if c.aggregate == Aggregate::Weighted {
                let cfg = c.weight_cfg();
                if !(0.0..=1.0).contains(&cfg.pass_at) || !(0.0..=1.0).contains(&cfg.gap_below) {
                    return Err(DcError::Comply(format!(
                        "control {:?}: pass_at and gap_below must be within 0.0..=1.0",
                        c.id
                    )));
                }
                if cfg.pass_at <= cfg.gap_below {
                    return Err(DcError::Comply(format!(
                        "control {:?}: pass_at ({}) must exceed gap_below ({})",
                        c.id, cfg.pass_at, cfg.gap_below
                    )));
                }
                if !(0.0..=1.0).contains(&cfg.max_unknown_share) {
                    return Err(DcError::Comply(format!(
                        "control {:?}: max_unknown_share must be within 0.0..=1.0",
                        c.id
                    )));
                }
            }

            if let Some(na) = &c.not_applicable_if {
                validate_kind(&c.id, "not_applicable_if", na)?;
            }

            let mut seen_checks: HashSet<&str> = HashSet::new();
            for k in &c.checks {
                if k.id.trim().is_empty() {
                    return Err(DcError::Comply(format!(
                        "control {:?} has a check with an empty id",
                        c.id
                    )));
                }
                if !seen_checks.insert(k.id.as_str()) {
                    return Err(DcError::Comply(format!(
                        "control {:?}: duplicate check id {:?}",
                        c.id, k.id
                    )));
                }
                if k.weight <= 0.0 || !k.weight.is_finite() {
                    // A zero or negative weight silently removes a check from
                    // weighted scoring while still appearing in the report.
                    return Err(DcError::Comply(format!(
                        "control {:?} check {:?}: weight must be positive and finite",
                        c.id, k.id
                    )));
                }
                for g in &k.exclude_globs {
                    Glob::new(g).map_err(|e| {
                        DcError::Comply(format!(
                            "control {:?} check {:?}: exclude_globs: {e}",
                            c.id, k.id
                        ))
                    })?;
                }
                validate_kind(&c.id, &k.id, &k.kind)?;
            }
        }
        Ok(())
    }

    /// Total check count, for the report manifest.
    pub fn check_count(&self) -> usize {
        self.controls.iter().map(|c| c.checks.len()).sum()
    }
}

/// Compile every regex and glob a check kind carries.
fn validate_kind(control_id: &str, check_id: &str, kind: &CheckKind) -> Result<()> {
    let ctx = |what: &str, e: DcError| {
        DcError::Comply(format!(
            "control {control_id:?} check {check_id:?}: {what}: {e}"
        ))
    };
    match kind {
        CheckKind::RegexMatchInGlob { glob, pattern }
        | CheckKind::RegexMustNotMatch { glob, pattern } => {
            Glob::new(glob).map_err(|e| ctx("glob", e))?;
            Regex::new(pattern).map_err(|e| {
                DcError::Comply(format!(
                    "control {control_id:?} check {check_id:?}: invalid regex {pattern:?}: {e}"
                ))
            })?;
        }
        CheckKind::SymbolExists { name_pattern, .. } => {
            Regex::new(name_pattern).map_err(|e| {
                DcError::Comply(format!(
                    "control {control_id:?} check {check_id:?}: invalid symbol regex: {e}"
                ))
            })?;
        }
        CheckKind::TomlPath { assert, .. } | CheckKind::JsonPath { assert, .. } => {
            if let Assertion::Matches { pattern } = assert {
                Regex::new(pattern).map_err(|e| {
                    DcError::Comply(format!(
                        "control {control_id:?} check {check_id:?}: invalid assert regex: {e}"
                    ))
                })?;
            }
        }
        CheckKind::FileExists { paths } => {
            if paths.is_empty() {
                return Err(DcError::Comply(format!(
                    "control {control_id:?} check {check_id:?}: file-exists needs at least one path"
                )));
            }
        }
        CheckKind::CommandExitCode { command, .. } => {
            if command.trim().is_empty() {
                return Err(DcError::Comply(format!(
                    "control {control_id:?} check {check_id:?}: empty command"
                )));
            }
        }
        CheckKind::FileAbsent { .. } => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped SOC 2 pack, parsed as part of the test suite so a broken
    /// pack fails CI rather than an audit.
    const SOC2: &str = include_str!("../packs/soc2-tsc.toml");

    const MINIMAL: &str = r#"
[framework]
id = "demo"
name = "Demo"
version = "1.0.0"
authority = "None"
scope_note = "nothing"

[[controls]]
id = "D1"
title = "A control"
checks = [
  { id = "c1", kind = "file-exists", paths = ["README.md"], on_match = "pass", on_no_match = "gap" },
]
"#;

    #[test]
    fn parses_the_shipped_soc2_pack() {
        let pack = Pack::from_toml_str(SOC2).expect("shipped pack must parse");
        assert_eq!(pack.framework.id, "soc2-tsc-2017");
        assert!(
            !pack.framework.scope_note.trim().is_empty(),
            "pack must state its scope"
        );

        let ids: Vec<&str> = pack.controls.iter().map(|c| c.id.as_str()).collect();
        for want in ["CC6.1", "CC6.6", "CC7.2", "CC8.1"] {
            assert!(ids.contains(&want), "missing {want} in {ids:?}");
        }
        // The organizational controls must be present-and-Unknown, not omitted:
        // a pack that silently drops them lets a reader believe coverage is
        // complete.
        assert!(ids.iter().any(|i| i.starts_with("CC1.")), "{ids:?}");
        assert!(
            pack.check_count() >= 15,
            "only {} checks",
            pack.check_count()
        );
    }

    #[test]
    fn parses_a_minimal_pack() {
        let pack = Pack::from_toml_str(MINIMAL).expect("parse");
        assert_eq!(pack.controls.len(), 1);
        let c = &pack.controls[0];
        assert_eq!(c.aggregate, Aggregate::All);
        assert_eq!(c.severity, Severity::Medium);
        assert_eq!(c.checks[0].weight, 1.0);
        assert_eq!(c.checks[0].on_no_files, None);
    }

    #[test]
    fn rejects_malformed_toml() {
        let err = Pack::from_toml_str("this is not toml {{{").unwrap_err();
        assert!(format!("{err}").contains("parsing pack"), "{err}");
    }

    #[test]
    fn rejects_a_pack_with_no_controls() {
        let src = r#"
controls = []

[framework]
id = "x"
name = "X"
version = "1"
authority = "A"
"#;
        let err = Pack::from_toml_str(src).unwrap_err();
        assert!(format!("{err}").contains("no controls"), "{err}");
    }

    #[test]
    fn rejects_duplicate_control_ids() {
        let src = MINIMAL.replace(
            "[[controls]]\nid = \"D1\"",
            "[[controls]]\nid = \"D1\"\ntitle = \"dup\"\nchecks = [ { id = \"c1\", kind = \"file-absent\", path = \".env\", on_match = \"gap\", on_no_match = \"pass\" } ]\n\n[[controls]]\nid = \"D1\"",
        );
        let err = Pack::from_toml_str(&src).unwrap_err();
        assert!(format!("{err}").contains("duplicate control id"), "{err}");
    }

    #[test]
    fn rejects_duplicate_check_ids_within_a_control() {
        let src = r#"
[framework]
id = "x"
name = "X"
version = "1"
authority = "A"

[[controls]]
id = "D1"
title = "t"
checks = [
  { id = "same", kind = "file-exists", paths = ["a"], on_match = "pass", on_no_match = "gap" },
  { id = "same", kind = "file-exists", paths = ["b"], on_match = "pass", on_no_match = "gap" },
]
"#;
        let err = Pack::from_toml_str(src).unwrap_err();
        assert!(format!("{err}").contains("duplicate check id"), "{err}");
    }

    #[test]
    fn rejects_a_control_with_no_checks() {
        let src = r#"
[framework]
id = "x"
name = "X"
version = "1"
authority = "A"

[[controls]]
id = "D1"
title = "t"
checks = []
"#;
        let err = Pack::from_toml_str(src).unwrap_err();
        assert!(format!("{err}").contains("no checks"), "{err}");
    }

    #[test]
    fn rejects_weighted_thresholds_that_cross() {
        let src = r#"
[framework]
id = "x"
name = "X"
version = "1"
authority = "A"

[[controls]]
id = "D1"
title = "t"
aggregate = "weighted"
pass_at = 0.4
gap_below = 0.75
checks = [
  { id = "c1", kind = "file-exists", paths = ["a"], on_match = "pass", on_no_match = "gap" },
]
"#;
        let err = Pack::from_toml_str(src).unwrap_err();
        assert!(format!("{err}").contains("must exceed gap_below"), "{err}");
    }

    #[test]
    fn rejects_non_positive_weight() {
        let src = r#"
[framework]
id = "x"
name = "X"
version = "1"
authority = "A"

[[controls]]
id = "D1"
title = "t"
checks = [
  { id = "c1", kind = "file-exists", paths = ["a"], on_match = "pass", on_no_match = "gap", weight = 0.0 },
]
"#;
        let err = Pack::from_toml_str(src).unwrap_err();
        assert!(
            format!("{err}").contains("weight must be positive"),
            "{err}"
        );
    }

    #[test]
    fn rejects_an_invalid_regex_at_load_not_mid_audit() {
        let src = r#"
[framework]
id = "x"
name = "X"
version = "1"
authority = "A"

[[controls]]
id = "D1"
title = "t"
checks = [
  { id = "c1", kind = "regex-match-in-glob", glob = "**/*", pattern = "([unclosed", on_match = "pass", on_no_match = "gap" },
]
"#;
        let err = Pack::from_toml_str(src).unwrap_err();
        assert!(format!("{err}").contains("invalid regex"), "{err}");
    }

    #[test]
    fn rejects_look_around_which_the_regex_crate_cannot_compile() {
        // An easy trap for a pack author coming from PCRE: negative look-ahead
        // is the natural way to write "http:// but not localhost", and the
        // `regex` crate rejects it. Better a load error than a silent
        // never-matching check that reads as a clean pass.
        let src = r#"
[framework]
id = "x"
name = "X"
version = "1"
authority = "A"

[[controls]]
id = "D1"
title = "t"
checks = [
  { id = "c1", kind = "regex-must-not-match", glob = "**/*", pattern = "http://(?!localhost)", on_match = "gap", on_no_match = "pass" },
]
"#;
        let err = Pack::from_toml_str(src).unwrap_err();
        assert!(format!("{err}").contains("invalid regex"), "{err}");
    }

    #[test]
    fn rejects_an_invalid_glob_at_load() {
        let src = r#"
[framework]
id = "x"
name = "X"
version = "1"
authority = "A"

[[controls]]
id = "D1"
title = "t"
checks = [
  { id = "c1", kind = "regex-match-in-glob", glob = "{a,b", pattern = "x", on_match = "pass", on_no_match = "gap" },
]
"#;
        let err = Pack::from_toml_str(src).unwrap_err();
        assert!(format!("{err}").contains("glob"), "{err}");
    }

    #[test]
    fn rejects_file_exists_with_no_paths() {
        let src = r#"
[framework]
id = "x"
name = "X"
version = "1"
authority = "A"

[[controls]]
id = "D1"
title = "t"
checks = [
  { id = "c1", kind = "file-exists", paths = [], on_match = "pass", on_no_match = "gap" },
]
"#;
        let err = Pack::from_toml_str(src).unwrap_err();
        assert!(format!("{err}").contains("at least one path"), "{err}");
    }

    #[test]
    fn weight_cfg_falls_back_to_defaults() {
        let pack = Pack::from_toml_str(MINIMAL).expect("parse");
        let cfg = pack.controls[0].weight_cfg();
        let d = WeightCfg::default();
        assert!((cfg.pass_at - d.pass_at).abs() < f64::EPSILON);
        assert!((cfg.gap_below - d.gap_below).abs() < f64::EPSILON);
    }

    #[test]
    fn check_kind_labels_are_stable() {
        let k = CheckKind::FileAbsent {
            path: ".env".into(),
        };
        assert_eq!(k.label(), "file-absent");
        assert!(k.describe().contains(".env"));
    }
}
