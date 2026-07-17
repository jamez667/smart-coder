//! The PageRank repo map (spec 05, aider) — the headline retrieval feature.
//!
//! We index a set of files into a symbol-dependency graph: a node per definition
//! (`file::symbol`), and an edge from a file to every definition it references.
//! PageRank over that graph scores how *central* each symbol is — the symbols
//! everything else leans on. Boosts (personalization) lift symbols named in the
//! current task and files already in play, so the top of the map is what matters
//! *right now*. The output is a compact, token-budgeted listing of the
//! highest-ranked definitions with their `path:line`, which a small model can use
//! to jump straight to the right place instead of scanning the repo.

use std::collections::HashMap;

use crate::pagerank::{pagerank, Edge};
use crate::symbols::{extract_symbols, Language};

/// One file to index: its repo-relative path and its source text.
pub struct SourceFile {
    pub path: String,
    pub source: String,
}

/// A ranked definition in the repo map.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedSymbol {
    pub name: String,
    pub path: String,
    pub line: usize,
    pub score: f64,
}

/// Boost knobs for personalization (aider's identifier/file boosts).
#[derive(Debug, Clone, Default)]
pub struct Boosts {
    /// Symbol names mentioned in the current task/conversation (~10× in aider).
    pub mentioned_symbols: Vec<String>,
    /// Files already in play (chat/working set) (~50× in aider).
    pub in_play_files: Vec<String>,
}

/// Build a PageRank-ranked repo map over `files`, returning definitions ordered by
/// descending centrality (with boosts applied). At most `top_k` are returned.
pub fn build_repo_map(files: &[SourceFile], boosts: &Boosts, top_k: usize) -> Vec<RankedSymbol> {
    // 1. Extract symbols per file.
    struct Def {
        name: String,
        path: String,
        line: usize,
    }
    let mut defs: Vec<Def> = Vec::new();
    // name -> indices of def-nodes with that name (a name may be defined > once).
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    // per-file references and a synthetic "file node" index.
    let mut file_refs: Vec<(String, Vec<String>)> = Vec::new();

    for f in files {
        let lang = match Language::from_path(&f.path) {
            Some(l) => l,
            None => continue,
        };
        let syms = extract_symbols(lang, &f.source);
        for d in syms.defs {
            let idx = defs.len();
            by_name.entry(d.name.clone()).or_default().push(idx);
            defs.push(Def {
                name: d.name,
                path: f.path.clone(),
                line: d.line,
            });
        }
        file_refs.push((f.path.clone(), syms.refs));
    }

    if defs.is_empty() {
        return Vec::new();
    }

    // 2. Node layout: def nodes [0..defs.len()), then one node per file after.
    let n_defs = defs.len();
    let file_node: HashMap<&str, usize> = file_refs
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (p.as_str(), n_defs + i))
        .collect();
    let n = n_defs + file_refs.len();

    // 3. Edges: file node -> each def it references (by name).
    let mut edges: Vec<Edge> = Vec::new();
    for (path, refs) in &file_refs {
        let from = file_node[path.as_str()];
        for r in refs {
            if let Some(targets) = by_name.get(r) {
                for &t in targets {
                    // Don't credit a file for referencing its own definition only;
                    // self-edges within a file are fine (they still signal use).
                    edges.push(Edge {
                        from,
                        to: t,
                        weight: 1.0,
                    });
                }
            }
        }
    }

    // 4. Personalization: boost mentioned symbols and in-play files.
    let mentioned: std::collections::HashSet<&str> = boosts
        .mentioned_symbols
        .iter()
        .map(|s| s.as_str())
        .collect();
    let in_play: std::collections::HashSet<&str> =
        boosts.in_play_files.iter().map(|s| s.as_str()).collect();
    let mut personalization = vec![1.0_f64; n];
    for (i, d) in defs.iter().enumerate() {
        if mentioned.contains(d.name.as_str()) {
            personalization[i] += 10.0;
        }
        if in_play.contains(d.path.as_str()) {
            personalization[i] += 50.0;
        }
    }

    // 5. Rank.
    let ranks = pagerank(n, &edges, 0.85, &personalization, 50);

    let mut ranked: Vec<RankedSymbol> = defs
        .iter()
        .enumerate()
        .map(|(i, d)| RankedSymbol {
            name: d.name.clone(),
            path: d.path.clone(),
            line: d.line,
            score: ranks[i],
        })
        .collect();
    // Highest score first; tie-break by path/line for determinism.
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    ranked.truncate(top_k);
    ranked
}

