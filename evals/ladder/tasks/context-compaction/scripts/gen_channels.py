"""Generate the telemetry channel modules for the context-compaction rung.

SIZING, measured rather than guessed. A generated 180-line channel module costs
~2,250 tokens as a numbered `read_file` observation; `lib.rs` + `series.rs`
together cost ~1,780. The prompt's fitting ceiling on tiel is 25,966 tokens
(budget 26,419 less the 453-token six-tool schema), so:

    8 channels  = 19,779  (76% of fit)   -- never compacts
   11 channels  = 26,529  (102% of fit)  -- first count that saturates
   12 channels  = 28,779  (111% of fit)  -- chosen, for margin

Twelve, because a model does not read in a tidy order: it interleaves
`run_verification` and re-reads, and the rung must still cross the ceiling.

Every channel hand-rolls the SAME windowed walk carrying the SAME boundary
defect: `s.t_ms > from` where the doc comment promises a closed interval, so a
sample landing exactly on a window's start is reported in neither of two
adjacent panes. All twelve are wrong on purpose -- a model that fixes only the
module named by the first failing test leaves eleven sites red, and the contract
test's cross-channel invariant is what catches that.

Every spike expression uses BOTH `prev` and `cur`. That is not decoration: a
one-sample test leaves `prev` unbound and the fixture ships with compiler
warnings, and uniform shape is what makes the duplication legible ("a reviewer
comparing two channels should be reading constants, not control flow").

Written as a file rather than a shell heredoc: the bodies contain apostrophes
and Rust lifetimes (`+ '_`), which the shell mangles.
"""

import os

BASE = os.path.join(
    "evals", "ladder", "tasks", "context-compaction", "fixture", "src"
)

# (module, Type, unit, capacity, spike_desc, spike_expr, module_note)
#
# spike_desc must be short: it lands in a `/// Has this channel ever seen ...?`
# line that has to stay inside 95 characters.
CHANNELS = [
    (
        "ambient", "Ambient", "degrees C", 64,
        "a cabin temperature step life support cannot answer",
        "(cur - prev).abs() > 1.5",
        "Ambient runs slowest of the twelve: the bus posts one reading a second and\n"
        "//! the loop reacts over tens of seconds, so a step here is a real event\n"
        "//! rather than sensor chatter.",
    ),
    (
        "coolant", "Coolant", "litres/min", 128,
        "a flow rate collapsing below the pump floor",
        "cur < 4.0 && prev >= 4.0",
        "Coolant is edge-triggered on purpose. A pump sitting below the floor for a\n"
        "//! minute is one fault, not sixty, so the test compares against the previous\n"
        "//! sample rather than judging the current one alone.",
    ),
    (
        "hull", "Hull", "microstrain", 256,
        "a strain excursion the structural log needs",
        "cur.abs() > 900.0 && prev.abs() <= 900.0",
        "Hull is the noisiest channel on the bus and the only one whose readings are\n"
        "//! signed: compression is negative strain, and a large excursion either way\n"
        "//! is what the structural log wants -- but only its leading edge.",
    ),
    (
        "optics", "Optics", "lux", 96,
        "a sensor blinded by the docking ring",
        "cur > 40_000.0 && prev <= 40_000.0",
        "Optics saturates rather than clipping, so a blinded sensor reports a\n"
        "//! plausible number rather than an obvious error. The threshold is the\n"
        "//! manufacturer saturation point, not a tuned value.",
    ),
    (
        "power", "Power", "amps", 192,
        "a draw that would trip the breaker if it held",
        "cur > 55.0 && prev <= 55.0",
        "Power is sampled fastest of the twelve. The breaker trips on a sustained\n"
        "//! overdraw, so the crossing is what gets flagged: one sample above the\n"
        "//! threshold is worth seeing but is not on its own a fault.",
    ),
    (
        "reactor", "Reactor", "megawatts", 160,
        "an output step the governor did not command",
        "(cur - prev).abs() > 8.0",
        "Reactor output is commanded, so an UNCOMMANDED step is the interesting\n"
        "//! event. The governor set-point changes arrive on another channel; this\n"
        "//! one sees only what the plant actually did.",
    ),
    (
        "thruster", "Thruster", "kilonewtons", 112,
        "thrust inconsistent with the commanded burn",
        "cur.abs() > 220.0 && prev.abs() <= 220.0",
        "Thrusters fire in both directions, so the check is symmetric. A reading\n"
        "//! beyond the envelope in either sense means the gimbal and the load cell\n"
        "//! disagree about what happened.",
    ),
    (
        "vent", "Vent", "kilopascals", 80,
        "a pressure drop suggesting a seal has let go",
        "prev - cur > 12.0",
        "Vent is the only channel where the SIGN of the change matters: a fast rise\n"
        "//! is the compressor doing its job, a fast fall is a leak.",
    ),
    (
        "gyro", "Gyro", "degrees/s", 144,
        "a rate step no commanded attitude change explains",
        "(cur - prev).abs() > 25.0",
        "Gyro readings are differentiated once already, so a STEP in rate is a\n"
        "//! second derivative: the hull was pushed by something the attitude\n"
        "//! controller did not ask for.",
    ),
    (
        "cryo", "Cryo", "kelvin", 224,
        "a boil-off rate the tank cannot sustain",
        "cur - prev > 0.8",
        "Cryo warms monotonically between top-ups, so only a RISE is interesting and\n"
        "//! the check is deliberately asymmetric. A fall is the pump running, which\n"
        "//! is the tank working as designed.",
    ),
    (
        "comms", "Comms", "decibel-milliwatts", 72,
        "a link margin falling toward the noise floor",
        "prev - cur > 6.0",
        "Comms is measured in dBm, so the numbers are negative and a DROP is a loss\n"
        "//! of margin. Six dB is one halving of received power, which is the point\n"
        "//! the link budget starts to matter.",
    ),
    (
        "dock", "Dock", "millimetres", 48,
        "a clamp drifting out of alignment tolerance",
        "(cur - prev).abs() > 3.0",
        "Dock is the shallowest channel: the clamp is only instrumented while a ship\n"
        "//! is berthed, so the series covers minutes rather than the half-hour the\n"
        "//! others hold.",
    ),
]

