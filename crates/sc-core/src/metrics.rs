//! Tool-call validity metrics — the measurable spine of M1 (spec 07).
//!
//! M1's exit criterion is **≥95% valid tool calls**: malformed calls must always
//! be recovered or escalated, never acted on. To hold ourselves to that we count
//! every model turn's tool-call outcome, so the rate is a number the harness (and
//! the eval suite) can assert on, not a vibe.

/// Counts of tool-call outcomes over a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCallMetrics {
    /// Turns that produced a schema-valid call (executed).
    pub valid: usize,
    /// Turns whose output failed to parse/validate (repaired, never executed).
    pub invalid: usize,
}

impl ToolCallMetrics {
    /// Record a turn that produced a valid, executed call.
    pub fn record_valid(&mut self) {
        self.valid += 1;
    }

    /// Record a turn whose output was malformed and fed back for repair.
    pub fn record_invalid(&mut self) {
        self.invalid += 1;
    }

    /// Total tool-call attempts (valid + invalid).
    pub fn total(&self) -> usize {
        self.valid + self.invalid
    }

    /// Fraction of attempts that were valid, in `[0.0, 1.0]`. An empty run (no
    /// attempts) is treated as `1.0` — there was nothing invalid.
    pub fn valid_rate(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            1.0
        } else {
            self.valid as f64 / total as f64
        }
    }

    /// Fold another set of counts into this one (for aggregating across tasks).
    pub fn merge(&mut self, other: &ToolCallMetrics) {
        self.valid += other.valid;
        self.invalid += other.invalid;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_run_is_perfectly_valid() {
        let m = ToolCallMetrics::default();
        assert_eq!(m.total(), 0);
        assert_eq!(m.valid_rate(), 1.0);
    }

    #[test]
    fn computes_the_valid_rate() {
        let mut m = ToolCallMetrics::default();
        for _ in 0..19 {
            m.record_valid();
        }
        m.record_invalid();
        assert_eq!(m.total(), 20);
        assert_eq!(m.valid_rate(), 0.95);
    }

    #[test]
    fn merge_aggregates_counts() {
        let mut a = ToolCallMetrics {
            valid: 3,
            invalid: 1,
        };
        let b = ToolCallMetrics {
            valid: 7,
            invalid: 0,
        };
        a.merge(&b);
        assert_eq!(
            a,
            ToolCallMetrics {
                valid: 10,
                invalid: 1
            }
        );
    }
}
