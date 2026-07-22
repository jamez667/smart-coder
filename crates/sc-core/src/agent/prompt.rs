//! Prompt-building helpers for the agent loop: reading workspace files and rendering
//! them into the retrieved-zone blocks the model sees each turn.
//!
//! Every function here is a pure `workspace → String` (or `→ Vec<…>`) read: the repo
//! signature map, the progress ledger of files that already exist, the pinned bodies of
//! the focused file(s) and the files they import, and the source blob the diagnostic
//! pass reads. None depend on the rest of the loop, so they live apart from it.

use std::path::Path;

use sc_index::Boosts;

/// Read every on-disk source file (excluding tests/caches) as a [`crate::diagnose::SourceFile`]
/// for the diagnostic pass. A pathologically large file is skipped so one blob can't blow the
/// diagnostic prompt (these apps are far under the cap).
pub(super) fn gather_sources(workspace: &Path) -> Vec<crate::diagnose::SourceFile> {
    const MAX_BYTES: usize = 64 * 1024;
    sc_tools::source_files(workspace)
        .into_iter()
        .filter_map(|rel| {
            let contents = std::fs::read_to_string(workspace.join(&rel)).ok()?;
            (contents.len() <= MAX_BYTES).then(|| crate::diagnose::SourceFile {
                path: rel,
                contents,
            })
        })
        .collect()
}

/// The workspace files the focused file(s) IMPORT FROM — resolved from their Python import
/// statements (`import store`, `from store import add`) to `<module>.py` paths that exist in
/// the workspace. These are the files the model actually needs the CODE of (signatures alone
/// aren't enough — it re-reads them to see args/behavior), so the caller pins their full
/// bodies. Excludes the focused files themselves. Best-effort + Python-only (the stack);
/// returns workspace-relative paths.
pub(super) fn imported_files(workspace: &Path, focus: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in focus {
        let Ok(src) = std::fs::read_to_string(workspace.join(f)) else {
            continue;
        };
        for line in src.lines() {
            let t = line.trim();
            // `from <mod> import ...` or `import <mod>[, <mod2>]`.
            let mods: Vec<&str> = if let Some(rest) = t.strip_prefix("from ") {
                rest.split_whitespace().next().into_iter().collect()
            } else if let Some(rest) = t.strip_prefix("import ") {
                rest.split(',')
                    .map(|m| m.split_whitespace().next().unwrap_or(""))
                    .collect()
            } else {
                continue;
            };
            for m in mods {
                // Only the top-level module (`a.b` → `a`); map to `<a>.py`.
                let base = m.split('.').next().unwrap_or("").trim();
                if base.is_empty() {
                    continue;
                }
                let candidate = format!("{base}.py");
                if focus.iter().any(|x| x == &candidate) || out.contains(&candidate) {
                    continue;
                }
                if workspace.join(&candidate).is_file() {
                    out.push(candidate);
                }
            }
        }
    }
    out
}

/// Render a SIGNATURE MAP (each indexed symbol as `path:line  name`) of files OTHER than the
/// given `exclude` set — the "distant" context (files the focused file does not import). Empty
/// if there's nothing left to show.
pub(super) fn render_other_files_map(workspace: &Path, exclude: &[String], top_k: usize) -> String {
    let map = sc_index::repo_map(
        workspace,
        &Boosts {
            mentioned_symbols: Vec::new(),
            in_play_files: Vec::new(),
        },
        top_k,
    );
    if map.is_empty() {
        return String::new();
    }
    // The map header is the first line; each entry line is `  path:line  symbol`. Keep the
    // header + only the entries whose path is NOT a focused file.
    let mut out: Vec<&str> = Vec::new();
    for (i, line) in map.lines().enumerate() {
        if i == 0 {
            out.push(line); // header
            continue;
        }
        let path = line.trim().split(':').next().unwrap_or("");
        if !exclude.iter().any(|f| f == path) {
            out.push(line);
        }
    }
    // Header-only (everything was excluded) → nothing useful to show.
    if out.len() <= 1 {
        String::new()
    } else {
        out.join("\n")
    }
}

/// Render the filesystem progress ledger: the source files that ACTUALLY exist in
/// `workspace` right now (read fresh each turn), framed so a small model stops re-creating
/// files it already wrote and notices required files it has not made yet. Excludes the
/// frozen tests and tooling caches (via `sc_tools::source_files`).
pub(super) fn render_progress_ledger(workspace: &Path) -> String {
    let files = sc_tools::source_files(workspace);
    if files.is_empty() {
        return "Files you have created so far: (none yet — the workspace is empty). Create \
                the source files the task requires."
            .to_string();
    }
    let mut s = String::from(
        "Files that ALREADY EXIST in the workspace (do NOT re-create these — `create_file` \
         will fail on a path listed here; use `edit_file` or `write_file` to change one):\n",
    );
    for f in &files {
        s.push_str("  ");
        s.push_str(f);
        s.push('\n');
    }
    s.push_str(
        "Compare this list to the files the task requires above: create any required file \
         that is NOT listed here next.",
    );
    s
}

