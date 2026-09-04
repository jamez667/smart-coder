//! Lexical search over the persistent index (spec 23 — smart search).
//!
//! The replacement for grep-as-retrieval. `search_code` was a flat regex grep: when
//! the question says "trail" and the code says `width_head`, it returned nothing, and
//! the model burned turns guessing synonyms. This scores the whole index against a
//! natural-language question and answers with *functions*, ranked.
//!
//! Everything here is deterministic — same repo bytes plus same question gives the
//! same bytes out, which is what makes the retrieval eval falsifiable and a failed
//! investigation diagnosable ("the evidence was wrong" versus "the model ignored
//! it"). No randomness, no hash-order iteration, and every tie broken explicitly.

use crate::lexicon::tokenize;
use crate::store::{Posting, RepoIndex};

/// Most hits a search returns. Chosen against the 200-line observation cap: 25 hits
/// plus a header is a fifth of the budget, leaving room for the model to actually
/// read something afterwards.
pub const MAX_HITS: usize = 25;

/// BM25 term-frequency saturation. The standard starting value; the point of the
/// parameter is that the tenth occurrence of a word says barely more than the third.
const K1: f64 = 1.2;

/// BM25 length normalization. The standard starting value, and it matters here: a
/// 400-line file should not outrank a 20-line function merely for containing more
/// words.
const B: f64 = 0.75;

/// How much a test file's score is damped.
///
/// Not zero: a test is sometimes exactly the right thing to read, because it states
/// the expected behaviour in prose the user's words match. But a test *describes*
/// code rather than being it, and test names are long sentences full of ordinary
/// English (`a_reply_that_was_all_reasoning_and_no_answer_still_raises_the_fault`),
/// which is precisely the shape a natural-language query matches by accident.
/// Observed on this repo: "why does a tool result get cut off" returned three test
/// entries in its top five while the truncation code itself sat below them.
///
/// Applied per *symbol*, from the index's `is_test` flag, not per path: most tests
/// here are inline `#[cfg(test)] mod tests` blocks inside the very files they test,
/// so a path rule would leave them undamped and a whole-file rule would damp the
/// production code sharing the file.
const TEST_DAMPING: f64 = 0.35;

/// One ranked result: a definition (or a location, for files without symbols) that
/// matched, and which of the query's words it matched.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Workspace-relative, `/`-separated.
    pub path: String,
    /// 1-based line: the definition's line, or the anchor line for a symbol-less file.
    pub line: usize,
    /// The enclosing definition's name, when there is one.
    pub symbol: Option<String>,
    /// Query terms this hit matched, in query order.
    pub matched: Vec<String>,
    pub score: f64,
}

