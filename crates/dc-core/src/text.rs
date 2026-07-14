//! Generic text/JSON scanning primitives shared across the crate.
//!
//! These are pure `&str → …` utilities with no dependency on the tool surface or
//! the agent loop: pulling a balanced `{...}`/`[...]` block out of the prose a small
//! model wraps its tool call in, escaping/unescaping JSON string bodies leniently,
//! and locating values by key position when the JSON is too malformed to parse.
//!
//! [`strategy`](crate::strategy) builds its tool-call repair on top of these, and
//! [`planner`](crate::planner) pulls its step array out with [`extract_json_array`].
//! Keeping them here (rather than inside `strategy`) means neither module reaches
//! into the other just to borrow a string scanner.

/// Find the first balanced `{...}` block, ignoring braces inside JSON strings.
/// Tolerates the surrounding prose a small model tends to emit around its call.
pub fn extract_json_object(text: &str) -> Option<&str> {
    extract_balanced(text, '{', '}')
}

/// Find the first balanced `[...]` block, ignoring brackets inside JSON strings.
/// Used by the planner to pull a step array out of a small model's noisy reply.
pub fn extract_json_array(text: &str) -> Option<&str> {
    extract_balanced(text, '[', ']')
}

/// Find ALL top-level balanced `{...}` blocks in order. Some models (Gemma-4) emit
/// several tool calls in ONE turn, separated by markers like `<tool_call|>`, e.g.
/// `{read_file}<tool_call|>{create_file}<tool_call|>{run_verification}`. The loop runs
/// one action per turn, so we need every candidate to pick the one that makes progress.
pub(crate) fn extract_all_json_objects(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(obj) = extract_balanced(rest, '{', '}') {
        out.push(obj);
        // Advance past this object. `obj` is a slice of `rest`; find where it ends.
        let end = (obj.as_ptr() as usize - rest.as_ptr() as usize) + obj.len();
        if end >= rest.len() {
            break;
        }
        rest = &rest[end..];
    }
    out
}

/// Find the first balanced `open..close` block, ignoring delimiters inside JSON
/// strings (with escape handling).
pub(crate) fn extract_balanced(text: &str, open: char, close: char) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find(open)?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        // Only ASCII delimiters matter; UTF-8 continuation bytes are >= 0x80 and
        // never collide with these, so byte scanning is safe.
        let ch = bytes[i] as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
        } else if ch == '"' {
            in_str = true;
        } else if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(&text[start..=i]);
            }
        }
    }
    None
}

/// Escape raw (unescaped) control characters that appear INSIDE JSON string values —
/// a literal newline/carriage-return/tab a model emitted instead of `\n`/`\r`/`\t`.
/// JSON forbids raw control chars in strings, so `serde_json` rejects them; a coder
/// model writing multi-line code in an argument hits this constantly. We only touch
/// chars inside string literals (tracking quote/escape state), so structural JSON is
/// untouched and an already-escaped `\n` (backslash + n) passes through verbatim.
pub(crate) fn escape_raw_control_chars_in_strings(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + 16);
    let mut in_str = false;
    let mut escaped = false;
    for ch in json.chars() {
        if in_str {
            if escaped {
                escaped = false;
                out.push(ch);
                continue;
            }
            match ch {
                '\\' => {
                    escaped = true;
                    out.push(ch);
                }
                '"' => {
                    in_str = false;
                    out.push(ch);
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        } else {
            if ch == '"' {
                in_str = true;
            }
            out.push(ch);
        }
    }
    out
}

/// Resolve the standard JSON string escapes (`\n \t \r \" \\ \/`) a model wrote correctly,
/// leaving any other backslash sequence and all raw characters as-is. Lenient on purpose:
/// the input is a recovered literal that may mix escaped and raw characters.
pub(crate) fn unescape_json_string_lenient(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Read the first JSON-quoted string value appearing after `key` in `raw` (the value of a
/// well-formed `"key":"value"`). `None` if absent. Used by the key-aware `repair_*` calls for
/// the `path`, which precedes the broken `content` and is itself well-formed.
pub(crate) fn quoted_value_after(raw: &str, key: &str) -> Option<String> {
    let key_pos = raw.find(key)?;
    let after = &raw[key_pos + key.len()..];
    let colon = after.find(':')?;
    let rest = &after[colon + 1..];
    let open_q = rest.find('"')?;
    let body = &rest[open_q + 1..];
    // Scan to the unescaped closing quote.
    let mut out = String::new();
    let mut escaped = false;
    for ch in body.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            out.push(ch);
            escaped = true;
        } else if ch == '"' {
            return Some(unescape_json_string_lenient(&out));
        } else {
            out.push(ch);
        }
    }
    None
}

/// Find the `"`,`"new_str"`:`"` boundary between the two edit_file values and return
/// `(old_literal, new_literal)`. The separator is the model's own `","new_str":"` with possible
/// whitespace; we match on `new_str"` and trim back over the quote/colon/comma. `None` if absent.
pub(crate) fn split_on_new_str(body: &str) -> Option<(&str, &str)> {
    let key = body.find("new_str")?;
    // old part = everything before the separator's leading `"`. Walk back from `new_str` over
    // optional whitespace, the opening `"`, whitespace, the `:`, whitespace, the closing `"`,
    // whitespace, the `,` — but simplest robust cut: old ends at the last `"` before `new_str`,
    // new begins at the first `"` after the `:` that follows `new_str`.
    let before = &body[..key];
    let old_end = before.rfind('"')?; // the `"` that opened `"new_str"` ... actually before it
                                      // Trim a trailing comma/quote run: old_str literal is before the `","` separator.
    let old_lit = before[..old_end].trim_end_matches(['"', ',', ' ', '\t', '\n', '\r']);
    let after = &body[key + "new_str".len()..];
    let colon = after.find(':')?;
    let rest = &after[colon + 1..];
    let oq = rest.find('"')?;
    let new_lit = &rest[oq + 1..];
    Some((old_lit, new_lit))
}

/// The contents of the LAST fenced ```` ``` ````…```` ``` ```` code block in `raw` (the model's
/// final/most complete version when it shows a draft then a revision), or `None` if there's no
/// fence.
pub(crate) fn fenced_code_block(raw: &str) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut lines = raw.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with("```") {
            let mut body = String::new();
            for l in lines.by_ref() {
                if l.trim() == "```" {
                    break;
                }
                body.push_str(l);
                body.push('\n');
            }
            if !body.trim().is_empty() {
                blocks.push(body);
            }
        }
    }
    blocks.pop()
}

