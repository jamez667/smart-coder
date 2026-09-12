// Contract test for the telemetry window aggregate. FROZEN: a solver must not
// modify this file.
use void_telemetry_task::series::Series;
use void_telemetry_task::{Sample, WindowStats};

use void_telemetry_task::chan_ambient::Ambient;
use void_telemetry_task::chan_comms::Comms;
use void_telemetry_task::chan_coolant::Coolant;
use void_telemetry_task::chan_cryo::Cryo;
use void_telemetry_task::chan_dock::Dock;
use void_telemetry_task::chan_gyro::Gyro;
use void_telemetry_task::chan_hull::Hull;
use void_telemetry_task::chan_optics::Optics;
use void_telemetry_task::chan_power::Power;
use void_telemetry_task::chan_reactor::Reactor;
use void_telemetry_task::chan_thruster::Thruster;
use void_telemetry_task::chan_vent::Vent;

/// Readings at 0, 250, 500, 750 and 1000 ms, so 0 and 1000 sit exactly on the
/// boundaries the dashboard's adjacent panes share.
fn boundary_samples() -> Vec<Sample> {
    vec![
        Sample::new(0, 10.0),
        Sample::new(250, 20.0),
        Sample::new(500, 30.0),
        Sample::new(750, 40.0),
        Sample::new(1000, 50.0),
    ]
}

// ---- the shared primitive -------------------------------------------------

#[test]
fn series_window_stats_includes_both_endpoints() {
    let mut s = Series::with_capacity(16);
    for x in boundary_samples() {
        s.push(x);
    }
    let w = s.window_stats(0, 1000);
    assert_eq!(w.count, 5, "a closed interval holds every sample");
    assert_eq!(w.min, 10.0);
    assert_eq!(w.max, 50.0);
    assert_eq!(w.mean, 30.0);
}

#[test]
fn series_window_stats_reports_a_sample_on_the_shared_boundary_in_both_panes() {
    // The dashboard draws [0,500] and [500,1000] side by side. The reading at
    // 500 belongs to both; reporting it in neither is how a sample vanishes.
    let mut s = Series::with_capacity(16);
    for x in boundary_samples() {
        s.push(x);
    }
    let left = s.window_stats(0, 500);
    let right = s.window_stats(500, 1000);
    assert_eq!(left.count, 3, "0, 250, 500");
    assert_eq!(right.count, 3, "500, 750, 1000");
    assert_eq!(left.max, 30.0, "the boundary sample is in the left pane");
    assert_eq!(right.min, 30.0, "and in the right pane");
}

#[test]
fn series_window_stats_is_empty_when_nothing_is_in_range() {
    let mut s = Series::with_capacity(16);
    for x in boundary_samples() {
        s.push(x);
    }
    assert_eq!(s.window_stats(2_000, 3_000), WindowStats::empty());
}

#[test]
fn series_window_stats_handles_a_single_sample_window() {
    let mut s = Series::with_capacity(16);
    for x in boundary_samples() {
        s.push(x);
    }
    let w = s.window_stats(250, 250);
    assert_eq!(w.count, 1);
    assert_eq!((w.min, w.max, w.mean), (20.0, 20.0, 20.0));
}

#[test]
fn series_window_stats_respects_the_ring_after_it_wraps() {
    // Capacity 3, five pushes: only the last three survive, and the window must
    // aggregate those in time order rather than storage order.
    let mut s = Series::with_capacity(3);
    for x in boundary_samples() {
        s.push(x);
    }
    let w = s.window_stats(0, 1000);
    assert_eq!(w.count, 3, "the ring holds three");
    assert_eq!(w.min, 30.0, "the oldest survivor is the 500ms sample");
    assert_eq!(w.max, 50.0);
}

// ---- the invariant across every channel -----------------------------------