TEMPLATE = '''//! Telemetry channel: {name} ({unit}).
//!
//! {note}
//!
//! The channel owns its series, decides what a spike means for this sensor, and
//! renders its own dashboard row. Nothing here is shared with the other eleven
//! by design: a pressure sensor and a thermal sensor have nothing to say to each
//! other, and the shapes their readings take are genuinely different.

use crate::series::Series;
use crate::{{Sample, WindowStats}};

/// How many samples this channel retains.
///
/// Sized per sensor: a fast channel needs more depth to cover the same wall time
/// as a slow one, and the dashboard longest pane is thirty seconds.
pub const CAPACITY: usize = {cap};

/// The {name} channel: a series, plus this sensor's interpretation of it.
#[derive(Debug, Clone)]
pub struct {ty} {{
    series: Series,
    /// Set once the channel has seen a spike, and never cleared. The dashboard
    /// distinguishes a channel that HAS misbehaved from one misbehaving now, and
    /// the flag must outlive the sample that set it.
    flagged: bool,
}}

impl {ty} {{
    /// A channel with an empty series at this sensor's configured depth.
    pub fn new() -> Self {{
        Self {{
            series: Series::with_capacity(CAPACITY),
            flagged: false,
        }}
    }}

    /// Record one reading from the bus.
    ///
    /// Spike detection happens on the way in rather than during a window query:
    /// the flag has to survive the sample that set it being evicted from the
    /// ring, which is why it is a field and not a computed property.
    pub fn record(&mut self, s: Sample) {{
        if let Some(prev) = self.series.iter().last().map(|p| p.value) {{
            let cur = s.value;
            if {spike} {{
                self.flagged = true;
            }}
        }}
        self.series.push(s);
    }}

    /// Has this channel ever seen {desc}?
    pub fn flagged(&self) -> bool {{
        self.flagged
    }}

    /// The samples currently retained, oldest first.
    pub fn samples(&self) -> impl Iterator<Item = &Sample> + '_ {{
        self.series.iter()
    }}

    /// How many samples are retained.
    pub fn len(&self) -> usize {{
        self.series.len()
    }}

    pub fn is_empty(&self) -> bool {{
        self.series.is_empty()
    }}

    /// Aggregate the readings in the window `[from, to]`, inclusive at BOTH ends.
    ///
    /// A window is a closed interval. The dashboard draws `[0, 1000]` and
    /// `[1000, 2000]` as adjacent panes, and a reading landing exactly on the
    /// shared boundary belongs to both of them. Reporting it in neither is how a
    /// sample vanishes from a run.
    ///
    /// Walks in time order and stops at the first sample past `to`: the series is
    /// sorted, so there is nothing after it that could still be in range.
    pub fn window(&self, from: u64, to: u64) -> WindowStats {{
        let mut count = 0usize;
        let mut min = f64::MAX;
        let mut max = f64::MIN;
        let mut sum = 0.0f64;
        for s in self.series.iter() {{
            if s.t_ms > to {{
                break;
            }}
            if s.t_ms > from {{
                count += 1;
                if s.value < min {{
                    min = s.value;
                }}
                if s.value > max {{
                    max = s.value;
                }}
                sum += s.value;
            }}
        }}
        if count == 0 {{
            return WindowStats::empty();
        }}
        WindowStats {{
            count,
            min,
            max,
            mean: sum / count as f64,
        }}
    }}

    /// The dashboard row for this channel over `[from, to]`.
    ///
    /// Rendered here rather than by the dashboard because the unit and the
    /// precision belong to the sensor: {unit} at one decimal is what the {name}
    /// gauge has always shown, and matching it is the point of the row.
    pub fn row(&self, from: u64, to: u64) -> String {{
        let w = self.window(from, to);
        if w.count == 0 {{
            return String::from("{name} -");
        }}
        format!(
            "{name} n={{}} min={{:.1}} max={{:.1}} mean={{:.1}} {unit}{{}}",
            w.count,
            w.min,
            w.max,
            w.mean,
            if self.flagged {{ " !" }} else {{ "" }}
        )
    }}

    /// The peak reading in the window, or `None` when the window is empty.
    ///
    /// Separate from `window` because the alert strip wants the peak without
    /// paying for the rest of the aggregate.
    pub fn peak(&self, from: u64, to: u64) -> Option<f64> {{
        let w = self.window(from, to);
        if w.count == 0 {{
            None
        }} else {{
            Some(w.max)
        }}
    }}

    /// How many readings fall in the window. The alert strip counts before it
    /// decides whether a pane is worth drawing at all.
    pub fn count_in(&self, from: u64, to: u64) -> usize {{
        self.window(from, to).count
    }}

    /// Mean over the whole retained history, ignoring windows entirely.
    pub fn lifetime_mean(&self) -> Option<f64> {{
        if self.series.is_empty() {{
            return None;
        }}
        let mut sum = 0.0;
        let mut n = 0usize;
        for s in self.series.iter() {{
            sum += s.value;
            n += 1;
        }}
        Some(sum / n as f64)
    }}

    /// The span the retained samples cover, as `(first, last)` in ms.
    pub fn span_ms(&self) -> Option<(u64, u64)> {{
        match (self.series.first_t_ms(), self.series.last_t_ms()) {{
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        }}
    }}
}}

impl Default for {ty} {{
    fn default() -> Self {{
        Self::new()
    }}
}}
'''

