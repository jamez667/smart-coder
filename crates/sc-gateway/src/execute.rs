//! Running one routed capability.
//!
//! Each arm knows exactly one thing and nothing about routing, simplification,
//! or the other capabilities. That is the whole point of the split: a capability
//! is small enough to be obviously correct, and the hard part lives in the
//! classifier where it can be tested without running anything.
//!
//! Deterministic and retrieval capabilities run against the real workspace via
//! the existing crates. `Live` and `Web` go through injected seams so a test can
//! drive the entire gateway with no model and no network.

use std::path::Path;

use sc_model::{GenerateRequest, Message, ModelBackend};

use crate::capability::Capability;
use crate::types::{Class, Need};

/// How many ranked symbols a repo map returns.
///
/// Small on purpose: the map exists to orient a model, and a hundred ranked
/// symbols orients nobody.
const REPO_MAP_TOP_K: usize = 40;

/// The outbound-network seam. Not implemented here: the gateway must be
/// testable and buildable with no HTTP client, and *what* answers a web need is
/// a deployment choice, not a gateway one.
pub trait WebSearch {
    fn search(&self, query: &str) -> Result<String, String>;
}

/// Everything a capability might need to run.
pub struct Ctx<'a> {
    pub workspace: &'a Path,
    /// Live-model seam. `None` means live capabilities are unavailable, which
    /// is reported as such rather than silently answered by something else.
    pub model: Option<&'a dyn ModelBackend>,
    /// Web seam, same contract as `model`.
    pub web: Option<&'a dyn WebSearch>,
    /// How to run the verification suite: the sandbox and the configured
    /// command. `None` means verification is unavailable here — the gateway
    /// never invents a test command, for the same reason it never composes a
    /// path.
    pub verify: Option<&'a Verify<'a>>,
}

/// The verification seam: the sandbox to run in and the command to run.
///
/// Both come from run configuration, never from the model or the need text.
pub struct Verify<'a> {
    pub sandbox: &'a sc_verify::Sandbox,
    pub command: &'a str,
}

/// Run one capability. Returns raw, unsimplified output.
pub fn run(cap: &Capability, need: &Need, ctx: &Ctx<'_>) -> Result<String, String> {
    match cap.name {
        "file.read" => {
            let path = require_path(need)?;
            let mut args = vec![("path", Arg::Str(&path))];
            // Only pass a window when the need actually asked for one. Sending
            // start/limit unconditionally would turn "read x.rs" into a
            // truncated read the caller never asked for.
            if let Some((start, limit)) = line_window(&need.text) {
                args.push(("start", Arg::Int(start)));
                args.push(("limit", Arg::Int(limit)));
            }
            call_tool("read_file", &args, ctx.workspace)
        }
        "file.list" => {
            // Parse the directory out of the need when no scope was supplied.
            // This used to fall back to "." unconditionally, so "list the files
            // in crates/sc-gateway/src" listed the REPOSITORY ROOT and said
            // nothing about it — a confident wrong answer, which is the exact
            // failure this design exists to prevent. Found by running the
            // gateway against this repo rather than a fixture.
            //
            // "." remains the fallback for a genuinely unscoped "list the
            // files", where the working directory is the honest reading.
            let path = need
                .scope
                .clone()
                .or_else(|| directory_term(&need.text))
                .unwrap_or_else(|| ".".to_string());
            call_tool("list_dir", &[("path", Arg::Str(&path))], ctx.workspace)
        }
        "code.search" => {
            let q = search_terms(&need.text);
            call_tool("search_code", &[("query", Arg::Str(&q))], ctx.workspace)
        }
        "code.symbol" => {
            let name = symbol_term(&need.text)
                .ok_or_else(|| "no symbol name found in the need".to_string())?;
            // Returns its own actionable message when nothing is found, which is
            // more useful to the caller than a generic gateway error.
            Ok(sc_index::find_symbol(ctx.workspace, &name))
        }
        "code.function" => {
            let path = require_path(need)?;
            let name = symbol_term(&need.text)
                .ok_or_else(|| "no function name found in the need".to_string())?;
            call_tool(
                "read_function",
                &[("path", Arg::Str(&path)), ("name", Arg::Str(&name))],
                ctx.workspace,
            )
        }
        "repo.map" => {
            // The need itself is the boost signal: symbols it names are the ones
            // worth ranking up.
            let boosts = sc_index::Boosts {
                mentioned_symbols: identifier_terms(&need.text),
                in_play_files: need.scope.iter().cloned().collect(),
            };
            let map = sc_index::repo_map(ctx.workspace, &boosts, REPO_MAP_TOP_K);
            if map.trim().is_empty() {
                Err("repo map came back empty".to_string())
            } else {
                Ok(map)
            }
        }
        "cargo.info" => {
            // The crate name is optional: with none, the tool lists them all,
            // which is the right answer to "what crates are there".
            match crate_term(&need.text) {
                Some(name) => call_tool("cargo_info", &[("crate", Arg::Str(&name))], ctx.workspace),
                None => call_tool("cargo_info", &[], ctx.workspace),
            }
        }
        "perf.hotspots" => {
            // The profile path is configuration, not something to guess at: a
            // wrong path here reads as "no hotspots" rather than as an error.
            let path = need
                .scope
                .clone()
                .or_else(|| require_path(need).ok())
                .ok_or_else(|| {
                    "this need names no profile file (e.g. target/sc-profile.folded)".to_string()
                })?;
            let mut args = vec![("path", Arg::Str(&path))];
            // The tool clamps this itself (1..=100), so passing a number the
            // need actually stated is safe; omitting it takes the tool's own
            // default rather than a number the gateway invented.
            if let Some(n) = count_term(&need.text) {
                args.push(("limit", Arg::Int(n)));
            }
            call_tool("profile_hotspots", &args, ctx.workspace)
        }
        "verify.run" => {
            let verify = ctx.verify.ok_or_else(|| {
                "this need requires the test suite and no verify command is configured".to_string()
            })?;
            let report =
                sc_verify::run_verification_in(verify.sandbox, ctx.workspace, verify.command);
            // The report already summarizes failure-first — leading with failing
            // cases and their messages rather than listing the passes. Returning
            // that instead of raw output means the gateway inherits the work
            // sc-verify already does rather than re-deriving it from text.
            Ok(report.observation())
        }
        "reason.explain" => {
            let model = ctx.model.ok_or_else(|| {
                "this need requires a live model and none is configured".to_string()
            })?;
            let req = GenerateRequest::new(vec![
                Message::system(
                    "Answer the question directly and briefly. State plainly if you \
                     do not know.",
                ),
                Message::user(need.text.clone()),
            ]);
            model
                .generate(&req)
                .map(|r| r.content)
                .map_err(|e| format!("model call failed: {e}"))
        }
        "web.search" => {
            let web = ctx.web.ok_or_else(|| {
                "this need requires web access and none is configured".to_string()
            })?;
            web.search(&need.text)
        }
        other => Err(format!("capability {other} has no executor")),
    }
}