/// Search the index for `query`, best first.
///
/// Scoring is BM25-shaped: saturating term frequency times inverse document
/// frequency, with each occurrence weighted by the field it came from. Scores
/// aggregate per **enclosing definition**, because "this function" is the unit a
/// model can act on with `read_function` — a list of matching lines is a list of
/// places to look, which is the work the harness is supposed to have already done.
pub fn search(index: &RepoIndex, query: &str) -> Vec<Hit> {
    let terms = query_terms(query);
    if terms.is_empty() || index.files.is_empty() {
        return Vec::new();
    }

    // Document frequency per term, counted over FILES: a term in every file (`self`,
    // `let`) carries no locating power and must not.
    let n_docs = index.files.len() as f64;
    let df: Vec<f64> = terms
        .iter()
        .map(|t| {
            index
                .files
                .values()
                .filter(|f| f.postings.iter().any(|p| &p.term == t))
                .count() as f64
        })
        .collect();

    // Mean anchor length in postings, for BM25's length normalization.
    let mut anchor_lens: std::collections::BTreeMap<(&str, usize), f64> =
        std::collections::BTreeMap::new();
    for (path, rec) in &index.files {
        for p in &rec.postings {
            *anchor_lens.entry((path.as_str(), p.line)).or_insert(0.0) += p.count as f64;
        }
    }
    let avg_len = if anchor_lens.is_empty() {
        1.0
    } else {
        anchor_lens.values().sum::<f64>() / anchor_lens.len() as f64
    };

    let mut hits: Vec<Hit> = Vec::new();
    for (path, rec) in &index.files {
        // (anchor line) -> (score, matched term indices)
        let mut per_anchor: std::collections::BTreeMap<usize, (f64, Vec<usize>)> =
            std::collections::BTreeMap::new();

        for (ti, term) in terms.iter().enumerate() {
            let idf = idf(n_docs, df[ti]);
            if idf <= 0.0 {
                continue;
            }
            for p in rec.postings.iter().filter(|p| &p.term == term) {
                let len = anchor_lens
                    .get(&(path.as_str(), p.line))
                    .copied()
                    .unwrap_or(1.0);
                let entry = per_anchor.entry(p.line).or_insert((0.0, Vec::new()));
                entry.0 += score_posting(p, idf, len, avg_len);
                if !entry.1.contains(&ti) {
                    entry.1.push(ti);
                }
            }
        }

        for (line, (mut score, matched)) in per_anchor {
            let enclosing = index.enclosing_symbol(path, line);
            let is_test = enclosing.map(|s| s.is_test).unwrap_or(false) || is_test_path(path);
            let damping = if is_test { TEST_DAMPING } else { 1.0 };
            // Covering more of the question is worth more than repeating one word of
            // it: a function matching "trail", "thin" AND "thick" is the answer,
            // while one matching "thin" forty times is a rendering loop.
            let coverage = matched.len() as f64 / terms.len() as f64;
            score *= (1.0 + coverage) * damping;
            hits.push(Hit {
                path: path.clone(),
                line,
                symbol: enclosing.map(|s| s.name.clone()),
                matched: matched.into_iter().map(|i| terms[i].clone()).collect(),
                score,
            });
        }
    }

    // Score descending, then path ascending, then line ascending. Total and explicit:
    // no two hits may ever compare equal, or the order becomes an accident of
    // whatever order the map happened to yield.
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    hits.truncate(MAX_HITS);
    hits
}

/// Inverse document frequency, floored at zero.
///
/// The `+ 0.5` smoothing is BM25's, and the floor matters: a term in *every* file
/// yields a negative raw idf, which would make a file score better for lacking the
/// word. Floored, such a term simply contributes nothing.
fn idf(n_docs: f64, df: f64) -> f64 {
    if df <= 0.0 {
        return 0.0;
    }
    (((n_docs - df + 0.5) / (df + 0.5)) + 1.0).ln().max(0.0)
}

/// One posting's contribution: field weight times BM25's saturating term frequency.
fn score_posting(p: &Posting, idf: f64, len: f64, avg_len: f64) -> f64 {
    let tf = p.count as f64 * p.field.weight();
    let norm = tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * len / avg_len.max(1.0)));
    idf * norm
}

/// Whether a path is a test file — the fallback for text with no enclosing symbol
/// (a bare comment in an integration-test file still belongs to a test).
fn is_test_path(rel: &str) -> bool {
    crate::store::path_is_test(rel)
}

