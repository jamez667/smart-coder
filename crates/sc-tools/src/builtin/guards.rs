//! Pre-write tripwires: the checks that catch small-model corruption *before* it
//! reaches the disk.
//!
//! Each guard here exists because of an observed live failure — a nested tool call
//! written into a source file, a straddled brace dropped by a range edit, a helper
//! re-pasted into a file that already defined it. They are deliberately cheap and
//! approximate: a regression check ("this edit made it worse"), not a correctness
//! proof, so a pre-existing mess is never blamed on the current edit.

/// Does `body` look like the model leaked a tool call (or a ```json fence wrapping one) into a
/// file-content field, instead of sending raw source? The model does this both at the START of the
/// content and EMBEDDED mid-file (a real code prefix, then a `{"tool":...}` block), so we scan the
/// whole body — not just the prefix — for the tell-tale shapes seen corrupting source files.
pub fn looks_like_tool_call_json(body: &str) -> bool {
    // A ```json / ```rs / ```rust fence anywhere — scaffolding the model meant as a code block.
    if body.contains("```json") || body.contains("```rs") || body.contains("```rust") {
        return true;
    }
    // A JSON object opening with a `"tool"` key, anywhere in the body. Match `{` optionally
    // followed by whitespace/newlines then a "tool" (or 'tool') key — the nested-call signature.
    // Cheap scan: find each '{', skip whitespace, check for the tool key.
    let bytes = body.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'{' {
            let rest = body[i + 1..].trim_start();
            if rest.starts_with("\"tool\"") || rest.starts_with("'tool'") {
                return true;
            }
        }
    }
    false
}

/// Does this path look like brace-delimited source we should balance-check? (Rust/JS/TS/etc.)
/// Python/other whitespace-structured files are skipped — their `{}` are dict/set literals, not
/// blocks, so a balance count is noise.
pub fn is_code_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    [
        ".rs", ".js", ".ts", ".jsx", ".tsx", ".go", ".java", ".c", ".h", ".cpp", ".css",
    ]
    .iter()
    .any(|e| p.ends_with(e))
}

/// Net delimiter balance of a source string: (curly, paren, square). A naive char count that
/// ignores strings/comments — good enough as a tripwire, since a straddled-brace edit_lines shifts
/// a count by exactly ±1 and string/comment noise is the SAME in before/after (it's a regression
/// check, not an absolute correctness check).
fn delim_balance(s: &str) -> (i64, i64, i64) {
    let (mut c, mut p, mut b) = (0i64, 0i64, 0i64);
    for ch in s.chars() {
        match ch {
            '{' => c += 1,
            '}' => c -= 1,
            '(' => p += 1,
            ')' => p -= 1,
            '[' => b += 1,
            ']' => b -= 1,
            _ => {}
        }
    }
    (c, p, b)
}

/// If `before` was delimiter-balanced but `after` is not, return a message naming the delimiter
/// that went out of balance. `None` when the edit didn't introduce an imbalance (either both
/// balanced, or `before` was already unbalanced — a partial file mid-build — so we don't blame
/// this edit for a pre-existing state).
pub fn delimiter_regression(before: &str, after: &str) -> Option<String> {
    let (bc, bp, bb) = delim_balance(before);
    if bc != 0 || bp != 0 || bb != 0 {
        return None; // pre-existing imbalance; not this edit's fault
    }
    let (ac, ap, ab) = delim_balance(after);
    let which = |n: i64, open: char, close: char| -> Option<String> {
        if n > 0 {
            Some(format!("{n} unclosed '{open}' (missing {n} '{close}')."))
        } else if n < 0 {
            Some(format!("{} extra '{close}' (no matching '{open}').", -n))
        } else {
            None
        }
    };
    which(ac, '{', '}')
        .or_else(|| which(ap, '(', ')'))
        .or_else(|| which(ab, '[', ']'))
        .map(|d| format!("this edit unbalanced the file's delimiters: {d}"))
}

/// Top-level definition names in `src`, keyed by kind+name (e.g. `fn:draw_row`, `struct:Rect`),
/// with a count. Scans line-leading `fn` / `pub fn` / `struct` / `enum` / `trait` / `const` /
/// `static` declarations — a lightweight signal (no full parse) that's enough to catch a
/// re-emitted definition. `impl` blocks are deliberately excluded (multiple `impl` of a type are
/// legal). Visibility/`async`/`unsafe`/`pub(crate)` prefixes are skipped.
pub fn top_level_defs(src: &str) -> std::collections::HashMap<String, usize> {
    use std::collections::HashMap;
    let mut out: HashMap<String, usize> = HashMap::new();
    for line in src.lines() {
        // Only TOP-LEVEL defs (no leading indentation) — a nested `fn` inside another fn/impl is a
        // different scope and legitimately repeatable; we want file-level redefinitions.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        // Strip leading visibility / modifiers so `pub async unsafe fn foo` still keys on `foo`.
        let mut t = line.trim();
        for kw in [
            "pub(crate)",
            "pub",
            "async",
            "unsafe",
            "default",
            "const",
            "extern \"C\"",
        ] {
            if let Some(rest) = t.strip_prefix(kw) {
                if rest.starts_with([' ', '\t']) || rest.is_empty() {
                    t = rest.trim_start();
                }
            }
        }
        let kind = ["fn", "struct", "enum", "trait", "static"]
            .into_iter()
            .find(|kw| {
                t.strip_prefix(kw)
                    .is_some_and(|r| r.starts_with([' ', '\t']))
            });
        if let Some(kind) = kind {
            let rest = t[kind.len()..].trim_start();
            // The name is up to the first delimiter: `(` for fn, `<`/`{`/`:`/whitespace otherwise.
            if let Some(name) = rest
                .split(|c: char| c == '(' || c == '<' || c == '{' || c == ':' || c.is_whitespace())
                .next()
                .filter(|s| !s.is_empty())
            {
                *out.entry(format!("{kind}:{name}")).or_default() += 1;
            }
        }
    }
    out
}

/// If `after` introduces a DUPLICATE top-level definition — a `fn`/`struct`/`enum`/`trait` name
/// that now appears more times than it did in `before` AND appears more than once — return a
/// message naming it. This is the coder's block-duplication failure: asked to add a helper to a
/// file that already has it, the model re-emits the existing definition (and often other nearby
/// ones), producing an `E0428 "defined multiple times"` that breaks the build. Rejecting the write
/// makes the model EDIT the existing definition instead of pasting a second copy. `None` when the
/// edit adds no new duplication (a pre-existing duplicate isn't blamed on this edit).
pub fn duplicate_definition(before: &str, after: &str) -> Option<String> {
    let bd = top_level_defs(before);
    let ad = top_level_defs(after);
    // Find a name whose count went UP and is now >1 — i.e. this edit created (or worsened) a
    // duplicate. Report the most-egregious (highest after-count) for a clear message.
    ad.iter()
        .filter(|(k, &n)| n > 1 && n > bd.get(*k).copied().unwrap_or(0))
        .max_by_key(|(_, &n)| n)
        .map(|(k, &n)| {
            let (kind, name) = k.split_once(':').unwrap_or(("item", k));
            format!(
                "this edit would define `{name}` ({kind}) {n} times — it ALREADY EXISTS in the \
                 file. Rust rejects a duplicate definition (E0428). Do NOT paste a second copy: \
                 EDIT the existing `{name}` in place (change its body/signature) instead of adding \
                 a new one. If you meant a different helper, give it a different name."
            )
        })
}
