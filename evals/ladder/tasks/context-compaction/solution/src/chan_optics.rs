//! Telemetry channel: optics (lux).
//!
//! Optics saturates rather than clipping, so a blinded sensor reports a
//! plausible number rather than an obvious error. The threshold is the
//! manufacturer saturation point, not a tuned value.
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
pub const CAPACITY: usize = 96;

/// The optics channel: a series, plus this sensor's interpretation of it.
#[derive(Debug, Clone)]
pub struct Optics {
    series: Series,
    /// Set once the channel has seen a spike, and never cleared. The dashboard
    /// distinguishes a channel that HAS misbehaved from one misbehaving now, and
    /// the flag must outlive the sample that set it.
    flagged: bool,
}

impl Optics {
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
            if cur > 40_000.0 && prev <= 40_000.0 {
                self.flagged = true;
            }
        }
        self.series.push(s);
    }

    /// Has this channel ever seen a sensor blinded by the docking ring?
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
    /// The walk itself belongs to `Series` -- every channel wanted the same one,
    /// and each keeping its own copy is what let the boundary drift.
    pub fn window(&self, from: u64, to: u64) -> WindowStats {
        self.series.window_stats(from, to)
    }

    /// The dashboard row for this channel over `[from, to]`.
    ///
    /// Rendered here rather than by the dashboard because the unit and the
    /// precision belong to the sensor: lux at one decimal is what the optics
    /// gauge has always shown, and matching it is the point of the row.
    pub fn row(&self, from: u64, to: u64) -> String {
        let w = self.window(from, to);
        if w.count == 0 {
            return String::from("optics -");
        }
        format!(
            "optics n={} min={:.1} max={:.1} mean={:.1} lux{}",
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

impl Default for Optics {
    fn default() -> Self {
        Self::new()
    }
}
