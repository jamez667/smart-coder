//! The storage backend trait every cache layer implements.

/// A read-through key/value layer.
pub trait Store {
    /// Human-readable name, for the stats line.
    fn name(&self) -> &'static str;

    /// Fetch a key, or `None` if this layer does not hold it.
    fn get(&self, key: &str) -> Option<String>;

    /// Insert or replace a key.
    fn put(&mut self, key: &str, value: &str);

    /// How many keys this layer currently holds.
    fn len(&self) -> usize;
}
