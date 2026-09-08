//! The simplifier: capability output in, the minimum the model needs out.
//!
//! Two jobs that are easy to conflate and must not be:
//!
//! * **Extraction** — deterministic, per-shape, lossless *about the thing that
//!   matters*: 400 lines of test output become the 3 lines that failed. Always
//!   on, free, and testable against fixtures.
//! * **Summarization** — a model compressing output for another model. Lossy
//!   with no checksum: the summarizer does not know what the caller was going
//!   to do with the output, so it can drop the one field that mattered and the
//!   caller cannot tell. Off by default, and when on it reports what it did.
//!
//! Everything removed is *named* in the trace. A simplifier that silently drops
//! a stack trace and a capability that never produced one are indistinguishable
//! to whoever is debugging the run at 2am — so this never drops silently.

use sc_model::{GenerateRequest, Message, ModelBackend};

use crate::types::Trace;

/// How aggressively to reduce output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    /// Deterministic extraction only. Never invokes a model.
    #[default]
    Extract,
    /// Extraction, then a model summarizes what survived. Lossy — opt in.
    Summarize { max_chars: usize },
}

/// Hard ceiling on what any capability may hand back, before extraction.
///
/// A small model does not get more correct with more bytes — it gets more
/// confused. This is the backstop for a capability that returns something
/// pathological; the per-shape extractors below do the real work.
pub const MAX_RAW_CHARS: usize = 24_000;

/// Reduce `raw` to what the model needs, recording every removal in `trace`.
pub fn simplify(
    raw: &str,
    level: Level,
    trace: &mut Trace,
    backend: Option<&dyn ModelBackend>,
) -> String {
    trace.raw_bytes = raw.len();

    let mut text = raw.to_string();
    if text.len() > MAX_RAW_CHARS {
        let kept = floor_char_boundary(&text, MAX_RAW_CHARS);
        trace
            .dropped
            .push(format!("{} chars over the raw ceiling", text.len() - kept));
        text.truncate(kept);
    }

    let extracted = extract(&text, trace);

    let out = match level {
        Level::Extract => extracted,
        Level::Summarize { max_chars } => match backend {
            None => extracted,
            Some(b) => summarize(&extracted, max_chars, trace, b),
        },
    };

    trace.out_bytes = out.len();
    out
}

/// Deterministic, shape-aware reduction.
///
/// Recognises the output shapes this workspace actually produces. An unknown
/// shape is passed through untouched — guessing at an unrecognised format is
/// how a simplifier eats the one line that mattered.
fn extract(text: &str, trace: &mut Trace) -> String {
    if let Some(out) = extract_test_failures(text, trace) {
        return out;
    }
    if let Some(out) = extract_compiler_errors(text, trace) {
        return out;
    }
    collapse_blank_runs(text, trace)
}

/// Cargo/rustc test output: keep the failures and the count, drop the passes.
///
/// A passing test tells the model nothing it can act on. The failure lines and
/// the summary line are the entire signal.
fn extract_test_failures(text: &str, trace: &mut Trace) -> Option<String> {
    if !text.contains("test result:") && !text.contains("running ") {
        return None;
    }
    let mut kept = Vec::new();
    let mut passes = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if t.ends_with("... ok") || t.ends_with("... ignored") {
            passes += 1;
            continue;
        }
        kept.push(line);
    }
    if passes == 0 {
        return None;
    }
    trace.dropped.push(format!("{passes} passing test lines"));
    Some(kept.join("\n"))
}

/// rustc diagnostics: keep errors, drop warnings *and say how many*.
///
/// Warnings are real information — this drops them because a model fixing a
/// build does not need them, not because they are noise. The count stays so
/// nobody concludes the build was clean.
fn extract_compiler_errors(text: &str, trace: &mut Trace) -> Option<String> {
    if !text.contains("error[") && !text.contains("error:") {
        return None;
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut warnings = 0usize;
    let mut in_warning = false;
    for line in text.lines() {
        let t = line.trim_start();

        // A new diagnostic header ends whatever block was open. Checked FIRST,
        // so a warning block is only ever closed by the next diagnostic — not
        // by one of its own continuation lines.
        if t.starts_with("warning:") {
            warnings += 1;
            in_warning = true;
            continue;
        }
        if t.starts_with("error") || t.starts_with("Compiling") || t.starts_with("Finished") {
            in_warning = false;
        }

        // Everything between a warning header and the next diagnostic belongs
        // to that warning: the `--> file:line`, the source snippet, the `|`
        // gutter, the `= note:` footer, and the blank line after it. An earlier
        // version tested only for leading whitespace, which kept every `-->`
        // line and left the output barely smaller than it started.
        if in_warning {
            continue;
        }
        kept.push(line);
    }
    if warnings == 0 {
        return None;
    }
    trace.dropped.push(format!("{warnings} warnings"));
    Some(kept.join(
        "
",
    ))
}

/// The one universally safe reduction: runs of blank lines carry no meaning.
fn collapse_blank_runs(text: &str, trace: &mut Trace) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blanks = 0usize;
    let mut removed = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                removed += 1;
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    if removed > 0 {
        trace.dropped.push(format!("{removed} blank lines"));
    }
    out.trim_end().to_string()
}

/// Model-driven compression. Lossy, unverifiable, and marked as such.
///
/// Skipped entirely when the text is already under budget — paying a model call
/// to shorten something that already fits is pure loss.
fn summarize(
    text: &str,
    max_chars: usize,
    trace: &mut Trace,
    backend: &dyn ModelBackend,
) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut req = GenerateRequest::new(vec![
        Message::system(
            "Compress the following tool output to the facts a coding agent needs to act. \
             Keep every file path, line number, symbol name and error code verbatim. \
             Drop prose. If you are unsure whether a detail matters, keep it.",
        ),
        Message::user(text.to_string()),
    ]);
    // Greedy: a summarizer that paraphrases differently each run makes every
    // downstream failure irreproducible.
    req.temperature = 0.0;
    match backend.generate(&req) {
        Ok(resp) => {
            trace.summarized = true;
            trace.dropped.push(format!(
                "summarized {} chars -> {}",
                text.len(),
                resp.content.len()
            ));
            resp.content
        }
        // A failed summarizer must not lose the output. Fall back to the
        // extracted text: too long beats absent.
        Err(_) => {
            trace
                .dropped
                .push("summarizer failed; returned extracted text".to_string());
            text.to_string()
        }
    }
}

/// Largest index <= `max` that is a char boundary, so truncation never splits a
/// multi-byte character.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut i = max.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
