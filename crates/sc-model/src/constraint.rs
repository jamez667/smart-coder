//! Output constraints — how the harness asks a backend to *enforce* structure on
//! a tool call (spec 02 — capabilities & constrained decoding).
//!
//! These types are expressed in the gateway's own vocabulary, deliberately
//! independent of the `sc-tools` registry: the model layer must not depend on the
//! tool layer (that arrow points the other way). `sc-tools` knows how to *produce*
//! a [`ToolSchema`] / GBNF grammar from its `ToolSpec`s; this crate only carries
//! and applies them.
//!
//! A request may carry at most one [`OutputConstraint`]. Each concrete backend
//! applies the variant it supports (per its [`crate::Capabilities`]) and ignores
//! the rest — capability negotiation happens above, in the strategy layer.

/// What structural enforcement a backend supports (spec 02 — capabilities).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCalling {
    /// Plain completion only — the harness must prompt + parse + repair.
    None,
    /// OpenAI-style `tools` / `tool_choice` function calling.
    OpenAiStyle,
    /// GBNF grammar-constrained decoding (llama.cpp).
    Gbnf,
}

/// A single function/tool definition in the OpenAI function-calling shape: a
/// name, a description, and a JSON-Schema object for its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// A JSON-Schema object (`{"type":"object","properties":{...},"required":[...]}`)
    /// describing the parameters. Built by `sc-tools` from a `ToolSpec`.
    pub parameters: serde_json::Value,
}

/// An output constraint attached to a [`crate::GenerateRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputConstraint {
    /// Constrain to one of these tool/function definitions (OpenAI-style FC).
    Tools(Vec<ToolSchema>),
    /// Constrain decoding to this GBNF grammar (llama.cpp).
    Grammar(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_schema_carries_a_json_schema_object() {
        let s = ToolSchema {
            name: "read_file".into(),
            description: "Read a file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        };
        assert_eq!(s.parameters["required"][0], "path");
    }

    #[test]
    fn constraint_variants_are_distinct() {
        let tools = OutputConstraint::Tools(vec![]);
        let grammar = OutputConstraint::Grammar("root ::= \"x\"".into());
        assert_ne!(tools, grammar);
    }
}