/// The query's terms, de-duplicated, in first-appearance order — so `matched` reads
/// back in the order the user said them.
fn query_terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tokenize(query) {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Render hits the way the model sees them: one line each, densest evidence first,
/// **no source lines**.
///
/// Quoting code here would tempt the model to answer from fragments; it has
/// `read_file` and `read_function` for the actual reading, and the whole point of
/// this tool is to tell it *where* to point them.
pub fn render(query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!(
            "search_code {query:?}: no matches. Try fewer or different words, or \
             list_dir/read_file to look directly."
        );
    }
    let mut out = format!("search_code {query:?}: {} hit(s):\n", hits.len());
    for h in hits {
        let what = match &h.symbol {
            Some(name) => format!("fn {name}"),
            None => "(file)".to_string(),
        };
        out.push_str(&format!(
            "{}:{}  {}  matched: {}\n",
            h.path,
            h.line,
            what,
            h.matched.join(", ")
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn temp_repo(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sc-index-search-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// A miniature of the real thing: the trail-drawing function, its neighbours, and
    /// the comment that actually contains the user's words.
    fn starfield_repo() -> PathBuf {
        let root = temp_repo("starfield");
        write(
            &root,
            "src/fx/starfield.rs",
            "\
//! Procedural parallax starfield.

pub struct Starfield {
    stars: Vec<Star>,
}

/// Draw the static twinkling stars.
pub fn draw_stars(&self, alpha: f32) {
    for s in &self.stars {
        batch.dot(s.pos, s.size, alpha);
    }
}

/// Motion trails: each star's trail length scales with its layer.
pub fn draw_trails(&self, intensity: f32) {
    let base_len = 30.0 + 320.0 * intensity;
    for s in &self.stars {
        // Flip: thin (catching up) at head, thick (trailing) at tail
        let width_head = 0.8 + s.size * 0.5;
        let width_tail = width_head * 1.8;
        batch.line(head, mid, width_head);
        batch.line(mid, tail, width_tail);
    }
}
",
        );
        write(
            &root,
            "src/input.rs",
            "\
/// Handle the keyboard.
pub fn poll_input(state: &mut State) {
    state.thrust = key_down(Key::W);
}
",
        );
        write(
            &root,
            "src/ui/hud.rs",
            "\
/// Draw the heads-up display.
pub fn draw_hud(&self, score: u32) {
    self.text(format!(\"score {score}\"));
}
",
        );
        root
    }

    /// **The canonical query.** This is the question from the spec, asked the way a
    /// user asks it, and it must land on the function that draws the trails. If this
    /// regresses, the whole retrieval story regresses.
    #[test]
    fn the_starfield_question_finds_the_trail_drawing_function() {
        let root = starfield_repo();
        let idx = RepoIndex::build(&root);
        let hits = search(
            &idx,
            "why is the trail behind the stars thin before it gets thick",
        );

        assert!(!hits.is_empty(), "no hits at all");
        let top = &hits[0];
        assert_eq!(top.path, "src/fx/starfield.rs", "{hits:#?}");
        assert_eq!(top.symbol.as_deref(), Some("draw_trails"), "{hits:#?}");
        // And it says WHY it matched, so a human can debug a bad ranking.
        for w in ["trail", "thin", "thick"] {
            assert!(top.matched.contains(&w.to_string()), "{:?}", top.matched);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The words were only ever in a comment. This is the finding the spec is built
    /// on: the answer to a vague question is often verbatim in a comment.
    #[test]
    fn a_query_whose_words_live_only_in_comments_still_ranks_first() {
        let root = temp_repo("comments");
        write(
            &root,
            "a.rs",
            "\
// This is where the flicker happens when the buffer is stale.
pub fn commit_frame(&mut self) {
    self.swap();
}
",
        );
        write(&root, "b.rs", "pub fn unrelated() { let x = 1; }\n");
        let idx = RepoIndex::build(&root);
        let hits = search(&idx, "why does the screen flicker with a stale buffer");
        assert_eq!(hits[0].symbol.as_deref(), Some("commit_frame"), "{hits:#?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Field weights are the design, so they are pinned: at equal frequency, a
    /// comment hit outranks a code hit.
    #[test]
    fn a_comment_hit_outranks_a_body_hit_at_equal_frequency() {
        let root = temp_repo("weights");
        write(&root, "commented.rs", "// widget\npub fn alpha() {}\n");
        write(&root, "coded.rs", "pub fn beta() { let widget = 1; }\n");
        let idx = RepoIndex::build(&root);
        let hits = search(&idx, "widget");
        assert_eq!(hits[0].path, "commented.rs", "{hits:#?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn covering_more_of_the_question_beats_repeating_one_word() {
        let root = temp_repo("coverage");
        // One function says "trail" many times; the other says trail AND thick once.
        write(
            &root,
            "loop.rs",
            "pub fn spam() {\n// trail trail trail trail trail trail\n}\n",
        );
        write(&root, "both.rs", "pub fn real() {\n// trail thick\n}\n");
        let idx = RepoIndex::build(&root);
        let hits = search(&idx, "trail thick");
        assert_eq!(hits[0].path, "both.rs", "{hits:#?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **Determinism.** Same index, same query, same bytes — twice.
    #[test]
    fn the_same_query_twice_returns_the_same_bytes() {
        let root = starfield_repo();
        let idx = RepoIndex::build(&root);
        let q = "why is the trail behind the stars thin before it gets thick";
        assert_eq!(render(q, &search(&idx, q)), render(q, &search(&idx, q)));

        // And a rebuilt index answers identically to the first one.
        let idx2 = RepoIndex::build(&root);
        assert_eq!(render(q, &search(&idx, q)), render(q, &search(&idx2, q)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn results_are_capped_and_rendered_without_source_lines() {
        let root = temp_repo("cap");
        for i in 0..60 {
            write(&root, &format!("f{i}.rs"), "// widget\npub fn a() {}\n");
        }
        let idx = RepoIndex::build(&root);
        let hits = search(&idx, "widget");
        assert!(hits.len() <= MAX_HITS, "{} hits", hits.len());

        let out = render("widget", &hits);
        assert!(out.lines().count() <= MAX_HITS + 1);
        // No source line ever appears -- the model has read_file for that.
        assert!(!out.contains("pub fn a()"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_or_stopword_only_query_is_not_an_error() {
        let root = starfield_repo();
        let idx = RepoIndex::build(&root);
        assert!(search(&idx, "").is_empty());
        assert!(search(&idx, "why is the").is_empty());
        // And the rendering says what to do instead of dead-ending.
        let out = render("why is the", &[]);
        assert!(out.contains("no matches"), "{out}");
        assert!(
            out.contains("read_file") || out.contains("list_dir"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Test names are long sentences of ordinary English, which is exactly what a
    /// natural-language query matches by accident. Damped, not excluded: a test is
    /// sometimes the clearest statement of intended behaviour.
    #[test]
    fn a_test_file_is_outranked_by_the_code_it_tests() {
        let root = temp_repo("testdamp");
        // Same words on both sides, so the ONLY thing separating them is the
        // damping -- otherwise the fixture, not the rule, decides the order.
        write(
            &root,
            "src/truncate.rs",
            "// a result is cut off at the cap
pub fn truncate_result() {}
",
        );
        write(
            &root,
            "tests/events.rs",
            "// a result is cut off at the cap
fn truncate_result_works() {}
",
        );
        // And the inline form, which is how most of this project's tests are written.
        write(
            &root,
            "src/inline.rs",
            "#[cfg(test)]
mod tests {
    // a result is cut off at the cap
    #[test]
    fn truncate_result_inline() {}
}
",
        );
        // A little unrelated bulk, so "cut"/"off"/"result" are not in every file
        // (a term in every document scores zero by design).
        for i in 0..6 {
            write(
                &root,
                &format!("src/other{i}.rs"),
                "pub fn unrelated() {}
",
            );
        }
        let idx = RepoIndex::build(&root);
        let hits = search(&idx, "where does a result get cut off");
        assert_eq!(hits[0].path, "src/truncate.rs", "{hits:#?}");
        // Both forms of test rank below it, and both are still present.
        for p in ["tests/events.rs", "src/inline.rs"] {
            let at = hits.iter().position(|h| h.path == p);
            assert!(
                at.is_some_and(|i| i > 0),
                "{p} should rank below: {hits:#?}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_term_in_every_file_carries_no_weight() {
        let root = temp_repo("ubiquitous");
        for i in 0..10 {
            write(
                &root,
                &format!("f{i}.rs"),
                "pub fn handler() { let state = 1; }\n",
            );
        }
        write(
            &root,
            "special.rs",
            "pub fn handler() { let beacon = 1; }\n",
        );
        let idx = RepoIndex::build(&root);
        // "state" is everywhere; "beacon" is in one file. The rare word decides.
        let hits = search(&idx, "state beacon");
        assert_eq!(hits[0].path, "special.rs", "{hits:#?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
