//! Measuring a collection, so `len=N` can be checked.
//!
//! This is a **measurement** pass, deliberately separate from `sc-index`'s symbol
//! graph. That query exists to build a def/ref graph for PageRank and
//! `find_symbol`; it captures functions and types and nothing else. Widening it
//! to carry consts and array arities would inject noise into the repo map and
//! every `find_symbol` result workspace-wide to serve one consumer, and a count
//! has nowhere to live in `SymbolDef` without widening that struct for every
//! caller. So `sc-trace` parses Rust itself, narrowly (spec 17).
//!
//! ## Two truths, and why both are needed
//!
//! ```text
//! pub const ALL: [Phase; 5] = [Specs, Architecture, Layout, Stages, Decomp];
//! //                   ^ declared     ^^^^^^^^^^^^ five elements
//! ```
//!
//! The `5` in the type and the five initializer elements are *independently*
//! checkable, and they are not the same claim. This matters because spec 17's
//! motivating bug — `ThinkPolicy` carrying "a dead array slot sized for the phase
//! that no longer existed" — was precisely a case where the declared length
//! stayed 6 while the live content shrank to 5. A spec-versus-code check alone
//! reports OK whenever the spec also says 6, reproducing the bug rather than
//! catching it. So both are measured and cross-checked.
//!
//! ## What is deliberately not counted
//!
//! `[value; N]` (the repeat expression) has one *element node* but N values.
//! Counting element nodes there would report 1 and fire a false `Stale`. The
//! grammar makes the two forms distinguishable only by an anonymous `;` child, so
//! that is what is tested — and on a repeat form the element count is withheld
//! rather than guessed, leaving the declared length to answer alone.

use tree_sitter::{Node, Parser};

/// What a collection turned out to hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cardinality {
    /// Elements actually present in the initializer. `None` when the form makes
    /// counting them meaningless (the repeat expression).
    pub elements: Option<usize>,
    /// The length written in the type — the `5` in `[Phase; 5]`. `None` for
    /// forms that declare no length (slices, `Vec`, enums).
    pub declared: Option<usize>,
}

impl Cardinality {
    /// Did anything at all get measured? A `Cardinality` where nothing did is
    /// how a `len=` on a function becomes `Unknown` rather than a false verdict.
    pub fn is_measurable(&self) -> bool {
        self.elements.is_some() || self.declared.is_some()
    }
}

/// The verdict on a `len=N` assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LenVerdict {
    /// Everything measurable agrees with N.
    Ok,
    /// A measurable count disagrees with N — the spec is stale.
    Stale { detail: String },
    /// `elements != declared`: the *code* is internally inconsistent — a dead
    /// array slot. Surfaced regardless of what N the spec claimed, because this
    /// is the exact shape of the bug spec 17 was written for.
    Inconsistent { elements: usize, declared: usize },
    /// Nothing was measurable. The checker is limited here, so this is
    /// `Unknown` — never `Broken`, and never a silent pass.
    Unmeasurable { why: String },
}

/// Check `expect` against a measured cardinality.
pub fn verdict(card: &Cardinality, expect: usize) -> LenVerdict {
    // A code-internal inconsistency outranks the spec's claim: a dead slot is
    // wrong whether or not the spec happens to agree with the declared length.
    if let (Some(elements), Some(declared)) = (card.elements, card.declared) {
        if elements != declared {
            return LenVerdict::Inconsistent { elements, declared };
        }
    }
    match (card.elements, card.declared) {
        (None, None) => LenVerdict::Unmeasurable {
            why: "not a countable collection (no array literal, slice or enum body)".to_string(),
        },
        (Some(n), _) | (None, Some(n)) => {
            if n == expect {
                LenVerdict::Ok
            } else {
                let what = if card.elements.is_some() {
                    "elements"
                } else {
                    "declared length"
                };
                LenVerdict::Stale {
                    detail: format!("spec says len={expect}, code has {n} ({what})"),
                }
            }
        }
    }
}

