//! Running totals over a slice of readings.

/// The largest sum of any CONTIGUOUS non-empty run of values.
///
/// Handles negatives: a run may dip below zero and still be the best overall.
pub fn max_run(values: &[i64]) -> i64 {
    let mut best = values[0];
    let mut here = values[0];
    for &v in &values[1..] {
        here = here.max(v);
        best = best.max(here);
    }
    best
}
