// Contract test for Window. FROZEN: a solver must not modify this file.
#[path = "lib.rs"]
mod lib;
use lib::Window;

#[test]
fn mean_of_a_partial_window() {
    let mut w = Window::new(4);
    w.push(2);
    w.push(4);
    assert_eq!(w.mean(), 3);
}

#[test]
fn values_come_back_oldest_first() {
    let mut w = Window::new(3);
    w.push(1);
    w.push(2);
    assert_eq!(w.values(), vec![1, 2]);
}

#[test]
fn a_full_window_evicts_the_oldest() {
    let mut w = Window::new(3);
    for v in [1, 2, 3, 4] {
        w.push(v);
    }
    assert_eq!(w.values(), vec![2, 3, 4]);
    assert_eq!(w.mean(), 3);
}
