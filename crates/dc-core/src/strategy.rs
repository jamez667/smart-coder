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
    /// Render the actionable repair instruction sent back to the model. Includes
    /// a concrete example, because a small model needs the *shape* — telling it
    /// "you got it wrong" isn't enough; showing a valid call is (spec 04).
    pub fn repair_prompt(&self) -> String {
        let detail = match self {
            RepairError::NoJson => "no JSON tool object found in your reply".to_string(),
            RepairError::BadJson(e) => format!("the JSON was malformed: {e}"),
            RepairError::Invalid(v) => v.to_string(),
        };
        format!(
            "ERROR: {detail}.\n\
             Every reply MUST be exactly one JSON object with a \"tool\" field — \
             do NOT invent tool output or describe results. Examples:\n\
             {{\"tool\":\"read_file\",\"path\":\"file.py\"}}\n\
             {{\"tool\":\"edit_file\",\"path\":\"file.py\",\"old_str\":\"old\",\"new_str\":\"new\"}}\n\
             {{\"tool\":\"run_verification\"}}\n\
             Reply with ONE such object and nothing else."
        )
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
        let objects = extract_all_json_objects(raw);
        if objects.is_empty() {
            return Err(RepairError::NoJson);
        }
        // Validate each candidate object (tolerating raw control chars in strings).
        // A model may batch several calls in one turn (Gemma-4); collect the valid ones.
        let mut valid: Vec<ValidatedCall> = Vec::new();
        let mut last_err: Option<RepairError> = None;
        for json in &objects {
            let value: serde_json::Value = match serde_json::from_str(json)
                .or_else(|_| serde_json::from_str(&escape_raw_control_chars_in_strings(json)))
            {
                Ok(v) => v,
                Err(e) => {
                    last_err = Some(RepairError::BadJson(e.to_string()));
                    continue;
                }
            };
            match registry.validate(&value) {
                Ok(call) => valid.push(call),
                Err(e) => last_err = Some(RepairError::Invalid(e)),
            }
        }
        if valid.is_empty() {
            // Last resort before giving up: a `write_file`/`create_file` whose `content` is a
            // whole source file routinely contains characters that are illegal *inside* a JSON
            // string — most often a Python `"""docstring"""`, whose inner `"` closes the JSON
            // string early and breaks the parse (the writefile-docstring-json-break bug). The
            // control-char escaper above can't fix an inner quote. So when strict parsing fails
            // entirely, try a KEY-AWARE recovery: pull the literal `content` body out of the
            // raw text and rebuild a valid call. Only fires on the already-failing branch, so it
            // can't change the result of a turn that parsed normally.
            if let Some(value) = repair_file_content_call(raw) {
                if let Ok(call) = registry.validate(&value) {
                    return Ok(call);
                }
            }
            // Nothing parsed/validated — surface the most specific error for repair.
            return Err(last_err.unwrap_or(RepairError::NoJson));
        }
        // One action per turn (preserves observe→react). When the model batched calls,
        // run the FIRST one that makes progress (edit/create/run/finish) — the leading
        // reads are it re-confirming context it already has. Else take the first valid.
        let chosen = valid
            .iter()
            .position(|c| is_progress_tool(&c.name))
            .unwrap_or(0);
        Ok(valid.into_iter().nth(chosen).unwrap())
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
pub fn select_strategy(caps: &Capabilities) -> Box<dyn ToolCallStrategy + Send + Sync> {
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

/// Escape raw (unescaped) control characters that appear INSIDE JSON string values —
/// a literal newline/carriage-return/tab a model emitted instead of `\n`/`\r`/`\t`.
/// JSON forbids raw control chars in strings, so `serde_json` rejects them; a coder
/// model writing multi-line code in an argument hits this constantly. We only touch
/// chars inside string literals (tracking quote/escape state), so structural JSON is
/// untouched and an already-escaped `\n` (backslash + n) passes through verbatim.
fn escape_raw_control_chars_in_strings(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + 16);
    let mut in_str = false;
    let mut escaped = false;
    for ch in json.chars() {
        if in_str {
            if escaped {
                escaped = false;
                out.push(ch);
                continue;
            }
            match ch {
                '\\' => {
                    escaped = true;
                    out.push(ch);
                }
                '"' => {
                    in_str = false;
                    out.push(ch);
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        } else {
            if ch == '"' {
                in_str = true;
            }
            out.push(ch);
        }
    }
    out
}

/// Find the first balanced `[...]` block, ignoring brackets inside JSON strings.
/// Used by the planner to pull a step array out of a small model's noisy reply.
pub fn extract_json_array(text: &str) -> Option<&str> {
    extract_balanced(text, '[', ']')
}

/// Find ALL top-level balanced `{...}` blocks in order. Some models (Gemma-4) emit
/// several tool calls in ONE turn, separated by markers like `<tool_call|>`, e.g.
/// `{read_file}<tool_call|>{create_file}<tool_call|>{run_verification}`. The loop runs
/// one action per turn, so we need every candidate to pick the one that makes progress.
fn extract_all_json_objects(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(obj) = extract_balanced(rest, '{', '}') {
        out.push(obj);
        // Advance past this object. `obj` is a slice of `rest`; find where it ends.
        let end = (obj.as_ptr() as usize - rest.as_ptr() as usize) + obj.len();
        if end >= rest.len() {
            break;
        }
        rest = &rest[end..];
    }
    out
}

/// Tools that change the workspace or end the run. When a model batches several calls
/// in one turn (e.g. `read → create → verify → finish`), the leading reads are it
/// re-confirming context; the call that actually *makes progress* is the first of
/// these. The loop preserves observe-react by running just that one and feeding back.
fn is_progress_tool(name: &str) -> bool {
    matches!(
        name,
        "edit_file" | "create_file" | "write_file" | "run_command" | "run_verification" | "finish"
    )
}

/// Key-aware recovery for a `write_file`/`create_file` call whose `content` body broke
/// strict JSON parsing (an unescaped inner `"` from a Python `"""docstring"""`, an inner `}`
/// from code, etc.). Rather than parse the malformed JSON, pull the fields out by position:
/// the `tool` and `path` come before `content` and are well-formed; everything from after
/// `"content":"` to the LAST `"` (the value's real closing quote, since content is the final
/// field a model emits) is taken as the LITERAL file body. Returns a rebuilt JSON object
/// (serde re-escapes the body correctly) for the normal validation path, or `None` if the
/// shape doesn't match (so non-file calls fall through to the existing error).
fn repair_file_content_call(raw: &str) -> Option<serde_json::Value> {
    // Identify a file-content tool. Accept either order of quoting/spacing a model emits.
    let tool = ["write_file", "create_file"]
        .into_iter()
        .find(|t| raw.contains(&format!("\"{t}\"")))?;

    // `path`: a well-formed `"path":"<...>"` — read the first quoted value after the key.
    let path = quoted_value_after(raw, "\"path\"")?;

    // `content`: take everything after the opening quote of its value up to the final closing
    // quote of the object. The model emits `content` last, so the body runs from there to the
    // last `"` before the trailing `}` — rfind the closer so inner quotes don't truncate it.
    let key_pos = raw.find("\"content\"")?;
    let after_key = &raw[key_pos + "\"content\"".len()..];
    // Skip `:` and whitespace, then the opening `"`.
    let colon = after_key.find(':')?;
    let rest = &after_key[colon + 1..];
    let open_q = rest.find('"')?;
    let body_region = &rest[open_q + 1..];
    // The value ends at the last `"` in the remaining text (before/at the closing brace). If
    // there's a trailing `"}` / `" }`, the closer is that quote; else the last quote present.
    let close_rel = body_region.rfind('"')?;
    let literal = &body_region[..close_rel];

    // Un-escape only the standard JSON escapes the model DID write correctly (so a properly
    // escaped `\n`/`\"` in the body becomes the real char); leave everything else literal.
    let content = unescape_json_string_lenient(literal);

    let mut obj = serde_json::Map::new();
    obj.insert("tool".to_string(), serde_json::Value::String(tool.to_string()));
    obj.insert("path".to_string(), serde_json::Value::String(path));
    obj.insert("content".to_string(), serde_json::Value::String(content));
    Some(serde_json::Value::Object(obj))
}

/// Read the first JSON-quoted string value appearing after `key` in `raw` (the value of a
/// well-formed `"key":"value"`). `None` if absent. Used by [`repair_file_content_call`] for
/// the `path`, which precedes the broken `content` and is itself well-formed.
fn quoted_value_after(raw: &str, key: &str) -> Option<String> {
    let key_pos = raw.find(key)?;
    let after = &raw[key_pos + key.len()..];
    let colon = after.find(':')?;
    let rest = &after[colon + 1..];
    let open_q = rest.find('"')?;
    let body = &rest[open_q + 1..];
    // Scan to the unescaped closing quote.
    let mut out = String::new();
    let mut escaped = false;
    for ch in body.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            out.push(ch);
            escaped = true;
        } else if ch == '"' {
            return Some(unescape_json_string_lenient(&out));
        } else {
            out.push(ch);
        }
    }
    None
}

/// Resolve the standard JSON string escapes (`\n \t \r \" \\ \/`) a model wrote correctly,
/// leaving any other backslash sequence and all raw characters as-is. Lenient on purpose:
/// the input is a recovered literal that may mix escaped and raw characters.
fn unescape_json_string_lenient(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
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
    fn batched_turn_runs_the_first_progress_call_not_the_leading_read() {
        // Gemma-4 emits several calls in one turn separated by `<tool_call|>`. The loop
        // runs one action/turn, so we must pick the call that makes PROGRESS (create),
        // not the leading re-read the model already has. Observed live 2026-06-24.
        let reg = default_registry();
        let raw = "{\"tool\":\"read_file\",\"path\":\"test_app.py\"}<tool_call|>\
                   {\"tool\":\"create_file\",\"path\":\"app.py\",\"content\":\"x = 1\"}<tool_call|>\
                   {\"tool\":\"run_verification\"}<tool_call|>{\"tool\":\"finish\"}";
        let call = ParseRepair.extract(raw, &reg).unwrap();
        assert_eq!(
            call.name, "create_file",
            "must skip the leading read to the create"
        );
        assert_eq!(call.str("path"), Some("app.py"));
    }

    #[test]
    fn batched_reads_only_returns_the_first() {
        // If every batched call is a no-op read, just take the first (no progress call).
        let reg = default_registry();
        let raw = "{\"tool\":\"read_file\",\"path\":\"a.py\"}<tool_call|>\
                   {\"tool\":\"read_file\",\"path\":\"b.py\"}";
        let call = ParseRepair.extract(raw, &reg).unwrap();
        assert_eq!(call.name, "read_file");
        assert_eq!(call.str("path"), Some("a.py"));
    }

    #[test]
    fn tolerates_prose_and_braces_in_strings() {
        let reg = default_registry();
        let raw = "Sure:\n{\"tool\":\"write_file\",\"path\":\"x\",\"content\":\"a { b } c\"}\ndone";
        let call = ParseRepair.extract(raw, &reg).unwrap();
        assert_eq!(call.str("content"), Some("a { b } c"));
    }

    #[test]
    fn tolerates_raw_newlines_inside_a_string_value() {
        // A coder model writes a multi-line `old_str` with LITERAL newlines (and even a
        // mix of escaped + raw, exactly as qwen3-coder-30b did 2026-06-23). Strict JSON
        // forbids raw control chars in strings; the sanitizer must rescue it.
        let reg = default_registry();
        let raw = "{\"tool\":\"edit_file\",\"path\":\"app.py\",\"old_str\":\"def page():\n    start = n * 3\",\"new_str\":\"def page():\n    start = (n-1) * 3\"}";
        let call = ParseRepair.extract(raw, &reg).unwrap();
        assert_eq!(call.name, "edit_file");
        assert_eq!(call.str("old_str"), Some("def page():\n    start = n * 3"));
        assert_eq!(
            call.str("new_str"),
            Some("def page():\n    start = (n-1) * 3")
        );
    }

    #[test]
    fn write_file_with_a_literal_python_docstring_is_recovered() {
        // The writefile-docstring-json-break: a model writes a real Python `"""docstring"""`
        // inside `content`, whose inner `"` closes the JSON string early so strict parsing
        // fails and the file is never written. The key-aware fallback must recover it.
        let reg = default_registry();
        let raw = "{\"tool\":\"write_file\",\"path\":\"app.py\",\"content\":\"def f():\n    \"\"\"doc string\"\"\"\n    return 1\n\"}";
        let call = ParseRepair
            .extract(raw, &reg)
            .expect("the docstring write_file must be recovered, not dropped");
        assert_eq!(call.name, "write_file");
        assert_eq!(call.str("path"), Some("app.py"));
        // The literal body (triple quotes intact) is preserved.
        let content = call.str("content").unwrap();
        assert!(content.contains("\"\"\"doc string\"\"\""), "got: {content:?}");
        assert!(content.contains("def f():") && content.contains("return 1"));
    }

    #[test]
    fn recovery_handles_content_whose_body_contains_braces() {
        // Code content with `{` / `}` (a dict) AND an inner quote — the balanced-brace object
        // scan can mis-cut here, so recovery must still pull the right body by key position.
        let reg = default_registry();
        let raw = "{\"tool\":\"create_file\",\"path\":\"d.py\",\"content\":\"X = {\"a\": 1}\nY = \"\"\"q\"\"\"\n\"}";
        let call = ParseRepair.extract(raw, &reg).expect("recovered");
        assert_eq!(call.name, "create_file");
        let content = call.str("content").unwrap();
        assert!(content.contains("X = {\"a\": 1}"), "got: {content:?}");
        assert!(content.contains("\"\"\"q\"\"\""));
    }

    #[test]
    fn recovery_does_not_fire_on_a_well_formed_call() {
        // A normal, parseable write_file must take the strict path and be byte-exact — the
        // fallback only runs when strict parsing fails, so this proves no regression.
        let reg = default_registry();
        let raw = r#"{"tool":"write_file","path":"a.py","content":"x = 1\ny = 2\n"}"#;
        let call = ParseRepair.extract(raw, &reg).unwrap();
        assert_eq!(call.str("content"), Some("x = 1\ny = 2\n"));
    }

    #[test]
    fn already_escaped_newlines_still_parse_unchanged() {
        // The sanitizer must not double-escape a correctly-escaped `\n`.
        let reg = default_registry();
        let raw = r#"{"tool":"write_file","path":"a.py","content":"x = 1\ny = 2"}"#;
        let call = ParseRepair.extract(raw, &reg).unwrap();
        assert_eq!(call.str("content"), Some("x = 1\ny = 2"));
    }

    #[test]
    fn no_json_is_a_distinct_repairable_error() {
        let reg = default_registry();
        let err = ParseRepair.extract("no json here", &reg).unwrap_err();
        assert_eq!(err, RepairError::NoJson);
        let prompt = err.repair_prompt();
        // The repair shows the model a concrete valid example, not just "wrong".
        assert!(prompt.contains("\"tool\""), "{prompt}");
        assert!(prompt.contains("read_file"), "{prompt}");
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
