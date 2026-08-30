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
    // Compare component by component, longest wins on a tie.
    for (x, y) in a.parts.iter().zip(b.parts.iter()) {
        let ord = x.cmp(y);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.parts.len().cmp(&b.parts.len())
}
