//! The capability table: one entry per thing the gateway can do.
//!
//! A capability declares what it answers, in the vocabulary a caller would
//! actually use. That declaration is the *only* input to routing — so adding a
//! capability is a table edit, never a change to the classifier, and the
//! classifier can be tested against the table without any capability running.
//!
//! Each capability knows exactly what it does and nothing about the others.

use crate::types::{Class, Need};

/// One thing the gateway can do.
#[derive(Debug, Clone)]
pub struct Capability {
    /// Stable identifier, used in traces and tests.
    pub name: &'static str,
    /// What it answers, one line, in the caller's terms.
    pub description: &'static str,
    /// Cost/determinism class — drives ordering, caching and trust.
    pub class: Class,
    /// Words that mean "this capability". Matched case-insensitively against
    /// the need. Weighted higher than `hints` because they name the operation.
    pub verbs: &'static [&'static str],
    /// Words that merely *suggest* this capability — nouns, objects, context.
    pub hints: &'static [&'static str],
    /// The underlying tool this maps to, where one exists. Kept so the gateway
    /// can be diffed against the tool surface it replaces.
    pub backing_tool: Option<&'static str>,
    /// What this capability needs supplied before it can run at all.
    ///
    /// **The structural signal, as opposed to the lexical one.** Vocabulary says
    /// what a need sounds like; this says what it actually contains. A caller who
    /// names a file AND a line range has supplied everything `file.read` requires
    /// — that is a fact about the need, not a guess from its wording, and no
    /// amount of trailing explanation changes it.
    ///
    /// Six rounds of hand-tuning verbs and hints each fixed one phrasing and
    /// broke another, because a model that explains WHY it wants something adds
    /// words that score rival capabilities. Requirements do not drift that way.
    pub requires: &'static [Requirement],
    /// Does this capability need a path to be meaningful?
    pub needs_path: bool,
    /// Does this capability accept a line window (`start`/`limit`)?
    ///
    /// Declared rather than inferred so the executor never passes a window to a
    /// tool whose schema has no such parameter — that would be a validation
    /// error the model would see and could not fix.
    pub windowed: bool,
}

/// Something a capability needs before it can run.
///
/// Each maps to an extractor the executor ALREADY uses to build its call, so a
/// requirement being "satisfied" means exactly "the executor could run this",
/// not "the wording suggests it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// A file or directory path (or a caller-supplied scope).
    Path,
    /// A path naming a FILE specifically — it has an extension.
    ///
    /// Distinct from [`Requirement::Path`] because "src/lib.rs" and "src/agent"
    /// are different requests, and a rule that cannot tell them apart finds
    /// three capabilities runnable for a bare filename and gives up.
    FilePath,
    /// A path naming a DIRECTORY — separators but no file extension.
    DirPath,
    /// A path naming a recorded PROFILE — a `.folded` stack file.
    ///
    /// `perf.hotspots` cannot read an arbitrary file, so declaring it as wanting
    /// any `FilePath` made it a phantom rival for every bare filename and stopped
    /// "src/lib.rs" resolving to the one capability that could actually serve it.
    ProfilePath,
    /// A line range or line number.
    LineRange,
    /// An identifier-shaped word — a function, struct or type name.
    Symbol,
    /// A hyphenated crate name.
    CrateName,
}

impl Requirement {
    /// Is this requirement met by the need?
    ///
    /// Deliberately mirrors the executor's own parsing. A requirement that said
    /// "satisfied" where the executor then failed to extract the value would be
    /// worse than no signal at all.
    pub fn satisfied_by(self, need: &Need) -> bool {
        let text = &need.text;
        match self {
            Requirement::Path => need.scope.is_some() || text.split_whitespace().any(is_path_like),
            Requirement::FilePath => match &need.scope {
                Some(s) => has_extension(s),
                None => text.split_whitespace().any(has_extension),
            },
            Requirement::DirPath => match &need.scope {
                Some(s) => !has_extension(s),
                None => text
                    .split_whitespace()
                    .any(|w| is_path_like(w) && !has_extension(w)),
            },
            Requirement::ProfilePath => {
                let is_profile = |w: &str| {
                    let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.');
                    w.ends_with(".folded") || w.ends_with(".perf") || w.ends_with(".prof")
                };
                match &need.scope {
                    Some(s) => is_profile(s),
                    None => text.split_whitespace().any(is_profile),
                }
            }
            Requirement::LineRange => names_a_line_range(&text.to_lowercase()),
            Requirement::Symbol => text.split_whitespace().any(is_identifier_like),
            Requirement::CrateName => text
                .split_whitespace()
                .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-'))
                .any(|w| w.len() > 3 && w.contains('-') && !w.starts_with('-')),
        }
    }
}

