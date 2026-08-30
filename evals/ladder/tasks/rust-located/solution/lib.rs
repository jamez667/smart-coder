//! A tiny interval store: merge overlapping ranges as they are inserted.

/// Half-open interval `[start, end)`.
pub type Span = (i64, i64);

/// Insert `new` into `spans` (sorted, non-overlapping), merging any it touches.
///
/// Returns the resulting sorted, non-overlapping set.
pub fn insert(spans: &[Span], new: Span) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut cur = new;
    for &(s, e) in spans {
        if e <= cur.0 || s >= cur.1 {
            // Disjoint: keep it as-is.
            out.push((s, e));
        } else {
            // Overlaps the one being inserted: absorb it.
            cur = (cur.0.min(s), cur.1.max(e));
        }
    }
    out.push(cur);
    out.sort();
    out
}
