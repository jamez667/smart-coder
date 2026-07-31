//! The anchor grammar: `<!--@ … -->` in spec Markdown → a checkable claim.
//!
//! An anchor is an HTML comment, so specs stay readable prose for a human — the
//! primary audience — and the checker reads only the anchors. Three kinds,
//! deliberately few (spec 17):
//!
//! ```text
//! <!--@ crates/sc-web/src/mirror_server.rs -->   a path exists
//! <!--@ sc_web::mint_token -->                   a symbol exists
//! <!--@ sc_workflow::Phase::ALL len=5 -->        …and a collection has 5 members
//! ```
//!
//! Two rules the parser must not bend:
//!
//! * **A malformed anchor is retained, not dropped.** It becomes an `Unknown`
//!   claim with a note. An anchor that vanishes on a typo is a check that lies by
//!   omission — the spec would read as governed while nothing verified it.
//! * **Path vs symbol is decided, never guessed.** `/` or a file extension means
//!   a path; `::` means a symbol. A token with neither is malformed, and saying
//!   so is better than picking one and being confidently wrong.

use serde::{Deserialize, Serialize};

/// A parsed symbol path, split into the parts that carry different reliability.
///
/// The asymmetry is the whole design: the **crate segment is reliable** (it maps
/// to a workspace member, verifiably), while **module segments are not** —
/// re-exports mean a symbol's use-path routinely differs from its file path
/// (`sc_workflow::artifact_dirs` lives in `artifact_dir.rs`). So module segments
/// never reject a candidate; they only break ties.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRef {
    /// First segment: `sc_workflow`. Required.
    pub crate_seg: String,
    /// Middle segments: `config::types`. Advisory only.
    pub module_segs: Vec<String>,
    /// Final segment: `artifact_dirs`, `UiConfig`, `ALL`.
    pub name: String,
    /// The type that owns an associated item — `Phase` in `Phase::ALL`.
    ///
    /// Inferred when the second-to-last segment is `TypeCased` and therefore
    /// cannot be a module (Rust modules are snake_case by convention, and every
    /// module in this workspace follows it). This is what makes `Phase::ALL`
    /// addressable at all: five distinct `ALL` consts exist across four crates
    /// here, and without an owner they are indistinguishable.
    pub owner: Option<String>,
}

impl SymbolRef {
    /// The anchor text this was parsed from, reconstructed for messages.
    pub fn display_path(&self) -> String {
        let mut segs = vec![self.crate_seg.clone()];
        segs.extend(self.module_segs.iter().cloned());
        if let Some(owner) = &self.owner {
            segs.push(owner.clone());
        }
        segs.push(self.name.clone());
        segs.join("::")
    }
}

/// What an anchor asserts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnchorKind {
    /// This path exists, and this spec governs it. Language-agnostic.
    Path { path: String },
    /// This symbol exists.
    Symbol { sym: SymbolRef },
    /// This symbol exists and names a collection with `expect` members.
    SymbolLen { sym: SymbolRef, expect: usize },
    /// The anchor could not be parsed. Retained rather than dropped, and
    /// resolved as `Unknown` — never silently discarded.
    Malformed { why: String },
}

/// One anchor, located in the spec that wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub kind: AnchorKind,
    /// Workspace-relative spec path, `/`-separated.
    pub spec: String,
    /// 1-based line in the spec.
    pub line: usize,
    /// The verbatim inner text, so an error can quote what the human wrote.
    pub raw: String,
}

impl Anchor {
    /// What this anchor points at, for the report's target column.
    pub fn target(&self) -> String {
        match &self.kind {
            AnchorKind::Path { path } => path.clone(),
            AnchorKind::Symbol { sym } => sym.display_path(),
            AnchorKind::SymbolLen { sym, expect } => format!("{} len={expect}", sym.display_path()),
            AnchorKind::Malformed { .. } => self.raw.clone(),
        }
    }
}

const OPEN: &str = "<!--@";
const CLOSE: &str = "-->";

/// Extensions that mark a bare token as a path even without a `/` — so an anchor
/// naming a file in the repo root still resolves as one.
const PATH_EXTENSIONS: &[&str] = &[
    ".rs", ".py", ".cs", ".md", ".toml", ".json", ".html", ".js", ".sh", ".ps1", ".yml", ".yaml",
    ".txt", ".css",
];