/// How many of a capability's requirements the need supplies.
///
/// `(met, total)`. A capability with no requirements returns `(0, 0)` and gains
/// nothing from this signal — it has to win on vocabulary alone, which is right:
/// there is nothing structural to check.
pub fn requirements_met(cap: &Capability, need: &Need) -> (usize, usize) {
    let met = cap.requires.iter().filter(|r| r.satisfied_by(need)).count();
    (met, cap.requires.len())
}

/// Does the need state a line range or line number?
///
/// Requires the WORD "line" beside a digit, so a version number or a symbol like
/// `sha256` is never read as a range.
pub(crate) fn names_a_line_range(lower: &str) -> bool {
    let Some(at) = lower.find("line") else {
        return false;
    };
    lower[at..].chars().take(24).any(|c| c.is_ascii_digit())
}

/// Is this token identifier-shaped — `snake_case`, `CamelCase`, or `fn()`?
pub(crate) fn is_identifier_like(word: &str) -> bool {
    let clean = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
    if clean.len() < 2 || is_path_like(word) {
        return false;
    }
    clean.contains('_')
        || word.ends_with("()")
        || (clean.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && clean.chars().any(|c| c.is_ascii_lowercase()))
}

/// Score how well this capability matches a need, 0 = no match.
///
/// Deliberately a transparent integer score rather than an embedding: a
/// misroute has to be explainable, and "matched verb `read`, +10" is a debug
/// session that ends in one line. Embeddings can slot in behind the same
/// signature once there's traffic to learn from.
pub fn score(cap: &Capability, need_lower: &str) -> u32 {
    let need_lower = &split_identifiers(&strip_paths(need_lower));
    let mut total = 0;
    for verb in cap.verbs {
        if contains_word(need_lower, verb) {
            total += 10;
        }
    }
    for hint in cap.hints {
        if contains_word(need_lower, hint) {
            total += 3;
        }
    }
    total
}

