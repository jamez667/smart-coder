//! Structured logs, as one JSON object per line on stdout.
//!
//! ## Why not `tracing`
//!
//! Because [`Cargo.toml`](../Cargo.toml) means what it says: `sc-proto` is the
//! only workspace dependency, so a reader can audit what is inside this
//! container by reading a short list. `serde_json` is already here, a log line
//! is a flat map of scalars, and the whole implementation is one `println!` at
//! the end of a builder. Adding the `tracing` ecosystem — subscriber, filter,
//! layer, appender — would be the largest dependency tree in the crate, in
//! service of a server that emits tens of lines an hour.
//!
//! It also buys nothing here. `tracing` earns its keep with spans across async
//! tasks; this server is synchronous and thread-per-request, so a line already
//! corresponds to exactly one thing that happened.
//!
//! ## Why NDJSON
//!
//! The log's destination is a log aggregator (Loki, via whatever scrapes Docker
//! on the host). One JSON object per line is what those parse natively, and it
//! turns "grep the container log and hope" into a query with named fields:
//! `| json | msg="signup refused"` instead of matching an interpolated
//! sentence that changes the next time somebody rewords it.
//!
//! That is also why [`Line::msg`] is a `&'static str` and variable content goes
//! in *fields*. A message built with `format!` is a message nobody can query
//! for.
//!
//! ## Everything goes to stdout
//!
//! Including errors. Docker captures both streams and hands them to the same
//! scraper, so splitting by severity produces two interleavings that have to be
//! merged back — and merged on a timestamp, which is exactly what the `level`
//! field already tells you without the merge. One stream, one ordering.
//!
//! ## Three levels, and no setting to filter them
//!
//! `info`, `warn`, `error`. A filter would need an environment variable, an
//! entry in the Portainer stack file, a row in the drift test that keeps those
//! two honest, and a paragraph of spec — to suppress lines from a server whose
//! entire output is a startup banner and one line per request. If volume ever
//! becomes the problem, the honest fix is to stop logging something specific,
//! not to add a dial that hides it.

use crate::store::now_ms;

/// The service every line is stamped with.
///
/// **Query on this, not on the container name.** The scraper labels lines with
/// the Docker container name, and under Swarm that name carries a task id that
/// changes on every redeploy — so a dashboard pinned to it silently stops
/// matching the next time this deploys. This field does not move.
pub const SVC: &str = "sc-server";

/// How severe one line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// Something that happened, being assembled. Nothing is written until [`Line::emit`].
#[must_use = "a log line does nothing until `emit` is called"]
pub struct Line {
    level: Level,
    msg: &'static str,
    /// Insertion-ordered, so a line reads the way it was written rather than
    /// however a map decided to hash it.
    fields: Vec<(&'static str, serde_json::Value)>,
}

/// Something worth knowing.
pub fn info(msg: &'static str) -> Line {
    Line::new(Level::Info, msg)
}

/// Something an operator should look at, that is not a failure.
pub fn warn(msg: &'static str) -> Line {
    Line::new(Level::Warn, msg)
}

/// Something failed.
pub fn error(msg: &'static str) -> Line {
    Line::new(Level::Error, msg)
}

impl Line {
    fn new(level: Level, msg: &'static str) -> Line {
        Line {
            level,
            msg,
            fields: Vec::new(),
        }
    }

    /// Attach a field.
    ///
    /// The key is `&'static str` so a field *name* can never be interpolated
    /// into existence. A log worth querying needs a closed set of keys as much
    /// as it needs a closed set of messages: one line writing `user_id` and
    /// another writing `userId` is a query that silently misses half its data.
    pub fn with(mut self, key: &'static str, value: impl Into<serde_json::Value>) -> Line {
        self.fields.push((key, value.into()));
        self
    }

    /// Attach something rendered with `Display` — for errors, which are the one
    /// thing whose text genuinely varies.
    pub fn text(self, key: &'static str, value: impl std::fmt::Display) -> Line {
        self.with(key, value.to_string())
    }

    /// Tie this line to a request, so an access line and the error it produced
    /// can be found together.
    pub fn req(self, id: &str) -> Line {
        self.with("req", id.to_string())
    }

    /// Write it.
    pub fn emit(self) {
        // One `println!` of one finished string. `println!` takes the stdout
        // lock for the duration of the call, so a complete line cannot interleave
        // with another thread's — which is the whole of this module's thread
        // safety, and why there is no mutex or global state anywhere in it.
        println!("{}", self.render(now_ms()));
    }

