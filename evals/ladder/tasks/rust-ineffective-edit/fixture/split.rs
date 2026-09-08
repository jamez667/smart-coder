//! Breaking a duration in milliseconds into whole hours, minutes and seconds.

/// A duration split into its parts. Each field is the whole count of that unit
/// *after* the larger units have been taken out, so `minutes` is always 0..60
/// and `seconds` is always 0..60.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parts {
    pub hours: u64,
    pub minutes: u64,
    pub seconds: u64,
}

const SEC: u64 = 1_000;
const MIN: u64 = 60 * SEC;
const HOUR: u64 = 60 * MIN;

/// Split `ms` into whole hours, minutes and seconds.
pub fn split(ms: u64) -> Parts {
    Parts {
        hours: ms / HOUR,
        minutes: ms / MIN,
        seconds: (ms / SEC) % 60,
    }
}
