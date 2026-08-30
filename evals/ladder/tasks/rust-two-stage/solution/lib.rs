//! Version strings: parse and compare.

/// A dotted version, e.g. `1.4.12`.
#[derive(Debug, PartialEq, Eq)]
pub struct Version {
    pub parts: Vec<u32>,
}

/// Parse a dotted version. Returns `None` if any component is not a number.
pub fn parse(s: &str) -> Option<Version> {
    let mut parts = Vec::new();
    for p in s.split('.') {
        parts.push(p.parse::<u32>().ok()?);
    }
    Some(Version { parts })
}

/// Order two versions. Shorter versions are padded with zeros, so `1.4` == `1.4.0`.
pub fn compare(a: &Version, b: &Version) -> std::cmp::Ordering {
    // Compare component by component over the LONGER of the two, treating a missing
    // component as zero -- so 1.4 and 1.4.0 are equal, but 1.4 is still below 1.4.1.
    let n = a.parts.len().max(b.parts.len());
    for i in 0..n {
        let x = a.parts.get(i).copied().unwrap_or(0);
        let y = b.parts.get(i).copied().unwrap_or(0);
        let ord = x.cmp(&y);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}
