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
//! * **Never gut a paged read.** A numbered file read is contiguous source, not a
//!   log: cutting its middle out hides the very code that was asked for. Those go
//!   through [`truncate_paged_read`], which keeps a contiguous prefix and names the
//!   `start` that resumes it.

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

/// Truncate a **paged, numbered file read** to at most `max_lines` lines, keeping a
/// CONTIGUOUS prefix and telling the model exactly how to fetch the rest.
///
/// A file read is not a log. Its lines are contiguous source the model is about to
/// edit, and it can always ask for the next page — so the head/tail slice
/// [`truncate_observation`] applies to logs is the worst possible cut here: it
/// removes the MIDDLE of the window, which is usually the code that was asked for,
/// while the header still claims the full range. The model then reasons about a hole
/// it cannot see and has no way to recover the missing region.
///
/// So: keep the observation's first line (the `read_file <path> (lines A-B of T):`
/// header), then the first `max_lines` body lines, then ONE continuation line that
/// names what was kept and the `start` that resumes it, derived from the `N: ` prefix
/// of the last kept line:
///
/// ```text
/// … [showing lines 2800-3599 of the 2800-5800 you asked for; pass start=3600 for the next page]
/// ```
///
/// If the body is not numbered (a caller routed something else through here), the cut
/// is still a contiguous prefix and the note degrades to a plain
/// `… [N trailing line(s) not shown]` count. The middle is never dropped either way,
/// so none of these markers is a *blind* cut and none uses the head/tail path's
/// `[N line(s) truncated] …` wording that flags one.
pub fn truncate_paged_read(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines || max_lines == 0 {
        return text.to_string();
    }

    // The first line is the tool's header (`read_file x.rs (lines A-B of T):`); it is
    // not body, and dropping it would strip the only statement of what file this is.
    // A single-line input can't have one, so guard the split.
    let (header, body) = match lines.split_first() {
        Some((h, b)) if !b.is_empty() => (Some(*h), b),
        _ => (None, &lines[..]),
    };

    // Budget: the header and the continuation note both cost a line.
    let reserved = usize::from(header.is_some()) + 1;
    let keep = max_lines.saturating_sub(reserved).max(1).min(body.len());
    let kept = &body[..keep];
    let dropped = body.len() - keep;

    let note = match (line_number(kept[keep - 1]), header.and_then(asked_range)) {
        // Numbered body and a header that says what was asked for: the full hint.
        (Some(last), Some((from, to))) => format!(
            "… [showing lines {}-{last} of the {from}-{to} you asked for; pass start={} for the \
             next page]",
            line_number(kept[0]).unwrap_or(from),
            last + 1
        ),
        // Numbered body, unparseable header: still name the next start, which is the
        // only part the model actually acts on.
        (Some(last), None) => format!(
            "… [showing lines {}-{last}; {dropped} more line(s) not shown, pass start={} for the \
             next page]",
            line_number(kept[0]).unwrap_or(1),
            last + 1
        ),
        // Not numbered — no start to name, but the cut is still a clean prefix, so it
        // says "not shown" rather than the head/tail path's `[N line(s) truncated] …`.
        // That marker is the blind-cut detector's needle (see `blind_cut` in sc-core),
        // and a contiguous prefix is not a blind cut: nothing was removed from the
        // middle of what the model can see.
        (None, _) => format!("… [{dropped} trailing line(s) not shown]"),
    };

    let mut out: Vec<&str> = Vec::with_capacity(max_lines + 1);
    out.extend(header);
    out.extend(kept.iter().copied());
    out.push(&note);
    out.join("\n")
}

/// The `N` of a `N: <source>` numbered read line, if it is one.
fn line_number(line: &str) -> Option<usize> {
    let (n, _) = line.split_once(": ")?;
    n.trim().parse().ok()
}

