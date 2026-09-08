//! Generate machine-enforceable constraints from the registry (spec 02/04).
//!
//! Two producers, both derived from the same [`ToolSpec`]s so the constraint can
//! never drift from validation:
//!
//! * [`params_json_schema`] — a JSON-Schema object for one tool's parameters,
//!   used to build the OpenAI-style function definitions (native FC).
//! * [`registry_gbnf`] — a single GBNF grammar whose language is exactly the set
//!   of valid tool-call JSON objects, for llama.cpp constrained decoding. This is
//!   the strongest guarantee: malformed calls become *impossible* by construction
//!   rather than caught after the fact (spec 02).

use serde_json::{json, Value};

use crate::spec::{ParamType, ToolRegistry, ToolSpec};

/// A JSON-Schema `object` describing a tool's parameters: typed properties plus a
/// `required` list. Used to populate an OpenAI `function.parameters` field.
pub fn params_json_schema(spec: &ToolSpec) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for p in &spec.params {
        let ty = match p.ty {
            ParamType::String | ParamType::OptionalString => "string",
            ParamType::Integer | ParamType::OptionalInteger => "integer",
        };
        properties.insert(
            p.name.to_string(),
            json!({"type": ty, "description": p.description}),
        );
        if p.ty.required() {
            required.push(Value::String(p.name.to_string()));
        }
    }
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": Value::Array(required),
        // Strict schemas: no kitchen-sink extra fields (spec 04).
        "additionalProperties": false
    })
}

/// A GBNF grammar (llama.cpp) whose language is the set of valid tool-call JSON
/// objects for the whole registry: `root ::= tool_a | tool_b | ...`, each
/// alternative pinning the `"tool"` value and the exact object shape.
///
/// We hand-roll the small grammar rather than depend on a JSON-Schema→GBNF
/// converter: the tool surface is tiny and this keeps the output readable and the
/// crate dependency-light. Optional params are modelled as present-or-absent.
pub fn registry_gbnf(registry: &ToolRegistry) -> String {
    build_gbnf(registry, None)
}

/// **Experiment, opt in.** The same tool-call grammar with a bounded, unconstrained
/// scratchpad in front of the call: `root ::= think? scratch? call`.
///
/// Spec 02's alignment-tax caveat says to constrain only the envelope and let the
/// model reason first. The strict [`registry_gbnf`] forbids *any* prose, and the
/// investigate path adopted it for exactly that reason -- unconstrained reasoning
/// ran to the token cap. This variant sits between the two: reasoning is allowed but
/// **bounded in the grammar itself**, so rambling past `max_chars` is impossible
/// rather than merely discouraged. Whether that buys accuracy without giving the
/// token budget back is the thing to measure; nothing selects this by default.
///
/// The prefix, in the position before the object:
///
/// * `think` -- an optional `<think>…</think>` block for reasoning models whose
///   template opens one, bounded to `max_chars` characters with no `<` inside (the
///   closing tag is the only delimiter, so `<` cannot be allowed to recur).
/// * `scratch` -- an optional run of at most `max_chars` characters that cannot
///   contain `{`, terminated by a newline. Because `{` is excluded, the first `{` in
///   the reply is unambiguously the start of the tool object.
///
/// The bound is written as GBNF's `{m,n}` repetition range (`[^{]{0,N}`), which
/// llama.cpp has parsed since PR #6640 (June 2024, b3091 and later). An older server
/// rejects the grammar outright rather than silently ignoring the bound; a chained
/// `char?` expansion would work everywhere but makes a 400-wide grammar unreadable,
/// and every server this project talks to is far newer than that.
pub fn registry_gbnf_with_scratchpad(registry: &ToolRegistry, max_chars: usize) -> String {
    build_gbnf(registry, Some(max_chars))
}

/// One generator for both shapes so the call alternatives and shared terminals can
/// never drift between them: the strict grammar is the `None` case, byte for byte.
fn build_gbnf(registry: &ToolRegistry, scratchpad: Option<usize>) -> String {
    let mut out = String::new();
    let alts: Vec<String> = registry
        .specs()
        .iter()
        .map(|s| format!("call-{}", sanitize(s.name)))
        .collect();
    match scratchpad {
        None => {
            out.push_str("root ::= ");
            out.push_str(&alts.join(" | "));
            out.push('\n');
        }
        Some(n) => {
            out.push_str("root ::= think? scratch? call\n");
            out.push_str("call ::= ");
            out.push_str(&alts.join(" | "));
            out.push('\n');
            out.push_str(&format!(
                "think ::= \"<think>\" [^<]{{0,{n}}} \"</think>\"\n\
                 scratch ::= [^{{]{{0,{n}}} \"\\n\"\n"
            ));
        }
    }

    for spec in registry.specs() {
        out.push_str(&tool_rule(spec));
        out.push('\n');
    }

    // Shared terminals.
    out.push_str(
        "string ::= \"\\\"\" ( [^\"\\\\] | \"\\\\\" . )* \"\\\"\"\n\
         integer ::= \"-\"? [0-9]+\n\
         ws ::= [ \\t\\n]*\n",
    );
    out
}

