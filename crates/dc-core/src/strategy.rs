//! Tool-call strategies — the heart of the M1 reliability story (spec 02/04).
//!
//! Getting a *well-formed* tool call out of a small model is the hardest
//! small-model problem. The harness adapts its approach to what the backend can
//! enforce (spec 02 — capabilities):
//!
//! | Backend supports          | Strategy                                   |
//! | ------------------------- | ------------------------------------------ |
//! | GBNF grammar (llama.cpp)  | constrain decoding to the tool grammar     |
//! | JSON-schema / native FC   | native tool-calling / schema mode          |
//! | nothing (plain completion)| prompt + parse + **repair loop**           |
//!
//! Every strategy shares one post-condition: turn a model turn into either a
//! validated [`ValidatedCall`] or a structured [`RepairError`] that the loop
//! renders into a precise re-prompt. The strategy owns *how* tools are presented
//! and decoded; the loop owns budgets, observation feedback, and stopping.

use dc_model::{Capabilities, GenerateRequest, OutputConstraint, ToolCalling, ToolSchema};
use dc_tools::{params_json_schema, registry_gbnf, ToolRegistry, ValidatedCall, ValidationError};

/// Why extracting a tool call from a model turn failed. Distinguishes "no JSON at
/// all" from "JSON but invalid against the schema" so the repair message is
/// precise (spec 03 — feed back the exact error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairError {
    /// No JSON object could be found in the model output.
    NoJson,
    /// A JSON object was found but didn't parse.
    BadJson(String),
    /// Parsed fine but failed schema validation.
    Invalid(ValidationError),
}

impl RepairError {
    /// Render the actionable repair instruction sent back to the model.
    pub fn repair_prompt(&self) -> String {
        let detail = match self {
            RepairError::NoJson => "no JSON tool object found in your reply".to_string(),
            RepairError::BadJson(e) => format!("the JSON was malformed: {e}"),
            RepairError::Invalid(v) => v.to_string(),
        };
        format!("ERROR: {detail}. Reply with EXACTLY ONE JSON tool object and nothing else.")
    }
}

impl std::fmt::Display for RepairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.repair_prompt())
    }
}

/// A strategy for eliciting and decoding a single tool call.
pub trait ToolCallStrategy {
    /// A short identifier for logs/metrics (e.g. `"parse-repair"`, `"native-fc"`).
    fn name(&self) -> &str;

    /// The instruction block describing the available tools, appended to the
    /// system prompt. Strategies that constrain decoding can keep this lighter,
    /// since validity is enforced downstream.
    fn system_preamble(&self, registry: &ToolRegistry) -> String;

    /// Mutate the outgoing request to apply any backend-side constraint (native
    /// tools, JSON-schema mode, GBNF grammar). The default does nothing — correct
    /// for the plain-completion parse+repair path.
    fn prepare_request(&self, _req: &mut GenerateRequest, _registry: &ToolRegistry) {}

    /// Turn raw model output into a validated call or a structured repair error.
    fn extract(&self, raw: &str, registry: &ToolRegistry) -> Result<ValidatedCall, RepairError>;
}

/// The universal fallback: prompt for a JSON object, parse it tolerantly, and
/// validate against the registry. Works on *any* backend, so it's the floor under
/// every other strategy (spec 02 — "prompt + parse + repair").
pub struct ParseRepair;

impl ToolCallStrategy for ParseRepair {
    fn name(&self) -> &str {
        "parse-repair"
    }

    fn system_preamble(&self, registry: &ToolRegistry) -> String {
        let mut s = String::from(
            "Each turn, respond with EXACTLY ONE JSON object and nothing else. \
             Choose one tool:\n",
        );
        for spec in registry.specs() {
            s.push_str("{\"tool\":\"");
            s.push_str(spec.name);
            s.push('"');
            for p in &spec.params {
                s.push_str(",\"");
                s.push_str(p.name);
                s.push_str("\":");
                s.push_str(match p.ty {
                    dc_tools::ParamType::Integer | dc_tools::ParamType::OptionalInteger => "<int>",
                    _ => "\"<string>\"",
                });
            }
            s.push_str("}  — ");
            s.push_str(spec.description);
            s.push('\n');
        }
        s.push_str(
            "Paths are relative to the project root; you cannot escape it. \
             Do NOT modify any test files. Call finish when done.",
        );
        s
    }

