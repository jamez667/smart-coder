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
///
/// A `#` that is part of a value (a URL fragment, say) is NOT a comment. Only a
/// `#` at the start of the line or preceded by whitespace opens one.
pub fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while let Some(off) = line[i..].find('#') {
        let at = i + off;
        if at == 0 || bytes[at - 1].is_ascii_whitespace() {
            return &line[..at];
        }
        i = at + 1;
    }
    line
}