/// A tool argument. Typed because the registry validates types strictly: a
/// line number sent as a string is a validation error, not a coercion.
enum Arg<'a> {
    Str(&'a str),
    Int(i64),
}

/// Build and run a validated call against the real tool registry.
///
/// Goes through validation rather than calling the executor directly so the
/// gateway inherits the registry schema checks and path sandboxing — the
/// gateway is a new front on that surface, never a way around it.
fn call_tool(tool: &str, args: &[(&str, Arg)], workspace: &Path) -> Result<String, String> {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "tool".to_string(),
        serde_json::Value::String(tool.to_string()),
    );
    for (k, v) in args {
        obj.insert(
            k.to_string(),
            match v {
                Arg::Str(s) => serde_json::Value::String(s.to_string()),
                Arg::Int(n) => serde_json::Value::from(*n),
            },
        );
    }
    let registry = sc_tools::default_registry();
    let call = registry
        .validate(&serde_json::Value::Object(obj))
        .map_err(|e| format!("gateway built an invalid {tool} call: {e}"))?;
    match sc_tools::execute(&call, workspace) {
        sc_tools::ToolOutcome::Observation(text) => Ok(text),
        sc_tools::ToolOutcome::Finished => Err("unexpected finish from a read tool".to_string()),
    }
}

fn require_path(need: &Need) -> Result<String, String> {
    if let Some(s) = &need.scope {
        return Ok(s.clone());
    }
    need.text
        .split_whitespace()
        .find(|w| w.contains('/') || w.contains('\\') || looks_like_filename(w))
        .map(|w| {
            w.trim_matches(|c: char| !c.is_ascii_graphic() || c == ',')
                .to_string()
        })
        .ok_or_else(|| "this need names no path".to_string())
}

fn looks_like_filename(w: &str) -> bool {
    match w.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty() && !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Pull a DIRECTORY out of a need.
///
/// Distinct from [`require_path`], which looks for a file: a directory has no
/// extension, so `crates/sc-gateway/src` is path-shaped by its separators alone.
/// A bare word is never treated as a directory — "list the files" must not
/// resolve "files" into a path that does not exist.
fn directory_term(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| {
                !c.is_ascii_alphanumeric() && !matches!(c, '/' | '\\' | '.' | '_' | '-')
            })
        })
        .find(|w| w.contains('/') || w.contains('\\'))
        .map(|w| w.trim_end_matches(['/', '\\']).to_string())
        .filter(|w| !w.is_empty())
}

