//! The three strategies and the capability-driven choice between them.
//!
//! The ordering reflects the reliability hierarchy: grammar > native FC >
//! parse+repair. [`ParseRepair`] works on any backend, so it's the floor under
//! every other strategy.

use sc_model::{Capabilities, GenerateRequest, OutputConstraint, ToolCalling, ToolSchema};
use sc_tools::{
    params_json_schema, registry_gbnf, registry_gbnf_with_scratchpad, ToolRegistry, ValidatedCall,
};

use super::error::{RepairError, ToolCallStrategy};
use super::repair::{
    is_progress_tool, looks_swallowed, recover_swallowed_call, repair_edit_file_call,
    repair_file_content_call, repair_mislabelled_tool_call, repair_truncated_file_write,
    validated_calls,
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
            // The model used the WRONG TOOL NAME for an otherwise perfect call — it was
            // ORDERED to call `write_file` and complied with the name while sending an
            // `edit_file`'s `old_str`/`new_str`. The JSON parses fine, so none of the
            // body-repair rungs above are even relevant; what failed is the name against the
            // schema. Believe the arguments when they name exactly one tool (see
            // `repair_mislabelled_tool_call` for how strict that is).
            for json in crate::text::extract_all_json_objects(raw) {
                if !json.contains("\"tool\"") {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(json).or_else(|_| {
                    serde_json::from_str(&crate::text::escape_raw_control_chars_in_strings(json))
                }) else {
                    continue;
                };
                if let Some(fixed) = repair_mislabelled_tool_call(&value, registry) {
                    if let Ok(call) = registry.validate(&fixed) {
                        return Ok(call);
                    }
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

    /// **The one measured terminator: `</tool_call>`.**
    ///
    /// The failure this ends (see [`ToolCallStrategy::stop_sequences`]): the model emits
    /// one complete valid call, closes it with `</tool_call>`, and then keeps going --
    /// starting a SECOND call in a different format and running to the token cap. Every
    /// wasted token in that reply comes AFTER this marker, so cutting there costs nothing
    /// and saves the rest of the generation.
    ///
    /// Why only this one, and why it is safe:
    ///
    /// * It is MARKUP, not code. The ChatML/Qwen family (Mellum2, Qwen2.5-Coder, Tiel)
    ///   writes it as the closing half of its own `<tool_call>…</tool_call>` wrapper --
    ///   it is a chat-template artifact leaking into the content, which is precisely why
    ///   it never belongs in a payload. Contrast a SYNTACTIC stop, tested earlier: a
    ///   closing brace followed by a newline occurs inside a `write_file` content body,
    ///   and one run in five then produced NO tool call at all.
    /// * It is checked at `openai.rs:2051`, where the same marker is documented from a
    ///   live capture -- `{"tool":"finish"}</tool_call>` repeated ~60 times to the cap.
    ///
    /// Deliberately EXCLUDED: `<|im_end|>` (a special token the server already treats as
    /// end-of-generation; sending it as a text stop is redundant), and the terminators of
    /// families not in use here -- `</function>`, `<|tool▁call▁end|>`. An unobserved stop
    /// string is pure downside: it can only cut output we did want.
    ///
    /// The residual risk is a payload that legitimately CONTAINS `</tool_call>` -- a
    /// `write_file` whose content is itself a chat template, or a test fixture carrying
    /// the literal. That reply is cut mid-JSON and yields no parseable call, so
    /// [`crate::FaultKind::StopSequenceMisfire`] reports it rather than letting it read
    /// as the model declining to act.
    fn stop_sequences(&self) -> Vec<String> {
        vec!["</tool_call>".to_string()]
    }

    fn prepare_request(&self, req: &mut GenerateRequest, registry: &ToolRegistry) {
        req.constraint = Some(OutputConstraint::Tools(tool_schemas(registry)));
        req.stop = self.stop_sequences();
    }

    fn request_overhead_text(&self, registry: &ToolRegistry) -> String {
        // The same `{"type":"function","function":{...}}` shape the backend sends,
        // so the count matches what the server tokenizes.
        let defs: Vec<serde_json::Value> = tool_schemas(registry)
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        serde_json::Value::Array(defs).to_string()
    }

    fn extract(&self, raw: &str, registry: &ToolRegistry) -> Result<ValidatedCall, RepairError> {
        ParseRepair.extract(raw, registry)
    }
}

/// GBNF grammar-constrained decoding (llama.cpp). The strongest guarantee:
/// decoding is constrained to the exact tool-call grammar, so the output is valid
/// by construction. Extraction still validates (belt-and-braces) via the same
/// registry path.
///
/// The plain value `Grammar` is the strict grammar -- a bare object, no prose --
/// which is what every caller has always meant by it. [`Grammar::with_scratchpad`]
/// is the opt-in experiment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Grammar {
    /// EXPERIMENT (spec 02's alignment-tax caveat). `Some(max_chars)` puts a bounded,
    /// unconstrained scratchpad in front of the call so the model can reason before it
    /// acts; `None` (the default) is the strict envelope-only grammar the investigate
    /// path measured at 18 tokens a call. Nothing sets this by default: it exists to be
    /// A/B'd on the investigate path, not to be believed.
    scratchpad: Option<usize>,
}

/// `Grammar` the value: the strict strategy. Kept so `Box::new(sc_core::Grammar)` reads
/// and compiles exactly as it did when `Grammar` was a unit struct; the struct gained a
/// field and a braced struct occupies only the type namespace, so the value namespace is
/// free for this.
#[allow(non_upper_case_globals)]
pub const Grammar: Grammar = Grammar { scratchpad: None };

impl Grammar {
    /// The scratchpad experiment: allow up to `max_chars` characters of free reasoning
    /// (and an optional `<think>…</think>` block) before the tool object, with the bound
    /// enforced by the grammar itself. See [`sc_tools::registry_gbnf_with_scratchpad`].
    pub fn with_scratchpad(max_chars: usize) -> Self {
        Self {
            scratchpad: Some(max_chars),
        }
    }
}

impl ToolCallStrategy for Grammar {
    fn name(&self) -> &str {
        // Distinct names so a measurement can tell the two arms apart in the logs.
        match self.scratchpad {
            None => "gbnf",
            Some(_) => "gbnf+scratchpad",
        }
    }

    fn system_preamble(&self, registry: &ToolRegistry) -> String {
        // The grammar enforces shape; the prompt still lists tools so the model
        // knows what each does (the grammar can't convey intent).
        let base = ParseRepair.system_preamble(registry);
        match self.scratchpad {
            None => base,
            // The strict preamble says "nothing else"; here the model is invited to think
            // first, and told the one constraint the grammar will impose on that thinking
            // so it does not fight the decoder at a `{`.
            Some(n) => format!(
                "You may reason briefly first -- at most {n} characters, containing no \
                 '{{' -- on lines before the JSON object. Then:\n{base}"
            ),
        }
    }

    fn prepare_request(&self, req: &mut GenerateRequest, registry: &ToolRegistry) {
        let grammar = match self.scratchpad {
            None => registry_gbnf(registry),
            Some(n) => registry_gbnf_with_scratchpad(registry, n),
        };
        req.constraint = Some(OutputConstraint::Grammar(grammar));
        // No stop sequences: the grammar already halts decoding at the end of a
        // well-formed object, and a text stop could only cut a payload short.
        req.stop = self.stop_sequences();
    }

    fn extract(&self, raw: &str, registry: &ToolRegistry) -> Result<ValidatedCall, RepairError> {
        // ParseRepair already tolerates prose before the object (it scans for balanced
        // `{…}` blocks carrying a `"tool"` key), so the scratchpad needs no scanner of its
        // own. The one thing it must NOT see is a `<think>` block: the grammar lets that
        // block contain `{`, and a narrated `{"tool":…}` inside it would be picked up as
        // the call. Drop the block, then extract exactly as the strict path does.
        let raw = match self.scratchpad {
            None => raw,
            Some(_) => strip_think_block(raw),
        };
        ParseRepair.extract(raw, registry)
    }
}

/// `raw` with a leading `<think>…</think>` block removed (leading whitespace tolerated).
/// Only a block at the very start counts -- that is the only position the scratchpad
/// grammar allows one -- and an unterminated block is left alone for ParseRepair to
/// report on.
fn strip_think_block(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    let Some(after_open) = trimmed.strip_prefix("<think>") else {
        return raw;
    };
    match after_open.find("</think>") {
        Some(end) => &after_open[end + "</think>".len()..],
        None => raw,
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
