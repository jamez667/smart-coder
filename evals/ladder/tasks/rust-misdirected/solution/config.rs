//! Parses a whole config file into key/value pairs.

#[path = "tokenizer.rs"]
mod tokenizer;

use tokenizer::{split_pair, strip_comment};

/// Parse `text` into pairs, skipping blanks and comment-only lines.
pub fn parse(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(pair) = split_pair(line) {
            out.push(pair);
        }
    }
    out
}
