//! Escaping for the log sink: a line-oriented format where a record is one line.

/// Escape `s` so it survives a round trip through the line-oriented log format.
///
/// A record is one line, so a literal newline inside a field would split the
/// record in two. Newlines become `\n`, tabs become `\t`, and the escape
/// character itself becomes `\\` — without that last one, a literal backslash
/// followed by `n` is indistinguishable from an escaped newline.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

/// Undo [`escape`].
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}
