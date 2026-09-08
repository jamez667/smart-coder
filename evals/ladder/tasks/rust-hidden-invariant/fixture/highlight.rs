//! Renders a caret line under the offending part of a source line, the way a
//! compiler diagnostic does.

#[path = "span.rs"]
pub mod span;

use span::{chunk_count, line_span, Span};

/// The source line containing `at`, without its trailing newline.
pub fn source_line(text: &str, at: usize) -> &str {
    let s = line_span(text, at);
    &text[s.start..s.end]
}

/// A caret line: spaces up to `at`, then a `^` under the byte at `at`.
pub fn caret(text: &str, at: usize) -> String {
    let s = line_span(text, at);
    let col = at - s.start;
    let mut out = " ".repeat(col);
    out.push('^');
    out
}

/// A whole two-line diagnostic body: the source line, then the caret line.
pub fn diagnostic(text: &str, at: usize) -> String {
    format!("{}\n{}", source_line(text, at), caret(text, at))
}

/// The span highlighted by a diagnostic at `at`.
pub fn highlight_span(text: &str, at: usize) -> Span {
    line_span(text, at)
}

/// How many terminal rows a diagnostic for `at` occupies at `width` columns:
/// the wrapped source line, plus one row for the caret.
pub fn rendered_rows(text: &str, at: usize, width: usize) -> usize {
    let s = line_span(text, at);
    chunk_count(s.len(), width) + 1
}
