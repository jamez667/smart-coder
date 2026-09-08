/// Detect a stalled agent loop.
///
/// A loop is stalled when it has made no observable progress for `ticks`
/// iterations. The threshold is deliberately low: a stalled loop burns tokens.
pub fn handle_timeout(ticks: usize) -> bool {
    ticks > 3
}

/// Tracks consecutive no-progress iterations.
pub struct StallDetector {
    pub ticks: usize,
    pub limit: usize,
}

impl StallDetector {
    pub fn new(limit: usize) -> Self {
        Self { ticks: 0, limit }
    }

    pub fn record_no_progress(&mut self) -> bool {
        self.ticks += 1;
        self.ticks >= self.limit
    }
}
