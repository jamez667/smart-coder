// Contract test for the duration formatter. FROZEN: a solver must not modify
// this file.
#[path = "format.rs"]
mod format;

use format::split::{split, Parts};
use format::{label, render, render_unreduced};

// --- the reported symptom: the summary line ---

#[test]
fn a_summary_line_past_an_hour_shows_the_minutes_within_that_hour() {
    // 1h 25m 30s. The minute field is minutes-within-the-hour, not total minutes.
    assert_eq!(render(5_130_000), "1:25:30");
}

#[test]
fn a_summary_line_past_two_hours_shows_the_minutes_within_that_hour() {
    // 2h 45m 09s.
    assert_eq!(render(9_909_000), "2:45:09");
}

#[test]
fn a_summary_line_under_an_hour_is_unaffected() {
    // 0h 42m 07s.
    assert_eq!(render(2_527_000), "0:42:07");
}

// --- the same defect, seen through the short label ---

#[test]
fn a_short_label_past_an_hour_shows_the_minutes_within_that_hour() {
    assert_eq!(label(5_130_000), "1h 25m");
}

// --- the invariant the split itself must hold ---

#[test]
fn a_split_never_lets_a_field_overflow_its_unit() {
    for ms in [0u64, 999, 59_000, 60_000, 3_599_000, 3_600_000, 5_130_000, 86_399_000] {
        let p = split(ms);
        assert!(p.minutes < 60, "minutes overflowed for {ms}ms: {p:?}");
        assert!(p.seconds < 60, "seconds overflowed for {ms}ms: {p:?}");
    }
}

#[test]
fn a_split_still_totals_back_to_the_original() {
    for ms in [0u64, 59_000, 3_600_000, 5_130_000, 9_909_000] {
        let Parts {
            hours,
            minutes,
            seconds,
        } = split(ms);
        assert_eq!(
            hours * 3_600_000 + minutes * 60_000 + seconds * 1_000,
            ms / 1_000 * 1_000,
            "parts do not total back for {ms}ms"
        );
    }
}

// --- everything that already worked must keep working ---

#[test]
fn a_short_duration_is_reported_in_seconds_alone() {
    assert_eq!(label(45_000), "45s");
    assert_eq!(label(0), "0s");
}

#[test]
fn a_summary_line_pads_the_minute_and_second_columns() {
    assert_eq!(render(65_000), "0:01:05");
}

#[test]
fn hand_assembled_parts_are_carried_up_before_rendering() {
    // The "remind me in 90 minutes" path: these parts never went through
    // `split`, so the render path reduces them itself.
    let typed = Parts {
        hours: 0,
        minutes: 90,
        seconds: 75,
    };
    assert_eq!(render_unreduced(typed), "1:31:15");
}
