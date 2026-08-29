//! The mutating tools: `write_file`, `create_file`, `append_file`, `edit_file`,
//! `edit_lines`, `edit_function`.
//!
//! Every writer runs the [`guards`](super::guards) tripwires before touching disk,
//! and every failure is a *self-correcting* observation: it tells the model what to
//! send instead, because a bare "error" just makes a small model retry the same
//! thing. The anchored/line-addressed/by-name edits exist as a ladder — when one
//! addressing mode keeps failing, the error steers to the next.

use std::path::Path;

use super::guards::{delimiter_regression, duplicate_definition, is_code_path};
use super::read::locate_function;
use super::util::safe_join;

/// A file with more than this many lines is too large to safely OVERWRITE with `write_file`:
/// a small/mid model can't faithfully reproduce that much code and drops functions or leaves an
/// unterminated string, breaking the build (observed live: the 30B looping write_file on a
/// 790-line terrain.rs, each rewrite introducing a fresh syntax error). Such a file must be
/// changed with surgical `edit_file` / `append_file` instead.
const WRITE_FILE_OVERWRITE_MAX_LINES: usize = 150;

pub fn write_file(workspace: &Path, path: &str, content: &str) -> String {
    match safe_join(workspace, path) {
        Ok(p) => {
            // Guard: refuse to OVERWRITE a large existing file — steer to surgical edits. New
            // files and small files are fine; this only blocks the destructive rewrite of a big
            // one, which is where the model corrupts the codebase.
            if let Ok(existing) = std::fs::read_to_string(&p) {
                let existing_lines = existing.lines().count();
                if existing_lines > WRITE_FILE_OVERWRITE_MAX_LINES {
                    return format!(
                        "write_file {path} rejected: {path} already exists and is {existing_lines} \
                         lines — too large to safely overwrite (a full rewrite drops code and \
                         breaks the build). Use edit_file to change a specific snippet, or \
                         append_file to add new code at the end. Make a small, surgical change."
                    );
                }
            }
            // Duplicate-definition guard: reject content that defines the same top-level item
            // twice (comparing against an empty "before" surfaces any internal duplicate).
            if is_code_path(path) {
                if let Some(msg) = duplicate_definition("", content) {
                    return format!("write_file {path} rejected: {msg}");
                }
            }
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&p, content) {
                Ok(()) => format!("write_file {path} ok ({} bytes)", content.len()),
                Err(e) => format!("write_file {path} error: {e}"),
            }
        }
        Err(e) => format!("write_file {path} rejected: {e}"),
    }
}

pub fn create_file(workspace: &Path, path: &str, content: &str) -> String {
    match safe_join(workspace, path) {
        Ok(p) => {
            if p.exists() {
                return format!(
                    "create_file {path} error: already exists (use edit_file or write_file)"
                );
            }
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&p, content) {
                Ok(()) => format!("create_file {path} ok ({} bytes)", content.len()),
                Err(e) => format!("create_file {path} error: {e}"),
            }
        }
        Err(e) => format!("create_file {path} rejected: {e}"),
    }
}

/// Append `content` to the end of a file, creating it (and any parent dirs) if it
/// doesn't exist. This is the escape hatch for building a file too large for a small
/// model to emit in one `write_file` reply: the model writes the head, then appends
/// the tail in bounded chunks so no single reply's JSON gets truncated mid-string.
pub fn append_file(workspace: &Path, path: &str, content: &str) -> String {
    use std::io::Write;
    match safe_join(workspace, path) {
        Ok(p) => {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Duplicate-definition guard: appending a block that re-defines an existing top-level
            // item is the coder's biggest corruption (observed live: 227 lines re-appending modal
            // primitives that already existed → E0428). Reject the append and steer to editing the
            // existing definition. Only for code files that already exist; a brand-new file can't
            // duplicate anything.
            let existing = std::fs::read_to_string(&p).unwrap_or_default();
            if is_code_path(path) && !existing.is_empty() {
                let after = format!("{existing}{content}");
                if let Some(msg) = duplicate_definition(&existing, &after) {
                    return format!("append_file {path} rejected: {msg}");
                }
            }
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
            {
                Ok(mut f) => match f.write_all(content.as_bytes()) {
                    Ok(()) => {
                        let total = std::fs::metadata(&p).map(|m| m.len()).unwrap_or_default();
                        format!(
                            "append_file {path} ok (+{} bytes, {total} total)",
                            content.len()
                        )
                    }
                    Err(e) => format!("append_file {path} error: {e}"),
                },
                Err(e) => format!("append_file {path} error: {e}"),
            }
        }
        Err(e) => format!("append_file {path} rejected: {e}"),
    }
}

