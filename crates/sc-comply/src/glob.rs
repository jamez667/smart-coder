//! Glob patterns, compiled to `regex` at pack-load time.
//!
//! Deliberately hand-rolled rather than pulling in `globset`. Three reasons:
//! the crate stays on workspace dependencies only (matching `sc-verify`'s
//! zero-dep instinct); the translation is directly unit-testable; and a
//! malformed glob becomes a *pack-load* error rather than a surprise halfway
//! through an audit, which matters when the output is an auditor's artifact.
//!
//! Supported syntax:
//!
//! | Pattern | Meaning                                        |
//! |---------|------------------------------------------------|
//! | `*`     | any run of characters except `/`               |
//! | `**/`   | zero or more path segments                     |
//! | `**`    | any run of characters, including `/`           |
//! | `?`     | exactly one character except `/`               |
//! | `{a,b}` | alternation (nestable)                         |
//! | `[abc]` | a character class, passed through to `regex`   |
//!
//! Paths are matched workspace-relative and forward-slashed.

use regex::Regex;
use sc_proto::{DcError, Result};

/// A compiled glob.
#[derive(Debug, Clone)]
pub struct Glob {
    pattern: String,
    re: Regex,
}

impl Glob {
    /// Compile a glob. Fails on unbalanced braces or an untranslatable pattern.
    pub fn new(pattern: &str) -> Result<Self> {
        let re_src = glob_to_regex(pattern)?;
        let re = Regex::new(&re_src).map_err(|e| {
            DcError::Comply(format!(
                "glob {pattern:?} compiled to an invalid regex: {e}"
            ))
        })?;
        Ok(Glob {
            pattern: pattern.to_string(),
            re,
        })
    }

    /// Does this glob match a workspace-relative, forward-slashed path?
    pub fn is_match(&self, path: &str) -> bool {
        self.re.is_match(path)
    }