/// Measure the item named `name` in `source`, requiring `owner` when given.
///
/// `None` means the item was not found (or the source did not parse) — which the
/// caller must not confuse with "found, but uncountable". That distinction is the
/// difference between `Broken` and `Unknown`.
pub fn measure(source: &str, name: &str, owner: Option<&str>) -> Option<Cardinality> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let bytes = source.as_bytes();
    // Search and measure in one traversal: the located node borrows the tree, and
    // handing it back across the tree's lifetime buys nothing.
    find_and_measure(tree.root_node(), bytes, name, owner, None)
}

/// Depth-first walk, carrying the enclosing `impl`/`trait` type down as `owner`.
fn find_and_measure(
    node: Node<'_>,
    bytes: &[u8],
    name: &str,
    want_owner: Option<&str>,
    enclosing: Option<String>,
) -> Option<Cardinality> {
    // `impl Phase { … }` / `trait T { … }` — the type name is the owner of every
    // associated item beneath it. This is what makes `Phase::ALL` addressable and
    // distinguishes it from the four other `ALL` consts in this workspace.
    let enclosing = match node.kind() {
        "impl_item" | "trait_item" => named_child_text(node, bytes, "type_identifier")
            .map(Some)
            .unwrap_or(enclosing),
        _ => enclosing,
    };

    if is_measurable_item(node.kind()) && item_name(node, bytes).as_deref() == Some(name) {
        // An anchor naming an owner must match one; an anchor naming none accepts
        // any. Requiring a free item for an owner-less anchor would reject
        // `sc_comply::Section::ALL` written without its type, which is a real
        // spelling a human would use.
        let owner_ok = match want_owner {
            Some(want) => enclosing.as_deref() == Some(want),
            None => true,
        };
        if owner_ok {
            return Some(measure_item(node, bytes));
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_and_measure(child, bytes, name, want_owner, enclosing.clone()) {
            return Some(found);
        }
    }
    None
}

/// Item kinds a `len=` assertion could conceivably be about.
fn is_measurable_item(kind: &str) -> bool {
    matches!(kind, "const_item" | "static_item" | "enum_item")
}

/// The declared name of a const/static/enum.
fn item_name(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let wanted = match node.kind() {
        "enum_item" => "type_identifier",
        _ => "identifier",
    };
    named_child_text(node, bytes, wanted)
}

/// Measure one located item.
fn measure_item(node: Node<'_>, bytes: &[u8]) -> Cardinality {
    if node.kind() == "enum_item" {
        // An enum's "length" is its variant count. No declared length exists.
        return Cardinality {
            elements: child_of_kind(node, "enum_variant_list")
                .map(|list| count_children_of_kind(list, "enum_variant")),
            declared: None,
        };
    }

    let mut card = Cardinality::default();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            // `[Phase; 5]` — possibly behind a `&'static` reference.
            "array_type" => card.declared = declared_len(child, bytes),
            "reference_type" => {
                if let Some(inner) = child_of_kind(child, "array_type") {
                    card.declared = declared_len(inner, bytes);
                }
            }
            // `[a, b, c]` — possibly behind a `&`.
            "array_expression" => card.elements = element_count(child),
            "reference_expression" => {
                if let Some(inner) = child_of_kind(child, "array_expression") {
                    card.elements = element_count(inner);
                }
            }
            _ => {}
        }
    }
    card
}

/// The `N` in `[T; N]`, when the type declares one. A slice type `[T]` has no
/// `integer_literal` child and correctly yields `None`.
fn declared_len(array_type: Node<'_>, bytes: &[u8]) -> Option<usize> {
    child_of_kind(array_type, "integer_literal")
        .and_then(|n| n.utf8_text(bytes).ok())
        .and_then(parse_int_literal)
}