/// Replace a whole function/method by name with `new_body`. Resolves the function's span via
/// tree-sitter, then splices — no exact snippet or line numbers for the model to get wrong.
pub fn edit_function(workspace: &Path, path: &str, name: &str, new_body: &str) -> String {
    let (src, start, end, count) = match locate_function(workspace, path, name) {
        Ok(v) => v,
        Err(e) => return format!("edit_function {e}"),
    };
    let p = match safe_join(workspace, path) {
        Ok(p) => p,
        Err(e) => return format!("edit_function {path} rejected: {e}"),
    };
    let new_body = new_body.replace("\r\n", "\n").replace('\r', "\n");
    let had_trailing_nl = src.ends_with('\n');
    let lines: Vec<&str> = src.lines().collect();

    let mut out: Vec<String> = Vec::new();
    out.extend(lines[..start - 1].iter().map(|l| l.to_string()));
    out.extend(new_body.split('\n').map(|l| l.to_string()));
    out.extend(lines[end..].iter().map(|l| l.to_string()));
    let mut joined = out.join("\n");
    if had_trailing_nl {
        joined.push('\n');
    }

    // Reuse the brace-balance tripwire: replacing a whole function should keep the file balanced;
    // if the new_body drops/adds a delimiter, reject with the same guidance rather than writing
    // a file that won't compile.
    if is_code_path(path) {
        if let Some(msg) = delimiter_regression(&src, &joined) {
            return format!(
                "edit_function {path}:{name} rejected: {msg} Your new_body isn't brace-balanced \
                 against the rest of the file — recount the braces in the function you sent."
            );
        }
    }

    match std::fs::write(&p, &joined) {
        Ok(()) => {
            let dup = if count > 1 {
                format!(" (note: {count} functions named `{name}`; edited the FIRST)")
            } else {
                String::new()
            };
            format!(
                "edit_function {path}:{name} ok (replaced lines {start}..={end}; file now {} lines){dup}",
                joined.lines().count()
            )
        }
        Err(e) => format!("edit_function {path}:{name} error: {e}"),
    }
}

