// Contract test for Lru. FROZEN: a solver must not modify this file.
#[path = "lib.rs"]
mod lib;
use lib::Lru;

#[test]
fn holds_what_fits() {
    let mut c = Lru::new(2);
    c.put("a", 1);
    c.put("b", 2);
    assert_eq!(c.get("a"), Some(1));
    assert_eq!(c.get("b"), Some(2));
}

#[test]
fn evicts_the_least_recently_used() {
    let mut c = Lru::new(2);
    c.put("a", 1);
    c.put("b", 2);
    c.put("c", 3); // evicts "a"
    assert_eq!(c.get("a"), None);
    assert_eq!(c.get("b"), Some(2));
    assert_eq!(c.get("c"), Some(3));
}

#[test]
fn a_read_counts_as_use() {
    let mut c = Lru::new(2);
    c.put("a", 1);
    c.put("b", 2);
    c.get("a"); // "a" is now the most recent, so "b" is next out
    c.put("c", 3);
    assert_eq!(c.get("b"), None);
    assert_eq!(c.get("a"), Some(1));
}

/// The invariant every operation must preserve: `keys()` and the stored entries
/// describe the same set, and neither ever exceeds the capacity.
#[test]
fn the_cache_never_exceeds_capacity() {
    let mut c = Lru::new(3);
    for (i, k) in ["a", "b", "c", "d", "e", "a", "f"].iter().enumerate() {
        c.put(k, i as i64);
        assert!(
            c.keys().len() <= 3,
            "after put({k}) the cache holds {:?}, over capacity",
            c.keys()
        );
    }
}