/// The `A-B` a read header claims to be showing: `... (lines A-B of T):`.
fn asked_range(header: &str) -> Option<(usize, usize)> {
    let after = header.split("(lines ").nth(1)?;
    let span = after.split_whitespace().next()?;
    let (a, b) = span.split_once('-')?;
    Some((a.parse().ok()?, b.parse().ok()?))
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

    /// A numbered read cut at the cap keeps a CONTIGUOUS run from where the model
    /// asked, and names the start that resumes it. Nothing is taken from the middle.
    #[test]
    fn a_paged_read_keeps_a_contiguous_prefix_and_names_the_next_start() {
        let body: String = (2800..=5800)
            .map(|i| {
                format!(
                    "{i}: // line {i}
"
                )
            })
            .collect();
        let obs = format!(
            "read_file big.rs (lines 2800-5800 of 8000):
{body}"
        );
        let out = truncate_paged_read(&obs, 800);

        assert!(out.starts_with("read_file big.rs (lines 2800-5800 of 8000):"));
        // Contiguous from the requested start, with no hole anywhere in it.
        let kept: Vec<usize> = out
            .lines()
            .skip(1)
            .filter_map(|l| l.split_once(": ").and_then(|(n, _)| n.parse().ok()))
            .collect();
        assert_eq!(
            kept.first().copied(),
            Some(2800),
            "starts where asked: {}",
            &out[..80]
        );
        assert!(
            kept.windows(2).all(|w| w[1] == w[0] + 1),
            "the kept region has no hole"
        );
        // 800 total = header + 798 body + note.
        assert_eq!(kept.len(), 798);
        assert_eq!(kept.last().copied(), Some(3597));
        assert!(out.lines().count() <= 800);
        assert!(
            out.ends_with(
                "… [showing lines 2800-3597 of the 2800-5800 you asked for; pass start=3598 for the next page]"
            ),
            "the continuation names the right next start, got: {}",
            out.lines().last().unwrap()
        );
        // And the middle of the kept range is genuinely there.
        assert!(out.contains("3000: // line 3000"));
    }

    /// The whole point: what the head/tail path drops, this one keeps.
    #[test]
    fn a_paged_read_never_loses_its_middle() {
        let body: String = (1..=2000)
            .map(|i| {
                format!(
                    "{i}: x
"
                )
            })
            .collect();
        let obs = format!(
            "read_file big.rs (lines 1-2000 of 8000):
{body}"
        );
        let paged = truncate_paged_read(&obs, 800);
        let logged = truncate_observation(&obs, 800, true);

        assert!(
            !paged.contains(
                "
1000: x"
            ),
            "line 1000 is past the cap, so absent"
        );
        assert!(
            paged.contains(
                "
700: x"
            ),
            "everything up to the cap is present"
        );
        // The old path keeps the tail and drops the middle; this one does the reverse.
        assert!(
            logged.contains(
                "
2000: x"
            ),
            "head_tail keeps the tail"
        );
        assert!(
            !paged.contains(
                "
2000: x"
            ),
            "a prefix cut does not"
        );
    }

    /// A read that fits is returned byte-for-byte — no note, no header surgery.
    #[test]
    fn a_paged_read_within_the_cap_is_untouched() {
        let obs = "read_file a.rs (3 lines):
1: a
2: b
3: c";
        assert_eq!(truncate_paged_read(obs, 800), obs);
    }

    /// Defensive: an oversized observation whose body is NOT numbered still gets a
    /// contiguous prefix and a plain count — never a hole in the middle.
    #[test]
    fn an_unnumbered_paged_observation_still_gets_a_clean_prefix() {
        let body: String = (1..=100)
            .map(|i| {
                format!(
                    "plain line {i}
"
                )
            })
            .collect();
        let obs = format!(
            "read_file weird.txt:
{body}"
        );
        let out = truncate_paged_read(&obs, 10);

        assert!(out.starts_with("read_file weird.txt:"));
        assert!(out.contains("plain line 1"), "the prefix starts at the top");
        assert!(out.contains("plain line 8"), "and runs contiguously: {out}");
        assert!(!out.contains("plain line 9"), "and stops at the cap");
        assert!(!out.contains("plain line 100"), "no tail is spliced on");
        assert!(out.ends_with("… [92 trailing line(s) not shown]"), "{out}");
        assert!(out.lines().count() <= 10);
    }

    /// The marker a clean prefix cut leaves must NOT be the blind-cut needle
    /// (`[N line(s) truncated] …`), which sc-core reads as "the middle went missing".
    #[test]
    fn a_paged_cut_never_writes_the_blind_cut_marker() {
        let numbered: String = (1..=500)
            .map(|i| {
                format!(
                    "{i}: x
"
                )
            })
            .collect();
        let plain: String = (1..=500)
            .map(|_| {
                "x
"
                .to_string()
            })
            .collect();
        for body in [numbered, plain] {
            let out = truncate_paged_read(
                &format!(
                    "read_file a.rs (lines 1-500 of 900):
{body}"
                ),
                50,
            );
            assert!(
                !out.contains("line(s) truncated]"),
                "a contiguous prefix is not a blind cut: {}",
                out.lines().last().unwrap()
            );
        }
    }

    #[test]
    fn zero_budget_returns_input() {
        // max_lines == 0 is treated as "no line cap" (caller controls via tokens).
        let s = "a\nb\nc\nd";
        assert_eq!(truncate_observation(s, 0, false), s);
    }
}