    /// The original pattern text, for the report manifest.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

/// Translate a glob into an anchored regex source string.
///
/// The subtle case is `**/`: it must match zero segments as well as many, so
/// `**/*.rs` matches both `a/b/c.rs` and a top-level `c.rs`. That is why it
/// becomes `(?:.*/)?` rather than `.*/`.
pub fn glob_to_regex(pattern: &str) -> Result<String> {
    if pattern.is_empty() {
        return Err(DcError::Comply("glob pattern is empty".to_string()));
    }

    let mut out = String::with_capacity(pattern.len() * 2 + 4);
    out.push('^');

    // Tracks open `{`; used to validate balance and to translate `,` correctly
    // (a comma outside braces is a literal).
    let mut brace_depth: usize = 0;
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                let is_double = i + 1 < chars.len() && chars[i + 1] == '*';
                if is_double {
                    if i + 2 < chars.len() && chars[i + 2] == '/' {
                        // `**/` — zero or more leading segments.
                        out.push_str("(?:.*/)?");
                        i += 3;
                    } else {
                        // A trailing or bare `**` — cross segment boundaries.
                        out.push_str(".*");
                        i += 2;
                    }
                } else {
                    // A single `*` never crosses a `/`.
                    out.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            '{' => {
                brace_depth += 1;
                out.push_str("(?:");
                i += 1;
            }
            '}' => {
                brace_depth = brace_depth
                    .checked_sub(1)
                    .ok_or_else(|| DcError::Comply(format!("glob {pattern:?}: unmatched '}}'")))?;
                out.push(')');
                i += 1;
            }
            ',' if brace_depth > 0 => {
                out.push('|');
                i += 1;
            }
            '[' => {
                // Pass a character class through, but find its end so we do not
                // escape the contents.
                let mut j = i + 1;
                if j < chars.len() && (chars[j] == '!' || chars[j] == '^') {
                    j += 1;
                }
                // A `]` immediately after the opener is a literal `]`.
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j >= chars.len() {
                    return Err(DcError::Comply(format!("glob {pattern:?}: unmatched '['")));
                }
                out.push('[');
                // Glob negation is `!`; regex wants `^`.
                let mut k = i + 1;
                if chars[k] == '!' {
                    out.push('^');
                    k += 1;
                }
                for &cc in &chars[k..j] {
                    out.push(cc);
                }
                out.push(']');
                i = j + 1;
            }
            _ => {
                // Everything else is a literal. `regex::escape` on a single
                // char handles `.`, `+`, `(`, `$`, etc.
                out.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }

    if brace_depth != 0 {
        return Err(DcError::Comply(format!("glob {pattern:?}: unmatched '{{'")));
    }

    out.push('$');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, path: &str) -> bool {
        Glob::new(pattern).expect("compile").is_match(path)
    }

    #[test]
    fn double_star_slash_matches_zero_or_more_segments() {
        // The case that silently breaks naive implementations: `**/*.rs` must
        // match a file at the root, not only nested ones.
        assert!(matches("**/*.rs", "a/b/c.rs"));
        assert!(matches("**/*.rs", "c.rs"));
        assert!(matches("**/*.rs", "a/c.rs"));
        assert!(!matches("**/*.rs", "c.py"));
    }

    #[test]
    fn single_star_does_not_cross_a_slash() {
        assert!(matches("*.rs", "c.rs"));
        assert!(!matches("*.rs", "a/b.rs"));
        assert!(matches("src/*.rs", "src/lib.rs"));
        assert!(!matches("src/*.rs", "src/a/lib.rs"));
    }

    #[test]
    fn bare_double_star_matches_everything() {
        assert!(matches("**/*", "a/b/c.rs"));
        assert!(matches("**/*", "c.rs"));
        assert!(matches("**", "a/b/c.rs"));
    }

    #[test]
    fn question_mark_matches_one_non_slash_char() {
        assert!(matches("?.rs", "a.rs"));
        assert!(!matches("?.rs", "ab.rs"));
        assert!(!matches("?.rs", "/.rs"));
    }

    #[test]
    fn brace_alternation() {
        assert!(matches("**/*.{yml,yaml}", "ci/build.yml"));
        assert!(matches("**/*.{yml,yaml}", "ci/build.yaml"));
        assert!(!matches("**/*.{yml,yaml}", "ci/build.json"));
    }

    #[test]
    fn nested_brace_alternation() {
        // The real CC8.1 pattern shape.
        let p = "{.github/workflows/*.{yml,yaml},.woodpecker/*.yml}";
        assert!(matches(p, ".github/workflows/ci.yml"));
        assert!(matches(p, ".github/workflows/ci.yaml"));
        assert!(matches(p, ".woodpecker/release.yml"));
        assert!(!matches(p, ".github/workflows/ci.json"));
        assert!(!matches(p, "other/ci.yml"));
    }

    #[test]
    fn comma_outside_braces_is_a_literal() {
        assert!(matches("a,b.txt", "a,b.txt"));
        assert!(!matches("a,b.txt", "a.txt"));
    }

    #[test]
    fn metacharacters_are_escaped() {
        // A literal dot must not match an arbitrary character.
        assert!(matches("a.txt", "a.txt"));
        assert!(!matches("a.txt", "axtxt"));
        // Parens, plus and dollar are literals in a glob.
        assert!(matches("a+(b)$.txt", "a+(b)$.txt"));
    }

    #[test]
    fn character_classes_pass_through() {
        assert!(matches("f[ao]o.rs", "foo.rs"));
        assert!(matches("f[ao]o.rs", "fao.rs"));
        assert!(!matches("f[ao]o.rs", "fzo.rs"));
    }

    #[test]
    fn negated_character_class_uses_regex_caret() {
        assert!(matches("f[!x]o.rs", "foo.rs"));
        assert!(!matches("f[!x]o.rs", "fxo.rs"));
    }

    #[test]
    fn dotfiles_and_dot_directories_match() {
        // Compliance evidence lives in dotfiles constantly, so unlike shell
        // globbing there is no special-casing of a leading dot.
        assert!(matches("**/*", ".gitignore"));
        assert!(matches(".github/**/*.yml", ".github/workflows/ci.yml"));
        assert!(matches(".env", ".env"));
    }

    #[test]
    fn anchoring_is_full_path_not_substring() {
        assert!(!matches("b.rs", "a/b.rs"));
        assert!(!matches("a", "abc"));
    }

    #[test]
    fn unmatched_open_brace_is_a_load_error() {
        let err = Glob::new("{a,b").unwrap_err();
        assert!(format!("{err}").contains("unmatched '{'"), "{err}");
    }

    #[test]
    fn unmatched_close_brace_is_a_load_error() {
        let err = Glob::new("a,b}").unwrap_err();
        assert!(format!("{err}").contains("unmatched '}'"), "{err}");
    }

    #[test]
    fn unmatched_bracket_is_a_load_error() {
        let err = Glob::new("f[ao.rs").unwrap_err();
        assert!(format!("{err}").contains("unmatched '['"), "{err}");
    }

    #[test]
    fn empty_glob_is_a_load_error() {
        assert!(Glob::new("").is_err());
    }

    #[test]
    fn pattern_is_retained_for_the_manifest() {
        let g = Glob::new("**/*.rs").expect("compile");
        assert_eq!(g.pattern(), "**/*.rs");
    }
}
