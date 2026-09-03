//! The small slice of Markdown a model actually emits, parsed into renderable spans.
//!
//! Assistant prose arrives as Markdown — `**bold**`, `` `code` ``, fenced blocks,
//! `##` headings, `-` bullets — and the panel used to push the whole thing into one
//! flat line of size-12 text. A page of that reads as an undifferentiated wall,
//! which is the difference between this panel and every other chat client.
//!
//! Deliberately NOT a full CommonMark implementation. Links, tables, nested
//! emphasis, reference definitions and HTML are not what a coding model's answer
//! leans on, and each one is a parser branch that can mangle ordinary prose. This
//! handles the five constructs that carry the structure and leaves everything else
//! as literal text — a paragraph that renders plainly is a far smaller failure than
//! one mangled by an over-eager parser.

/// One line of rendered output, already classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// A `#`-prefixed heading. `level` is 1-3; deeper ones render as 3.
    Heading { level: u8, spans: Vec<Span> },
    /// A `-` / `*` / `1.` list item, already stripped of its marker.
    Bullet { spans: Vec<Span> },
    /// A line inside a ``` fence. Rendered monospace, verbatim.
    Code(String),
    /// Ordinary prose.
    Para { spans: Vec<Span> },
    /// A deliberate gap between paragraphs. Carries no text.
    Blank,
}

/// A run of text within a block, with the one bit of styling it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    Plain(String),
    Bold(String),
    /// Inline `` `code` `` — monospace, tinted.
    Code(String),
}

/// Parse `src` into blocks, in order.
///
/// Never fails and never panics: anything unrecognised comes back as `Para`.
pub fn parse(src: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut in_fence = false;

    for raw in src.lines() {
        let trimmed = raw.trim_end();

        // A fence toggles verbatim mode. The fence lines themselves are not shown —
        // they are punctuation, not content.
        if trimmed.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push(Block::Code(trimmed.to_string()));
            continue;
        }

        let line = trimmed.trim_start();
        if line.is_empty() {
            // Collapse runs of blank lines: one gap is a paragraph break, three is
            // a hole in the feed.
            if !matches!(out.last(), Some(Block::Blank) | None) {
                out.push(Block::Blank);
            }
            continue;
        }

        if let Some(rest) = heading(line) {
            out.push(rest);
            continue;
        }
        if let Some(rest) = bullet(line) {
            out.push(Block::Bullet { spans: spans(rest) });
            continue;
        }
        out.push(Block::Para { spans: spans(line) });
    }

    // A trailing gap adds nothing but scroll.
    if matches!(out.last(), Some(Block::Blank)) {
        out.pop();
    }
    out
}

/// `## Text` → a heading, or `None` if the line is not one.
fn heading(line: &str) -> Option<Block> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?;
    Some(Block::Heading {
        level: (hashes as u8).min(3),
        spans: spans(rest.trim()),
    })
}

/// `- item`, `* item` or `1. item` → the item text, or `None`.
fn bullet(line: &str) -> Option<&str> {
    for m in ["- ", "* ", "• "] {
        if let Some(rest) = line.strip_prefix(m) {
            return Some(rest);
        }
    }
    // `12. item` — digits, a dot, a space.
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0 {
        if let Some(rest) = line[digits..].strip_prefix(". ") {
            return Some(rest);
        }
    }
    None
}

/// Split a line into styled spans on `**bold**` and `` `code` ``.
///
/// Unclosed markers are left as literal text: a lone `**` in prose is far more
/// likely than an emphasis the model forgot to close, and swallowing the rest of
/// the paragraph to chase a closer is the worse failure.
fn spans(line: &str) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Inline code first: backticks win over asterisks, so `**` inside a code
        // span stays literal.
        if chars[i] == '`' {
            if let Some(end) = find(&chars, i + 1, '`', 1) {
                flush(&mut out, &mut plain);
                out.push(Span::Code(chars[i + 1..end].iter().collect()));
                i = end + 1;
                continue;
            }
        }
        if chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(end) = find_bold(&chars, i + 2) {
                flush(&mut out, &mut plain);
                out.push(Span::Bold(chars[i + 2..end].iter().collect()));
                i = end + 2;
                continue;
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    flush(&mut out, &mut plain);
    out
}

