//! Grounding the task before the first phase runs.
//!
//! A workflow phase is a direct model call that otherwise sees only the task string,
//! so "implement PLAN-lakes.md" produced a design that ignored both the plan and the
//! real files (observed live 2026-07-14: it invented `src/lakes/*.rs` and never
//! touched the plan's `gen/terrain.rs`). These helpers inject the plan's body, the
//! real contents of the files it names, and a survey of what already exists.

use std::path::Path;

/// Augment the task with the grounding every phase needs: the full body of a `PLAN-*.md` the
/// task references (so the design follows the actual plan, not a guess from its filename), and
/// a survey of the source files that already exist (so it edits real files like `gen/terrain.rs`
/// instead of inventing a fresh module tree). Both are read from `workspace`; if there's no plan
/// reference or no files, that section is simply omitted.
pub(super) fn ground_task(task: &str, workspace: &Path) -> String {
    let mut out = String::from(task);

    if let Some((name, body)) = referenced_plan(task, workspace) {
        out.push_str(&format!(
            "\n\n=== The feature plan you are designing for ({name}) — follow it ===\n{body}\n\
             Design for exactly this plan: its Approach, its Files-to-touch, and its Steps. Do \
             NOT invent a different structure or ignore the files it names."
        ));

        // Inject the FULL CONTENTS of the existing files the plan says it will touch. Without
        // this the design phases saw only filenames and invented an architecture that doesn't
        // exist — an `ElevationField` struct, a `lake_generator` module, an `Elevation` enum —
        // so every downstream build stage stalled hunting fictional symbols. Showing the real
        // code makes the architecture/layout/breakdown reference types that actually exist.
        let touched = plan_touched_files(&body, workspace);
        for f in touched.iter().take(6) {
            if let Ok(src) = std::fs::read_to_string(workspace.join(f)) {
                let clipped = clip_chars(&src, 12_000);
                out.push_str(&format!(
                    "\n\n=== EXISTING contents of {f} (design against the REAL types/functions \
                     here; the plan may name things that do not exist — use what is actually in \
                     this code) ===\n{clipped}\n=== end {f} ==="
                ));
            }
        }
    }

    let files = sc_core::source_files(workspace);
    if !files.is_empty() {
        out.push_str(
            "\n\n=== Source files that ALREADY EXIST in this project (edit these; do not \
             reinvent the layout) ===\n",
        );
        for f in files.iter().take(200) {
            out.push_str("  ");
            out.push_str(f);
            out.push('\n');
        }
        out.push_str(
            "Ground the architecture and layout in these real files and the plan's \
             Files-to-touch — new files only where the plan calls for them.",
        );
    }

    out
}

/// If `task` names a feature spec that exists in `workspace` — `specs/<slug>.md` or a legacy
/// `PLAN-<slug>.md` — return `(name, contents)`. Mirrors the agent loop's `referenced_plan` so
/// the workflow pins the same spec.
pub(super) fn referenced_plan(task: &str, workspace: &Path) -> Option<(String, String)> {
    let token = task
        .split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | ','))
        .map(|t| t.trim_end_matches('.'))
        .find(|t| {
            let norm = t.replace('\\', "/");
            let low = norm.to_ascii_lowercase();
            (low.starts_with("specs/") && low.ends_with(".md")) || {
                let up = norm.to_ascii_uppercase();
                up.starts_with("PLAN-") && up.ends_with(".MD")
            }
        })?;
    let body = std::fs::read_to_string(workspace.join(token)).ok()?;
    if body.trim().is_empty() {
        return None;
    }
    Some((token.to_string(), body))
}

/// Extract the existing source files a plan says it will touch, so their real contents can
/// ground the design. Scans the plan body for path-shaped tokens (ending in a code extension)
/// and resolves each to a REAL file in the project — matching by path suffix, because a plan
/// often names a crate-relative path (`gen/terrain.rs`) while the file lives deeper
/// (`crates/city/src/gen/terrain.rs`). Returns workspace-relative paths, deduped, order-preserving.
pub(super) fn plan_touched_files(plan_body: &str, workspace: &Path) -> Vec<String> {
    const EXTS: [&str; 8] = [".rs", ".py", ".js", ".ts", ".go", ".java", ".css", ".html"];
    let all = sc_core::source_files(workspace); // real files, workspace-relative
    let mut out: Vec<String> = Vec::new();
    for tok in plan_body
        .split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | ',' | ':'))
    {
        let t = tok.trim().trim_end_matches(&['.', ';'][..]);
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if !EXTS.iter().any(|e| lower.ends_with(e)) {
            continue;
        }
        // Resolve: exact match, else a real file whose path ends with this token (with a '/'
        // boundary so "terrain.rs" doesn't match "myterrain.rs").
        let norm = t.replace('\\', "/");
        let resolved = if workspace.join(&norm).is_file() {
            Some(norm.clone())
        } else {
            all.iter()
                .find(|f| {
                    let fp = f.replace('\\', "/");
                    fp == norm || fp.ends_with(&format!("/{norm}"))
                })
                .cloned()
        };
        if let Some(r) = resolved {
            if !out.contains(&r) {
                out.push(r);
            }
        }
    }
    out
}

/// Clip `s` to at most `max` chars on a char boundary, noting the truncation. Keeps the HEAD
/// (imports, type/struct decls, signatures — what a design needs), which is what sits at the
/// top of a source file.
pub(super) fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    format!(
        "{}\n… (file truncated — first {max} chars shown)",
        &s[..end]
    )
}
