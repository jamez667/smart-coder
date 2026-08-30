//! Splits a config line into key and value.

/// Split `line` at the FIRST `=`, trimming whitespace from both halves.
///
/// Returns `None` for a line with no `=`.
pub fn split_pair(line: &str) -> Option<(String, String)> {
    let idx = line.find('=')?;
    let key = line[..idx].trim().to_string();
    let value = line[idx + 1..].trim().to_string();
    Some((key, value))
}

/// Strip a trailing `# comment`, if any.
pub fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}