/// Render a ranked repo map as compact text, each line `path:line  symbol`. The
/// caller token-budgets by choosing `top_k`; this is the human/model-facing form.
pub fn render_repo_map(symbols: &[RankedSymbol]) -> String {
    if symbols.is_empty() {
        return "(no indexed symbols)".to_string();
    }
    let mut out = String::from("repo map (most-referenced symbols):\n");
    for s in symbols {
        out.push_str(&format!("  {}:{}  {}\n", s.path, s.line, s.name));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust(path: &str, src: &str) -> SourceFile {
        SourceFile {
            path: path.to_string(),
            source: src.to_string(),
        }
    }

    #[test]
    fn central_symbol_ranks_above_a_leaf() {
        // `core` is called from two other files; `unused` is never referenced.
        let files = vec![
            rust("core.rs", "pub fn core() -> u32 { 1 }\nfn unused() {}"),
            rust("a.rs", "fn a() { core(); }"),
            rust("b.rs", "fn b() { core(); }"),
        ];
        let map = build_repo_map(&files, &Boosts::default(), 10);
        let core = map.iter().find(|s| s.name == "core").unwrap();
        let unused = map.iter().find(|s| s.name == "unused").unwrap();
        assert!(core.score > unused.score, "map={map:?}");
        // The most central symbol is first.
        assert_eq!(map[0].name, "core");
        assert_eq!(map[0].path, "core.rs");
    }

    #[test]
    fn mentioned_symbol_is_boosted() {
        let files = vec![
            rust("core.rs", "fn core() {}\nfn helper() {}"),
            rust("a.rs", "fn a() { core(); }"),
        ];
        // Without a boost, core (referenced) outranks helper (not referenced).
        let plain = build_repo_map(&files, &Boosts::default(), 10);
        let helper_plain = plain.iter().find(|s| s.name == "helper").unwrap().score;

        // Boost `helper` by name -> its score should rise.
        let boosts = Boosts {
            mentioned_symbols: vec!["helper".to_string()],
            in_play_files: vec![],
        };
        let boosted = build_repo_map(&files, &boosts, 10);
        let helper_boosted = boosted.iter().find(|s| s.name == "helper").unwrap().score;
        assert!(helper_boosted > helper_plain, "boost raised helper");
    }

    #[test]
    fn in_play_file_is_boosted() {
        let files = vec![
            rust("core.rs", "fn core() {}"),
            rust("scratch.rs", "fn scratch() {}"),
        ];
        let boosts = Boosts {
            mentioned_symbols: vec![],
            in_play_files: vec!["scratch.rs".to_string()],
        };
        let map = build_repo_map(&files, &boosts, 10);
        // scratch.rs is in play, so `scratch` should top the map.
        assert_eq!(map[0].name, "scratch");
    }

    #[test]
    fn top_k_limits_output() {
        let files = vec![rust(
            "a.rs",
            "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}",
        )];
        let map = build_repo_map(&files, &Boosts::default(), 2);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn render_is_compact_and_path_lined() {
        let files = vec![rust("a.rs", "fn alpha() {}")];
        let map = build_repo_map(&files, &Boosts::default(), 10);
        let text = render_repo_map(&map);
        assert!(text.contains("a.rs:1  alpha"), "{text}");
    }

    #[test]
    fn non_source_files_are_skipped() {
        let files = vec![
            SourceFile {
                path: "README.md".into(),
                source: "# not code".into(),
            },
            rust("a.rs", "fn a() {}"),
        ];
        let map = build_repo_map(&files, &Boosts::default(), 10);
        assert!(map.iter().all(|s| s.path == "a.rs"));
    }

    #[test]
    fn empty_input_renders_placeholder() {
        assert_eq!(render_repo_map(&[]), "(no indexed symbols)");
    }
}