/// One alternative: an object literal with the tool name pinned and each param
/// as a key/value pair (required pairs are mandatory; optional ones may appear).
fn tool_rule(spec: &ToolSpec) -> String {
    let rule = format!("call-{}", sanitize(spec.name));
    let mut body = format!(
        "\"{{\" ws \"\\\"tool\\\"\" ws \":\" ws \"\\\"{}\\\"\"",
        spec.name
    );
    for p in &spec.params {
        let val = match p.ty {
            ParamType::Integer | ParamType::OptionalInteger => "integer",
            _ => "string",
        };
        let pair = format!(" ws \",\" ws \"\\\"{}\\\"\" ws \":\" ws {}", p.name, val);
        if p.ty.required() {
            body.push_str(&pair);
        } else {
            // Optional: the whole pair may be absent.
            body.push_str(&format!(" ({})?", pair.trim_start()));
        }
    }
    body.push_str(" ws \"}\"");
    format!("{rule} ::= {body}")
}

/// GBNF rule names allow `-`, but not arbitrary tool characters; v1 tool names
/// are already `[a-z_]`, so this is a defensive normalization.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::default_registry;

    #[test]
    fn json_schema_marks_required_and_optional() {
        let reg = default_registry();
        let schema = params_json_schema(reg.get("write_file").unwrap());
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["content"]["type"], "string");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("path")));
        assert!(required.contains(&json!("content")));
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn finish_schema_has_no_required_params() {
        let reg = default_registry();
        let schema = params_json_schema(reg.get("finish").unwrap());
        assert!(schema["required"].as_array().unwrap().is_empty());
    }

    #[test]
    fn gbnf_root_lists_every_tool_as_an_alternative() {
        let reg = default_registry();
        let g = registry_gbnf(&reg);
        assert!(g.starts_with("root ::= "), "{g}");
        for spec in reg.specs() {
            // Each tool has a rule (name sanitized for GBNF: '_' -> '-').
            let rule = format!("call-{}", sanitize(spec.name));
            assert!(g.contains(&rule), "missing rule {rule} in:\n{g}");
            // The tool's wire name is pinned as a literal inside its rule.
            assert!(
                g.contains(&format!("\\\"{}\\\"", spec.name)),
                "name not pinned: {}",
                spec.name
            );
        }
        // Shared terminals are defined.
        assert!(g.contains("string ::="));
        assert!(g.contains("integer ::="));
    }

    /// A two-tool registry (a read with optional ints, and finish) -- small enough that
    /// the goldens below are readable in full.
    fn two_tool_registry() -> ToolRegistry {
        let specs = crate::builtin::read_only_registry()
            .specs()
            .iter()
            .filter(|s| s.name == "read_file" || s.name == "finish")
            .cloned()
            .collect();
        ToolRegistry::new(specs)
    }

    const TWO_TOOL_STRICT: &str = r###"root ::= call-read-file | call-finish
