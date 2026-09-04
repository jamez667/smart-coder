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

mod health;
mod lexicon;
mod pagerank;
mod repomap;
mod search;
mod store;
mod symbols;
mod trace;
mod walk;
mod workspace;

pub use health::{
    health, render_health, FileHealth, Health, Size, FILE_SPLIT_LINES, FILE_WARN_LINES,
    GIANT_FN_LINES,
};
pub use lexicon::{tokenize, Field, STOPWORDS};
pub use pagerank::{pagerank, Edge};
pub use repomap::{build_repo_map, render_repo_map, Boosts, RankedSymbol, SourceFile};
pub use search::{render, search, Hit, MAX_HITS};
pub use store::{
    FileRecord, IndexedSymbol, Posting, RepoIndex, INDEX_FORMAT_VERSION, INDEX_REL_PATH,
};
pub use symbols::{
    count_functions_named, definition_spans, extract_all, extract_symbols, function_span,
    FileSymbols, Language, SymbolDef,
};
pub use trace::{render_trace, resolve_trace, Frame};
pub use walk::{
    is_skipped_dir, is_skipped_file, relative, walk, WalkOptions, WalkedFile,
    PROMPT_MAX_FILE_BYTES, SKIP_DIRS, SKIP_FILES,
};
pub use workspace::{collect_sources, find_symbol, find_symbol_hits, repo_map, SymbolHit};
