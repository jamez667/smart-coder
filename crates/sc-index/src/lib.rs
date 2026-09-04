//! `sc-index` — the retrieval index (spec 05 / spec 01).
//!
//! A lightweight index over the working repo so the Context Manager pulls in only
//! what's relevant rather than dumping whole files. Two capabilities:
//!
//! * a **PageRank repo map** (aider-style): a tree-sitter symbol
//!   definition/reference graph scored by PageRank, with boosts for symbols named
//!   in the current task and files already in play — relevance precomputed from
//!   the code's actual structure instead of asking a small model to navigate.
//! * **lexical search** + symbol lookup, surfaced to the agent as the
//!   `find_symbol` tool.

mod pagerank;
mod repomap;
mod symbols;
mod walk;
mod workspace;

pub use pagerank::{pagerank, Edge};
pub use repomap::{build_repo_map, render_repo_map, Boosts, RankedSymbol, SourceFile};
pub use symbols::{
    count_functions_named, extract_symbols, function_span, FileSymbols, Language, SymbolDef,
};
pub use walk::{
    is_skipped_dir, relative, walk, WalkOptions, WalkedFile, PROMPT_MAX_FILE_BYTES, SKIP_DIRS,
};
pub use workspace::{collect_sources, find_symbol, find_symbol_hits, repo_map, SymbolHit};
