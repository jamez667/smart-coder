//! The mutating tools: `write_file`, `create_file`, `append_file`, `edit_file`,
//! `edit_lines`, `edit_function`.
//!
//! Every writer runs the [`guards`](super::guards) tripwires before touching disk,
//! and every failure is a *self-correcting* observation: it tells the model what to
//! send instead, because a bare "error" just makes a small model retry the same
//! thing. The anchored/line-addressed/by-name edits exist as a ladder — when one
//! addressing mode keeps failing, the error steers to the next.

use std::path::Path;

use super::dropped_def::dropped_definition;
use super::guards::{
    delimiter_regression, destructive_replacement, duplicate_definition, identical_replacement,
    indistinct_anchor, is_code_path,
};
use super::read::{locate_function, Located};
use super::util::{from_lf, number_lines, safe_join, to_lf, uses_crlf};

/// A file with more than this many lines is too large to safely OVERWRITE with `write_file`:
/// a small/mid model can't faithfully reproduce that much code and drops functions or leaves an
/// unterminated string, breaking the build (observed live: the 30B looping write_file on a
/// 790-line terrain.rs, each rewrite introducing a fresh syntax error). Such a file must be
/// changed with surgical `edit_file` / `append_file` instead.
///
/// Public because the agent loop's failed-edit escalation has to answer the SAME question
/// before it steers a stuck model at `write_file`: telling it to rewrite a file this guard
/// will then refuse is a deadlock. It was a duplicated literal `150` in
/// `sc_core::agent`; one constant, one answer.
pub const WRITE_FILE_OVERWRITE_MAX_LINES: usize = 150;

/// The word every no-op observation carries, so the loop (and a human reading the log)
/// can tell "the tool worked and changed nothing" from "the tool worked and edited the
/// file". It must NOT read as a hard error to
/// [`looks_like_failure`](../../../sc_core/agent/dispatch/fn.looks_like_failure.html):
/// the tool did its job, the request was vacuous. So: no "error", no "rejected", no
/// "not found", no "failed" in the status line.
const NO_OP: &str = "no-op";

/// Why an `edit_file` changed nothing.
const NO_OP_EDIT_FILE: &str = "old_str and new_str are identical, so the replacement \
    changed nothing. If the change is already in the file, move on; if it is not, the \
    anchor or the replacement is wrong.";
/// Why a `write_file`/`create_file` changed nothing.
const NO_OP_SAME_BYTES: &str = "the content is byte-for-byte what the file already \
    holds. If the change is already in the file, move on; if it is not, the content you \
    sent is wrong.";
/// Why an `append_file` changed nothing.
const NO_OP_EMPTY_APPEND: &str = "content is empty, so nothing was appended. Send the \
    text you meant to add.";
/// Why an `edit_lines` changed nothing.
const NO_OP_EDIT_LINES: &str = "new_text is identical to the lines it would replace, so \
    the edit changed nothing. If the change is already in the file, move on; if it is \
    not, the line range or the replacement is wrong.";
/// Why an `edit_function` changed nothing.
const NO_OP_EDIT_FUNCTION: &str = "new_body is identical to the function already in the \
    file, so the edit changed nothing. If the change is already in the file, move on; if \
    it is not, the body you sent is wrong.";

/// The observation for a write that changed no bytes: `<tool> <path> no-op (nothing
/// written): <why>`.
///
/// THE BUG THIS EXISTS FOR. `edit_file` answered "ok (1 replacement)" for an edit that
/// replaced a string with itself. Measured on one Mellum run: four verbatim no-op turns,
/// each answered "ok", and the model reasoned from a false premise for the rest of the
/// run. A write that changes nothing must say so — every writer, every time.
///
/// Deliberately not an error. The tool worked; the edit was vacuous. Wording it as an
/// error would make `looks_like_failure` treat a harmless turn as a failure to react to.
fn no_op(prefix: &str, why: &str) -> String {
    format!("{prefix} {NO_OP} (nothing written): {why}")
}

