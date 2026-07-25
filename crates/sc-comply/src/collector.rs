//! The collector seam: how a check kind becomes an observation.
//!
//! The trait is sync (the whole core of this workspace is; only `sc-win` pulls
//! in tokio, for its GUI event loop), fallible, and object-safe. Those three
//! properties are what let a future retrieval/LLM-backed collector drop in
//! without reshaping a single type here.
//!
//! See `docs/specs/13-compliance-evidence.md`.

use std::path::Path;

use sc_proto::Result;

use crate::evidence::Evidence;
use crate::pack::{Check, CheckKind};
use crate::scan::TextFile;

/// At most this many citations per check. A `regex-must-not-match` over a repo
/// that has committed a thousand keys should report the problem, not paste a
/// thousand lines into an auditor's report.
pub const MAX_EVIDENCE_PER_CHECK: usize = 20;

/// Run-wide options.
#[derive(Debug, Clone)]
pub struct ComplyOptions {
    /// Whether `command-exit-code` checks may actually execute.
    ///
    /// **Defaults to `false`, deliberately.** A pack is data an auditor may
    /// have downloaded from a vendor; letting it run arbitrary shell commands
    /// against a checkout makes the pack format an attack vector. When
    /// disabled, such checks yield `Unknown` with a stated reason — they are
    /// never silently skipped, because a silent skip would read as clean.
    pub allow_commands: bool,
    /// Cap on citations per check.
    pub max_evidence_per_check: usize,
}

impl Default for ComplyOptions {
    fn default() -> Self {
        ComplyOptions {
            allow_commands: false,
            max_evidence_per_check: MAX_EVIDENCE_PER_CHECK,
        }
    }
}

impl ComplyOptions {
    /// The capabilities switched off for this run, named in the report so a
    /// reader knows why some controls came back `Unknown`.
    pub fn disabled_capabilities(&self) -> Vec<String> {
        let mut v = Vec::new();
        if !self.allow_commands {
            v.push("command-exit-code".to_string());
        }
        v
    }
}

/// Everything a collector may need, built once per audit and shared by
/// reference.
///
/// Scanning is done up front precisely so it happens once: a 40-control pack
/// over a 5k-file repo would otherwise re-walk the tree ~150 times.
pub struct AuditContext<'a> {
    /// Absolute path to the audited workspace.
    pub root: &'a Path,
    /// Every readable UTF-8 text file, workspace-relative and forward-slashed.
    pub files: &'a [TextFile],
    pub options: &'a ComplyOptions,
}

impl<'a> AuditContext<'a> {
    pub fn new(root: &'a Path, files: &'a [TextFile], options: &'a ComplyOptions) -> Self {
        AuditContext {
            root,
            files,
            options,
        }
    }
}

/// What a collector saw.
///
/// Deliberately *not* a `Finding`: the collector reports the raw observation,
/// and the engine maps it through the pack's `on_match`/`on_no_match`/
/// `on_no_files` policy. Keeping policy out of collectors is what makes both
/// halves independently testable — and it is why the same regex collector can
/// serve both `regex-match-in-glob` and `regex-must-not-match`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Did the check's condition hold?
    ///
    /// `None` means *could not determine*: no file matched the glob, the file
    /// was unparseable, the language was unsupported, or the capability was
    /// disabled. This is the value that eventually becomes `Unknown`, and
    /// collectors must return it honestly rather than guessing `Some(false)`.
    pub matched: Option<bool>,
    /// Citations. Populated for **both** outcomes — a passing control needs
    /// evidence just as much as a failing one, since an auditor must be able to
    /// verify a pass.
    pub evidence: Vec<Evidence>,
    /// Human explanation. Always populated when `matched` is `None`.
    pub note: Option<String>,
}

impl Observation {
    /// The condition held.
    pub fn matched(evidence: Vec<Evidence>) -> Self {
        Observation {
            matched: Some(true),
            evidence,
            note: None,
        }
    }

    /// The condition did not hold, and we could genuinely look.
    pub fn not_matched(evidence: Vec<Evidence>) -> Self {
        Observation {
            matched: Some(false),
            evidence,
            note: None,
        }
    }

    /// We could not determine the answer. The reason is mandatory: it is what
    /// the auditor's manual-evidence worklist is built from.
    pub fn indeterminate(note: impl Into<String>) -> Self {
        Observation {
            matched: None,
            evidence: vec![],
            note: Some(note.into()),
        }
    }
}

