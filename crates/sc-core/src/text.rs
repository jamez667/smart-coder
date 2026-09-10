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

/// The byte offset in `raw` just past the OPENING quote of `key`'s string value — where that
/// value's body begins. `None` if the key, its `:` or its opening `"` is absent.
///
/// Used by the edit repair to start its closing-quote scan at the LAST of the two bodies it
/// spans, so a `"` inside the FIRST body can never be mistaken for the region's end.
pub(crate) fn value_start_after(raw: &str, key: &str) -> Option<usize> {
    let key_pos = raw.find(key)?;
    let after = key_pos + key.len();
    let colon = raw[after..].find(':')?;
    let rest = after + colon + 1;
    let open_q = raw[rest..].find('"')?;
    Some(rest + open_q + 1)
}

/// The closing quote of a recovered string value — the `"` that really ends the body, whatever
/// key order the model emitted.
///
/// The salvage paths cannot parse their input (that is why they exist), so they bound a string
/// value by scanning rather than lexing. The old rule was "the last `\"` in the remaining text",
/// which silently assumed the broken key was the LAST key in the object. Mellum emits
/// `{"content":…,"path":…,"tool":…}` — content FIRST — so that rule walked straight past the
/// body's closer and took the rest of the envelope as source, splicing
/// `","path":"render.rs","tool":"write_file` into the file (observed live on the
/// `rust-multi-site` rung: a fully correct model fix was turned into a build failure by the
/// harness).
///
/// The rule here is two-sided, because neither "first quote" nor "last quote" is right on its
/// own. A candidate closer must be
///
/// 1. **unescaped** — an odd run of `\` immediately before it means it is a `\"` inside the body
///    (the downstream lenient unescaper exists precisely because bodies carry those); and
/// 2. at a **structural boundary** — the next non-whitespace byte is `,` or `}`, or the text
///    ends; and
/// 3. followed by a **well-formed object tail** — everything after it is `,"key":<scalar>` pairs
///    running out to the final `}`. That third test is what separates the real closer from a
///    quote inside the body that merely happens to precede a `,` (source code containing the
///    literal text `","path":"`, a `{"a": 1}` dict, a Python `"""docstring"""`).
///
/// Candidates are tested left to right and the FIRST that satisfies all three wins, so a
/// content-first object stops at the body's own closer instead of eating the envelope. When the
/// body is the object's last member, the only tail is `}` and the answer is the same quote the
/// old `rfind` found — content-last behaviour is unchanged.
///
/// Returns the BYTE OFFSET of that quote within `body`, or `None` when no candidate qualifies —
/// the callers then fall through to their existing error path rather than guessing, because a
/// wrong bound writes garbage into a source file, which is the bug this fixes.
pub(crate) fn structural_closing_quote(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut fallback: Option<usize> = None;
    for (i, ch) in body.char_indices() {
        if ch != '"' {
            continue;
        }
        // (1) Escaped? Count the run of `\` immediately before this quote. Odd → it is a `\"`.
        let mut backslashes = 0usize;
        let mut j = i;
        while j > 0 && bytes[j - 1] == b'\\' {
            backslashes += 1;
            j -= 1;
        }
        if backslashes % 2 == 1 {
            continue;
        }
        // (2) Structural? The next non-whitespace byte must end the member or the object.
        let rest = &body[i + 1..];
        let next = rest.trim_start().as_bytes().first().copied();
        if !matches!(next, None | Some(b',') | Some(b'}')) {
            continue;
        }
        // (3) Is the remainder a well-formed object tail? If so this is the closer.
        if is_object_tail(rest) {
            return Some(i);
        }
        // A structural-looking quote whose tail does NOT check out is kept only as a last
        // resort: it covers a trailing envelope the model mangled beyond recognition, and it
        // reproduces the old `rfind` answer for a content-last body.
        fallback = Some(i);
    }
    fallback
}

/// Whether `s` is the tail of a JSON object after one member's value: optional whitespace, then
/// either the closing `}` or `,` followed by more `"key":<scalar>` members out to that `}`.
/// Values are only scalars (string / number / bool / null) — every tool argument is one, and
/// refusing nested structures keeps this from accepting arbitrary code that happens to balance.
/// Trailing text after the `}` is allowed (models add prose).
fn is_object_tail(s: &str) -> bool {
    let mut rest = s.trim_start();
    loop {
        match rest.as_bytes().first() {
            Some(b'}') | None => return true,
            Some(b',') => rest = rest[1..].trim_start(),
            _ => return false,
        }
        // A key: a well-formed quoted string with no inner escapes or quotes.
        let Some(after_open) = rest.strip_prefix('"') else {
            return false;
        };
        let Some(key_end) = after_open.find(['"', '\\']) else {
            return false;
        };
        if after_open.as_bytes()[key_end] != b'"' {
            return false; // an escape inside a key — not a plain envelope key
        }
        rest = after_open[key_end + 1..].trim_start();
        let Some(after_colon) = rest.strip_prefix(':') else {
            return false;
        };
        rest = after_colon.trim_start();
        // A scalar value.
        if let Some(after_q) = rest.strip_prefix('"') {
            // A string: scan to its unescaped closing quote.
            let mut escaped = false;
            let mut end = None;
            for (k, c) in after_q.char_indices() {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    end = Some(k);
                    break;
                }
            }
            let Some(end) = end else { return false };
            rest = after_q[end + 1..].trim_start();
        } else {
            let end = rest
                .find([',', '}', ' ', '\t', '\n', '\r'])
                .unwrap_or(rest.len());
            let (lit, tail) = rest.split_at(end);
            if lit.is_empty()
                || !(lit == "true" || lit == "false" || lit == "null" || lit.parse::<f64>().is_ok())
            {
                return false;
            }
            rest = tail.trim_start();
        }
    }
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
