//! The retrieval eval (spec 23 — Measurement).
//!
//! Every other eval in this crate needs a model, a GPU and a tolerance for
//! nondeterminism. This one needs none of them: search is a pure function of the
//! repo bytes and the question, so "did retrieval get worse?" is a question a unit
//! test can answer, on any machine, in milliseconds.
//!
//! That is the entire argument for a deterministic lexical core. A ranking change
//! that would have been an unfalsifiable vibe ("investigations feel worse lately")
//! is instead a red build naming the query that regressed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Where in the ranking a target has to appear.
///
/// Two bars rather than one because they mean different things. **Strict** (top 5)
/// is "the model would see this in a lead block" — the bar that matters for the
/// investigate path, where only a handful of lines fit. **Loose** (top 25) is "the
/// index can at least find it", which is the bar for a question whose answer is
/// genuinely spread across a repo.
pub const STRICT_K: usize = 5;
/// The loose bar: the target is somewhere in the returned hits.
pub const LOOSE_K: usize = sc_index::MAX_HITS;

/// One question and what it should find.
#[derive(Debug, Clone, Deserialize)]
pub struct RetrievalQuery {
    pub id: String,
    /// Asked the way a user asks it — vague, symptom-first, in the user's words.
    pub question: String,
    /// Fixture directory, relative to the suite file.
    pub fixture: String,
    /// Workspace-relative path the answer lives in. Optional: some questions are
    /// graded only on finding the right *symbol*, wherever it lives.
    #[serde(default)]
    pub path: Option<String>,
    /// The definition that should be found. Optional: for a question whose answer
    /// is a file rather than one function.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Whether this query must hit the strict (top-5) bar. When false, the loose
    /// bar applies.
    #[serde(default)]
    pub strict: bool,
    /// `"miss"` marks a query the lexical index is *expected* to fail.
    ///
    /// A suite of only-winnable questions measures nothing, and quietly becomes a
    /// suite somebody tuned the ranker against. Declaring the known misses keeps the
    /// gap visible and turns the suite red the day one of them starts working —
    /// which is news worth having, not a silent improvement.
    #[serde(default)]
    pub expect: Option<String>,
}

impl RetrievalQuery {
    /// Whether this query asserts the target is NOT found.
    fn expects_miss(&self) -> bool {
        self.expect.as_deref() == Some("miss")
    }
}

/// The parsed suite.
#[derive(Debug, Clone, Deserialize)]
pub struct RetrievalSuite {
    pub queries: Vec<RetrievalQuery>,
    #[serde(skip)]
    dir: PathBuf,
}

impl RetrievalSuite {
    /// Load a suite from a TOML file. The fixture paths inside it are resolved
    /// relative to the file, so a suite can be run from any working directory.
    pub fn load(path: &Path) -> Result<RetrievalSuite, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut suite: RetrievalSuite =
            toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        suite.dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok(suite)
    }

    /// Run every query, in suite order.
    ///
    /// Indexes are built once per fixture and reused. Several questions point at
    /// this repository, and rebuilding its index for each of them cost 105 seconds
    /// of a 110-second suite — a gate slow enough that people stop running it.
    pub fn run(&self) -> Vec<QueryResult> {
        let mut built: BTreeMap<String, sc_index::RepoIndex> = BTreeMap::new();
        self.queries
            .iter()
            .map(|q| {
                let fixture = self.dir.join(&q.fixture);
                if !fixture.is_dir() {
                    return QueryResult::missing(
                        q,
                        format!("no such fixture: {}", fixture.display()),
                    );
                }
                let index = built.entry(q.fixture.clone()).or_insert_with(|| {
                    // `build`, not `open`: the eval must never depend on a cache,
                    // and must never leave one behind in a fixture directory.
                    let mut idx = sc_index::RepoIndex::build(&fixture);
                    // **The suite must not answer its own questions.** When the
                    // fixture is this repository, `evals/retrieval/suite.toml` is
                    // inside it -- and it contains every question verbatim, so it
                    // won the top hit for all seven smart-coder queries on the
                    // first run. A perfectly self-fulfilling, perfectly useless
                    // result.
                    idx.files.retain(|p, _| !p.starts_with("evals/retrieval/"));
                    idx
                });
                grade(q, index)
            })
            .collect()
    }
}