/// A source of evidence for one or more check kinds.
///
/// Object-safe by construction, so the registry holds `Box<dyn Collector>` and
/// a `RetrievalCollector { backend, index }` can later be *added* rather than
/// *integrated*.
pub trait Collector {
    /// Stable name, recorded in [`Evidence::produced_by`] so a reader can tell
    /// a deterministic regex hit from a model inference at a glance.
    fn name(&self) -> &'static str;

    /// Can this collector evaluate this kind of check?
    ///
    /// Registry order decides precedence, which is the affordance that lets a
    /// future collector claim a kind a built-in also handles — an LLM fallback
    /// for `symbol-exists` in a language `sc-index` cannot parse, say.
    fn handles(&self, kind: &CheckKind) -> bool;

    /// Evaluate the check.
    ///
    /// `Err` means the *collector itself* broke — I/O failure, a model backend
    /// down. It must never be used for a legitimately-failing check, because
    /// the engine turns `Err` into [`ControlStatus::Error`], which dominates
    /// every aggregation rule. Reporting a routine gap as an error would
    /// corrupt the score just as badly as reporting an error as a pass.
    ///
    /// [`ControlStatus::Error`]: crate::status::ControlStatus::Error
    fn collect(&self, check: &Check, ctx: &AuditContext<'_>) -> Result<Observation>;
}

/// The set of collectors available to a run.
pub struct Registry {
    collectors: Vec<Box<dyn Collector>>,
}

impl Registry {
    /// Build a registry from an explicit collector list. First match wins.
    pub fn new(collectors: Vec<Box<dyn Collector>>) -> Self {
        Registry { collectors }
    }

    /// The deterministic built-ins. No capabilities beyond the filesystem, and
    /// commands remain gated by [`ComplyOptions::allow_commands`].
    pub fn builtin() -> Self {
        use crate::collectors::{
            CommandCollector, FileCollector, RegexCollector, StructuredCollector, SymbolCollector,
        };
        Registry::new(vec![
            Box::new(FileCollector),
            Box::new(RegexCollector),
            Box::new(SymbolCollector),
            Box::new(StructuredCollector),
            Box::new(CommandCollector),
        ])
    }

    /// The first collector that handles this kind.
    pub fn resolve(&self, kind: &CheckKind) -> Option<&dyn Collector> {
        self.collectors
            .iter()
            .find(|c| c.handles(kind))
            .map(|b| &**b)
    }

    pub fn len(&self) -> usize {
        self.collectors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.collectors.is_empty()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Registry::builtin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_disabled_by_default() {
        // The single most important default in the crate.
        let o = ComplyOptions::default();
        assert!(!o.allow_commands);
        assert_eq!(
            o.disabled_capabilities(),
            vec!["command-exit-code".to_string()]
        );
    }

    #[test]
    fn enabling_commands_clears_the_disabled_list() {
        let o = ComplyOptions {
            allow_commands: true,
            ..Default::default()
        };
        assert!(o.disabled_capabilities().is_empty());
    }

    #[test]
    fn indeterminate_always_carries_a_reason() {
        let o = Observation::indeterminate("no files matched the glob");
        assert_eq!(o.matched, None);
        assert!(o.note.is_some());
        assert!(o.evidence.is_empty());
    }

    #[test]
    fn builtin_registry_resolves_every_check_kind() {
        let r = Registry::builtin();
        let kinds = vec![
            CheckKind::FileExists {
                paths: vec!["a".into()],
            },
            CheckKind::FileAbsent { path: "a".into() },
            CheckKind::RegexMatchInGlob {
                glob: "**/*".into(),
                pattern: "x".into(),
            },
            CheckKind::RegexMustNotMatch {
                glob: "**/*".into(),
                pattern: "x".into(),
            },
            CheckKind::SymbolExists {
                name_pattern: "x".into(),
                languages: vec![],
            },
            CheckKind::TomlPath {
                path: "a".into(),
                key_path: "b".into(),
                assert: crate::pack::Assertion::Exists,
            },
            CheckKind::JsonPath {
                path: "a".into(),
                key_path: "b".into(),
                assert: crate::pack::Assertion::Exists,
            },
            CheckKind::CommandExitCode {
                command: "true".into(),
                expect_codes: vec![0],
                timeout_secs: 1,
            },
        ];
        for k in &kinds {
            assert!(r.resolve(k).is_some(), "no collector handles {}", k.label());
        }
    }

    #[test]
    fn registry_is_not_empty_and_reports_its_size() {
        let r = Registry::builtin();
        assert!(!r.is_empty());
        assert_eq!(r.len(), 5);
    }
}