/// Load the same boundary-straddling readings into every channel, then assert
/// each one reports the endpoint sample.
///
/// This is the test a single-site fix cannot pass. Every channel hand-rolled the
/// same walk, so every channel drops the sample at `from`; fixing only the
/// channel named by the first failure leaves the other eleven red here.
#[test]
fn every_channel_includes_the_window_start() {
    macro_rules! check {
        ($name:literal, $ty:ty) => {{
            let mut c = <$ty>::new();
            for x in boundary_samples() {
                c.record(x);
            }
            let w = c.window(0, 1000);
            assert_eq!(
                w.count, 5,
                "{} dropped the sample at the window start",
                $name
            );
            assert_eq!(w.min, 10.0, "{} lost the earliest reading", $name);

            // And the same across a shared boundary, both sides.
            let left = c.window(0, 500);
            let right = c.window(500, 1000);
            assert_eq!(left.count, 3, "{} left pane", $name);
            assert_eq!(right.count, 3, "{} right pane", $name);
        }};
    }

    check!("ambient", Ambient);
    check!("comms", Comms);
    check!("coolant", Coolant);
    check!("cryo", Cryo);
    check!("dock", Dock);
    check!("gyro", Gyro);
    check!("hull", Hull);
    check!("optics", Optics);
    check!("power", Power);
    check!("reactor", Reactor);
    check!("thruster", Thruster);
    check!("vent", Vent);
}

/// `peak` and `count_in` read the same window, so they inherit the same bug and
/// must be fixed by the same change rather than patched separately.
#[test]
fn every_channel_peaks_and_counts_over_the_closed_interval() {
    macro_rules! check {
        ($name:literal, $ty:ty) => {{
            let mut c = <$ty>::new();
            for x in boundary_samples() {
                c.record(x);
            }
            assert_eq!(c.count_in(0, 250), 2, "{} count_in", $name);
            assert_eq!(c.peak(0, 250), Some(20.0), "{} peak", $name);
            assert_eq!(c.peak(0, 0), Some(10.0), "{} single-point peak", $name);
        }};
    }

    check!("ambient", Ambient);
    check!("comms", Comms);
    check!("coolant", Coolant);
    check!("cryo", Cryo);
    check!("dock", Dock);
    check!("gyro", Gyro);
    check!("hull", Hull);
    check!("optics", Optics);
    check!("power", Power);
    check!("reactor", Reactor);
    check!("thruster", Thruster);
    check!("vent", Vent);
}

// ---- everything that already worked must keep working ---------------------

#[test]
fn an_empty_channel_reports_the_empty_window() {
    let c = Ambient::new();
    assert_eq!(c.window(0, 1000), WindowStats::empty());
    assert_eq!(c.peak(0, 1000), None);
    assert!(c.is_empty());
    assert_eq!(c.lifetime_mean(), None);
    assert_eq!(c.span_ms(), None);
}

#[test]
fn the_lifetime_mean_ignores_windows() {
    let mut c = Reactor::new();
    for x in boundary_samples() {
        c.record(x);
    }
    assert_eq!(c.lifetime_mean(), Some(30.0));
    assert_eq!(c.span_ms(), Some((0, 1000)));
    assert_eq!(c.len(), 5);
}

#[test]
fn a_window_past_the_last_sample_is_empty_not_saturated() {
    let mut c = Vent::new();
    for x in boundary_samples() {
        c.record(x);
    }
    assert_eq!(c.window(1_001, 5_000), WindowStats::empty());
    assert_eq!(c.row(1_001, 5_000), "vent -");
}

#[test]
fn the_spike_flag_survives_eviction_from_the_ring() {
    // Ambient flags a step over 1.5 degrees. Push the step, then push enough
    // quiet samples to evict it: the flag must remain set.
    let mut c = Ambient::new();
    c.record(Sample::new(0, 10.0));
    c.record(Sample::new(1, 20.0));
    assert!(c.flagged(), "premise: the step set the flag");
    for i in 0..200u64 {
        c.record(Sample::new(100 + i, 20.0));
    }
    assert!(c.flagged(), "the flag outlives the sample that set it");
}

#[test]
fn a_quiet_channel_never_flags() {
    let mut c = Hull::new();
    for i in 0..50u64 {
        c.record(Sample::new(i * 10, 100.0));
    }
    assert!(!c.flagged());
}

#[test]
fn the_row_renders_the_channel_unit_and_precision() {
    let mut c = Power::new();
    c.record(Sample::new(0, 10.0));
    c.record(Sample::new(500, 20.0));
    let row = c.row(0, 500);
    assert!(row.starts_with("power "), "got {row}");
    assert!(row.contains("n=2"), "got {row}");
    assert!(row.contains("amps"), "got {row}");
}