/// Strip the instruction words out of a need, leaving the thing being sought.
///
/// The model wrote "search for handle_timeout in the agent loop"; the tool
/// wants "handle_timeout". Doing this in code rather than asking the model for
/// a clean query is the same rule as resolving paths in the harness.
fn search_terms(text: &str) -> String {
    // **Identifier-shaped words first, blocklist second.** A blocklist of filler
    // can never be complete: "please find all occurrences of LINE_RANGE_BONUS in
    // the codebase" leaked the word "codebase" into the query and found NOTHING,
    // where the direct tool found five hits. Adding "codebase" to the list would
    // just move the boundary to the next unlisted word.
    //
    // What a caller searches for is almost always identifier-shaped —
    // SCREAMING_CASE, snake_case, CamelCase, or quoted. English prose is not. So
    // when the need contains such a word, that IS the query and the sentence
    // around it is irrelevant.
    let identifiers: Vec<&str> = text
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_'))
        .filter(|w| w.len() >= 2 && looks_like_search_target(w))
        .collect();
    if !identifiers.is_empty() {
        return identifiers.join(" ");
    }

    // No identifier in sight: a prose search for a phrase. Fall back to stripping
    // the words that are unambiguously instructions rather than content.
    const NOISE: &[&str] = &[
        "search",
        "for",
        "find",
        "grep",
        "locate",
        "the",
        "a",
        "an",
        "in",
        "of",
        "all",
        "occurrences",
        "usages",
        "references",
        "where",
        "is",
        "are",
        "please",
        "show",
        "me",
        "codebase",
        "repo",
        "repository",
        "project",
        "workspace",
        "everywhere",
        "anywhere",
    ];
    let terms: Vec<&str> = text
        .split_whitespace()
        .filter(|w| {
            let clean = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
            !clean.is_empty() && !NOISE.contains(&clean.to_lowercase().as_str())
        })
        .collect();
    if terms.is_empty() {
        text.to_string()
    } else {
        terms.join(" ")
    }
}

/// Is this word shaped like something a caller would search a codebase for?
///
/// Structural, not a word list: an identifier carries a case convention or an
/// underscore that ordinary English does not.
fn looks_like_search_target(w: &str) -> bool {
    if w.contains('_') {
        return true;
    }
    // SCREAMING_CASE without an underscore, e.g. `MAXLEN`.
    if w.len() >= 3
        && w.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return true;
    }
    // CamelCase: an interior capital after a lowercase run.
    let mut lower_seen = false;
    for c in w.chars() {
        if c.is_ascii_lowercase() {
            lower_seen = true;
        } else if c.is_ascii_uppercase() && lower_seen {
            return true;
        }
    }
    false
}

/// Pull the most identifier-shaped word out of a need.
///
/// Prefers `snake_case`/`CamelCase`/`fn()` shapes over ordinary English words,
/// because those are what a symbol lookup can actually resolve.
fn symbol_term(text: &str) -> Option<String> {
    // Position beats shape. "the body of the classify function" names its target
    // in plain lowercase, which every shape rule below rejects — so a direct
    // `read_function` returned the function while the gateway returned "no
    // function name found in the need". Found by running the two paths side by
    // side on the same needs, which no fixture test had done.
    if let Some(name) = symbol_beside_a_kind_word(text) {
        return Some(name);
    }
    let mut best: Option<(u32, String)> = None;
    for word in text.split_whitespace() {
        let clean = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
        if clean.len() < 2 {
            continue;
        }
        let mut rank = 0;
        if clean.contains('_') {
            rank += 3;
        }
        if clean.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && clean.chars().any(|c| c.is_ascii_lowercase())
        {
            rank += 2;
        }
        if word.ends_with("()") {
            rank += 4;
        }
        if rank == 0 {
            continue;
        }
        if best.as_ref().is_none_or(|(r, _)| rank > *r) {
            best = Some((rank, clean.to_string()));
        }
    }
    best.map(|(_, w)| w)
}