    fn extract(&self, raw: &str, registry: &ToolRegistry) -> Result<ValidatedCall, RepairError> {
        let json = extract_json_object(raw).ok_or(RepairError::NoJson)?;
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| RepairError::BadJson(e.to_string()))?;
        registry.validate(&value).map_err(RepairError::Invalid)
    }
}

/// Build the OpenAI-style function definitions for a registry.
fn tool_schemas(registry: &ToolRegistry) -> Vec<ToolSchema> {
    registry
        .specs()
        .iter()
        .map(|s| ToolSchema {
            name: s.name.to_string(),
            description: s.description.to_string(),
            parameters: params_json_schema(s),
        })
        .collect()
}

/// Native function-calling (OpenAI-style). Attaches the tool schemas as an
/// [`OutputConstraint::Tools`]; the backend forwards them as `tools`/`tool_choice`
/// and normalizes the returned `tool_calls[0]` back into the uniform JSON shape,
/// so extraction is the same validate-against-registry path as parse+repair.
pub struct NativeTools;

impl ToolCallStrategy for NativeTools {
    fn name(&self) -> &str {
        "native-fc"
    }

    fn system_preamble(&self, _registry: &ToolRegistry) -> String {
        // The tool schemas travel structurally, so the prompt stays light — we
        // only state the contract (spec 02 — don't over-constrain the reasoning).
        "Use the provided tools. Call exactly one tool per turn. Paths are relative \
         to the project root. Do NOT modify any test files. Call finish when done."
            .to_string()
    }

    fn prepare_request(&self, req: &mut GenerateRequest, registry: &ToolRegistry) {
        req.constraint = Some(OutputConstraint::Tools(tool_schemas(registry)));
    }

    fn extract(&self, raw: &str, registry: &ToolRegistry) -> Result<ValidatedCall, RepairError> {
        ParseRepair.extract(raw, registry)
    }
}

/// GBNF grammar-constrained decoding (llama.cpp). The strongest guarantee:
/// decoding is constrained to the exact tool-call grammar, so the output is valid
/// by construction. Extraction still validates (belt-and-braces) via the same
/// registry path.
pub struct Grammar;

impl ToolCallStrategy for Grammar {
    fn name(&self) -> &str {
        "gbnf"
    }

    fn system_preamble(&self, registry: &ToolRegistry) -> String {
        // The grammar enforces shape; the prompt still lists tools so the model
        // knows what each does (the grammar can't convey intent).
        ParseRepair.system_preamble(registry)
    }

    fn prepare_request(&self, req: &mut GenerateRequest, registry: &ToolRegistry) {
        req.constraint = Some(OutputConstraint::Grammar(registry_gbnf(registry)));
    }

    fn extract(&self, raw: &str, registry: &ToolRegistry) -> Result<ValidatedCall, RepairError> {
        ParseRepair.extract(raw, registry)
    }
}

/// Choose the strongest tool-call strategy the backend can enforce (spec 02).
///
/// Returns a boxed strategy so the loop can hold it behind the trait object. The
/// ordering reflects the reliability hierarchy: grammar > native FC > parse+repair.
pub fn select_strategy(caps: &Capabilities) -> Box<dyn ToolCallStrategy> {
    match caps.tool_calling {
        ToolCalling::Gbnf => Box::new(Grammar),
        ToolCalling::OpenAiStyle => Box::new(NativeTools),
        ToolCalling::None => Box::new(ParseRepair),
    }
}

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