/// Parse every anchor in one spec document.
///
/// Anchors inside fenced code blocks **are** parsed. Spec 17's own example
/// anchor sits in a fence and points at real code, so skipping fences would make
/// the spec that defines anchors the one document that cannot be checked. The
/// cost is that a future illustrative anchor pointing at a deliberately fake
/// symbol would report `Broken`; the escape hatch if that ever bites is a
/// distinct marker, not a fence rule.
pub fn parse_anchors(spec_path: &str, contents: &str) -> Vec<Anchor> {
    let mut out = Vec::new();
    for (i, text) in contents.lines().enumerate() {
        let line = i + 1;
        let mut rest = text;
        // Several anchors can share a line; each keeps its own position.
        while let Some(start) = rest.find(OPEN) {
            let after = &rest[start + OPEN.len()..];
            let Some(end) = after.find(CLOSE) else {
                // An unterminated `<!--@` is malformed, and is reported as such
                // rather than swallowing the remainder of the line.
                out.push(Anchor {
                    kind: AnchorKind::Malformed {
                        why: "unterminated anchor: no closing `-->`".to_string(),
                    },
                    spec: spec_path.to_string(),
                    line,
                    raw: after.trim().to_string(),
                });
                break;
            };
            let raw = after[..end].trim().to_string();
            out.push(Anchor {
                kind: parse_kind(&raw),
                spec: spec_path.to_string(),
                line,
                raw,
            });
            rest = &after[end + CLOSE.len()..];
        }
    }
    out
}

/// Classify one anchor's inner text.
fn parse_kind(raw: &str) -> AnchorKind {
    let mut parts = raw.split_whitespace();
    let Some(target) = parts.next() else {
        return AnchorKind::Malformed {
            why: "empty anchor".to_string(),
        };
    };

    // `len=N` is the only assertion today. Anything else after the target is a
    // typo worth surfacing rather than ignoring — a silently-dropped assertion
    // is a claim the reader believes is checked.
    let mut expect: Option<usize> = None;
    for extra in parts {
        let Some(value) = extra.strip_prefix("len=") else {
            return AnchorKind::Malformed {
                why: format!("unknown assertion {extra:?} (only `len=N` is supported)"),
            };
        };
        match value.parse::<usize>() {
            Ok(n) => expect = Some(n),
            Err(_) => {
                return AnchorKind::Malformed {
                    why: format!("`len=` needs a number, got {value:?}"),
                }
            }
        }
    }

    let looks_like_path =
        target.contains('/') || PATH_EXTENSIONS.iter().any(|e| target.ends_with(e));

    if looks_like_path {
        if expect.is_some() {
            return AnchorKind::Malformed {
                why: "`len=` applies to a symbol, not a path".to_string(),
            };
        }
        return AnchorKind::Path {
            path: target.replace('\\', "/"),
        };
    }

    if !target.contains("::") {
        return AnchorKind::Malformed {
            why: format!(
                "{target:?} is neither a path (needs `/` or a file extension) \
                 nor a symbol (needs `::`)"
            ),
        };
    }

    let Some(sym) = parse_symbol(target) else {
        return AnchorKind::Malformed {
            why: format!("could not parse {target:?} as a symbol path"),
        };
    };
    match expect {
        Some(expect) => AnchorKind::SymbolLen { sym, expect },
        None => AnchorKind::Symbol { sym },
    }
}

/// Split `sc_win::config::types::UiConfig` into its parts.
fn parse_symbol(target: &str) -> Option<SymbolRef> {
    let segs: Vec<&str> = target.split("::").filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return None;
    }
    let crate_seg = segs[0].to_string();
    let name = segs[segs.len() - 1].to_string();
    let mut middle: Vec<String> = segs[1..segs.len() - 1]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // A TypeCased second-to-last segment is the owning type, not a module: Rust
    // modules are snake_case and every module in this workspace follows that.
    // `Phase::ALL` → owner `Phase`; `config::types::UiConfig` → all modules.
    let owner = match middle.last() {
        Some(last) if is_type_cased(last) => middle.pop(),
        _ => None,
    };

    Some(SymbolRef {
        crate_seg,
        module_segs: middle,
        name,
        owner,
    })
}

