// Contract test for the diagnostic renderer. FROZEN: a solver must not modify
// this file.
#[path = "wrap.rs"]
mod wrap;

use wrap::highlight;
use wrap::highlight::span::{line_span, Span};
use wrap::wrap_line;

const SRC: &str = "let a = 1;\nlet bb = 22;\nlet ccc = 333;";

// --- the reported symptom ---

#[test]
fn wrapping_produces_no_empty_trailing_chunk() {
    // Reported by the narrow-terminal view: the middle line is 12 bytes, so at
    // width 6 it is exactly two chunks -- but the pane draws a blank third row.
    let chunks = wrap_line(SRC, 12, 6);
    assert_eq!(chunks, vec!["let bb".to_string(), " = 22;".to_string()]);
}

#[test]
fn a_line_that_does_not_divide_evenly_still_wraps() {
    // 14 bytes at width 6: 6 + 6 + 2.
    assert_eq!(
        wrap_line(SRC, 26, 6),
        vec!["let cc".to_string(), "c = 33".to_string(), "3;".to_string()]
    );
}

#[test]
fn a_line_shorter_than_the_width_is_one_chunk() {
    assert_eq!(wrap_line(SRC, 0, 40), vec!["let a = 1;".to_string()]);
}

// --- the other consumer of the same helper ---

#[test]
fn a_diagnostic_that_divides_evenly_occupies_three_rows() {
    // The middle line is 12 bytes: two wrapped rows at width 6, plus the caret
    // row. A renderer that reserves four scrolls the pane by a blank line.
    assert_eq!(highlight::rendered_rows(SRC, 12, 6), 3);
}

#[test]
fn a_diagnostic_row_count_matches_what_wrapping_actually_emits() {
    // The invariant the two callers share: rows == chunks + the caret row.
    for (at, width) in [(0usize, 6usize), (12, 6), (26, 6), (12, 4), (26, 7), (0, 40)] {
        assert_eq!(
            highlight::rendered_rows(SRC, at, width),
            wrap_line(SRC, at, width).len() + 1,
            "row count disagrees with wrapping at offset {at}, width {width}"
        );
    }
}

// --- everything that already worked must keep working ---

#[test]
fn a_source_line_excludes_its_newline() {
    assert_eq!(highlight::source_line(SRC, 12), "let bb = 22;");
    assert_eq!(highlight::source_line(SRC, 26), "let ccc = 333;");
}

#[test]
fn a_diagnostic_puts_the_caret_under_the_right_column() {
    assert_eq!(highlight::diagnostic(SRC, 15), "let bb = 22;\n    ^");
}

#[test]
fn a_highlight_span_covers_exactly_the_line() {
    assert_eq!(highlight::highlight_span(SRC, 12), Span::new(11, 23));
    assert_eq!(line_span(SRC, 12).len(), "let bb = 22;".len());
}