/// Score one query against an already-built index.
fn grade(q: &RetrievalQuery, index: &sc_index::RepoIndex) -> QueryResult {
    let hits = sc_index::search(index, &q.question);
    let rank = hits.iter().position(|h| matches(q, h)).map(|i| i + 1);
    let bar = if q.strict { STRICT_K } else { LOOSE_K };
    let found = rank.is_some_and(|r| r <= bar);
    QueryResult {
        id: q.id.clone(),
        strict: q.strict,
        rank,
        bar,
        // A known miss passes by NOT being found. If it starts being found, this
        // goes red -- deliberately, because the gap closing is news.
        passed: if q.expects_miss() { !found } else { found },
        expected_miss: q.expects_miss(),
        top: hits.first().map(describe),
        note: None,
    }
}

/// Whether a hit is the answer this query was looking for.
///
/// A `symbol` must match exactly. A `path` alone means "anywhere in this file",
/// which is the honest bar for a question whose answer is a file's whole job.
fn matches(q: &RetrievalQuery, hit: &sc_index::Hit) -> bool {
    if let Some(want) = &q.symbol {
        if hit.symbol.as_deref() != Some(want.as_str()) {
            return false;
        }
    }
    if let Some(want) = &q.path {
        // Suffix match, so a suite entry can name `pathfind.rs` without caring
        // whether the fixture nests it.
        if !(hit.path == *want || hit.path.ends_with(&format!("/{want}"))) {
            return false;
        }
    }
    q.symbol.is_some() || q.path.is_some()
}

fn describe(h: &sc_index::Hit) -> String {
    match &h.symbol {
        Some(s) => format!("{}:{} {s}", h.path, h.line),
        None => format!("{}:{}", h.path, h.line),
    }
}

/// What one query did.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub id: String,
    pub strict: bool,
    /// 1-based rank of the expected target, or `None` if it never appeared.
    pub rank: Option<usize>,
    /// The bar this query had to clear.
    pub bar: usize,
    pub passed: bool,
    /// Whether this query asserted the target would NOT be found.
    pub expected_miss: bool,
    /// The top hit, for a failure message that says what was found instead.
    pub top: Option<String>,
    /// Set when the query could not run at all.
    pub note: Option<String>,
}

impl QueryResult {
    fn missing(q: &RetrievalQuery, note: String) -> QueryResult {
        QueryResult {
            id: q.id.clone(),
            strict: q.strict,
            rank: None,
            bar: if q.strict { STRICT_K } else { LOOSE_K },
            passed: false,
            expected_miss: q.expects_miss(),
            top: None,
            note: Some(note),
        }
    }

    /// A one-line report: the rank reached, the bar, and what came first instead.
    pub fn line(&self) -> String {
        let mark = if self.passed { "PASS" } else { "FAIL" };
        let where_ = match (&self.note, self.rank, self.expected_miss) {
            (Some(n), _, _) => n.clone(),
            // A known miss that is still a miss: say so, so nobody reads the PASS
            // as the index having answered it.
            (None, None, true) => "not found (known miss, as declared)".to_string(),
            (None, Some(r), true) => format!("rank {r} — a known miss now RESOLVES"),
            (None, Some(r), false) => format!("rank {r} (bar {})", self.bar),
            (None, None, false) => format!(
                "not in top {} — top hit was {}",
                self.bar,
                self.top.as_deref().unwrap_or("(nothing)")
            ),
        };
        format!("{mark}  {:<32} {where_}", self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The suite as shipped must be well-formed and must every one of it pass.
    ///
    /// This is the gate: it runs in `scripts/check.sh`, with no model and no GPU, and
    /// a ranking change that makes any seeded question worse turns the build red with
    /// the query named.
    #[test]
    fn the_shipped_retrieval_suite_passes() {
        let suite_path = repo_root().join("evals/retrieval/suite.toml");
        let suite = RetrievalSuite::load(&suite_path).expect("suite loads");
        assert!(
            suite.queries.len() >= 8,
            "the suite should cover a real spread of questions, got {}",
            suite.queries.len()
        );

        let results = suite.run();
        let failed: Vec<&QueryResult> = results.iter().filter(|r| !r.passed).collect();
        if !failed.is_empty() {
            let report: Vec<String> = results.iter().map(|r| r.line()).collect();
            panic!(
                "{} of {} retrieval queries failed:\n{}",
                failed.len(),
                results.len(),
                report.join("\n")
            );
        }
    }

    /// **Determinism, end to end.** The whole suite must produce identical results
    /// twice; anything else and the eval cannot be used as a gate.
    #[test]
    fn the_suite_is_deterministic() {
        let suite = RetrievalSuite::load(&repo_root().join("evals/retrieval/suite.toml")).unwrap();
        assert_eq!(suite.run(), suite.run());
    }

    /// The repo root, found by walking up from this crate. `CARGO_MANIFEST_DIR` is
    /// `crates/sc-eval`, and the suite lives at the workspace root.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf()
    }
}