/// Replace lines `start..=end` (1-based, inclusive) with `new_text`. The line-addressed edit:
/// no snippet to reproduce, so a model editing a large file it holds imperfectly can't fail on
/// a hallucinated anchor — it just names the line numbers shown in the file view. An empty range
/// (`end == start - 1`) inserts before `start`. Line endings are normalized to LF (matches
/// edit_file). Self-correcting errors on an out-of-range or inverted span.
pub fn edit_lines(
    workspace: &Path,
    path: &str,
    start: Option<i64>,
    end: Option<i64>,
    new_text: &str,
) -> String {
    let p = match safe_join(workspace, path) {
        Ok(p) => p,
        Err(e) => return format!("edit_lines {path} rejected: {e}"),
    };
    let raw = match std::fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => return format!("edit_lines {path} error: {e}"),
    };
    let (Some(start), Some(end)) = (start, end) else {
        return format!("edit_lines {path} error: start and end must be integers (1-based lines)");
    };
    let content = raw.replace("\r\n", "\n").replace('\r', "\n");
    let new_text = new_text.replace("\r\n", "\n").replace('\r', "\n");
    let had_trailing_nl = content.ends_with('\n');
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len() as i64;

    // Validate. `end == start - 1` is the INSERT form (empty range). Otherwise 1 <= start <= end
    // <= total.
    let insert = end == start - 1;
    if start < 1 || start > total + 1 {
        return format!(
            "edit_lines {path} error: start {start} out of range (file has {total} lines). \
             Use a start between 1 and {}.",
            total + 1
        );
    }
    if !insert && (end < start || end > total) {
        return format!(
            "edit_lines {path} error: end {end} invalid for start {start} (file has {total} \
             lines). For a replace, use start <= end <= {total}; to INSERT before line {start}, \
             pass end = {}.",
            start - 1
        );
    }

    let s = (start - 1) as usize; // 0-based first line to drop
    let e = if insert { s } else { end as usize }; // 0-based end (exclusive after this)
    let mut out: Vec<String> = Vec::new();
    out.extend(lines[..s].iter().map(|l| l.to_string()));
    if !new_text.is_empty() {
        out.extend(new_text.split('\n').map(|l| l.to_string()));
    }
    out.extend(lines[e..].iter().map(|l| l.to_string()));
    let mut joined = out.join("\n");
    if had_trailing_nl {
        joined.push('\n');
    }
    // Brace-balance tripwire. The recurring edit_lines failure is dropping (or duplicating) a
    // closing `}`/`)`/`]` when the replaced range straddled one — the model edits blind to nesting,
    // then thrashes for turns un-breaking a delimiter it can't see. If this edit takes a
    // BALANCED file to an UNBALANCED one, reject it and name the offending delimiter, so the model
    // fixes its new_text now instead of after a compiler round-trip it keeps guessing wrong on.
    if is_code_path(path) {
        if let Some(msg) = duplicate_definition(&content, &joined) {
            return format!("edit_lines {path} rejected: {msg}");
        }
    }
    if is_code_path(path) && !insert {
        if let Some(msg) = delimiter_regression(&content, &joined) {
            // Replacing a range that straddles a brace forces the model to reproduce the exact
            // brace count — which it cannot reliably do (observed: it oscillates 3→2→1 and stalls).
            // Steer to the INSERT form instead: pick a line boundary that sits BETWEEN two
            // existing statements (e.g. just before the closing `}` of the match, or right after
            // an existing arm) and pass `end = start - 1` with new_text = the new, self-contained
            // balanced block. An insert never removes an existing delimiter, so it can't unbalance
            // the file — sidestepping the brace-counting problem entirely.
            let insert_line = start.saturating_sub(1).max(1);
            return format!(
                "edit_lines {path} rejected: {msg} Replacing a range that straddles a brace makes \
                 you reproduce the exact brace count, which keeps going wrong. Instead INSERT the \
                 new block without deleting anything: pass the SAME balanced new_text but with \
                 start = the line you want it BEFORE and end = start - 1 (e.g. start = {insert_line}, \
                 end = {}). Insert a self-contained, brace-balanced block between two existing \
                 lines — don't replace a range.",
                insert_line - 1
            );
        }
    }
    match std::fs::write(&p, &joined) {
        Ok(()) => {
            let action = if insert {
                format!("inserted before line {start}")
            } else {
                format!("replaced lines {start}..={end}")
            };
            format!(
                "edit_lines {path} ok ({action}; file now {} lines)",
                joined.lines().count()
            )
        }
        Err(e) => format!("edit_lines {path} error: {e}"),
    }
}

