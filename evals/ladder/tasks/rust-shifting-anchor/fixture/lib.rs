//! Shipping quotes, one function per zone.
//!
//! Every zone prices the same way — a flat handling fee, then a per-kilo rate up
//! to the zone's break weight, then a heavier rate above it, then a surcharge if
//! the parcel is over the zone's oversize limit. Only the numbers differ, so the
//! five bodies are deliberately identical in shape: a reviewer comparing two
//! zones should be reading constants, not control flow.

/// A quote in pence.
pub type Pence = u64;

pub fn quote_zone_a(grams: u64, longest_cm: u64) -> Pence {
    let handling = 250;
    let break_grams = 2_000;
    let light_rate = 40;
    let heavy_rate = 65;
    let oversize_cm = 60;
    let oversize_fee = 500;

    let light = grams.min(break_grams);
    let heavy = grams.saturating_sub(break_grams);
    let mut total = handling;
    total += light * light_rate / 1_000;
    total += heavy * heavy_rate / 1_000;
    if longest_cm > oversize_cm {
        total += oversize_fee;
    }
    total
}

pub fn quote_zone_b(grams: u64, longest_cm: u64) -> Pence {
    let handling = 300;
    let break_grams = 2_000;
    let light_rate = 55;
    let heavy_rate = 90;
    let oversize_cm = 60;
    let oversize_fee = 750;

    let light = grams.min(break_grams);
    let heavy = grams.saturating_sub(break_grams);
    let mut total = handling;
    total += light * light_rate / 1_000;
    total += heavy * heavy_rate / 1_000;
    if longest_cm > oversize_cm {
        total += oversize_fee;
    }
    total
}

pub fn quote_zone_c(grams: u64, longest_cm: u64) -> Pence {
    let handling = 400;
    let break_grams = 1_500;
    let light_rate = 70;
    let heavy_rate = 120;
    let oversize_cm = 50;
    let oversize_fee = 900;

    let light = grams.min(break_grams);
    let heavy = grams.saturating_sub(break_grams);
    let mut total = handling;
    total += light * light_rate / 1_000;
    total += heavy * heavy_rate / 1_000;
    if longest_cm >= oversize_cm {
        total += oversize_fee;
    }
    total
}

pub fn quote_zone_d(grams: u64, longest_cm: u64) -> Pence {
    let handling = 550;
    let break_grams = 1_500;
    let light_rate = 95;
    let heavy_rate = 160;
    let oversize_cm = 50;
    let oversize_fee = 1_200;

    let light = grams.min(break_grams);
    let heavy = grams.saturating_sub(break_grams);
    let mut total = handling;
    total += light * light_rate / 1_000;
    total += heavy * heavy_rate / 1_000;
    if longest_cm > oversize_cm {
        total += oversize_fee;
    }
    total
}

pub fn quote_zone_e(grams: u64, longest_cm: u64) -> Pence {
    let handling = 800;
    let break_grams = 1_000;
    let light_rate = 130;
    let heavy_rate = 220;
    let oversize_cm = 40;
    let oversize_fee = 1_800;

    let light = grams.min(break_grams);
    let heavy = grams.saturating_sub(break_grams);
    let mut total = handling;
    total += light * light_rate / 1_000;
    total += heavy * heavy_rate / 1_000;
    if longest_cm > oversize_cm {
        total += oversize_fee;
    }
    total
}