LIB = '''//! A slice of the void-claim telemetry subsystem.
//!
//! `series` is the shared ring buffer every sensor channel writes into. Each
//! `chan_*` module owns one channel: it decides what a sample means, what counts
//! as a spike for that channel, and how the dashboard should label it.
//!
//! The channels are independent by design -- a pressure sensor and a thermal
//! sensor have nothing to say to each other -- so each one grew its own copy of
//! the windowed-aggregate walk over the series it owns.

pub mod series;

{mods}

/// One reading, as the sensor bus delivers it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {{
    /// Milliseconds since the run began.
    pub t_ms: u64,
    pub value: f64,
}}

impl Sample {{
    pub const fn new(t_ms: u64, value: f64) -> Self {{
        Self {{ t_ms, value }}
    }}
}}

/// What a channel reports for one window of its own series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowStats {{
    /// How many samples fell inside the window.
    pub count: usize,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
}}

impl WindowStats {{
    /// The empty window. A channel with no samples in range reports this rather
    /// than `None`, so the dashboard always has a row to draw.
    pub fn empty() -> Self {{
        Self {{
            count: 0,
            min: 0.0,
            max: 0.0,
            mean: 0.0,
        }}
    }}
}}
'''


def main() -> None:
    os.makedirs(BASE, exist_ok=True)
    total = 0
    for (name, ty, unit, cap, desc, spike, note) in CHANNELS:
        body = TEMPLATE.format(
            name=name, ty=ty, unit=unit, cap=cap, desc=desc, spike=spike, note=note
        )
        path = os.path.join(BASE, "chan_%s.rs" % name)
        with open(path, "w", encoding="utf-8", newline="\n") as fh:
            fh.write(body)
        n = body.count("\n")
        total += n
        longest = max(len(l) for l in body.split("\n"))
        flag = "  <== OVER 95" if longest > 95 else ""
        print("  %-20s %3d lines  longest %3d%s" % (
            "chan_%s.rs" % name, n, longest, flag))

    # Alphabetical: rustfmt reorders `mod` declarations, and a fixture that
    # arrives needing `cargo fmt` is sloppy content shipped into an eval.
    mods = "\n".join(
        "pub mod chan_%s;" % name for name in sorted(c[0] for c in CHANNELS)
    )
    with open(os.path.join(BASE, "lib.rs"), "w", encoding="utf-8", newline="\n") as fh:
        fh.write(LIB.format(mods=mods))
    print("  %-20s %3d lines" % ("lib.rs", LIB.format(mods=mods).count("\n")))
    print("  %-20s %3d lines across %d channels" % ("TOTAL", total, len(CHANNELS)))


if __name__ == "__main__":
    main()
