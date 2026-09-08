//! A one-line summary of any store, for the status page.

#[path = "layers.rs"]
pub mod layers;

use layers::store::Store;

/// `"<name>: <n> keys"`.
pub fn summarise(s: &dyn Store) -> String {
    format!("{}: {} keys", s.name(), s.len())
}
