#[path = "lib.rs"]
mod lib;
use lib::{escape, unescape};

#[test]
fn a_literal_backslash_n_survives_a_round_trip() {
    let raw = "path C:\\new\\table";
    assert_eq!(unescape(&escape(raw)), raw);
}

#[test]
fn a_real_newline_survives_a_round_trip() {
    let raw = "line one\nline two";
    assert_eq!(unescape(&escape(raw)), raw);
}

#[test]
fn a_real_tab_survives_a_round_trip() {
    assert_eq!(unescape(&escape("a\tb")), "a\tb");
}

#[test]
fn an_escaped_record_is_a_single_line() {
    assert!(!escape("a\nb").contains('\n'));
}