/// Does this segment look like a type name rather than a module?
fn is_type_cased(seg: &str) -> bool {
    seg.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(contents: &str) -> Vec<AnchorKind> {
        parse_anchors("docs/specs/x.md", contents)
            .into_iter()
            .map(|a| a.kind)
            .collect()
    }

    #[test]
    fn parses_the_three_forms_from_the_spec_verbatim() {
        // Copied from real anchors in docs/specs/17-20.
        let doc = "\
A path <!--@ crates/sc-web/src/mirror_server.rs --> here.
A symbol <!--@ sc_web::mint_token --> here.
The pipeline runs five phases <!--@ sc_workflow::Phase::ALL len=5 -->
";
        let k = kinds(doc);
        assert_eq!(
            k[0],
            AnchorKind::Path {
                path: "crates/sc-web/src/mirror_server.rs".into()
            }
        );
        match &k[1] {
            AnchorKind::Symbol { sym } => {
                assert_eq!(sym.crate_seg, "sc_web");
                assert_eq!(sym.name, "mint_token");
                assert!(sym.owner.is_none(), "a free fn has no owner");
                assert!(sym.module_segs.is_empty());
            }
            other => panic!("expected Symbol, got {other:?}"),
        }
        match &k[2] {
            AnchorKind::SymbolLen { sym, expect } => {
                assert_eq!(*expect, 5);
                assert_eq!(sym.crate_seg, "sc_workflow");
                assert_eq!(sym.name, "ALL");
                assert_eq!(
                    sym.owner.as_deref(),
                    Some("Phase"),
                    "the owner is what makes ALL addressable"
                );
            }
            other => panic!("expected SymbolLen, got {other:?}"),
        }
    }

    #[test]
    fn a_type_cased_segment_is_an_owner_and_a_snake_cased_one_is_a_module() {
        // `sc_win::config::types::UiConfig` — all middle segments are modules.
        let k = kinds("<!--@ sc_win::config::types::UiConfig -->");
        match &k[0] {
            AnchorKind::Symbol { sym } => {
                assert_eq!(sym.crate_seg, "sc_win");
                assert_eq!(sym.module_segs, vec!["config", "types"]);
                assert_eq!(sym.name, "UiConfig");
                assert!(sym.owner.is_none(), "`types` is a module, not an owner");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_malformed_anchor_is_retained_not_dropped() {
        // The check must not lie by omission: an anchor that vanishes on a typo
        // leaves the spec reading as governed while nothing verifies it.
        let cases = [
            "<!--@ -->",
            "<!--@ justaword -->",
            "<!--@ sc_web::mint_token len=abc -->",
            "<!--@ sc_web::mint_token count=3 -->",
            "<!--@ crates/a.rs len=2 -->",
        ];
        for case in cases {
            let k = kinds(case);
            assert_eq!(k.len(), 1, "anchor dropped entirely: {case}");
            assert!(
                matches!(k[0], AnchorKind::Malformed { .. }),
                "{case} → {:?}",
                k[0]
            );
        }
    }

    #[test]
    fn an_unterminated_anchor_is_malformed_rather_than_eating_the_line() {
        let k = kinds("text <!--@ sc_web::mint_token and then nothing");
        assert_eq!(k.len(), 1);
        assert!(matches!(k[0], AnchorKind::Malformed { .. }), "{:?}", k[0]);
    }

    #[test]
    fn path_versus_symbol_is_decided_not_guessed() {
        // A `/` or a known extension → path.
        assert!(matches!(
            kinds("<!--@ crates/sc-web/src/dashboard.html -->")[0],
            AnchorKind::Path { .. }
        ));
        assert!(matches!(
            kinds("<!--@ Cargo.toml -->")[0],
            AnchorKind::Path { .. }
        ));
        // `::` → symbol.
        assert!(matches!(
            kinds("<!--@ sc_index::find_symbol -->")[0],
            AnchorKind::Symbol { .. }
        ));
        // Neither → malformed, rather than a confident wrong guess.
        assert!(matches!(
            kinds("<!--@ mint_token -->")[0],
            AnchorKind::Malformed { .. }
        ));
    }

    #[test]
    fn records_the_line_and_handles_several_anchors_on_one_line() {
        let doc = "first line\nboth <!--@ sc_a::x --> and <!--@ sc_b::y --> here\n";
        let anchors = parse_anchors("docs/specs/x.md", doc);
        assert_eq!(anchors.len(), 2);
        assert!(anchors.iter().all(|a| a.line == 2), "{anchors:?}");
        assert_eq!(anchors[0].spec, "docs/specs/x.md");
    }

    #[test]
    fn anchors_inside_a_code_fence_are_still_parsed() {
        // Spec 17's own example anchor sits in a fence and points at real code.
        // Skipping fences would make the spec defining anchors the one document
        // that cannot be checked.
        let doc = "```markdown\nThe pipeline runs five phases <!--@ sc_workflow::Phase::ALL len=5 -->\n```\n";
        assert_eq!(kinds(doc).len(), 1);
    }

    #[test]
    fn a_document_with_no_anchors_yields_none() {
        assert!(parse_anchors("docs/specs/x.md", "# Just prose\n\nNo anchors.\n").is_empty());
    }

    #[test]
    fn backslash_paths_are_normalized() {
        match &kinds("<!--@ crates\\sc-web\\src\\lib.rs -->")[0] {
            AnchorKind::Path { path } => assert_eq!(path, "crates/sc-web/src/lib.rs"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn target_renders_what_the_anchor_pointed_at() {
        let anchors = parse_anchors(
            "s.md",
            "<!--@ sc_workflow::Phase::ALL len=5 -->\n<!--@ crates/a.rs -->",
        );
        assert_eq!(anchors[0].target(), "sc_workflow::Phase::ALL len=5");
        assert_eq!(anchors[1].target(), "crates/a.rs");
    }
}
