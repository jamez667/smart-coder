// Contract test for max_run. FROZEN: a solver must not modify this file.
#[path = "lib.rs"]
mod lib;
use lib::max_run;

#[test]
fn single_value() {
    assert_eq!(max_run(&[5]), 5);
}

#[test]
fn all_positive_takes_everything() {
    assert_eq!(max_run(&[1, 2, 3]), 6);
}

#[test]
fn a_run_may_dip_negative_and_still_win() {
    // 4 + -1 + 3 == 6 beats the bare 4 or the bare 3.
    assert_eq!(max_run(&[4, -1, 3]), 6);
}

#[test]
fn all_negative_takes_the_least_bad() {
    assert_eq!(max_run(&[-5, -2, -9]), -2);
}

#[test]
fn leading_negatives_are_dropped() {
    assert_eq!(max_run(&[-3, 7, 2]), 9);
}
