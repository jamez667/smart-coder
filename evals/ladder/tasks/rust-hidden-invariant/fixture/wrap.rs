//! Word-wraps a source line to a column width, for the narrow terminal view.

#[path = "highlight.rs"]
pub mod highlight;

use highlight::span::{chunk_count, line_span};

/// Wrap the line containing `at` into chunks of at most `width` bytes.
///
/// The line is taken by span, so this never allocates the whole file.
pub fn wrap_line(text: &str, at: usize, width: usize) -> Vec<String> {
    let s = line_span(text, at);
    let line = &text[s.start..s.end];
    let n = chunk_count(line.len(), width);
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let i = (k * width).min(line.len());
        let j = (i + width).min(line.len());
        out.push(line[i..j].to_string());
    }
    out
}
