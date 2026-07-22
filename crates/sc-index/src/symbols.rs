//! Symbol extraction via tree-sitter (spec 05 — the PageRank repo map).
//!
//! For each source file we pull two things with a tree-sitter query:
//!
//! * **definitions** — functions, methods, structs/enums/traits (Rust),
//!   functions and classes (Python): the symbols other code can depend on.
//! * **references** — identifier uses, so we can later draw an edge from the
//!   file that *uses* a name to the file that *defines* it.
//!
//! This is the raw material for the symbol-dependency graph that PageRank scores.
//! Languages are pluggable: add a grammar + its def/ref node kinds and the rest of
//! the index works unchanged.

use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

/// A supported source language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    CSharp,
}

impl Language {
    /// Infer a language from a file extension, if supported.
    pub fn from_path(path: &str) -> Option<Language> {
        let ext = path.rsplit('.').next()?;
        match ext {
            "rs" => Some(Language::Rust),
            "py" => Some(Language::Python),
            "cs" => Some(Language::CSharp),
            _ => None,
        }
    }

    fn ts_language(self) -> tree_sitter::Language {
        match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        }
    }

    /// A tree-sitter query capturing `@def.name` (definition names) and
    /// `@ref` (identifier references).
    fn query_src(self) -> &'static str {
        match self {
            Language::Rust => {
                "(function_item name: (identifier) @def.name)
                 (struct_item name: (type_identifier) @def.name)
                 (enum_item name: (type_identifier) @def.name)
                 (trait_item name: (type_identifier) @def.name)
                 (function_signature_item name: (identifier) @def.name)
                 (call_expression function: (identifier) @ref)
                 (type_identifier) @ref"
            }
            Language::Python => {
                "(function_definition name: (identifier) @def.name)
                 (class_definition name: (identifier) @def.name)
                 (call function: (identifier) @ref)"
            }
            Language::CSharp => {
                "(class_declaration name: (identifier) @def.name)
                 (struct_declaration name: (identifier) @def.name)
                 (interface_declaration name: (identifier) @def.name)
                 (enum_declaration name: (identifier) @def.name)
                 (method_declaration name: (identifier) @def.name)
                 (invocation_expression function: (identifier) @ref)
                 (identifier) @ref"
            }
        }
    }
}

/// A definition found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDef {
    pub name: String,
    /// 1-based line of the definition.
    pub line: usize,
}

/// The symbols extracted from one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSymbols {
    pub defs: Vec<SymbolDef>,
    /// Distinct identifier names referenced in the file.
    pub refs: Vec<String>,
}

/// Parse `source` of `lang` and extract its definitions and references.
/// Returns an empty set (never an error) if parsing or the query fails, so a
/// single unparseable file can't break indexing.
pub fn extract_symbols(lang: Language, source: &str) -> FileSymbols {
    let ts_lang = lang.ts_language();
    let mut parser = Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        return FileSymbols::default();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return FileSymbols::default(),
    };
    let query = match Query::new(&ts_lang, lang.query_src()) {
        Ok(q) => q,
        Err(_) => return FileSymbols::default(),
    };

    let def_idx: Vec<u32> = capture_indices(&query, "def.name");
    let bytes = source.as_bytes();

    let mut out = FileSymbols::default();
    let mut seen_refs = std::collections::BTreeSet::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), bytes);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let text = cap.node.utf8_text(bytes).unwrap_or_default().to_string();
            if text.is_empty() {
                continue;
            }
            if def_idx.contains(&cap.index) {
                let line = cap.node.start_position().row + 1;
                out.defs.push(SymbolDef { name: text, line });
            } else if seen_refs.insert(text.clone()) {
                out.refs.push(text);
            }
        }
    }
    out
}

/// The 1-based, inclusive line span `(start, end)` of the function/method named `name` in
/// `source`, via tree-sitter — the primitive behind function-level read/edit. Matches the
/// FULL definition node (e.g. Rust `function_item`), so the span covers the whole function
/// body, not just its signature line. `None` if there's no such function or parsing fails.
///
/// If several functions share the name (overloads, methods on different impls), the FIRST in
/// source order is returned — the tools that use this note the ambiguity to the caller.
pub fn function_span(lang: Language, source: &str, name: &str) -> Option<(usize, usize)> {
    let ts_lang = lang.ts_language();
    let mut parser = Parser::new();
    parser.set_language(&ts_lang).ok()?;
    let tree = parser.parse(source, None)?;
    let bytes = source.as_bytes();

    // The node kinds that are "a function/method definition" per language, and the field that
    // holds the name. We walk the whole tree and return the first def node whose name matches.
    let (def_kinds, name_field): (&[&str], &str) = match lang {
        Language::Rust => (&["function_item"], "name"),
        Language::Python => (&["function_definition"], "name"),
        Language::CSharp => (&["method_declaration", "local_function_statement"], "name"),
    };
    find_fn_span(tree.root_node(), bytes, def_kinds, name_field, name)
}