    /// The line as it will be written. Split out so tests can read it without
    /// capturing stdout.
    fn render(&self, at_ms: u64) -> String {
        let mut out = String::with_capacity(128);
        out.push('{');
        push_pair(&mut out, "ts", &rfc3339_ms(at_ms).into());
        out.push(',');
        push_pair(&mut out, "level", &self.level.as_str().into());
        out.push(',');
        push_pair(&mut out, "svc", &SVC.into());
        out.push(',');
        push_pair(&mut out, "msg", &self.msg.into());
        for (key, value) in &self.fields {
            out.push(',');
            push_pair(&mut out, key, value);
        }
        out.push('}');
        out
    }
}

/// Append `"key":value`, both JSON-encoded.
///
/// `serde_json` escapes control characters, so a value carrying a newline — very
/// reachable, since [`Line::text`] renders arbitrary errors — is escaped rather
/// than splitting the record in two. NDJSON's entire contract is one line, one
/// record.
fn push_pair(out: &mut String, key: &str, value: &serde_json::Value) {
    // Both arms fall back rather than dropping the pair: a log that discards its
    // own content on an encoding edge case is worse than an ugly line.
    match serde_json::to_string(key) {
        Ok(k) => out.push_str(&k),
        Err(_) => out.push_str("\"?\""),
    }
    out.push(':');
    match serde_json::to_string(value) {
        Ok(v) => out.push_str(&v),
        Err(_) => out.push_str("null"),
    }
}

/// Unix milliseconds as RFC 3339, e.g. `2026-08-02T14:03:12.001Z`.
///
/// Hand-rolled because there is no date crate in this dependency list and one
/// timestamp does not justify adding one. The scraper stamps its own arrival
/// time, but that is when the line was *read*: a restart burst, or a scraper
/// that fell behind, arrives out of order and only this field can put it back.
pub(crate) fn rfc3339_ms(epoch_ms: u64) -> String {
    let secs = epoch_ms / 1000;
    let ms = epoch_ms % 1000;
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;

    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

/// Days since the Unix epoch to a civil (year, month, day).
///
/// Howard Hinnant's `civil_from_days`, which shifts the epoch to March 1st so
/// the leap day lands at the end of a year and the month-length pattern becomes
/// arithmetic rather than a table.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> serde_json::Value {
        serde_json::from_str(line).expect("every line must be one JSON object")
    }

    #[test]
    fn a_line_is_one_json_object_with_the_standard_fields() {
        let rendered = info("listening")
            .with("addr", "0.0.0.0:8420")
            .render(1_785_644_540_502);

        let v = parse(&rendered);
        assert_eq!(v["level"], "info");
        assert_eq!(v["svc"], SVC);
        assert_eq!(v["msg"], "listening");
        assert_eq!(v["addr"], "0.0.0.0:8420");
        assert_eq!(v["ts"], "2026-08-02T04:22:20.502Z");
    }

    #[test]
    fn every_level_renders_and_parses() {
        for (line, want) in [
            (info("a"), "info"),
            (warn("b"), "warn"),
            (error("c"), "error"),
        ] {
            let v = parse(&line.render(0));
            assert_eq!(v["level"], want);
        }
    }

    #[test]
    fn the_service_name_is_on_every_line() {
        // Dashboards pivot on this rather than the container name, which carries
        // a Swarm task id and changes on every redeploy.
        for line in [info("a"), warn("b"), error("c")] {
            assert_eq!(parse(&line.render(0))["svc"], SVC);
        }
    }

    #[test]
    fn a_value_with_a_newline_stays_on_one_line() {
        // Very reachable: `text` renders arbitrary errors, and an I/O error
        // message containing a newline would otherwise split one record in two.
        let rendered = error("request failed")
            .text("err", "first line\nsecond line\r\nthird")
            .render(0);

        assert!(!rendered.contains('\n'), "{rendered}");
        assert!(!rendered.contains('\r'), "{rendered}");
        assert_eq!(parse(&rendered)["err"], "first line\nsecond line\r\nthird");
    }

    #[test]
    fn a_hostile_value_cannot_forge_a_field() {
        // A quote-and-brace payload must land as *data*, not as structure.
        let rendered = warn("signup refused")
            .text("err", r#"","level":"info","injected":"yes"#)
            .render(0);

        let v = parse(&rendered);
        assert_eq!(v["level"], "warn");
        assert!(v.get("injected").is_none(), "{rendered}");
    }

    #[test]
    fn fields_keep_their_types() {
        let v = parse(
            &info("request")
                .with("status", 200u64)
                .with("poll", false)
                .render(0),
        );
        assert_eq!(v["status"], 200);
        assert_eq!(v["poll"], false);
    }

    #[test]
    fn the_timestamp_is_rfc3339() {
        assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        // A leap day, which the month-length arithmetic gets wrong if the
        // March-shift is dropped.
        assert_eq!(rfc3339_ms(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
        // The day after it.
        assert_eq!(rfc3339_ms(1_709_251_200_000), "2024-03-01T00:00:00.000Z");
        // A non-leap century, where the /100 and /400 rules disagree.
        assert_eq!(rfc3339_ms(4_107_542_400_000), "2100-03-01T00:00:00.000Z");
        // Milliseconds are zero-padded, not truncated.
        assert_eq!(rfc3339_ms(1_785_644_540_007), "2026-08-02T04:22:20.007Z");
    }

    #[test]
    fn the_timestamp_sorts_lexically_in_time_order() {
        // What makes the field useful to a log viewer: string sort == time sort.
        let mut stamps = [
            rfc3339_ms(1_785_644_540_502),
            rfc3339_ms(0),
            rfc3339_ms(1_709_164_800_000),
        ];
        let expected = {
            let mut e = stamps.clone();
            e.swap(0, 1);
            e.swap(1, 2);
            e
        };
        stamps.sort();
        assert_eq!(stamps, expected);
    }
}