pub fn write_file(workspace: &Path, path: &str, content: &str) -> String {
    match safe_join(workspace, path) {
        Ok(p) => {
            // Guard: refuse to OVERWRITE a large existing file — steer to surgical edits. New
            // files and small files are fine; this only blocks the destructive rewrite of a big
            // one, which is where the model corrupts the codebase.
            if let Ok(existing) = std::fs::read_to_string(&p) {
                // Rewriting a file with exactly what it already holds changes nothing.
                if existing == content {
                    return no_op(&format!("write_file {path}"), NO_OP_SAME_BYTES);
                }
                let existing_lines = existing.lines().count();
                if existing_lines > WRITE_FILE_OVERWRITE_MAX_LINES {
                    return format!(
                        "write_file {path} rejected: {path} already exists and is {existing_lines} \
                         lines — too large to safely overwrite (a full rewrite drops code and \
                         breaks the build). Use edit_file to change a specific snippet: anchor \
                         on a unique line you have READ, and send only the replacement. Make a \
                         small, surgical change."
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

/// `create_file` cannot be a no-op the way the other writers can: it refuses outright
/// when the path already exists (below), so the only write it ever performs creates a
/// file that was not there — which always changes the workspace, empty content included.
/// The identical-bytes case reaches `write_file`, which guards it.
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
            // A CRLF file stays CRLF: the appended text takes the file's endings. A new
            // or LF file gets the content exactly as given.
            let content = if uses_crlf(&existing) {
                from_lf(&to_lf(content), true)
            } else {
                content.to_string()
            };
            // Appending nothing changes nothing. Defence in depth: a model can't reach
            // this through the tool surface, because `content` is a required non-empty
            // `String` and the validator (`spec::validate_value`) rejects `""` before
            // dispatch. But this is a public fn, and a writer that can answer "ok (+0
            // bytes)" for a write that moved nothing is the exact bug `edit_file` had.
            // Placed BEFORE the open, so an empty append never conjures an empty file.
            if content.is_empty() {
                return no_op(&format!("append_file {path}"), NO_OP_EMPTY_APPEND);
            }
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
    let Located {
        src,
        start,
        end,
        count,
        crlf,
    } = match locate_function(workspace, path, name) {
        Ok(v) => v,
        Err(e) => return format!("edit_function {e}"),
    };
    let p = match safe_join(workspace, path) {
        Ok(p) => p,
        Err(e) => return format!("edit_function {path} rejected: {e}"),
    };
    let new_body = to_lf(new_body);
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

    // The new body is what the function already says — splicing it in changes nothing.
    // (`src` and `joined` are both LF, so this is a like-for-like comparison.)
    if joined == src {
        return no_op(&format!("edit_function {path}:{name}"), NO_OP_EDIT_FUNCTION);
    }
    match std::fs::write(&p, from_lf(&joined, crlf)) {
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
/// (`end == start - 1`) inserts before `start`. Edits in LF and writes back the file's own
/// line endings (matches edit_file). Self-correcting errors on an out-of-range or inverted span.
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
    let crlf = uses_crlf(&raw);
    let content = to_lf(&raw);
    let new_text = to_lf(new_text);
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
    // The addressed lines already read exactly like new_text — splicing them in changes
    // nothing. (Also catches the degenerate insert of empty text.)
    if joined == content {
        return no_op(&format!("edit_lines {path}"), NO_OP_EDIT_LINES);
    }
    match std::fs::write(&p, from_lf(&joined, crlf)) {
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
    // An anchor that cannot address anything is wrong before we look at the file at all --
    // so this runs ahead of the match, and for EVERY path, not just `is_code_path` ones (a
    // bare `!` is no more of an address in Python or Markdown than it is in Rust).
    if let Some(msg) = indistinct_anchor(old_str) {
        return format!("edit_file {path} rejected: {msg}");
    }
    // A replacement identical to its anchor cannot change anything, whatever the file holds, so
    // it is judged before the MATCH -- and saying so here is the whole point. The
    // `NO_OP_EDIT_FILE` answers below all sit past a successful match, so an identical pair whose
    // anchor missed used to fall through to `anchor not found; closest match:` instead: a message
    // about WHERE, when the defect is WHAT. Measured on `rust-two-stage`, that sent the model
    // hunting for a better anchor for 18 turns while the anchor was never the problem.
    if let Some(msg) = identical_replacement(old_str, new_str) {
        return format!("edit_file {path} rejected: {msg}");
    }
    // Normalize line endings to LF for matching/editing, on BOTH sides. A file checked out on
    // Windows is CRLF; the model, shown that file verbatim, faithfully copies CRLF into old_str
    // — but if we normalize only the file and not old_str, the `\r` in the anchor breaks the
    // match and EVERY edit fails (observed live 2026-07-15: the 30B's first, correct anchor on a
    // CRLF terrain.rs missed, and it spiralled into corrupting the file trying to "fix" it). Strip
    // `\r` from the file AND from old_str/new_str so a CRLF-copied anchor matches. We edit in LF
    // space and write back whichever endings the file had, so one edit never flips a CRLF
    // checkout to LF and shows up as a whole-file diff.
    let crlf = uses_crlf(&raw);
    let content = to_lf(&raw);
    let old_str = to_lf(old_str);
    let new_str = to_lf(new_str);
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
    if is_code_path(path) {
        // Destructive-replacement guard: this one judges the old_str/new_str PAIR, not the
        // resulting file, so it runs whether or not the anchor resolves — an edit that would
        // destroy the line is wrong at every match count, and rejecting it here keeps the model
        // from re-sending it down the fuzzy path.
        if let Some(msg) = destructive_replacement(&old_owned, &new_owned) {
            return format!("edit_file {path} rejected: {msg}");
        }
        // Same reasoning, one level up in meaning: an edit that drops a `fn` the anchor
        // carried is destroying a DEFINITION rather than a line, and `destructive_replacement`
        // cannot see it (its `new_str` is full of letters). Measured on `rust-trait-impl`: the
        // model swapped `fn len` for `fn evict` four times -- three of the `evict` bodies were
        // correct -- and every caller of `len` stopped compiling.
        if let Some(msg) = dropped_definition(&old_owned, &new_owned) {
            return format!("edit_file {path} rejected: {msg}");
        }
        if content.matches(&old_owned).count() == 1 {
            let after = content.replacen(&old_owned, &new_owned, 1);
            if let Some(msg) = duplicate_definition(&content, &after) {
                return format!("edit_file {path} rejected: {msg}");
            }
            // Brace-balance tripwire, for parity with edit_lines/edit_function: the anchored
            // editor was the ONLY writer with no delimiter check, and it is the one a small model
            // reaches for most. Same regression semantics — a pre-existing imbalance is never
            // blamed on this edit.
            if let Some(msg) = delimiter_regression(&content, &after) {
                return format!(
                    "edit_file {path} rejected: {msg} Your new_str isn't brace-balanced against \
                     the anchor it replaces — recount the delimiters in old_str and match them in \
                     new_str."
                );
            }
        }
    }
    edit_file_with(workspace, &p, path, &content, &old_owned, &new_owned, crlf)
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

/// Apply an `old_str`→`new_str` replacement to already-read `content` at `p`,
/// enforcing the exactly-once rule (with whole-line disambiguation and
/// self-correcting errors for small models).
///
/// The "exactly once" rule is the small-model safety net (spec 04): an ambiguous
/// anchor (0 or >1 matches) is rejected with a precise count instead of guessing.
///
/// `content`, `old_str` and `new_str` are LF; `crlf` says what the file on disk uses,
/// and every write here restores it.
fn edit_file_with(
    workspace: &Path,
    p: &Path,
    path: &str,
    content: &str,
    old_str: &str,
    new_str: &str,
    crlf: bool,
) -> String {
    let count = content.matches(old_str).count();
    if count == 0 {
        // Exact match failed. Before giving up, try a WHITESPACE-TOLERANT line match: a
        // model editing a large file often reproduces the block's TEXT correctly but gets the
        // indentation or inner spacing slightly wrong, so a byte-exact `old_str` never matches
        // and it thrashes (observed live: the 30B looping read→edit→write_file on terrain.rs).
        // If the anchor's non-blank lines match a unique run of the file's lines (comparing each
        // line's whitespace-collapsed text), replace that real run — the edit lands despite the
        // spacing drift.
        if let Some(fuzzed) = fuzzy_line_block_replace(content, old_str, new_str) {
            if fuzzed == content {
                return no_op(&format!("edit_file {path}"), NO_OP_EDIT_FILE);
            }
            return match std::fs::write(p, from_lf(&fuzzed, crlf)) {
                Ok(()) => format!(
                    "edit_file {path} ok (1 replacement, whitespace-tolerant match){}",
                    changed_region(path, &fuzzed, new_str)
                ),
                Err(e) => format!("edit_file {path} error: {e}"),
            };
        }
        // Still nothing. Last rung: an INDENTED SUB-EXPRESSION anchor. The model points at
        // the expression it wants to change rather than the whole statement, and prefixes it
        // with the indentation it believes the line carries — so `        self.buf[..]`
        // (8 spaces) never matches `            out.push(self.buf[..]);` (12), even though
        // the expression itself sits in exactly one place. Measured on the Mellum
        // transcripts: 24 of 43 anchor failures are this one class.
        //
        // The anchor is not a whole line, so `line_sig` above can't match it; and the
        // whole-line disambiguation further down is gated on `count > 1`, so it never runs
        // here. Hence this rung.
        //
        // SINGLE-LINE ONLY, deliberately. A multi-line `old_str` whose trimmed form happens
        // to occur once is a different and riskier proposition — the interior lines' own
        // indentation would have to match byte-exactly anyway, and whole/multi-line blocks
        // are already `fuzzy_line_block_replace`'s job. A newline in the anchor skips this.
        if let Some(spliced) = indent_tolerant_span_replace(content, old_str, new_str) {
            // The guards must still run on this path: a partial-line match is not a licence
            // to skip them. (`destructive_replacement` judges the old/new PAIR and has
            // already run in `edit_file`; `delimiter_regression` needs the resulting file,
            // which only exists here.)
            if is_code_path(path) {
                if let Some(msg) = delimiter_regression(content, &spliced) {
                    return format!(
                        "edit_file {path} rejected: {msg} Your new_str isn't brace-balanced \
                         against the anchor it replaces — recount the delimiters in old_str and \
                         match them in new_str."
                    );
                }
            }
            if spliced == content {
                return no_op(&format!("edit_file {path}"), NO_OP_EDIT_FILE);
            }
            return match std::fs::write(p, from_lf(&spliced, crlf)) {
                Ok(()) => {
                    format!(
                        "edit_file {path} ok (1 replacement, matched ignoring indentation){}",
                        changed_region(path, &spliced, new_str)
                    )
                }
                Err(e) => format!("edit_file {path} error: {e}"),
            };
        }
        // The anchor isn't in the file in any form. The usual cause for a small model is
        // that the edit already landed (or it's working from a stale view), so it keeps
        // re-proposing a change that's no longer applicable. Show it the place in the
        // CURRENT file that most resembles what it asked for, so it re-anchors there.
        return anchor_not_found(workspace, path, content, old_str, p);
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
            if joined == content {
                return no_op(&format!("edit_file {path}"), NO_OP_EDIT_FILE);
            }
            return match std::fs::write(p, from_lf(&joined, crlf)) {
                Ok(()) => format!(
                    "edit_file {path} ok (1 replacement, matched whole line {}){}",
                    i + 1,
                    changed_region(path, &joined, new_str)
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
            // Nothing matched even the first line (an anchor that opens with a blank line,
            // say): show the closest block rather than an empty promise -- and never the
            // whole file.
            return format!(
                "edit_file {path} error: old_str {old_str:?} is ambiguous ({count} matches); \
                 pick a UNIQUE anchor from the lines near the closest match below:\n{}",
                anchor_not_found(workspace, path, content, old_str, p)
                    .split_once('\n')
                    .map(|(_, block)| block.to_string())
                    .unwrap_or_default()
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
    if updated == content {
        return no_op(&format!("edit_file {path}"), NO_OP_EDIT_FILE);
    }
    match std::fs::write(p, from_lf(&updated, crlf)) {
        Ok(()) => format!(
            "edit_file {path} ok (1 replacement){}",
            changed_region(path, &updated, new_str)
        ),
        Err(e) => format!("edit_file {path} error: {e}"),
    }
}

/// Lines of context shown either side of the region an edit just changed.
const ECHO_CONTEXT: usize = 3;
/// The most lines an after-edit echo ever shows. `edit_file` draws the TIGHT observation
/// cap (`observation_cap_for`'s default, not the generous file-read one), so an echo that
/// ran long would evict the very context this exists to save.
const ECHO_MAX_LINES: usize = 24;

/// Whether an `SC_NO_EDIT_ECHO` value turns the after-edit echo off.
///
/// Split out so the A/B switch can be tested without `set_var`, which is process-global and
/// leaks across cargo's parallel test threads.
pub(crate) fn echo_disabled_by(v: Option<&str>) -> bool {
    v.is_some_and(|v| v != "0")
}

/// The file as it reads NOW around the text just written, numbered.
///
/// THE MEASURED BUG. A successful edit answered `ok (1 replacement)` and showed nothing, so
/// the model's picture of the file stayed one change out of date. Its next anchor was copied
/// from that stale picture and missed. On `engine-diagonal-wired` x6 the model aimed 140 of
/// 198 turns at the RIGHT files and still landed only 32 edits against 88 failures -- and of
/// 48 missed anchors, 13 came immediately after one of its own successful edits and ZERO came
/// after a `read_file`. The anchors were not bad; the view behind them was.
///
/// So every landing edit now returns the changed region. `docs/notes/next-harness-work.md`
/// item 4 called this before the evidence arrived: "returning a numbered view of the changed
/// region after an edit -- so the model can chain its next edit without a fresh `read_file`".
///
/// Bounded hard: [`ECHO_MAX_LINES`] beats the tight observation cap, and a `new_str` longer
/// than that is shown from its start rather than truncated in the middle, because the top of
/// a freshly written block is where the next anchor gets copied from.
fn changed_region(path: &str, updated: &str, new_str: &str) -> String {
    // An off switch, for measuring this feature against itself.
    //
    // It exists because the first run WITH the echo regressed the numbers it was meant to
    // improve: median failed-run turns 23 -> 34, budget-exhausted runs 4 -> 9, and median
    // PEAK PROMPT 7,857 -> 10,401 (+32%) across two 51-run passes. The echo is the obvious
    // suspect -- 113 emissions of up to 24 numbered lines, each persisting in the window --
    // but those passes differ in three commits, so the comparison cannot separate this
    // change from the others or from run-to-run variance.
    //
    // An env var rather than `AgentConfig`: threading a flag through the loop into this
    // crate is a real API change, and making one to run an experiment gets the experiment
    // shipped. If the A/B says keep it, this becomes proper config; if it says drop it, the
    // whole function goes and takes the switch with it.
    if echo_disabled_by(std::env::var("SC_NO_EDIT_ECHO").ok().as_deref()) {
        return String::new();
    }
    let lines: Vec<&str> = updated.lines().collect();
    // Only when a stale view is actually POSSIBLE. If the whole file fits inside the echo
    // window the model can already see all of it, so the echo is noise on every trivial
    // edit -- and it would rewrite the observation of a one-line file, which
    // `a_real_write_still_reports_exactly_as_before` pins byte-for-byte and is right to.
    // The threshold is the window itself rather than a new invented number.
    if lines.len() <= ECHO_MAX_LINES {
        return String::new();
    }
    // Where the new text starts, in line terms. `new_str` was just written, so it is present
    // -- but a whitespace-tolerant or indent-tolerant match may have altered it on the way in,
    // so fall back to its first non-blank line before giving up.
    let probe = new_str
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if probe.is_empty() {
        return String::new();
    }
    let Some(at) = lines
        .iter()
        .position(|l| l.trim() == probe || l.contains(probe))
    else {
        return String::new();
    };
    let written = new_str.lines().count().max(1);
    let lo = at.saturating_sub(ECHO_CONTEXT);
    let hi = (at + written + ECHO_CONTEXT)
        .min(lines.len())
        .min(lo + ECHO_MAX_LINES);
    format!(
        "\n{path} now reads:\n{}",
        number_lines(&lines[lo..hi], lo + 1)
    )
}

/// Lines of context shown either side of the closest block on a missed anchor.
const MISS_CONTEXT: usize = 3;
/// The most lines a missed-anchor message ever shows. A whole-file dump is what this
/// replaces: on a 900-line file it cost the model its window and told it nothing about
/// WHERE it had been looking.
const MISS_MAX_LINES: usize = 30;

/// The missed-anchor observation: `edit_file <path>: anchor not found; closest match:`
/// and the numbered lines around the file line that most resembles the anchor's first
/// line, [`MISS_CONTEXT`] either side of the anchor-length block, never more than
/// [`MISS_MAX_LINES`].
fn anchor_not_found(
    workspace: &Path,
    path: &str,
    content: &str,
    old_str: &str,
    p: &Path,
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let anchor: Vec<&str> = old_str.lines().collect();
    let probe = anchor
        .iter()
        .find(|l| !l.trim().is_empty())
        .copied()
        .unwrap_or("");
    // Before blaming the anchor, ask whether it belongs to a DIFFERENT file. A model that
    // copies an anchor out of the test it is trying to satisfy, then aims the edit at the
    // source, gets "anchor not found" and dutifully hunts for a better anchor -- forever.
    // Measured on `rust-two-stage`: the anchor occurred verbatim in test.rs and nowhere in
    // lib.rs, and the closest-match block shown instead was scored on tokens like `.` and
    // `a`, pointing into an unrelated function.
    if let Some(elsewhere) = anchor_in_a_sibling(workspace, p, old_str) {
        return elsewhere;
    }
    let Some(best) = closest_line(&lines, probe) else {
        return format!(
            "edit_file {path}: anchor not found; no line of the file resembles the anchor \
             ({} lines in the file -- read it again before editing)",
            lines.len()
        );
    };
    let lo = best.saturating_sub(MISS_CONTEXT);
    let hi = (best + anchor.len().max(1) + MISS_CONTEXT)
        .min(lines.len())
        .min(lo + MISS_MAX_LINES);
    format!(
        "edit_file {path}: anchor not found; closest match:\n{}",
        number_lines(&lines[lo..hi], lo + 1)
    )
}

/// The most sibling files scanned for a missed anchor. A miss is already the slow path, and
/// the answer only has to beat "no idea"; reading a whole large workspace to improve one
/// error message is not a trade worth making.
const SIBLING_SCAN_MAX: usize = 60;

/// The shortest anchor worth hunting for in other files, in non-whitespace CHARACTERS. A
/// short fragment (`}`, `let x`) occurs in half the repository, so a "found it elsewhere"
/// answer built on one would be noise pointing at an arbitrary file.
///
/// Counted in characters, not tokens, because a token count is a terrible proxy for
/// distinctiveness: the live `rust-two-stage` anchor,
/// `assert_eq!(compare(&v("1.4"), &v("1.4.0")), Ordering::Equal);`, is 3 whitespace-split
/// tokens and 58 characters. A 4-token floor rejected the exact case this exists for --
/// found by instrumenting the scan rather than reasoning about it.
const SIBLING_MIN_CHARS: usize = 24;

/// Does `old_str` appear verbatim in some OTHER file in the workspace? If so, name it.
///
/// THE BUG THIS EXISTS FOR. On `rust-two-stage` the model copied an assertion out of the
/// frozen `test.rs` and sent it as an `edit_file` anchor against `lib.rs`. The string occurs
/// exactly once in test.rs and not at all in lib.rs, so it got `anchor not found` with a
/// closest-match block scored on shared punctuation -- an unrelated function eight lines
/// into the file. It re-sent that anchor 14 times across 18 turns.
///
/// The harness could see the answer the whole time: it had the workspace, and `edit_file`
/// receives it. Matching is whole-line and exact (the same `line_sig` idea the fuzzy path
/// uses, minus the tolerance) because this only has to answer "is this text somewhere else",
/// and a fuzzy hit here would point the model at the wrong file with confidence.
fn anchor_in_a_sibling(workspace: &Path, target: &Path, old_str: &str) -> Option<String> {
    let probe: Vec<String> = old_str
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect();
    let weight: usize = probe
        .iter()
        .map(|l| l.chars().filter(|c| !c.is_whitespace()).count())
        .sum();
    if probe.is_empty() || weight < SIBLING_MIN_CHARS {
        return None;
    }
    let target = target.canonicalize().ok();
    for rel in super::util::source_files(workspace)
        .into_iter()
        .take(SIBLING_SCAN_MAX)
    {
        let Ok(cand) = safe_join(workspace, &rel) else {
            continue;
        };
        if cand.canonicalize().ok() == target {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&cand) else {
            continue;
        };
        let hay: Vec<String> = to_lf(&body)
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        let Some(at) = hay
            .windows(probe.len().max(1))
            .position(|w| w == probe.as_slice())
        else {
            continue;
        };
        return Some(format!(
            "edit_file: that anchor is not in this file -- it is in {rel}, at line {}. You are \
             editing the wrong file. If {rel} is the test that defines the behaviour, it is not \
             the thing to change: fix the source it exercises. Otherwise re-send this edit with \
             path {rel}.",
            at + 1
        ));
    }
    None
}

/// The index of the file line that shares the most whitespace-split tokens with
/// `probe` — the whitespace-signature overlap. Ties go to the line whose token count is
/// nearest the probe's, then to the earlier line. `None` when no line shares a token.
fn closest_line(lines: &[&str], probe: &str) -> Option<usize> {
    let probe_tokens: Vec<&str> = probe.split_whitespace().collect();
    if probe_tokens.is_empty() {
        return None;
    }
    let mut best: Option<(usize, usize, usize)> = None; // (overlap, length distance, idx)
    for (i, line) in lines.iter().enumerate() {
        let mut remaining: Vec<&str> = line.split_whitespace().collect();
        let len_distance = remaining.len().abs_diff(probe_tokens.len());
        let mut overlap = 0;
        for t in &probe_tokens {
            if let Some(pos) = remaining.iter().position(|r| r == t) {
                remaining.swap_remove(pos);
                overlap += 1;
            }
        }
        let better = match best {
            None => overlap > 0,
            Some((o, d, _)) => overlap > o || (overlap == o && len_distance < d),
        };
        if better {
            best = Some((overlap, len_distance, i));
        }
    }
    best.map(|(_, _, i)| i)
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

/// Whitespace-tolerant line replace: when `old_str` doesn't match byte-exactly, try to find a
/// UNIQUE run of file lines whose signatures equal the anchor's non-blank line signatures, and
/// replace that real run with `new_str`. Returns the whole new file content, or `None` if
/// there's no unique match (so the caller falls back to the error path).
///
/// A single-line anchor qualifies too: the drift a model introduces on one line (its
/// indentation, a tab for four spaces, trailing whitespace) is the same drift it introduces
/// on ten, and uniqueness is what keeps the match safe, not length. `new_str` is re-indented
/// to the matched block's leading whitespace so the replacement sits correctly.
fn fuzzy_line_block_replace(content: &str, old_str: &str, new_str: &str) -> Option<String> {
    let anchor_sigs: Vec<String> = old_str.lines().filter_map(line_sig).collect();
    if anchor_sigs.is_empty() {
        return None; // a blank anchor matches nothing
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

/// Indentation-tolerant SPAN replace, for a SINGLE-LINE anchor that names a
/// sub-expression rather than a whole line.
///
/// The model writes `        self.buf[(self.head + i) % self.cap]` — the expression it
/// means, carrying the indentation it *believes* the line has. The file line is
/// `            out.push(self.buf[(self.head + i) % self.cap]);`. The exact anchor
/// occurs zero times; the TRIMMED anchor occurs exactly once. That is unambiguous, so
/// the edit should land.
///
/// The rule, and nothing looser than it:
///
/// * the anchor must be single-line — a `\n` in `old_str` returns `None` (multi-line
///   blocks belong to [`fuzzy_line_block_replace`], which aligns whole-line signatures);
/// * `old_str.trim()` must be non-empty;
/// * it must occur EXACTLY once in the file as a plain substring. Zero or two-or-more
///   and we return `None` — the caller falls through to today's behaviour. We never
///   choose between candidates.
///
/// Only the matched SPAN is replaced, not the line: the anchor sits mid-line inside
/// `out.push(` … `);`, and that surrounding text must survive untouched. The replacement
/// is `new_str.trim()` for the same reason the match needed trimming — the model's
/// `new_str` carries the same phantom indentation as its `old_str`, and the line's real
/// indentation is already in the file, outside the span. A `new_str` that trims to
/// nothing deletes the span, which is a legitimate operation.
///
/// Returns the whole new file content, or `None` when the rule isn't met.
fn indent_tolerant_span_replace(content: &str, old_str: &str, new_str: &str) -> Option<String> {
    // Single-line anchors only. Said in the doc comment above, enforced here.
    if old_str.contains('\n') {
        return None;
    }
    let needle = old_str.trim();
    if needle.is_empty() {
        return None;
    }
    // Uniqueness is mandatory — 0 or 2+ and we decline rather than guess.
    if content.matches(needle).count() != 1 {
        return None;
    }
    Some(content.replacen(needle, new_str.trim(), 1))
}
