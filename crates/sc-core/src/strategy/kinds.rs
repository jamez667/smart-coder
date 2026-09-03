//! The three strategies and the capability-driven choice between them.
//!
//! The ordering reflects the reliability hierarchy: grammar > native FC >
//! parse+repair. [`ParseRepair`] works on any backend, so it's the floor under
//! every other strategy.

use sc_model::{Capabilities, GenerateRequest, OutputConstraint, ToolCalling, ToolSchema};
use sc_tools::{params_json_schema, registry_gbnf, ToolRegistry, ValidatedCall};

use super::error::{RepairError, ToolCallStrategy};
use super::repair::{
    is_progress_tool, looks_swallowed, recover_swallowed_call, repair_edit_file_call,
    repair_file_content_call, repair_truncated_file_write, validated_calls,
};

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
                    sc_tools::ParamType::Integer | sc_tools::ParamType::OptionalInteger => "<int>",
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
        let (valid, last_err) = validated_calls(raw, registry);
        if valid.is_empty() {
            // Swallowed-call recovery FIRST: the coder model narrated an illustration whose
            // unterminated string absorbed the real call, so the only balanced object is corrupt
            // and none validated. Dig the real, complete `{"tool":…}` out of the swallowed body
            // before the key-aware repairs below (which would grab the swallowed old_str).
            if let Some(value) = recover_swallowed_call(raw) {
                if let Ok(call) = registry.validate(&value) {
                    return Ok(call);
                }
            }
            // Last resort: key-aware recovery for a write_file/create_file whose content broke
            // strict parsing (a literal Python `"""docstring"""` — the inner `"` closes the
            // JSON string early). Only fires on the already-failing branch.
            if let Some(value) = repair_file_content_call(raw) {
                if let Ok(call) = registry.validate(&value) {
                    return Ok(call);
                }
            }
            // Truncation salvage: a small model's `write_file` whose `content` string was cut
            // off mid-body — the reply ends inside the string, so there's NO closing quote and
            // the JSON never parses. The doomed retry re-emits the same over-long content and is
            // truncated at the same place, looping until the stall detector kills it. Instead,
            // land the partial content that DID arrive; the model can then `append_file` the
            // rest in bounded chunks. Only fires after strict parse + the closed-quote repair
            // above both fail, so a well-formed or merely-quote-broken call never reaches here.
            if let Some(value) = repair_truncated_file_write(raw) {
                if let Ok(call) = registry.validate(&value) {
                    return Ok(call);
                }
            }
            // Same idea for edit_file, whose old_str/new_str bodies carry raw multi-line code
            // (the single largest parse-failure class observed live — 19/45 captured failures).
            if let Some(value) = repair_edit_file_call(raw) {
                if let Ok(call) = registry.validate(&value) {
                    return Ok(call);
                }
            }
            // A stray quote closing a NUMERIC argument (`"limit":60"`). Every repair above
            // is about string bodies whose inner quotes end a JSON string early; this is the
            // opposite shape and none of them reach it. Cheap, and it was costing whole
            // turns to a one-character mistake.
            if let Some(value) = super::repair::repair_stray_quote_after_number(raw) {
                if let Ok(call) = registry.validate(&value) {
                    return Ok(call);
                }
            }
            return Err(last_err.unwrap_or(RepairError::NoJson));
        }
        // A call is SWALLOWED when one of its string args contains an embedded `"tool":` — the
        // coder model narrates a call in prose (an illustration), its string never closes, and
        // the balanced-brace scan absorbs the REAL call that follows into that arg's value
        // (observed live 2026-07-15: an edit_file whose old_str was `pub struct Terrain{…{"tool":
        // "edit_file",…}`, corrupting the file). Prefer a clean call; if the ONLY calls are
        // swallowed, recover the real call from inside the swallowed string.
        let clean: Vec<&ValidatedCall> = valid.iter().filter(|c| !looks_swallowed(c)).collect();
        if clean.is_empty() {
            // Every parsed call is swallowed — dig the real call out of the last one's body.
            if let Some(value) = recover_swallowed_call(raw) {
                if let Ok(call) = registry.validate(&value) {
                    return Ok(call);
                }
            }
            // Recovery failed and every call is corrupt. REJECT rather than applying a swallowed
            // call — writing its run-on `new_str`/`content` verbatim would splice raw JSON into the
            // source file (the ship_render.rs corruption). An error re-prompts the model with the
            // "one JSON object" reminder, which is the safe outcome.
            return Err(RepairError::Swallowed);
        }
        // One action per turn (preserves observe→react). Among the clean calls, run the FIRST
        // that makes progress (edit/create/run/finish) — leading reads are re-confirmations —
        // else the first clean call.
        let chosen = clean
            .iter()
            .find(|c| is_progress_tool(&c.name))
            .or_else(|| clean.first())
            .copied()
            .expect("clean is non-empty (checked above)");
        Ok(chosen.clone())
    }
}

/// Build the OpenAI-style function definitions for a registry.
pub(super) fn tool_schemas(registry: &ToolRegistry) -> Vec<ToolSchema> {
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
pub fn select_strategy(caps: &Capabilities) -> Box<dyn ToolCallStrategy + Send + Sync> {
    match caps.tool_calling {
        ToolCalling::Gbnf => Box::new(Grammar),
        ToolCalling::OpenAiStyle => Box::new(NativeTools),
        ToolCalling::None => Box::new(ParseRepair),
    }
}