/// How many elements an `array_expression` holds.
///
/// `None` for the repeat form `[value; N]`, which the grammar distinguishes only
/// by an anonymous `;` child — its two named children are indistinguishable from
/// a two-element array, so counting them would fire a false `Stale`.
fn element_count(array_expr: Node<'_>) -> Option<usize> {
    let mut cursor = array_expr.walk();
    if array_expr
        .children(&mut cursor)
        .any(|c| !c.is_named() && c.kind() == ";")
    {
        return None;
    }
    Some(array_expr.named_child_count())
}

/// `5`, `5usize`, `1_000` → a number. Suffixes and separators are Rust-legal in
/// an array length, so parsing must survive them.
fn parse_int_literal(text: &str) -> Option<usize> {
    let digits: String = text
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    digits.parse().ok()
}

fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

fn count_children_of_kind(node: Node<'_>, kind: &str) -> usize {
    let mut cursor = node.walk();
    let n = node
        .children(&mut cursor)
        .filter(|c| c.kind() == kind)
        .count();
    n
}

fn named_child_text(node: Node<'_>, bytes: &[u8], kind: &str) -> Option<String> {
    child_of_kind(node, kind)
        .and_then(|n| n.utf8_text(bytes).ok())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shape from `crates/sc-workflow/src/phase.rs`.
    const PHASE_ALL: &str = "\
impl Phase {
    pub const ALL: [Phase; 5] = [
        Phase::Specs,
        Phase::Architecture,
        Phase::Layout,
        Phase::StageBreakdown,
        Phase::WorkDecomposition,
    ];
}
";

    #[test]
    fn measures_the_flagship_const_both_ways() {
        let card = measure(PHASE_ALL, "ALL", Some("Phase")).expect("found");
        assert_eq!(card.elements, Some(5));
        assert_eq!(card.declared, Some(5));
        assert_eq!(verdict(&card, 5), LenVerdict::Ok);
    }

    #[test]
    fn a_dead_array_slot_is_caught_whatever_the_spec_claims() {
        // THE regression test the whole spec exists for. `ThinkPolicy` carried
        // "a dead array slot sized for the phase that no longer existed": the
        // declared length stayed 6 while the live content shrank to 5.
        //
        // A spec-versus-code check alone would report OK here whenever the spec
        // also said 6 — reproducing the bug instead of catching it.
        let dead_slot = "\
impl Phase {
    pub const ALL: [Phase; 6] = [
        Phase::Specs,
        Phase::Architecture,
        Phase::Layout,
        Phase::StageBreakdown,
        Phase::WorkDecomposition,
    ];
}
";
        let card = measure(dead_slot, "ALL", Some("Phase")).expect("found");
        assert_eq!(card.elements, Some(5));
        assert_eq!(card.declared, Some(6));

        // Flagged even when the spec agrees with the (wrong) declared length.
        assert_eq!(
            verdict(&card, 6),
            LenVerdict::Inconsistent {
                elements: 5,
                declared: 6
            }
        );
        // And when it agrees with the element count.
        assert_eq!(
            verdict(&card, 5),
            LenVerdict::Inconsistent {
                elements: 5,
                declared: 6
            }
        );
    }

    #[test]
    fn a_spec_claiming_the_wrong_count_is_stale() {
        let card = measure(PHASE_ALL, "ALL", Some("Phase")).unwrap();
        match verdict(&card, 6) {
            LenVerdict::Stale { detail } => {
                assert!(detail.contains("len=6"), "{detail}");
                assert!(detail.contains('5'), "{detail}");
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn the_repeat_form_declines_to_count_rather_than_reporting_one() {
        // `[0u8; 5]` has ONE element node but five values. Counting named
        // children would report 2 here (the value and the length literal) and
        // fire a false Stale — the trap this rule exists to avoid.
        let src = "const REPEATED: [u8; 5] = [0u8; 5];";
        let card = measure(src, "REPEATED", None).expect("found");
        assert_eq!(
            card.elements, None,
            "element count is withheld, not guessed"
        );
        assert_eq!(card.declared, Some(5));
        // The declared length answers alone, so the assertion still checks.
        assert_eq!(verdict(&card, 5), LenVerdict::Ok);
        assert!(matches!(verdict(&card, 3), LenVerdict::Stale { .. }));
    }

    #[test]
    fn owner_disambiguates_the_five_all_consts_in_this_workspace() {
        // Real: Phase::ALL, Lens::ALL, Provider::ALL, ComplyModel::ALL and
        // Section::ALL all exist. Without owner matching they are one name.
        let src = "\
impl Phase {
    pub const ALL: [Phase; 5] = [A, B, C, D, E];
}
impl Lens {
    pub const ALL: [Lens; 4] = [A, B, C, D];
}
";
        let phase = measure(src, "ALL", Some("Phase")).unwrap();
        let lens = measure(src, "ALL", Some("Lens")).unwrap();
        assert_eq!(verdict(&phase, 5), LenVerdict::Ok);
        assert_eq!(verdict(&lens, 4), LenVerdict::Ok);
        // And crucially they are not each other.
        assert!(matches!(verdict(&phase, 4), LenVerdict::Stale { .. }));

        // An owner nobody declares finds nothing at all.
        assert!(measure(src, "ALL", Some("Ghost")).is_none());
    }

    #[test]
    fn a_static_slice_const_counts_its_elements() {
        // `Section::ALL` in sc-comply: `&'static [Section] = &[...]`. The type
        // declares no length, so elements answer alone.
        let src = "\
impl Section {
    pub const ALL: &'static [Section] = &[
        Section::Code,
        Section::Infrastructure,
        Section::Documentation,
        Section::Organizational,
    ];
}
";
        let card = measure(src, "ALL", Some("Section")).expect("found");
        assert_eq!(card.elements, Some(4));
        assert_eq!(card.declared, None, "a slice type declares no length");
        assert_eq!(verdict(&card, 4), LenVerdict::Ok);
    }

    #[test]
    fn an_enum_is_measured_by_its_variants() {
        let src = "pub enum Colour { Red, Green, Blue }";
        let card = measure(src, "Colour", None).expect("found");
        assert_eq!(card.elements, Some(3));
        assert_eq!(card.declared, None);
        assert_eq!(verdict(&card, 3), LenVerdict::Ok);
    }

    #[test]
    fn len_on_something_uncountable_is_unmeasurable_never_broken() {
        // A `len=` on a function is a spec author's mistake, but the honest
        // answer is "the checker cannot measure this", not "the code is gone".
        let src = "const NAME: &str = \"hello\";";
        let card = measure(src, "NAME", None).expect("the item IS there");
        assert!(!card.is_measurable());
        assert!(matches!(verdict(&card, 5), LenVerdict::Unmeasurable { .. }));
    }

    #[test]
    fn a_missing_item_is_not_found_at_all() {
        // Distinct from "found but uncountable" — this is what becomes Broken.
        assert!(measure(PHASE_ALL, "GHOST", None).is_none());
    }

    #[test]
    fn unparseable_source_yields_nothing_and_does_not_panic() {
        assert!(measure("@@@ not ::: rust {{{", "ALL", None).is_none());
        assert!(measure("", "ALL", None).is_none());
    }

    #[test]
    fn suffixed_and_separated_lengths_parse() {
        assert_eq!(parse_int_literal("5"), Some(5));
        assert_eq!(parse_int_literal("5usize"), Some(5));
        assert_eq!(parse_int_literal("1_000"), Some(1000));
        assert_eq!(parse_int_literal("N"), None);
    }

    #[test]
    fn measures_the_real_phase_all_from_this_repo() {
        // Against the actual file. If someone adds a sixth phase, this fails —
        // which is the entire point of the tool, applied to itself.
        let path = crate::test_support::repo_root().join("crates/sc-workflow/src/phase.rs");
        let src = std::fs::read_to_string(path).expect("phase.rs is readable");
        let card = measure(&src, "ALL", Some("Phase")).expect("Phase::ALL exists");
        assert_eq!(card.elements, Some(5), "five phases, not six");
        assert_eq!(card.declared, Some(5));
    }
}