/// Recursive pre-order search for the first def node named `name`, returning its 1-based
/// inclusive line span. Pre-order visitation means the earliest definition in source order
/// (and the outermost when nested) wins.
fn find_fn_span(
    node: tree_sitter::Node,
    bytes: &[u8],
    def_kinds: &[&str],
    name_field: &str,
    name: &str,
) -> Option<(usize, usize)> {
    if def_kinds.contains(&node.kind()) {
        if let Some(name_node) = node.child_by_field_name(name_field) {
            if name_node.utf8_text(bytes).ok() == Some(name) {
                return Some((node.start_position().row + 1, node.end_position().row + 1));
            }
        }
    }
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if let Some(span) = find_fn_span(c, bytes, def_kinds, name_field, name) {
            return Some(span);
        }
    }
    None
}

/// Count the functions/methods named `name` in `source`, so a caller can warn when a name is
/// ambiguous (the [`function_span`] it edits is only the first).
pub fn count_functions_named(lang: Language, source: &str, name: &str) -> usize {
    extract_symbols(lang, source)
        .defs
        .iter()
        .filter(|d| d.name == name)
        .count()
}

fn capture_indices(query: &Query, name: &str) -> Vec<u32> {
    query
        .capture_names()
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == name)
        .map(|(i, _)| i as u32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_language_from_extension() {
        assert_eq!(Language::from_path("src/a.rs"), Some(Language::Rust));
        assert_eq!(Language::from_path("x/y.py"), Some(Language::Python));
        assert_eq!(Language::from_path("README.md"), None);
        assert_eq!(Language::from_path("noext"), None);
    }

    #[test]
    fn function_span_covers_the_whole_rust_fn() {
        let src = "\
fn first() -> u32 {
    1
}

fn target(x: u32) -> u32 {
    let y = x + 1;
    y * 2
}

fn last() {}
";
        // `target` spans its full body (lines 5..=8, 1-based), not just the signature.
        assert_eq!(function_span(Language::Rust, src, "target"), Some((5, 8)));
        assert_eq!(function_span(Language::Rust, src, "first"), Some((1, 3)));
        assert_eq!(function_span(Language::Rust, src, "nope"), None);
    }

    #[test]
    fn function_span_handles_python_and_counts_dupes() {
        let src = "\
def a():
    return 1

def a():
    return 2
";
        // First definition in source order wins.
        assert_eq!(function_span(Language::Python, src, "a"), Some((1, 2)));
        assert_eq!(count_functions_named(Language::Python, src, "a"), 2);
        assert_eq!(count_functions_named(Language::Python, src, "a"), 2);
    }

    #[test]
    fn extracts_rust_defs_and_refs() {
        let src = "\
fn helper() -> u32 { 1 }
struct Widget { n: u32 }
fn main() {
    let w = Widget { n: helper() };
}
";
        let syms = extract_symbols(Language::Rust, src);
        let names: Vec<&str> = syms.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"helper"), "{names:?}");
        assert!(names.contains(&"Widget"), "{names:?}");
        assert!(names.contains(&"main"), "{names:?}");
        // helper() is called -> a reference; Widget is used as a type -> a reference.
        assert!(syms.refs.contains(&"helper".to_string()), "{:?}", syms.refs);
        assert!(syms.refs.contains(&"Widget".to_string()), "{:?}", syms.refs);
    }

    #[test]
    fn records_definition_line_numbers() {
        let src = "fn a() {}\nfn b() {}\n";
        let syms = extract_symbols(Language::Rust, src);
        let b = syms.defs.iter().find(|d| d.name == "b").unwrap();
        assert_eq!(b.line, 2);
    }

    #[test]
    fn extracts_python_defs() {
        let src = "\
def helper():
    return 1

class Widget:
    def method(self):
        return helper()
";
        let syms = extract_symbols(Language::Python, src);
        let names: Vec<&str> = syms.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"helper"), "{names:?}");
        assert!(names.contains(&"Widget"), "{names:?}");
        assert!(names.contains(&"method"), "{names:?}");
        assert!(syms.refs.contains(&"helper".to_string()), "{:?}", syms.refs);
    }

    #[test]
    fn extracts_csharp_defs() {
        let src = "\
using UnityEngine;

public class Player : MonoBehaviour {
    void Start() {
        Move();
    }
    void Move() {}
}
";
        let syms = extract_symbols(Language::CSharp, src);
        let names: Vec<&str> = syms.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"Player"), "{names:?}");
        assert!(names.contains(&"Start"), "{names:?}");
        assert!(names.contains(&"Move"), "{names:?}");
    }

    #[test]
    fn unparseable_input_yields_no_definitions_and_does_not_panic() {
        // tree-sitter is error-tolerant: garbage parses into an error tree. We must
        // not crash, and must not hallucinate definitions out of noise.
        let syms = extract_symbols(Language::Rust, "@@@ not ::: rust {{{");
        assert!(
            syms.defs.is_empty(),
            "no defs from garbage: {:?}",
            syms.defs
        );
    }

    #[test]
    fn empty_source_is_empty() {
        assert_eq!(extract_symbols(Language::Rust, ""), FileSymbols::default());
    }
}
