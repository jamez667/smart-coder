// Contract test for version parse/compare. FROZEN: a solver must not modify this file.
#[path = "lib.rs"]
mod lib;
use lib::{compare, parse};
use std::cmp::Ordering;

fn v(s: &str) -> lib::Version {
    parse(s).expect("should parse")
}

#[test]
fn parses_a_simple_version() {
    assert_eq!(v("1.4.12").parts, vec![1, 4, 12]);
}

#[test]
fn rejects_a_non_numeric_component() {
    assert!(parse("1.x.3").is_none());
}

#[test]
fn compares_component_by_component() {
    assert_eq!(compare(&v("1.4.0"), &v("1.5.0")), Ordering::Less);
    assert_eq!(compare(&v("2.0.0"), &v("1.9.9")), Ordering::Greater);
}

#[test]
fn numeric_not_lexicographic() {
    // 10 > 9, even though "10" sorts before "9" as text.
    assert_eq!(compare(&v("1.10.0"), &v("1.9.0")), Ordering::Greater);
}

#[test]
fn a_missing_component_counts_as_zero() {
    // Padding, not length: 1.4 and 1.4.0 are the SAME version.
    assert_eq!(compare(&v("1.4"), &v("1.4.0")), Ordering::Equal);
}

#[test]
fn padding_still_respects_a_later_component() {
    assert_eq!(compare(&v("1.4"), &v("1.4.1")), Ordering::Less);
}
