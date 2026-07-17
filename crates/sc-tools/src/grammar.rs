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
    let mut out = String::new();
    let alts: Vec<String> = registry
        .specs()
        .iter()
        .map(|s| format!("call-{}", sanitize(s.name)))
        .collect();
    out.push_str("root ::= ");
    out.push_str(&alts.join(" | "));
    out.push('\n');

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
}