/// Anchored edit: replace the single exact occurrence of `old_str` with `new_str`.
pub fn edit_file(workspace: &Path, path: &str, old_str: &str, new_str: &str) -> String {
    let p = match safe_join(workspace, path) {
        Ok(p) => p,
        Err(e) => return format!("edit_file {path} rejected: {e}"),
    };
    let raw = match std::fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => return format!("edit_file {path} error: {e}"),
    };
    if old_str.is_empty() {
        return format!("edit_file {path} error: old_str must not be empty");
    }
    // Normalize line endings to LF for matching/editing, on BOTH sides. A file checked out on
    // Windows is CRLF; the model, shown that file verbatim, faithfully copies CRLF into old_str
    // — but if we normalize only the file and not old_str, the `\r` in the anchor breaks the
    // match and EVERY edit fails (observed live 2026-07-15: the 30B's first, correct anchor on a
    // CRLF terrain.rs missed, and it spiralled into corrupting the file trying to "fix" it). Strip
    // `\r` from the file AND from old_str/new_str so a CRLF-copied anchor matches. We edit in LF
    // space and write LF — correct for source files.
    let content = raw.replace("\r\n", "\n").replace('\r', "\n");
    let old_str = old_str.replace("\r\n", "\n").replace('\r', "\n");
    let new_str = new_str.replace("\r\n", "\n").replace('\r', "\n");
    // Small models also emit a literal backslash-n (`\\n`) instead of a real
    // newline inside a multi-line old_str. Resolve the anchor to whichever form the
    // (normalized) file actually contains, un-escaping new_str to match.
    let (old_owned, new_owned) = if content.contains(&old_str) {
        (old_str.clone(), new_str.clone())
    } else {
        let unescaped = unescape_literal(&old_str);
        if unescaped != old_str && content.contains(&unescaped) {
            (unescaped, unescape_literal(&new_str))
        } else {
            (old_str.clone(), new_str.clone())
        }
    };
    // Duplicate-definition guard: if the exact anchor is present, we can compute the resulting file
    // directly and reject a replacement that would define an existing top-level item a second time
    // (the coder pasting a duplicate helper). Only when the anchor matches exactly once — the fuzzy
    // / whole-line fallbacks in `edit_file_with` are already the "couldn't match" recovery path.
    if is_code_path(path) && content.matches(&old_owned).count() == 1 {
        let after = content.replacen(&old_owned, &new_owned, 1);
        if let Some(msg) = duplicate_definition(&content, &after) {
            return format!("edit_file {path} rejected: {msg}");
        }
    }
    edit_file_with(&p, path, &content, &old_owned, &new_owned)
}

