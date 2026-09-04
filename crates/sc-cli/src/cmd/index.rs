//! `index`, `search`, `health`, `stack` — the repo-intelligence surface (spec 23).
//!
//! For humans, for scripts, and above all for debugging. `smart-coder search "<the
//! user's question>"` prints **exactly** the bytes the model would have been shown,
//! which turns "why did that investigation go sideways" into a reproducible one-liner
//! rather than an argument about what the model was probably thinking.
//!
//! None of these are model tools. The registry is the measured six and stays that way
//! (spec 23 — the model-facing surface); a report a person reads has no business
//! costing the model a menu slot.

use std::process::ExitCode;

use super::common::workspace;

/// `index` — build or refresh the index and report what is in it.
pub fn index(json: bool) -> ExitCode {
    let Some(ws) = workspace() else {
        return ExitCode::FAILURE;
    };
    let started = std::time::Instant::now();
    let idx = sc_index::RepoIndex::open(&ws);
    let elapsed = started.elapsed();

    let files = idx.files.len();
    let symbols: usize = idx.files.values().map(|f| f.symbols.len()).sum();
    let postings: usize = idx.files.values().map(|f| f.postings.len()).sum();
    let lines: usize = idx.files.values().map(|f| f.lines).sum();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "files": files,
                "lines": lines,
                "symbols": symbols,
                "postings": postings,
                "parsed": idx.parsed_count(),
                "ms": elapsed.as_millis() as u64,
                "cache": sc_index::INDEX_REL_PATH,
            })
        );
    } else {
        println!(
            "{files} files · {lines} lines · {symbols} symbols · {postings} postings\n\
             {} re-parsed in {}ms → {}",
            idx.parsed_count(),
            elapsed.as_millis(),
            sc_index::INDEX_REL_PATH,
        );
    }
    ExitCode::SUCCESS
}

/// `search <query>` — exactly what `search_code` would hand the model.
pub fn search(query: &str, json: bool) -> ExitCode {
    let Some(ws) = workspace() else {
        return ExitCode::FAILURE;
    };
    if query.trim().is_empty() {
        eprintln!("search: give me something to look for");
        return ExitCode::FAILURE;
    }
    let idx = sc_index::RepoIndex::open(&ws);
    let hits = sc_index::search(&idx, query);
    if json {
        let rows: Vec<_> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "path": h.path,
                    "line": h.line,
                    "symbol": h.symbol,
                    "matched": h.matched,
                    "score": h.score,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "query": query, "hits": rows }));
    } else {
        // The rendered form, byte for byte, so this is a faithful rehearsal of the
        // observation the model receives -- not a prettier summary of it.
        println!("{}", sc_index::render(query, &hits));
    }
    ExitCode::SUCCESS
}

/// `health` — line counts and size smells.
pub fn health(json: bool) -> ExitCode {
    let Some(ws) = workspace() else {
        return ExitCode::FAILURE;
    };
    let idx = sc_index::RepoIndex::open(&ws);
    let h = sc_index::health(&idx);
    if json {
        let notable: Vec<_> = h
            .notable
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.path,
                    "lines": f.lines,
                    "size": f.size.label(),
                    "functions": f.functions,
                    "todos": f.todos,
                    "giants": f.giants.iter()
                        .map(|(n, l)| serde_json::json!({"name": n, "lines": l}))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "files": h.files,
                "lines": h.lines,
                "functions": h.functions,
                "todos": h.todos,
                "notable": notable,
            })
        );
    } else {
        println!("{}", sc_index::render_health(&h));
    }
    ExitCode::SUCCESS
}

/// `stack` — resolve a stack trace read from stdin.
///
/// Named `stack` rather than `trace` because `trace` is spec traceability, and two
/// meanings for one verb in one CLI is a papercut nobody should have to learn.
pub fn stack(json: bool) -> ExitCode {
    let Some(ws) = workspace() else {
        return ExitCode::FAILURE;
    };
    let mut text = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).is_err() {
        eprintln!("stack: could not read stdin");
        return ExitCode::FAILURE;
    }
    let idx = sc_index::RepoIndex::open(&ws);
    let frames = sc_index::resolve_trace(&text, &idx);
    if json {
        let rows: Vec<_> = frames
            .iter()
            .map(|f| {
                serde_json::json!({
                    "raw_path": f.raw_path,
                    "path": f.path,
                    "line": f.line,
                    "symbol": f.symbol,
                    "reported": f.reported,
                    "workspace": f.in_workspace(),
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "frames": rows }));
    } else if frames.is_empty() {
        println!("no stack trace found in the input.");
    } else {
        println!("{}", sc_index::render_trace(&frames));
    }
    ExitCode::SUCCESS
}
