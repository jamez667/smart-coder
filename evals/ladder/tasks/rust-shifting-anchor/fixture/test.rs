// Contract test for the shipping quotes. FROZEN: a solver must not modify this
// file.
#[path = "lib.rs"]
mod lib;

use lib::*;

// --- the reported symptom ---

#[test]
fn a_parcel_exactly_at_the_zone_c_limit_is_not_oversize() {
    // The oversize limit is the largest size that still ships as a normal
    // parcel: a parcel AT the limit is inside it. Zone C charges the surcharge
    // anyway.
    let at_limit = quote_zone_c(1_000, 50);
    let under = quote_zone_c(1_000, 49);
    assert_eq!(
        at_limit, under,
        "a parcel at the zone C limit was surcharged: {at_limit} vs {under}"
    );
}

#[test]
fn a_parcel_over_the_zone_c_limit_is_still_oversize() {
    assert_eq!(quote_zone_c(1_000, 51) - quote_zone_c(1_000, 50), 900);
}

// --- the invariant across every zone ---

#[test]
fn every_zone_treats_its_limit_the_same_way() {
    // Same rule everywhere: at the limit is inside, one over is outside.
    let zones: [(fn(u64, u64) -> Pence, u64, u64); 5] = [
        (quote_zone_a, 60, 500),
        (quote_zone_b, 60, 750),
        (quote_zone_c, 50, 900),
        (quote_zone_d, 50, 1_200),
        (quote_zone_e, 40, 1_800),
    ];
    for (i, (q, limit, fee)) in zones.iter().enumerate() {
        assert_eq!(
            q(1_000, *limit),
            q(1_000, limit - 1),
            "zone {i} surcharges a parcel at its limit"
        );
        assert_eq!(
            q(1_000, limit + 1) - q(1_000, *limit),
            *fee,
            "zone {i} does not surcharge a parcel over its limit"
        );
    }
}

// --- everything that already worked must keep working ---

#[test]
fn the_zone_rates_are_unchanged() {
    // A 3kg parcel, well under every oversize limit.
    assert_eq!(quote_zone_a(3_000, 10), 250 + 80 + 65);
    assert_eq!(quote_zone_b(3_000, 10), 300 + 110 + 90);
    assert_eq!(quote_zone_c(3_000, 10), 400 + 105 + 180);
    assert_eq!(quote_zone_d(3_000, 10), 550 + 142 + 240);
    assert_eq!(quote_zone_e(3_000, 10), 800 + 130 + 440);
}

#[test]
fn a_zero_weight_parcel_pays_only_handling() {
    assert_eq!(quote_zone_a(0, 10), 250);
    assert_eq!(quote_zone_c(0, 10), 400);
    assert_eq!(quote_zone_e(0, 10), 800);
}