/// Neutralise the *directory* part of a path token, keeping the filename.
///
/// A DIRECTORY name is a location, not an instruction: opening
/// `crates/demo-core/src/lib.rs` is not a question about crates, and reading
/// `src/web/handler.rs` is not a request to search the web.
///
/// But the **filename is real evidence** that a file is wanted, and an earlier
/// version of this dropped the whole token. The A/B caught it: "show me lines 40
/// through 90 of pathfind.rs exactly" scored 3 and was REFUSED, because the one
/// word identifying it as a file read had been deleted before scoring. A model
/// asking clearly got a refusal, which is the worst outcome this design has.
///
/// So `crates/demo-core/src/lib.rs` becomes `lib.rs`: the misleading segments
/// go, the evidence stays. The executor still sees the untouched need and parses
/// the real path from it.
fn strip_paths(need_lower: &str) -> String {
    need_lower
        .split_whitespace()
        .map(|w| {
            if !is_path_like(w) {
                return w.to_string();
            }
            // Keep the last segment (the filename); drop the directories.
            w.rsplit(['/', '\\']).next().unwrap_or(w).to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Split `snake_case` tokens into their words for matching.
///
/// A model that has seen a tool surface types its names as prose: the A/B
/// captured "read_file pathfind.rs lines 30-114", which scored 3 and was
/// REFUSED because `read_file` is one identifier and never matched the verb
/// `read`. The word-boundary rule that stops `already` matching `read` was
/// also stopping the clearest request in the whole log.
///
/// Splitting on underscores keeps both properties: `read_file` contributes
/// `read` and `file`, while `already` still contributes nothing.
fn split_identifiers(text: &str) -> String {
    text.split_whitespace()
        .flat_map(|w| w.split('_'))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Does this token end in a plausible file extension?
///
/// The line between `src/lib.rs` (a file) and `src/agent` (a directory).
pub(crate) fn has_extension(word: &str) -> bool {
    let trimmed = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let stem = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    matches!(stem.rsplit_once('.'), Some((base, ext))
        if !base.is_empty() && (1..=6).contains(&ext.len())
            && ext.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Is this token a path or filename rather than a word?
///
/// Public so the classifier's `needs_path` check uses the SAME rule the scorer
/// uses. Two path rules drifted apart once already and refused a need whose
/// path was plainly present.
pub(crate) fn is_path_like(word: &str) -> bool {
    let trimmed = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if trimmed.contains('/') || trimmed.contains('\\') {
        return true;
    }
    // A dotted name with a plausible extension: `lib.rs`, `Cargo.toml`.
    // Deliberately strict — a sentence-ending "slow." must stay a word.
    match trimmed.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=6).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Whole-word containment, so `read` does not match `already` and `dir` does
/// not match `direction`. Substring matching here produced exactly those two
/// misroutes before this existed.
///
/// Public so the classifier's tie-breaker counts verb matches with the SAME
/// rule the scorer uses — a second matching rule that drifted from this one
/// would be a misroute waiting to happen.
pub fn contains_word(haystack: &str, needle: &str) -> bool {
    // Match the word and its ordinary inflections. "which file DEFINES
    // StallDetector" was refused because the table listed `defined` and
    // `definition` but not `defines` — three spellings of one idea, and a list
    // that must enumerate them will always be one form short of what somebody
    // types. Suffixes rather than a stemmer: the vocabulary here is English
    // verbs and nouns, and a real stemmer would be a dependency and a source of
    // surprises for a gain of nothing.
    if contains_exact_word(haystack, needle) {
        return true;
    }
    for suffix in ["s", "es", "d", "ed", "ing"] {
        if contains_exact_word(haystack, &format!("{needle}{suffix}")) {
            return true;
        }
    }
    // `define` + `s` covers "defines"; a needle that already ends in `e` also
    // inflects by dropping it ("declare" -> "declaring").
    if let Some(stem) = needle.strip_suffix('e') {
        for suffix in ["ing", "ed", "es"] {
            if contains_exact_word(haystack, &format!("{stem}{suffix}")) {
                return true;
            }
        }
    }
    false
}

/// Exact whole-word containment — the rule [`contains_word`] inflects around.
fn contains_exact_word(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_word_byte(haystack.as_bytes()[start - 1]);
        let after_ok = end == haystack.len() || !is_word_byte(haystack.as_bytes()[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        if from >= haystack.len() {
            break;
        }
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The capability table.
///
/// Every entry maps to something that already exists in the workspace — the
/// tool registry (`sc-tools`), the retrieval index (`sc-index`), or a live
/// model. Nothing here is speculative; the gateway is a new *front* on the
/// existing surface, not a new backend.
pub fn table() -> Vec<Capability> {
    vec![
        Capability {
            name: "file.read",
            description: "Return the contents of one named file.",
            class: Class::Deterministic,
            // `show`/`display` are deliberately NOT here. They are generic display
            // verbs every capability shares — "show me the file", "show me the
            // hotspots", "show me the function" — so weighting them as verbs makes
            // file.read tie with whatever the caller actually named. Adding `show`
            // here to fix one refusal immediately broke "show me the top 3 slowest
            // functions"; the benchmark caught it in the same run.
            verbs: &["read", "open", "cat", "contents", "paste"],
            hints: &[
                "file",
                "line",
                "lines",
                "whole",
                "entire",
                "full",
                "complete",
                "verbatim",
                "untruncated",
                "exactly",
                "source",
            ],
            backing_tool: Some("read_file"),
            requires: &[Requirement::FilePath],
            needs_path: true,
            windowed: true,
        },
        Capability {
            name: "file.list",
            description: "List the entries of one directory.",
            class: Class::Deterministic,
            verbs: &["list", "ls", "enumerate"],
            hints: &["dir", "directory", "folder", "files", "tree"],
            backing_tool: Some("list_dir"),
            requires: &[Requirement::DirPath],
            needs_path: true,
            windowed: false,
        },
        Capability {
            name: "code.search",
            description: "Find literal text across the workspace.",
            class: Class::Deterministic,
            // `find`/`locate` belong here — they are search verbs. Removing them
            // to stop "locate StallDetector" reaching code.search papered over
            // the real distinction, which is structural: a need naming an
            // IDENTIFIER can feed code.symbol, and plain text cannot.
            // `grep` and `occurrence`/`mention` are TEXTUAL — they ask about
            // appearances in files, so they beat a symbol lookup even when the
            // need names an identifier. `find`/`locate`/`search` are neutral
            // about what is being sought, so shape decides those (see
            // `SYMBOL_SHAPE_BONUS`). Splitting them is what lets "grep the
            // codebase for StallDetector" and "locate StallDetector" both be
            // right; a single list made one of them wrong whichever way it went.
            verbs: &["grep", "occurrence", "mention", "search", "find", "locate"],
            hints: &["text", "string", "pattern", "usages", "references"],
            backing_tool: Some("search_code"),
            requires: &[],
            needs_path: false,
            windowed: false,
        },
        Capability {
            name: "code.symbol",
            description: "Locate a named function, struct or type definition.",
            class: Class::Retrieval,
            // BASE forms only. `contains_word` inflects, so "define" covers
            // defines/defined/defining and "declare" covers declares/declared.
            // Listing the forms by hand is how "which file DEFINES X" came to be
            // refused while "defined" worked — a list must enumerate every
            // spelling, and will always be one short of what somebody types.
            verbs: &["symbol", "define", "definition", "declare", "where"],
            hints: &["struct", "enum", "type", "trait", "class"],
            backing_tool: Some("find_symbol"),
            requires: &[Requirement::Symbol],
            needs_path: false,
            windowed: false,
        },
        Capability {
            name: "code.function",
            description: "Return the body of one named function in one file.",
            class: Class::Deterministic,
            // "function"/"method" are VERBS here, not hints. A caller naming a
            // function is naming the operation — the A/B's "the exact source code
            // of the handle_timeout function" matched none of body/implementation
            // and was refused, because the one word that identified the need was
            // weighted as mere colour.
            verbs: &["function", "method", "body", "implementation", "signature"],
            hints: &["fn", "exact", "code", "verbatim"],
            backing_tool: Some("read_function"),
            requires: &[Requirement::FilePath, Requirement::Symbol],
            needs_path: true,
            windowed: false,
        },
        Capability {
            name: "repo.map",
            description: "Rank the most relevant symbols in the repo for a task.",
            class: Class::Retrieval,
            // "codebase"/"project" are NOUNS — what you are asking about, not
            // what you want done. Promoting them to verbs made "grep the
            // codebase for StallDetector" tie repo.map against code.search and
            // refuse a plainly-stated grep. They stay hints; "tour" and
            // "overview" are the words that actually name this operation.
            verbs: &[
                "overview",
                "map",
                "structure",
                "layout",
                "relevant",
                "orient",
                "tour",
            ],
            hints: &[
                "repo",
                "repository",
                "architecture",
                "about",
                "purpose",
                "codebase",
                "project",
            ],
            backing_tool: None,
            requires: &[],
            needs_path: false,
            windowed: false,
        },
        Capability {
            name: "cargo.info",
            description: "Describe a crate in this workspace, or list them all.",
            class: Class::Deterministic,
            verbs: &[
                "crate",
                "crates",
                "manifest",
                "dependency",
                "dependencies",
                "depends",
            ],
            hints: &["workspace", "package", "cargo", "toml"],
            backing_tool: Some("cargo_info"),
            requires: &[],
            needs_path: false,
            windowed: false,
        },
        Capability {
            name: "perf.hotspots",
            description: "List the hottest functions from a recorded CPU profile.",
            class: Class::Deterministic,
            verbs: &["hotspot", "hotspots", "profile", "slow", "slowest", "cpu"],
            hints: &[
                "performance",
                "perf",
                "folded",
                "flamegraph",
                "cost",
                "time",
            ],
            backing_tool: Some("profile_hotspots"),
            requires: &[Requirement::ProfilePath],
            needs_path: false,
            windowed: false,
        },
        Capability {
            name: "verify.run",
            description: "Run the project test suite and report what failed.",
            class: Class::Verify,
            // NOT the bare noun "test": the A/B showed "the full contents of
            // test.rs" tying verify.run against file.read, because a FILENAME
            // containing "test" is not a request to run anything. Every word kept
            // here names the ACTION or its outcome.
            // Base forms; `contains_word` inflects. "green"/"break" are how
            // people ask whether the suite is healthy, and both were missing.
            verbs: &[
                "run", "verify", "fail", "pass", "suite", "green", "break", "broken",
            ],
            hints: &[
                "test",
                "tests",
                "red",
                "green",
                "broken",
                "regression",
                "assertion",
                "build",
            ],
            backing_tool: Some("run_verification"),
            requires: &[],
            needs_path: false,
            windowed: false,
        },
        Capability {
            name: "reason.explain",
            description: "Explain or summarize something that no lookup can answer.",
            class: Class::Live,
            verbs: &[
                "explain",
                "why",
                "summarize",
                "describe",
                "compare",
                "reason",
            ],
            hints: &["mean", "means", "purpose", "difference", "intent"],
            backing_tool: None,
            requires: &[],
            needs_path: false,
            windowed: false,
        },
        Capability {
            name: "web.search",
            description: "Answer from outside the workspace: upstream docs, errors, releases.",
            class: Class::Web,
            verbs: &[
                "web",
                "online",
                "internet",
                "upstream",
                "docs",
                "documentation",
            ],
            // Keys on being OUTSIDE the workspace. "crates" belongs to
            // cargo.info, which answers from the local manifests — the noun
            // that names a registry is not evidence of wanting the network.
            hints: &[
                "release",
                "changelog",
                "version",
                "latest",
                "news",
                "published",
            ],
            backing_tool: None,
            requires: &[],
            needs_path: false,
            windowed: false,
        },
    ]
}