/// Find the first balanced `open..close` block, ignoring delimiters inside JSON
/// strings (with escape handling).
fn extract_balanced(text: &str, open: char, close: char) -> Option<&str> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use dc_tools::default_registry;

    #[test]
    fn extracts_a_clean_call() {
        let reg = default_registry();
        let call = ParseRepair
            .extract(r#"{"tool":"read_file","path":"a.txt"}"#, &reg)
            .unwrap();
        assert_eq!(call.name, "read_file");
        assert_eq!(call.str("path"), Some("a.txt"));
    }

    #[test]
    fn tolerates_prose_and_braces_in_strings() {
        let reg = default_registry();
        let raw = "Sure:\n{\"tool\":\"write_file\",\"path\":\"x\",\"content\":\"a { b } c\"}\ndone";
        let call = ParseRepair.extract(raw, &reg).unwrap();
        assert_eq!(call.str("content"), Some("a { b } c"));
    }

    #[test]
    fn no_json_is_a_distinct_repairable_error() {
        let reg = default_registry();
        let err = ParseRepair.extract("no json here", &reg).unwrap_err();
        assert_eq!(err, RepairError::NoJson);
        assert!(err.repair_prompt().contains("EXACTLY ONE JSON"));
    }

    #[test]
    fn schema_violation_surfaces_the_precise_reason() {
        let reg = default_registry();
        // valid JSON, wrong shape: read_file needs a path
        let err = ParseRepair
            .extract(r#"{"tool":"read_file"}"#, &reg)
            .unwrap_err();
        match &err {
            RepairError::Invalid(_) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }
        assert!(err.repair_prompt().contains("requires parameter"), "{err}");
    }

    #[test]
    fn unknown_tool_repair_lists_the_real_tools() {
        let reg = default_registry();
        let err = ParseRepair
            .extract(r#"{"tool":"delete_everything"}"#, &reg)
            .unwrap_err();
        let prompt = err.repair_prompt();
        assert!(prompt.contains("read_file"), "{prompt}");
    }

    #[test]
    fn preamble_lists_every_tool() {
        let reg = default_registry();
        let preamble = ParseRepair.system_preamble(&reg);
        for spec in reg.specs() {
            assert!(
                preamble.contains(spec.name),
                "missing {} in preamble",
                spec.name
            );
        }
    }

    #[test]
    fn native_strategy_attaches_a_tools_constraint() {
        let reg = default_registry();
        let mut req = dc_model::GenerateRequest::new(vec![]);
        NativeTools.prepare_request(&mut req, &reg);
        match req.constraint {
            Some(OutputConstraint::Tools(ref tools)) => {
                assert_eq!(tools.len(), reg.specs().len());
                assert!(tools.iter().any(|t| t.name == "read_file"));
            }
            other => panic!("expected Tools constraint, got {other:?}"),
        }
    }

    #[test]
    fn grammar_strategy_attaches_a_grammar_constraint() {
        let reg = default_registry();
        let mut req = dc_model::GenerateRequest::new(vec![]);
        Grammar.prepare_request(&mut req, &reg);
        match req.constraint {
            Some(OutputConstraint::Grammar(ref g)) => assert!(g.contains("root ::=")),
            other => panic!("expected Grammar constraint, got {other:?}"),
        }
    }

    #[test]
    fn all_strategies_share_the_same_validating_extractor() {
        // Whatever the strategy, a valid tool-call string validates and a bad one
        // is a repairable error — extraction is uniform across strategies.
        let reg = default_registry();
        let good = r#"{"tool":"finish"}"#;
        let bad = r#"{"tool":"nope"}"#;
        for s in [
            &ParseRepair as &dyn ToolCallStrategy,
            &NativeTools,
            &Grammar,
        ] {
            assert!(s.extract(good, &reg).is_ok(), "{} rejected good", s.name());
            assert!(s.extract(bad, &reg).is_err(), "{} accepted bad", s.name());
        }
    }

    #[test]
    fn select_strategy_follows_capabilities() {
        use dc_model::Capabilities;
        let caps = |tc| Capabilities {
            max_context_tokens: 8192,
            tool_calling: tc,
            on_device: false,
        };
        assert_eq!(
            select_strategy(&caps(ToolCalling::None)).name(),
            "parse-repair"
        );
        assert_eq!(
            select_strategy(&caps(ToolCalling::OpenAiStyle)).name(),
            "native-fc"
        );
        assert_eq!(select_strategy(&caps(ToolCalling::Gbnf)).name(), "gbnf");
    }
}
