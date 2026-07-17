//! Observation truncation (spec 05 — aggressive observation truncation).
//!
//! A 5k-line test log can't go back verbatim into an 8k window. Tool results are
//! squeezed to a line budget before re-entering the prompt, with two rules that
//! matter for a small model:
//!
//! * **Errors first.** When output has error-ish lines, they're the signal the
//!   model needs — keep them preferentially over surrounding noise.
//! * **Always flag the cut.** A truncated result is marked so the model knows
//!   output was elided and can ask to read more, rather than assuming it saw all.

/// Truncate `text` to at most `max_lines` lines, keeping a head and a tail and
/// marking the elision. Short input is returned unchanged.
///
/// When `prioritize_errors` is set and the text has more lines than fit, lines
/// that look like errors are surfaced first (after a small head for context).
pub fn truncate_observation(text: &str, max_lines: usize, prioritize_errors: bool) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines || max_lines == 0 {
        return text.to_string();
    }

    if prioritize_errors {
        let error_idx: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| looks_like_error(l))
            .map(|(i, _)| i)
            .collect();
        if !error_idx.is_empty() && error_idx.len() <= max_lines {
            return assemble_error_focused(&lines, &error_idx, max_lines);
        }
    }

    head_tail(&lines, max_lines)
}

/// Keep the first `head` and last `tail` lines with a marker between.
fn head_tail(lines: &[&str], max_lines: usize) -> String {
    let head = max_lines.div_ceil(2);
    let tail = max_lines - head;
    let omitted = lines.len() - head - tail;
    let mut out: Vec<String> = Vec::with_capacity(max_lines + 1);
    out.extend(lines[..head].iter().map(|s| s.to_string()));
    out.push(format!("… [{omitted} line(s) truncated] …"));
    out.extend(lines[lines.len() - tail..].iter().map(|s| s.to_string()));
    out.join("\n")
}

/// Keep a short head for context, then the error lines (with nearby context),
/// flagging what was skipped.
fn assemble_error_focused(lines: &[&str], error_idx: &[usize], max_lines: usize) -> String {
    // Reserve a couple of lines for a head; the rest for errors.
    let head = (max_lines.saturating_sub(error_idx.len())).min(2);
    let mut keep = std::collections::BTreeSet::new();
    for i in 0..head {
        keep.insert(i);
    }
    for &i in error_idx {
        keep.insert(i);
        if keep.len() >= max_lines {
            break;
        }
    }

    let mut out = Vec::new();
    let mut prev: Option<usize> = None;
    for &i in &keep {
        if let Some(p) = prev {
            if i > p + 1 {
                out.push(format!("… [{} line(s) skipped] …", i - p - 1));
            }
        }
        out.push(lines[i].to_string());
        prev = Some(i);
    }
    if let Some(p) = prev {
        if p + 1 < lines.len() {
            out.push(format!("… [{} line(s) skipped] …", lines.len() - p - 1));
        }
    }
    out.join("\n")
}

/// A line that looks like a failure/error worth keeping.
fn looks_like_error(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "error",
        "fail",
        "panic",
        "exception",
        "traceback",
        "assert",
        "fatal",
        "✗",
    ];
    NEEDLES.iter().any(|n| l.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_unchanged() {
        let s = "a\nb\nc";
        assert_eq!(truncate_observation(s, 10, true), s);
    }

    #[test]
    fn long_output_keeps_head_and_tail_and_flags_the_cut() {
        let lines: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");
        let out = truncate_observation(&text, 10, false);
        assert!(out.contains("line 1"), "head kept: {out}");
        assert!(out.contains("line 100"), "tail kept: {out}");
        assert!(out.contains("truncated"), "cut flagged: {out}");
        // Far fewer lines than the original.
        assert!(out.lines().count() <= 11);
    }

    #[test]
    fn error_lines_are_prioritized() {
        let mut lines: Vec<String> = (1..=50).map(|i| format!("ok line {i}")).collect();
        lines[30] = "ERROR: something broke at frobnicate()".to_string();
        let text = lines.join("\n");
        let out = truncate_observation(&text, 6, true);
        assert!(out.contains("ERROR: something broke"), "error kept: {out}");
        assert!(out.contains("skipped"), "skip flagged: {out}");
    }

    #[test]
    fn zero_budget_returns_input() {
        // max_lines == 0 is treated as "no line cap" (caller controls via tokens).
        let s = "a\nb\nc\nd";
        assert_eq!(truncate_observation(s, 0, false), s);
    }
}