call-read-file ::= "{" ws "\"tool\"" ws ":" ws "\"read_file\"" ws "," ws "\"path\"" ws ":" ws string (ws "," ws "\"start\"" ws ":" ws integer)? (ws "," ws "\"limit\"" ws ":" ws integer)? ws "}"
call-finish ::= "{" ws "\"tool\"" ws ":" ws "\"finish\"" ws "," ws "\"summary\"" ws ":" ws string ws "}"
string ::= "\"" ( [^"\\] | "\\" . )* "\""
integer ::= "-"? [0-9]+
ws ::= [ \t\n]*
"###;

    /// The strict grammar is untouched by the scratchpad refactor: one generator, and the
    /// `None` case is the old output byte for byte.
    #[test]
    fn strict_grammar_is_unchanged_by_the_scratchpad_variant() {
        assert_eq!(registry_gbnf(&two_tool_registry()), TWO_TOOL_STRICT);
    }

    /// The scratchpad grammar, in full, for the same two tools at 400 characters. The
    /// bound rides in the `{0,400}` repetition range on each prefix rule.
    #[test]
    fn scratchpad_grammar_is_the_strict_one_behind_a_bounded_prefix() {
        let expected = r###"root ::= think? scratch? call
call ::= call-read-file | call-finish
think ::= "<think>" [^<]{0,400} "</think>"
scratch ::= [^{]{0,400} "\n"
call-read-file ::= "{" ws "\"tool\"" ws ":" ws "\"read_file\"" ws "," ws "\"path\"" ws ":" ws string (ws "," ws "\"start\"" ws ":" ws integer)? (ws "," ws "\"limit\"" ws ":" ws integer)? ws "}"
call-finish ::= "{" ws "\"tool\"" ws ":" ws "\"finish\"" ws "," ws "\"summary\"" ws ":" ws string ws "}"
string ::= "\"" ( [^"\\] | "\\" . )* "\""
integer ::= "-"? [0-9]+
ws ::= [ \t\n]*
"###;
        let g = registry_gbnf_with_scratchpad(&two_tool_registry(), 400);
        assert_eq!(g, expected);
        assert!(g.starts_with("root ::= "));
    }

    /// Structurally: everything after the prefix rules is the strict grammar minus its root
    /// line, so the call alternatives can never drift between the two shapes.
    #[test]
    fn scratchpad_grammar_shares_the_call_rules_with_the_strict_one() {
        let reg = two_tool_registry();
        let strict = registry_gbnf(&reg);
        let scratch = registry_gbnf_with_scratchpad(&reg, 400);
        let strict_alts = strict
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("root ::= ");
        let scratch_call = scratch
            .lines()
            .nth(1)
            .unwrap()
            .trim_start_matches("call ::= ");
        assert_eq!(strict_alts, scratch_call, "same alternatives, same order");
        let strict_body: Vec<&str> = strict.lines().skip(1).collect();
        let scratch_body: Vec<&str> = scratch.lines().skip(4).collect();
        assert_eq!(
            strict_body, scratch_body,
            "call rules and terminals identical"
        );
    }
}

#[cfg(test)]
mod the_model_facing_contract {
    use crate::builtin::read_only_registry;
    use crate::grammar::registry_gbnf;

    /// **The investigate grammar is frozen.**
    ///
    /// Spec 23 re-backed `search_code` and `find_symbol` with an index, and the whole
    /// point was that the model could not tell. The six-tool menu is one of the few
    /// things in this project with a measurement behind it (`run_command` used 12/12
    /// with six tools, 3/12 with sixteen), so the tool names, their schemas and the
    /// grammar they generate are the contract -- improvements go *behind* them.
    ///
    /// A golden, byte for byte, written as a raw string so what you read here is
    /// exactly what the model is constrained by. Changing it is allowed; changing it
    /// by accident is not.
    #[test]
    fn the_read_only_registry_gbnf_is_unchanged() {
        let expected = r###"root ::= call-read-file | call-list-dir | call-search-code | call-find-symbol | call-read-function | call-finish
call-read-file ::= "{" ws "\"tool\"" ws ":" ws "\"read_file\"" ws "," ws "\"path\"" ws ":" ws string (ws "," ws "\"start\"" ws ":" ws integer)? (ws "," ws "\"limit\"" ws ":" ws integer)? ws "}"
call-list-dir ::= "{" ws "\"tool\"" ws ":" ws "\"list_dir\"" ws "," ws "\"path\"" ws ":" ws string ws "}"
call-search-code ::= "{" ws "\"tool\"" ws ":" ws "\"search_code\"" ws "," ws "\"query\"" ws ":" ws string ws "}"
call-find-symbol ::= "{" ws "\"tool\"" ws ":" ws "\"find_symbol\"" ws "," ws "\"name\"" ws ":" ws string ws "}"
call-read-function ::= "{" ws "\"tool\"" ws ":" ws "\"read_function\"" ws "," ws "\"path\"" ws ":" ws string ws "," ws "\"name\"" ws ":" ws string ws "}"
call-finish ::= "{" ws "\"tool\"" ws ":" ws "\"finish\"" ws "," ws "\"summary\"" ws ":" ws string ws "}"
string ::= "\"" ( [^"\\] | "\\" . )* "\""
integer ::= "-"? [0-9]+
ws ::= [ \t\n]*
"###;
        assert_eq!(registry_gbnf(&read_only_registry()), expected);
    }

    /// The tool names and arity the grammar is built from, stated independently of
    /// the grammar text so a rename cannot pass by editing one golden.
    #[test]
    fn the_read_only_menu_is_still_the_measured_six() {
        let reg = read_only_registry();
        let names: Vec<&str> = reg.specs().iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "read_file",
                "list_dir",
                "search_code",
                "find_symbol",
                "read_function",
                "finish"
            ]
        );
        // search_code still takes exactly one parameter, `query`.
        let sc = reg
            .specs()
            .iter()
            .find(|s| s.name == "search_code")
            .unwrap();
        let params: Vec<&str> = sc.params.iter().map(|p| p.name).collect();
        assert_eq!(params, vec!["query"]);
    }
}
