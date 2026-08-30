//! A least-recently-used cache with a fixed capacity.

pub struct Lru {
    cap: usize,
    /// Keys in use order, least-recent FIRST.
    order: Vec<String>,
    vals: Vec<(String, i64)>,
}

impl Lru {
    pub fn new(cap: usize) -> Self {
        Lru {
            cap,
            order: Vec::new(),
            vals: Vec::new(),
        }
    }

    fn touch(&mut self, key: &str) {
        self.order.push(key.to_string());
    }

    pub fn get(&mut self, key: &str) -> Option<i64> {
        let found = self.vals.iter().find(|(k, _)| k == key).map(|(_, v)| *v);
        if found.is_some() {
            self.touch(key);
        }
        found
    }

    pub fn put(&mut self, key: &str, val: i64) {
        if let Some(e) = self.vals.iter_mut().find(|(k, _)| k == key) {
            e.1 = val;
            self.touch(key);
            return;
        }
        if self.vals.len() == self.cap {
            let evict = self.order.remove(0);
            self.vals.retain(|(k, _)| *k != evict);
        }
        self.vals.push((key.to_string(), val));
        self.order.push(key.to_string());
    }

    /// Keys currently held, least-recently-used first.
    pub fn keys(&self) -> Vec<String> {
        self.order.clone()
    }
}
