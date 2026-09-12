//! The shared sample buffer every telemetry channel writes into.
//!
//! A `Series` is a fixed-capacity ring of `Sample`s in arrival order. The bus
//! delivers readings monotonically in `t_ms`, so the ring is always sorted by
//! time and a window query is a contiguous slice — no sorting, no scanning from
//! both ends.
//!
//! What lives here is deliberately narrow: storage, and the two accessors every
//! channel needs (`iter` and `len`). Anything that INTERPRETS a sample — what
//! counts as a spike, what the units mean, how to label it — belongs to the
//! channel that owns the sensor, not to the buffer.

use crate::{Sample, WindowStats};

/// A fixed-capacity ring of samples in arrival order.
///
/// Overwrites oldest-first once full, which is what the dashboard wants: a
/// telemetry pane shows the recent past and a run that never ends must not grow
/// without bound.
#[derive(Debug, Clone)]
pub struct Series {
    samples: Vec<Sample>,
    capacity: usize,
    /// Where the next push lands once the ring has wrapped. Meaningless until
    /// `samples.len() == capacity`.
    head: usize,
}

impl Series {
    /// An empty series that will hold at most `capacity` samples.
    ///
    /// A zero capacity is legal and yields a series that discards everything —
    /// the configuration for a channel whose sensor is not fitted on this hull.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            samples: Vec::new(),
            capacity,
            head: 0,
        }
    }

    /// Append a reading, evicting the oldest if the ring is full.
    pub fn push(&mut self, s: Sample) {
        if self.capacity == 0 {
            return;
        }
        if self.samples.len() < self.capacity {
            self.samples.push(s);
        } else {
            self.samples[self.head] = s;
            self.head = (self.head + 1) % self.capacity;
        }
    }

    /// How many samples the series currently holds.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The configured capacity, whether or not the ring has reached it.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Every sample in TIME order, oldest first.
    ///
    /// Once the ring has wrapped, arrival order and storage order differ: the
    /// oldest live sample sits at `head`, not at index 0. Callers walking a time
    /// window depend on this being sorted, so the wrap is resolved here rather
    /// than at each call site.
    pub fn iter(&self) -> impl Iterator<Item = &Sample> + '_ {
        let (a, b) = if self.samples.len() < self.capacity {
            (&self.samples[..], &self.samples[..0])
        } else {
            self.samples.split_at(self.head)
        };
        b.iter().chain(a.iter())
    }

    /// The time of the oldest live sample, if any.
    pub fn first_t_ms(&self) -> Option<u64> {
        self.iter().next().map(|s| s.t_ms)
    }

    /// The time of the newest live sample, if any.
    pub fn last_t_ms(&self) -> Option<u64> {
        self.iter().last().map(|s| s.t_ms)
    }

    /// Aggregate the readings in the window `[from, to]`, inclusive at BOTH ends.
    ///
    /// Lifted here from the channels, which had each hand-rolled it: a windowed
    /// aggregate is a property of a SERIES, not of what the readings happen to
    /// mean. Twelve copies also meant twelve chances to get the boundary wrong,
    /// and all twelve had: they tested `t_ms > from`, so a reading landing
    /// exactly on the shared edge of two adjacent dashboard panes was reported
    /// in neither.
    ///
    /// Walks in time order and stops at the first sample past `to`: the series
    /// is sorted, so nothing after it can still be in range.
    pub fn window_stats(&self, from: u64, to: u64) -> WindowStats {
        let mut count = 0usize;
        let mut min = f64::MAX;
        let mut max = f64::MIN;
        let mut sum = 0.0f64;
        for s in self.iter() {
            if s.t_ms > to {
                break;
            }
            if s.t_ms >= from {
                count += 1;
                if s.value < min {
                    min = s.value;
                }
                if s.value > max {
                    max = s.value;
                }
                sum += s.value;
            }
        }
        if count == 0 {
            return WindowStats::empty();
        }
        WindowStats {
            count,
            min,
            max,
            mean: sum / count as f64,
        }
    }
}
