//! Feature slicing for incremental integration.
//!
//! Closing a whole multi-file graph at once is what a small model fails at. Instead we
//! derive a pytest `-k` keyword per route file, then make each CUMULATIVE slice green
//! before adding the next — so the model only ever closes a small new slice on a base
//! that already passes. An app with no `routes_<feature>.py` files yields no slices and
//! the caller falls back to a single full integration pass.

use sc_swarm::TaskBoard;

/// A feature slice: a pytest `-k` keyword derived from a route file's name, plus that file.
/// The incremental integration walks these in dependency order, making each cumulative slice
/// (`-k "author or book"`) green before adding the next — so the model only ever closes a SMALL
/// new slice on a green base, never the whole multi-file graph at once.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct FeatureSlice {
    pub(super) keyword: String,
    #[allow(dead_code)] // kept for reporting/debugging; the keyword is what drives -k
    pub(super) file: String,
}

/// Map a source file to its pytest `-k` keyword, by FILENAME convention (not prose). A
/// `routes_<feature>.py` blueprint → `<feature>` (singularized): `routes_authors.py` → "author".
/// Infrastructure (`store`/`service`/`app`) and glue (templates/static) → `None`: they aren't a
/// testable feature on their own — they're folded into the first feature's base or caught by the
/// final full-suite pass. Returns `None` for anything not matching `routes_*.py`, which is what
/// makes single-`routes.py` apps (S1/S2) fall back to today's single integration pass.
pub(super) fn feature_keyword(file: &str) -> Option<String> {
    let name = std::path::Path::new(file)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(file);
    let stem = name.strip_suffix(".py")?;
    let feature = stem.strip_prefix("routes_")?;
    if feature.is_empty() {
        return None;
    }
    // Singularize a trailing plural so the keyword matches test names (`test_create_author`,
    // not `authors`). Only the simple `s` case — the test oracles use singular feature nouns.
    let singular = feature.strip_suffix('s').unwrap_or(feature);
    if singular.is_empty() {
        return None;
    }
    Some(singular.to_string())
}

/// Extract `def test_<name>` names from the frozen contract string (already loaded — no I/O).
pub(super) fn parse_test_names(contract: &str) -> Vec<String> {
    contract
        .lines()
        .filter_map(|line| {
            let t = line.trim_start();
            let rest = t.strip_prefix("def ")?;
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name.starts_with("test_") {
                Some(name)
            } else {
                None
            }
        })
        .collect()
}

/// Subtask ids in dependency order — a topological walk over `deps`, INDEPENDENT of current
/// status (the build loop has already marked everything Complete by the time slices are derived,
/// so we can't reuse `ready()`, which keys off Pending). Emits a subtask once all its deps have
/// been emitted; falls back to the lowest remaining id to break a cycle/dangling dep. Order
/// matches the build walk's because both follow deps.
pub(super) fn ordered_subtask_ids(board: &TaskBoard) -> Vec<String> {
    let ids: Vec<String> = board.subtasks().iter().map(|s| s.id.clone()).collect();
    let mut emitted: Vec<String> = Vec::new();
    while emitted.len() < ids.len() {
        let next = board
            .subtasks()
            .iter()
            .filter(|s| !emitted.contains(&s.id))
            .find(|s| s.deps.iter().all(|d| emitted.contains(d)))
            .map(|s| s.id.clone())
            .or_else(|| {
                // cycle / dep on a missing id: take the lowest remaining id, don't strand it.
                ids.iter()
                    .filter(|id| !emitted.contains(*id))
                    .min()
                    .cloned()
            });
        match next {
            Some(id) => emitted.push(id),
            None => break,
        }
    }
    emitted
}

/// Derive the ordered feature slices for incremental integration: each `routes_<feature>.py` in
/// dependency order whose keyword actually appears in a frozen test name. A keyword with no
/// matching test is skipped (nothing to verify); duplicates are de-duped preserving dep order.
/// Empty ⇒ the app has no `routes_<feature>.py` files (or no tests for them) ⇒ caller falls back
/// to today's single full integration pass.
pub(super) fn derive_slices(board: &TaskBoard, test_names: &[String]) -> Vec<FeatureSlice> {
    let has_test = |kw: &str| {
        let kw = kw.to_lowercase();
        test_names.iter().any(|t| t.to_lowercase().contains(&kw))
    };
    let mut slices: Vec<FeatureSlice> = Vec::new();
    for id in ordered_subtask_ids(board) {
        let Some(st) = board.subtasks().iter().find(|s| s.id == id) else {
            continue;
        };
        for file in &st.files {
            if let Some(keyword) = feature_keyword(file) {
                if has_test(&keyword) && !slices.iter().any(|s| s.keyword == keyword) {
                    slices.push(FeatureSlice {
                        keyword,
                        file: file.clone(),
                    });
                }
            }
        }
    }
    slices
}

/// The cumulative `-k` expression for slices `0..=upto`: `"author"`, `"author or book"`, …
pub(super) fn cumulative_k(slices: &[FeatureSlice], upto: usize) -> String {
    slices[..=upto]
        .iter()
        .map(|s| s.keyword.as_str())
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Append a pytest `-k "<expr>"` filter to the base verify command, so a slice runs only the
/// tests for the features built so far: `python -m pytest -q 'test_app.py'` → `… -k "author"`.
pub(super) fn slice_command(base: &str, k: &str) -> String {
    format!("{base} -k \"{k}\"")
}