/// The first non-empty line of a string, trimmed — for a tight one-line event summary.
pub fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Crude identifier extraction from free text (e.g. the task), to boost the repo map toward
/// symbols the user actually named (spec 05). Splits on non-identifier chars and keeps word-ish
/// tokens of length ≥ 3.
pub fn mentioned_identifiers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            flush_ident(&mut cur, &mut out);
        }
    }
    flush_ident(&mut cur, &mut out);
    out
}

fn flush_ident(cur: &mut String, out: &mut Vec<String>) {
    if cur.len() >= 3 && !out.contains(cur) {
        out.push(cur.clone());
    }
    cur.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_first_balanced_object_ignoring_prose_and_strings() {
        let raw = r#"sure, here you go: {"tool":"read_file","path":"a}b.txt"} — done"#;
        assert_eq!(
            extract_json_object(raw),
            Some(r#"{"tool":"read_file","path":"a}b.txt"}"#)
        );
    }

    #[test]
    fn extracts_balanced_array() {
        assert_eq!(
            extract_json_array("noise [\"a\", \"b]c\"] tail"),
            Some("[\"a\", \"b]c\"]")
        );
    }

    #[test]
    fn extracts_all_top_level_objects() {
        let raw = "{\"a\":1}<sep>{\"b\":2}";
        assert_eq!(
            extract_all_json_objects(raw),
            vec!["{\"a\":1}", "{\"b\":2}"]
        );
    }

    #[test]
    fn escapes_only_raw_control_chars_inside_strings() {
        // A raw newline inside the value becomes `\n`; an already-escaped one is untouched.
        let json = "{\"content\":\"line1\nline2\"}";
        assert_eq!(
            escape_raw_control_chars_in_strings(json),
            "{\"content\":\"line1\\nline2\"}"
        );
    }

    #[test]
    fn unescape_is_lenient_on_unknown_sequences() {
        assert_eq!(unescape_json_string_lenient(r"a\nb\qc"), "a\nb\\qc");
    }

    #[test]
    fn quoted_value_after_reads_the_first_value() {
        assert_eq!(
            quoted_value_after(r#"{"path":"app.py","content":"x"}"#, "\"path\""),
            Some("app.py".to_string())
        );
    }

    #[test]
    fn splits_edit_body_on_new_str() {
        let body = r#"old code","new_str":"new code"#;
        assert_eq!(split_on_new_str(body), Some(("old code", "new code")));
    }

    #[test]
    fn fenced_code_block_returns_the_last_block() {
        let raw = "draft:\n```\nfirst\n```\nfinal:\n```python\nsecond\n```\n";
        assert_eq!(fenced_code_block(raw).as_deref(), Some("second\n"));
    }

    #[test]
    fn first_line_skips_leading_blanks() {
        assert_eq!(first_line("\n  \n  hello \nworld"), "hello");
    }

    #[test]
    fn mentioned_identifiers_keeps_wordish_tokens() {
        assert_eq!(
            mentioned_identifiers("fix is_even in impl.sh, ok?"),
            vec!["fix", "is_even", "impl"]
        );
    }
}