/// Turn literal escape sequences a model may have emitted as text (`\n`, `\t`,
/// `\r`, `\"`, `\\`) into the real characters — used as a fallback when a
/// small model writes `\\n` instead of a real newline inside `old_str`.
fn unescape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    out.push('\n');
                    chars.next();
                }
                Some('t') => {
                    out.push('\t');
                    chars.next();
                }
                Some('r') => {
                    out.push('\r');
                    chars.next();
                }
                Some('"') => {
                    out.push('"');
                    chars.next();
                }
                Some('\\') => {
                    out.push('\\');
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Render `content` with 1-based line numbers, so an edit error can point a small
/// model at exact anchors to copy.
fn number_lines(content: &str) -> String {
    content
        .lines()
        .enumerate()
        .map(|(i, l)| format!("  {}: {}", i + 1, l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Apply an `old_str`→`new_str` replacement to already-read `content` at `p`,
/// enforcing the exactly-once rule (with whole-line disambiguation and
/// self-correcting errors for small models).
///
/// The "exactly once" rule is the small-model safety net (spec 04): an ambiguous
/// anchor (0 or >1 matches) is rejected with a precise count instead of guessing.
fn edit_file_with(p: &Path, path: &str, content: &str, old_str: &str, new_str: &str) -> String {
    let count = content.matches(old_str).count();
    if count == 0 {
        // Exact match failed. Before giving up, try a WHITESPACE-TOLERANT multi-line match: a
        // model editing a large file often reproduces the block's TEXT correctly but gets the
        // indentation or inner spacing slightly wrong, so a byte-exact `old_str` never matches
        // and it thrashes (observed live: the 30B looping read→edit→write_file on terrain.rs).
        // If the anchor's non-blank lines match a unique run of the file's lines (comparing each
        // line's whitespace-collapsed text), replace that real run — the edit lands despite the
        // spacing drift.
        if let Some(fuzzed) = fuzzy_line_block_replace(content, old_str, new_str) {
            return match std::fs::write(p, &fuzzed) {
                Ok(()) => format!("edit_file {path} ok (1 replacement, whitespace-tolerant match)"),
                Err(e) => format!("edit_file {path} error: {e}"),
            };
        }
        // The anchor isn't in the file. The usual cause for a small model is that
        // the edit already landed (or it's working from a stale view), so it keeps
        // re-proposing a change that's no longer applicable. Show the CURRENT file
        // with line numbers so it re-anchors on what's actually there now.
        let numbered = number_lines(content);
        return format!(
            "edit_file {path} error: old_str {old_str:?} not found (0 matches). The file may \
             already have that change. Here is the CURRENT content — pick your next anchor \
             from these exact lines:\n{numbered}"
        );
    }
    if count > 1 {
        // Whole-line disambiguation (spec 04 — do the work the small model can't).
        // A bare anchor like "return n" substring-matches both `    return n` and
        // `    return n % 2 == 0`. But as a *whole trimmed line* it matches exactly
        // one (`    return n`), which is unambiguously what the model meant. When
        // `old_str.trim()` equals exactly one line's trimmed text, edit that line
        // in place, preserving its indentation.
        let lines: Vec<&str> = content.lines().collect();
        let needle = old_str.trim();
        let line_hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim() == needle)
            .map(|(i, _)| i)
            .collect();
        if line_hits.len() == 1 {
            let i = line_hits[0];
            let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
            let trailing_newline = content.ends_with('\n');
            let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            out[i] = format!("{indent}{}", new_str.trim());
            let mut joined = out.join("\n");
            if trailing_newline {
                joined.push('\n');
            }
            return match std::fs::write(p, &joined) {
                Ok(()) => format!(
                    "edit_file {path} ok (1 replacement, matched whole line {})",
                    i + 1
                ),
                Err(e) => format!("edit_file {path} error: {e}"),
            };
        }

        // Couldn't disambiguate automatically — show each match in context so the model
        // can copy a longer, unique anchor.
        //
        // Matched on the anchor's FIRST line, not the whole anchor. `line.contains(
        // old_str)` can never be true for a multi-line `old_str` — no single line holds
        // a `\n` — so the message promised "copy a line from below verbatim" and then
        // showed nothing at all. Observed live on wireservice__csvkit-1281: eight
        // consecutive rejections on the same anchor, each followed by an empty list,
        // before the model found its own way out.
        let first = old_str.lines().next().unwrap_or(old_str).trim();
        let mut shown = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if !first.is_empty() && line.contains(first) {
                // A line either side, so a repeated line can be told apart by what
                // surrounds it — which is the whole task here.
                let lo = i.saturating_sub(1);
                let hi = (i + 2).min(lines.len());
                for (n, l) in lines[lo..hi].iter().enumerate() {
                    shown.push(format!("  line {}: {}", lo + n + 1, l));
                }
                shown.push(String::from("  ---"));
            }
        }
        if shown.is_empty() {
            // Nothing matched even the first line: the anchor is not in the file in any
            // recognisable form, so show the file rather than an empty promise.
            return format!(
                "edit_file {path} error: old_str {old_str:?} is ambiguous ({count} matches) but \
                 no single line of it could be located to show you. Here is the CURRENT file — \
                 pick your anchor from these exact lines:\n{}",
                number_lines(content)
            );
        }
        return format!(
            "edit_file {path} error: old_str {old_str:?} is ambiguous ({count} matches). \
             Pick a UNIQUE anchor — copy a whole distinct line (or two) from below verbatim, \
             including a neighbouring line if that is what makes it unique:\n{}",
            shown.join("\n")
        );
    }
    let updated = content.replacen(old_str, new_str, 1);
    match std::fs::write(p, &updated) {
        Ok(()) => format!("edit_file {path} ok (1 replacement)"),
        Err(e) => format!("edit_file {path} error: {e}"),
    }
}

/// Collapse a line to its whitespace-insensitive signature: trimmed, with internal runs of
/// whitespace squeezed to one space. Two lines that differ only in indentation/spacing share a
/// signature. Empty after trimming → `None` (blank lines are ignored when aligning a block).
fn line_sig(line: &str) -> Option<String> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    Some(t.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Whitespace-tolerant multi-line replace: when `old_str` doesn't match byte-exactly, try to
/// find a UNIQUE run of file lines whose signatures equal the anchor's non-blank line
/// signatures, and replace that real run with `new_str`. Returns the whole new file content, or
/// `None` if there's no unique multi-line match (so the caller falls back to the error path).
///
/// Only fires for a genuine multi-line anchor (≥2 non-blank lines) — a single-line fuzzy match
/// would be too eager and the exact/whole-line paths already handle single lines. `new_str` is
/// re-indented to the matched block's leading whitespace so the replacement sits correctly.
fn fuzzy_line_block_replace(content: &str, old_str: &str, new_str: &str) -> Option<String> {
    let anchor_sigs: Vec<String> = old_str.lines().filter_map(line_sig).collect();
    if anchor_sigs.len() < 2 {
        return None; // single-line anchors handled elsewhere; don't fuzzy-match those
    }
    let lines: Vec<&str> = content.lines().collect();
    // File-line signatures, keeping the original index (skip blank lines when aligning).
    let sig_idx: Vec<(usize, String)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| line_sig(l).map(|s| (i, s)))
        .collect();

    // Find windows of `sig_idx` whose signatures match `anchor_sigs` in order.
    let mut matches: Vec<(usize, usize)> = Vec::new(); // (first line idx, last line idx) in `lines`
    if sig_idx.len() >= anchor_sigs.len() {
        for w in 0..=sig_idx.len() - anchor_sigs.len() {
            if (0..anchor_sigs.len()).all(|k| sig_idx[w + k].1 == anchor_sigs[k]) {
                let first = sig_idx[w].0;
                let last = sig_idx[w + anchor_sigs.len() - 1].0;
                matches.push((first, last));
            }
        }
    }
    if matches.len() != 1 {
        return None; // must be unambiguous
    }
    let (first, last) = matches[0];

    // Re-indent `new_str` by the SAME leading-whitespace prefix the matched block's first line
    // carries, preserving each new line's OWN relative indentation. The model's old_str/new_str
    // are usually written with a flat or shallow indent; prefixing the block's real indent slots
    // them in correctly while keeping any nested structure the model intended.
    let block_indent: String = lines[first]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    // The anchor's own first-line indent — subtract it so we don't double-count.
    let anchor_indent: usize = old_str
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
        .unwrap_or(0);
    let new_block: Vec<String> = new_str
        .lines()
        .map(|l| {
            if l.trim().is_empty() {
                return String::new();
            }
            let own = l.chars().take_while(|c| c.is_whitespace()).count();
            // Relative indent past the anchor's baseline (never negative).
            let rel = own.saturating_sub(anchor_indent);
            format!("{block_indent}{}{}", " ".repeat(rel), l.trim_start())
        })
        .collect();

    let mut out: Vec<String> = Vec::new();
    out.extend(lines[..first].iter().map(|s| s.to_string()));
    out.extend(new_block);
    out.extend(lines[last + 1..].iter().map(|s| s.to_string()));
    let mut joined = out.join("\n");
    if content.ends_with('\n') {
        joined.push('\n');
    }
    Some(joined)
}