fn flush(out: &mut Vec<Span>, plain: &mut String) {
    if !plain.is_empty() {
        out.push(Span::Plain(std::mem::take(plain)));
    }
}

/// Index of the next `needle`, at least `min_len` characters after `from`.
fn find(chars: &[char], from: usize, needle: char, min_len: usize) -> Option<usize> {
    (from + min_len..chars.len()).find(|&j| chars[j] == needle)
}

/// Index of the next `**` after `from`.
fn find_bold(chars: &[char], from: usize) -> Option<usize> {
    (from..chars.len().saturating_sub(1))
        .find(|&j| chars[j] == '*' && chars[j + 1] == '*' && j > from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> Vec<Span> {
        vec![Span::Plain(s.to_string())]
    }

    #[test]
    fn ordinary_prose_is_one_paragraph() {
        assert_eq!(
            parse("just a sentence"),
            vec![Block::Para {
                spans: plain("just a sentence")
            }]
        );
    }

    #[test]
    fn headings_carry_their_level() {
        let b = parse("## Findings");
        assert_eq!(
            b,
            vec![Block::Heading {
                level: 2,
                spans: plain("Findings")
            }]
        );
        // Deeper than 3 clamps rather than growing indefinitely.
        assert!(matches!(
            parse("##### deep").first(),
            Some(Block::Heading { level: 3, .. })
        ));
        // A `#` with no space is not a heading — `#1` is prose, and so is a Rust
        // attribute or a colour like `#fff`.
        assert!(matches!(
            parse("#1 priority").first(),
            Some(Block::Para { .. })
        ));
    }

    #[test]
    fn bullets_lose_their_marker() {
        for line in ["- item", "* item", "1. item", "12. item"] {
            assert_eq!(
                parse(line),
                vec![Block::Bullet {
                    spans: plain("item")
                }],
                "failed on {line:?}"
            );
        }
    }

    #[test]
    fn a_fence_is_verbatim_and_its_markers_are_not_shown() {
        let b = parse("before\n```rust\nlet x = 1;\n```\nafter");
        assert_eq!(
            b,
            vec![
                Block::Para {
                    spans: plain("before")
                },
                Block::Code("let x = 1;".to_string()),
                Block::Para {
                    spans: plain("after")
                },
            ]
        );
    }

    /// Markdown inside a fence stays literal — a `**` in code is not emphasis.
    #[test]
    fn a_fence_does_not_style_its_contents() {
        let b = parse("```\nlet y = a ** b;\n```");
        assert_eq!(b, vec![Block::Code("let y = a ** b;".to_string())]);
    }

    #[test]
    fn bold_and_inline_code_become_spans() {
        assert_eq!(
            spans("a **bold** and `code` end"),
            vec![
                Span::Plain("a ".into()),
                Span::Bold("bold".into()),
                Span::Plain(" and ".into()),
                Span::Code("code".into()),
                Span::Plain(" end".into()),
            ]
        );
    }

    /// **An unclosed marker must not eat the rest of the line.**
    ///
    /// A lone `**` or backtick in prose is far more likely than emphasis the model
    /// forgot to close, and swallowing the paragraph to chase a closer turns a
    /// cosmetic miss into lost text.
    #[test]
    fn an_unclosed_marker_stays_literal() {
        assert_eq!(spans("2 ** 8 is 256"), plain("2 ** 8 is 256"));
        assert_eq!(spans("a ` tick"), plain("a ` tick"));
        assert_eq!(
            spans("**open but never closed"),
            plain("**open but never closed")
        );
    }

    /// Backticks win over asterisks, so `**` inside code stays literal.
    #[test]
    fn code_spans_take_precedence_over_emphasis() {
        assert_eq!(spans("`a ** b`"), vec![Span::Code("a ** b".into())]);
    }

    #[test]
    fn blank_lines_collapse_to_one_gap_and_never_trail() {
        let b = parse("one\n\n\n\ntwo\n\n");
        assert_eq!(
            b,
            vec![
                Block::Para {
                    spans: plain("one")
                },
                Block::Blank,
                Block::Para {
                    spans: plain("two")
                },
            ],
            "runs collapse, and no trailing gap"
        );
    }

    #[test]
    fn empty_input_is_no_blocks() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n").is_empty());
    }
}
