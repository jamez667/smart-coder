//! The demo core: a tiny parser used by the gateway benchmark fixtures.

/// Parse a duration like `30s` or `5m` into seconds.
pub fn parse_duration(input: &str) -> Option<u64> {
    let (num, unit) = input.split_at(input.len().checked_sub(1)?);
    let n: u64 = num.parse().ok()?;
    match unit {
        "s" => Some(n),
        "m" => Some(n * 60),
        "h" => Some(n * 3600),
        _ => None,
    }
}

/// Configuration loaded at startup.
pub struct Config {
    pub timeout_secs: u64,
    pub retries: u32,
}