/// The word adjacent to a kind word — "the `classify` function", "the `Config`
/// struct", "method `run`".
///
/// A caller who says "function" beside a word has named that word as the target,
/// whatever its capitalisation. Checked before the shape heuristics because it is
/// stronger evidence: shape is a guess about what a name looks like, position is
/// what the sentence actually says.
fn symbol_beside_a_kind_word(text: &str) -> Option<String> {
    const KINDS: [&str; 6] = ["function", "fn", "method", "struct", "enum", "trait"];
    let words: Vec<&str> = text.split_whitespace().collect();
    let clean = |w: &str| {
        w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .to_string()
    };
    for (i, w) in words.iter().enumerate() {
        if !KINDS.contains(&clean(w).to_lowercase().as_str()) {
            continue;
        }
        // "the classify function" (before) and "function classify" (after).
        // Filler words are skipped so "the body of the classify function" works.
        const FILLER: [&str; 6] = ["the", "a", "an", "of", "its", "this"];
        for candidate in [
            i.checked_sub(1).map(|j| words[j]),
            words.get(i + 1).copied(),
        ]
        .into_iter()
        .flatten()
        {
            let c = clean(candidate);
            if c.len() >= 2
                && !FILLER.contains(&c.to_lowercase().as_str())
                && !KINDS.contains(&c.to_lowercase().as_str())
                && c.chars().next().is_some_and(|ch| ch.is_ascii_alphabetic())
            {
                return Some(c);
            }
        }
    }
    None
}

/// A "top N" count stated in a need, e.g. "the top 5 hotspots".
///
/// Only counts that FOLLOW a quantity word are read, so a version number or a
/// path fragment cannot be mistaken for a row count. Returns `None` when the
/// need states no number, leaving the tool's own default in force.
fn count_term(text: &str) -> Option<i64> {
    const QUANTIFIERS: [&str; 4] = ["top", "first", "hottest", "slowest"];
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    words.iter().enumerate().find_map(|(i, w)| {
        let clean = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        QUANTIFIERS
            .contains(&clean)
            .then(|| {
                words
                    .get(i + 1)?
                    .trim_matches(|c: char| !c.is_ascii_digit())
                    .parse()
                    .ok()
            })
            .flatten()
    })
}

/// Parse a line range out of a need, as `(start, limit)`.
///
/// Recognises "lines 40-60", "lines 40 to 60", "from line 40", "line 88". This
/// is the narrowing the gateway exists to provide: without it, "read lines
/// 40-60 of x.rs" quietly returns the whole file, which is the exact failure —
/// too many bytes for no reason — the crate is meant to prevent.
///
/// Returns `None` when no range is named, so an unqualified read stays a full
/// read rather than being silently truncated.
fn line_window(text: &str) -> Option<(i64, i64)> {
    let lower = text.to_lowercase();
    let idx = lower.find("line")?;
    // Numbers appearing AFTER the word "line", so a path like `foo2.rs` or a
    // symbol like `sha256` before it cannot be read as a line number.
    let nums: Vec<i64> = lower[idx..]
        .split(|c: char| !c.is_ascii_digit())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .take(2)
        .collect();
    match nums.as_slice() {
        // "lines 40-60" -> start 40, 21 lines. Inclusive, matching how a person
        // reading a diff means it.
        [a, b] if b >= a => Some((*a, b - a + 1)),
        // A single "line 88": a window around it is more useful than one bare
        // line, which almost never carries enough context to act on.
        [a] => Some(((*a - CONTEXT_LINES).max(1), CONTEXT_LINES * 2 + 1)),
        _ => None,
    }
}

/// Lines of context around a single named line.
const CONTEXT_LINES: i64 = 10;

/// Pull a crate name out of a need.
///
/// Crate names in this workspace are `sc-`-prefixed and hyphenated, which is a
/// strong enough signal to match on directly. Returns `None` for a general
/// question, which correctly lists every crate.
fn crate_term(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_'))
        .find(|w| w.len() > 3 && w.contains('-') && !w.starts_with('-') && !w.ends_with('-'))
        .map(|w| w.to_string())
}

/// Every identifier-shaped word in a need, for repo-map boosting.
///
/// Unlike [`symbol_term`] this keeps all of them: boosting is a ranking hint
/// where a wrong guess costs a little relevance, not a wrong answer.
fn identifier_terms(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|w| {
            let clean = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
            let identifier = clean.contains('_')
                || (clean.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                    && clean.chars().any(|c| c.is_ascii_lowercase()));
            (clean.len() >= 2 && identifier).then(|| clean.to_string())
        })
        .collect()
}

/// Is this class available in the given context? Checked before running so an
/// unavailable capability refuses cleanly instead of failing mid-execution.
pub fn available(class: Class, ctx: &Ctx<'_>) -> bool {
    match class {
        Class::Deterministic | Class::Retrieval => true,
        Class::Verify => ctx.verify.is_some(),
        Class::Live => ctx.model.is_some(),
        Class::Web => ctx.web.is_some(),
    }
}
