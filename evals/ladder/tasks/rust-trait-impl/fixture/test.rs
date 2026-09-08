// Contract test for the store layers. FROZEN: a solver must not modify this file.
#[path = "stats.rs"]
mod stats;

use stats::layers::store::Store;
use stats::layers::{MemStore, PrefixStore, RingStore};

// The trait gains `evict(&mut self, key: &str) -> bool`: drop the key if this
// layer holds it, returning whether anything was dropped. `len` must agree
// afterwards, and every other key must survive.

// --- what already works must keep working ---

#[test]
fn the_existing_surface_is_unchanged() {
    let mut m = MemStore::new();
    m.put("a", "1");
    m.put("b", "2");
    assert_eq!(m.get("a").as_deref(), Some("1"));
    assert_eq!(m.len(), 2);
    assert_eq!(stats::summarise(&m), "mem: 2 keys");

    let mut r = RingStore::with_capacity(2);
    r.put("a", "1");
    r.put("b", "2");
    r.put("c", "3"); // evicts "a", the oldest
    assert_eq!(r.get("a"), None);
    assert_eq!(r.get("c").as_deref(), Some("3"));
    assert_eq!(r.len(), 2);

    let mut p = PrefixStore::new(MemStore::new(), "t1");
    p.put("a", "1");
    assert_eq!(p.get("a").as_deref(), Some("1"));
    assert_eq!(stats::summarise(&p), "prefix: 1 keys");
}

// --- the new method, at each impl ---

#[test]
fn mem_evicts_one_key_and_keeps_the_rest() {
    let mut m = MemStore::new();
    m.put("a", "1");
    m.put("b", "2");
    assert_eq!(m.evict("a"), true);
    assert_eq!(m.get("a"), None);
    assert_eq!(m.get("b").as_deref(), Some("2"));
    assert_eq!(m.len(), 1);
    assert_eq!(m.evict("a"), false, "evicting a missing key reports nothing dropped");
    assert_eq!(m.len(), 1);
}

#[test]
fn ring_evicts_and_shrinks() {
    let mut r = RingStore::with_capacity(3);
    r.put("a", "1");
    r.put("b", "2");
    r.put("c", "3");
    assert_eq!(r.evict("b"), true);
    assert_eq!(r.get("b"), None);
    assert_eq!(r.get("a").as_deref(), Some("1"));
    assert_eq!(r.get("c").as_deref(), Some("3"));
    assert_eq!(r.len(), 2, "an evicted ring slot is freed, not left occupied");
    assert_eq!(r.evict("zz"), false);
}

#[test]
fn ring_recovers_capacity_after_an_eviction() {
    // The trap: an eviction must FREE a slot, not blank one. After evicting,
    // the ring has room for a new key again -- and must not drop a survivor
    // to make that room.
    let mut r = RingStore::with_capacity(3);
    r.put("a", "1");
    r.put("b", "2");
    r.put("c", "3");
    assert_eq!(r.evict("b"), true);
    assert_eq!(r.len(), 2);
    r.put("d", "4");
    assert_eq!(r.len(), 3, "the freed slot takes the new key");
    for (k, v) in [("a", "1"), ("c", "3"), ("d", "4")] {
        assert_eq!(r.get(k).as_deref(), Some(v), "{k} was dropped to make room that already existed");
    }
    // Full again: the next insert displaces exactly one existing key, no more.
    r.put("e", "5");
    assert_eq!(r.len(), 3);
    assert_eq!(r.get("e").as_deref(), Some("5"));
    let survivors = ["a", "c", "d"].iter().filter(|k| r.get(k).is_some()).count();
    assert_eq!(survivors, 2, "exactly one existing key is displaced");
}

#[test]
fn prefix_evicts_through_its_namespace() {
    let mut inner = MemStore::new();
    inner.put("t1:a", "1");
    inner.put("t2:a", "2"); // another tenant's key with the same short name
    let mut p = PrefixStore::new(inner, "t1");
    assert_eq!(p.evict("a"), true);
    assert_eq!(p.get("a"), None);
    assert_eq!(p.len(), 1, "the other tenant's key must survive");
    assert_eq!(p.evict("a"), false);
}

#[test]
fn prefix_reports_what_the_inner_store_reported() {
    // A prefix layer owns no storage: it must return the inner store's answer,
    // not a guess of its own.
    let mut p = PrefixStore::new(RingStore::with_capacity(2), "t1");
    p.put("a", "1");
    assert_eq!(p.evict("nope"), false);
    assert_eq!(p.evict("a"), true);
    assert_eq!(p.len(), 0);
}

#[test]
fn eviction_is_reachable_through_the_trait_object() {
    let mut m = MemStore::new();
    m.put("k", "v");
    let dynref: &mut dyn Store = &mut m;
    assert_eq!(dynref.evict("k"), true);
    assert_eq!(stats::summarise(&m), "mem: 0 keys");
}