/// Render the current contents of the focused files for the retrieved zone, with
/// line numbers so a small model can copy an exact, unique `old_str`. Re-read from
/// the workspace each turn, so it always reflects edits already made.
pub(super) fn render_focus_files(workspace: &Path, files: &[String]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "The file to edit, with 1-based LINE NUMBERS (updates after each edit). To change a large \
         file, PREFER `edit_lines` — give the start/end line numbers shown here and the new text; \
         you do NOT need to copy the old text exactly (which is error-prone on a big file). The \
         line-number prefix `N| ` is NOT part of the file — never include it in new_text.\n",
    );
    let mut any = false;
    for f in files {
        let p = workspace.join(f);
        if let Ok(content) = std::fs::read_to_string(&p) {
            any = true;
            // Normalize to LF (a Windows CRLF file would otherwise poison anchor matching), then
            // prefix each line with its 1-based number so the model can target line RANGES with
            // edit_lines instead of reproducing an exact snippet — the anchor-hallucination that
            // traps a mid-size model on a large file.
            let content = content.replace("\r\n", "\n").replace('\r', "\n");
            let numbered = content
                .lines()
                .enumerate()
                .map(|(i, l)| format!("{}| {}", i + 1, l))
                .collect::<Vec<_>>()
                .join("\n");
            s.push_str(&format!(
                "\n=== {f} (line-numbered) ===\n{numbered}\n=== end {f} ===\n"
            ));
        }
    }
    if any {
        s
    } else {
        String::new()
    }
}

/// Render the full bodies of READ-ONLY CONTEXT files (the ones the focused file imports
/// from), clearly distinguished from the file-to-edit so the model doesn't confuse which to
/// change. Re-read fresh each turn so the view never goes stale. Empty if none readable.
pub(super) fn render_context_files(workspace: &Path, files: &[String]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "Files your file IMPORTS FROM, shown in full for reference — do NOT edit or read these, \
         just import from them (they update after each edit):\n",
    );
    let mut any = false;
    for f in files {
        if let Ok(content) = std::fs::read_to_string(workspace.join(f)) {
            any = true;
            s.push_str(&format!(
                "\n--- {f} (read-only) ---\n{content}\n--- end {f} ---\n"
            ));
        }
    }
    if any {
        s
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::test_util::temp_dir;

    #[test]
    fn progress_ledger_lists_existing_files_and_flags_empty() {
        let ws = temp_dir("ledger");
        // Empty workspace → "none yet", a prompt to start creating.
        let empty = render_progress_ledger(&ws);
        assert!(empty.contains("none yet"), "empty ledger: {empty}");

        // After writing real sources + a frozen test, the ledger lists the sources only.
        std::fs::create_dir_all(ws.join("templates")).unwrap();
        std::fs::write(ws.join("app.py"), "x").unwrap();
        std::fs::write(ws.join("templates/board.html"), "x").unwrap();
        std::fs::write(ws.join("test_app.py"), "x").unwrap(); // frozen → excluded
        let led = render_progress_ledger(&ws);
        assert!(led.contains("ALREADY EXIST"), "{led}");
        assert!(led.contains("app.py"), "{led}");
        assert!(led.contains("templates/board.html"), "{led}");
        assert!(
            !led.contains("test_app.py"),
            "the frozen test must not appear in the ledger: {led}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn imported_files_resolves_python_imports_to_existing_workspace_files() {
        let ws = temp_dir("imports");
        std::fs::write(
            ws.join("app.py"),
            "from store import add\nimport service, missingmod\nfrom pkg.sub import x\n",
        )
        .unwrap();
        std::fs::write(ws.join("store.py"), "").unwrap();
        std::fs::write(ws.join("service.py"), "").unwrap();
        std::fs::write(ws.join("pkg.py"), "").unwrap(); // `from pkg.sub` → top-level pkg.py
                                                        // missingmod has no file → not resolved.
        let mut got = imported_files(&ws, &["app.py".to_string()]);
        got.sort();
        assert_eq!(got, vec!["pkg.py", "service.py", "store.py"]);
        // The focused file itself is never in its own imports.
        assert!(!got.contains(&"app.py".to_string()));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
