//! Telemetry channel: coolant (litres/min).
//!
//! Coolant is edge-triggered on purpose. A pump sitting below the floor for a
//! minute is one fault, not sixty, so the test compares against the previous
//! sample rather than judging the current one alone.
//!
//! The channel owns its series, decides what a spike means for this sensor, and
//! renders its own dashboard row. Nothing here is shared with the other eleven
//! by design: a pressure sensor and a thermal sensor have nothing to say to each
//! other, and the shapes their readings take are genuinely different.

use crate::series::Series;
use crate::{Sample, WindowStats};

/// How many samples this channel retains.
///
/// Sized per sensor: a fast channel needs more depth to cover the same wall time
/// as a slow one, and the dashboard longest pane is thirty seconds.
pub const CAPACITY: usize = 128;

/// The coolant channel: a series, plus this sensor's interpretation of it.
#[derive(Debug, Clone)]
pub struct Coolant {
    series: Series,
    /// Set once the channel has seen a spike, and never cleared. The dashboard
    /// distinguishes a channel that HAS misbehaved from one misbehaving now, and
    /// the flag must outlive the sample that set it.
    flagged: bool,
}

impl Coolant {
    /// A channel with an empty series at this sensor's configured depth.
    pub fn new() -> Self {
        Self {
            series: Series::with_capacity(CAPACITY),
            flagged: false,
        }
    }

    /// Record one reading from the bus.
    ///
    /// Spike detection happens on the way in rather than during a window query:
    /// the flag has to survive the sample that set it being evicted from the
    /// ring, which is why it is a field and not a computed property.
    pub fn record(&mut self, s: Sample) {
        if let Some(prev) = self.series.iter().last().map(|p| p.value) {
            let cur = s.value;
            if cur < 4.0 && prev >= 4.0 {
                self.flagged = true;
            }
        }
        self.series.push(s);
    }

    /// Has this channel ever seen a flow rate collapsing below the pump floor?
    pub fn flagged(&self) -> bool {
        self.flagged
    }

    /// The samples currently retained, oldest first.
    pub fn samples(&self) -> impl Iterator<Item = &Sample> + '_ {
        self.series.iter()
    }

    /// How many samples are retained.
    pub fn len(&self) -> usize {
        self.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    /// Aggregate the readings in the window `[from, to]`, inclusive at BOTH ends.
    ///
    /// A window is a closed interval. The dashboard draws `[0, 1000]` and
    /// `[1000, 2000]` as adjacent panes, and a reading landing exactly on the
    /// shared boundary belongs to both of them. Reporting it in neither is how a
    /// sample vanishes from a run.
    ///
    /// Walks in time order and stops at the first sample past `to`: the series is
    /// sorted, so there is nothing after it that could still be in range.
    pub fn window(&self, from: u64, to: u64) -> WindowStats {
        let mut count = 0usize;
        let mut min = f64::MAX;
        let mut max = f64::MIN;
        let mut sum = 0.0f64;
        for s in self.series.iter() {
            if s.t_ms > to {
                break;
            }
            if s.t_ms > from {
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

    /// The dashboard row for this channel over `[from, to]`.
    ///
    /// Rendered here rather than by the dashboard because the unit and the
    /// precision belong to the sensor: litres/min at one decimal is what the coolant
    /// gauge has always shown, and matching it is the point of the row.
    pub fn row(&self, from: u64, to: u64) -> String {
        let w = self.window(from, to);
        if w.count == 0 {
            return String::from("coolant -");
        }
        format!(
            "coolant n={} min={:.1} max={:.1} mean={:.1} litres/min{}",
            w.count,
            w.min,
            w.max,
            w.mean,
            if self.flagged { " !" } else { "" }
        )
    }

    /// The peak reading in the window, or `None` when the window is empty.
    ///
    /// Separate from `window` because the alert strip wants the peak without
    /// paying for the rest of the aggregate.
    pub fn peak(&self, from: u64, to: u64) -> Option<f64> {
        let w = self.window(from, to);
        if w.count == 0 {
            None
        } else {
            Some(w.max)
        }
    }

    /// How many readings fall in the window. The alert strip counts before it
    /// decides whether a pane is worth drawing at all.
    pub fn count_in(&self, from: u64, to: u64) -> usize {
        self.window(from, to).count
    }

    /// Mean over the whole retained history, ignoring windows entirely.
    pub fn lifetime_mean(&self) -> Option<f64> {
        if self.series.is_empty() {
            return None;
        }
        let mut sum = 0.0;
        let mut n = 0usize;
        for s in self.series.iter() {
            sum += s.value;
            n += 1;
        }
        Some(sum / n as f64)
    }

    /// The span the retained samples cover, as `(first, last)` in ms.
    pub fn span_ms(&self) -> Option<(u64, u64)> {
        match (self.series.first_t_ms(), self.series.last_t_ms()) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        }
    }
}

impl Default for Coolant {
    fn default() -> Self {
        Self::new()
    }
}
