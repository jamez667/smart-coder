//! A slice of the void-claim telemetry subsystem.
//!
//! `series` is the shared ring buffer every sensor channel writes into. Each
//! `chan_*` module owns one channel: it decides what a sample means, what counts
//! as a spike for that channel, and how the dashboard should label it.
//!
//! The channels are independent by design -- a pressure sensor and a thermal
//! sensor have nothing to say to each other -- so each one grew its own copy of
//! the windowed-aggregate walk over the series it owns.

pub mod series;

pub mod chan_ambient;
pub mod chan_comms;
pub mod chan_coolant;
pub mod chan_cryo;
pub mod chan_dock;
pub mod chan_gyro;
pub mod chan_hull;
pub mod chan_optics;
pub mod chan_power;
pub mod chan_reactor;
pub mod chan_thruster;
pub mod chan_vent;

/// One reading, as the sensor bus delivers it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Milliseconds since the run began.
    pub t_ms: u64,
    pub value: f64,
}

impl Sample {
    pub const fn new(t_ms: u64, value: f64) -> Self {
        Self { t_ms, value }
    }
}

/// What a channel reports for one window of its own series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowStats {
    /// How many samples fell inside the window.
    pub count: usize,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
}

impl WindowStats {
    /// The empty window. A channel with no samples in range reports this rather
    /// than `None`, so the dashboard always has a row to draw.
    pub fn empty() -> Self {
        Self {
            count: 0,
            min: 0.0,
            max: 0.0,
            mean: 0.0,
        }
    }
}
