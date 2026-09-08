//! Three concrete stores, with quite different internals.

#[path = "store.rs"]
pub mod store;

use std::collections::HashMap;
use store::Store;

/// A plain hash map. Nothing is ever dropped.
pub struct MemStore {
    map: HashMap<String, String>,
}

impl MemStore {
    pub fn new() -> Self {
        MemStore { map: HashMap::new() }
    }
}

impl Store for MemStore {
    fn name(&self) -> &'static str {
        "mem"
    }
    fn get(&self, key: &str) -> Option<String> {
        self.map.get(key).cloned()
    }
    fn put(&mut self, key: &str, value: &str) {
        self.map.insert(key.to_string(), value.to_string());
    }
    fn len(&self) -> usize {
        self.map.len()
    }
}

/// A fixed-capacity ring. The oldest insertion is overwritten when it is full,
/// so entries live in insertion order in a Vec.
pub struct RingStore {
    slots: Vec<(String, String)>,
    cap: usize,
    next: usize,
}

impl RingStore {
    pub fn with_capacity(cap: usize) -> Self {
        RingStore { slots: Vec::new(), cap, next: 0 }
    }
}

impl Store for RingStore {
    fn name(&self) -> &'static str {
        "ring"
    }
    fn get(&self, key: &str) -> Option<String> {
        self.slots.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }
    fn put(&mut self, key: &str, value: &str) {
        if let Some(slot) = self.slots.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value.to_string();
            return;
        }
        if self.slots.len() < self.cap {
            self.slots.push((key.to_string(), value.to_string()));
        } else {
            self.slots[self.next] = (key.to_string(), value.to_string());
            self.next = (self.next + 1) % self.cap;
        }
    }
    fn len(&self) -> usize {
        self.slots.len()
    }
}

/// A namespaced view over another store: every key is prefixed on the way in
/// and stripped on the way out, so two tenants cannot collide.
pub struct PrefixStore<S: Store> {
    inner: S,
    prefix: String,
}

impl<S: Store> PrefixStore<S> {
    pub fn new(inner: S, prefix: &str) -> Self {
        PrefixStore { inner, prefix: prefix.to_string() }
    }
    fn full(&self, key: &str) -> String {
        format!("{}:{}", self.prefix, key)
    }
}

impl<S: Store> Store for PrefixStore<S> {
    fn name(&self) -> &'static str {
        "prefix"
    }
    fn get(&self, key: &str) -> Option<String> {
        self.inner.get(&self.full(key))
    }
    fn put(&mut self, key: &str, value: &str) {
        let k = self.full(key);
        self.inner.put(&k, value);
    }
    fn len(&self) -> usize {
        self.inner.len()
    }
}
