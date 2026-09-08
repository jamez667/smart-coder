//! Rendering a duration for the run summary line.

#[path = "split.rs"]
pub mod split;

use split::{split, Parts};

/// Render `ms` as `H:MM:SS` for the summary table.
///
/// The minute and second fields are two digits so the columns line up; the hour
/// field is not padded, because a run can be longer than nine hours and a fixed
/// width would truncate it.
pub fn render(ms: u64) -> String {
    let p = split(ms);
    format!("{}:{}:{}", p.hours, pad(p.minutes), pad(p.seconds))
}

/// Render `ms` as a short human label, e.g. `2h 5m` or `45s`.
///
/// Only the two most significant non-zero units are shown: past an hour the
/// seconds are noise, and under a minute the label is just seconds.
pub fn label(ms: u64) -> String {
    let p = split(ms);
    if p.hours > 0 {
        format!("{}h {}m", p.hours, p.minutes)
    } else if p.minutes > 0 {
        format!("{}m {}s", p.minutes, p.seconds)
    } else {
        format!("{}s", p.seconds)
    }
}

/// Render a split that was assembled by hand rather than by [`split`], e.g. the
/// numbers a user typed into the "remind me in" box.
///
/// Hand-assembled parts are not reduced — somebody will type 90 minutes — so
/// this carries any overflow up before formatting. Nothing that comes from
/// [`split`] needs this: that function reduces its own fields.
pub fn render_unreduced(p: Parts) -> String {
    let p = carry(p);
    format!("{}:{}:{}", p.hours, pad(p.minutes), pad(p.seconds))
}

/// Carry any overflowed field up into the next unit.
fn carry(p: Parts) -> Parts {
    let mut p = p;
    if p.seconds >= 60 {
        p.minutes += p.seconds / 60;
        p.seconds %= 60;
    }
    if p.minutes >= 60 {
        p.hours += p.minutes / 60;
        p.minutes %= 60;
    }
    p
}

/// Left-pad a unit to the two-digit column width used by the summary table.
fn pad(n: u64) -> String {
    if n < 10 {
        format!("0{n}")
    } else {
        format!("{n}")
    }
}
