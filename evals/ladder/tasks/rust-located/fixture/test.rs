// Contract test for insert. FROZEN: a solver must not modify this file.
#[path = "lib.rs"]
mod lib;
use lib::insert;

#[test]
fn into_empty() {
    assert_eq!(insert(&[], (2, 5)), vec![(2, 5)]);
}

#[test]
fn disjoint_stays_separate() {
    assert_eq!(insert(&[(1, 2)], (5, 6)), vec![(1, 2), (5, 6)]);
}

#[test]
fn overlapping_merges() {
    assert_eq!(insert(&[(1, 4)], (3, 7)), vec![(1, 7)]);
}

#[test]
fn spans_that_only_touch_do_not_merge() {
    // Half-open: [1,3) and [3,6) share no point, so they stay separate.
    assert_eq!(insert(&[(1, 3)], (3, 6)), vec![(1, 3), (3, 6)]);
}

#[test]
fn swallows_several_at_once() {
    assert_eq!(insert(&[(1, 2), (3, 4), (5, 6)], (0, 7)), vec![(0, 7)]);
}
