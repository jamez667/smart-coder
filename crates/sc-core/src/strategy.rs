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

use sc_model::{Capabilities, GenerateRequest, OutputConstraint, ToolCalling, ToolSchema};
use sc_tools::{params_json_schema, registry_gbnf, ToolRegistry, ValidatedCall, ValidationError};

use crate::text::{
    escape_raw_control_chars_in_strings, extract_all_json_objects, fenced_code_block,
    quoted_value_after, split_on_new_str, unescape_json_string_lenient,
};

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
    /// Every call in the reply was corrupt/run-on (a string arg absorbed the next arg or call), so
    /// applying any would splice raw JSON into a file. Rejected — the model is re-prompted.
    Swallowed,
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
            RepairError::Swallowed => {
                "your reply ran multiple tool calls together and a string argument absorbed the \
                 next one — the edit content was corrupt"
                    .to_string()
            }
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

/// Whether a validated call has been SWALLOWED / RUN-ON: one of its string arguments contains an
/// embedded tool-call OR edit-key marker, meaning the model's broken quoting let this arg's value
/// absorb the NEXT argument or a following call. Such content is corrupt and must not be written to
/// a file — otherwise raw JSON like `…};","old_str":"use …` lands in the source (observed live
/// 2026-07-21: `ship_render.rs` got a `new_str` that ran on into a second `old_str`, corrupting the
/// import block and breaking the build).
///
/// Detects two shapes:
/// * an embedded `"tool":` — a following call swallowed into this arg (the original case), and
/// * an embedded edit-key marker like `","old_str":` / `","new_str":` / `","content":` — the arg's
///   value ran past its closing quote into the NEXT key. A legitimate code edit never contains a
///   `"` immediately followed by one of these JSON keys and a `:`.
fn looks_swallowed(call: &ValidatedCall) -> bool {
    ["old_str", "new_str", "new_text", "content", "command"]
        .iter()
        .filter_map(|k| call.str(k))
        .any(value_is_runon)
}

/// Whether a single string argument value is corrupt: it embeds a following tool call (`"tool":`)
/// or ran on into the NEXT JSON key (`","old_str":`, `","new_str":`, …). Shared by
/// [`looks_swallowed`] and [`recover_swallowed_call`] so recovery can't resurrect a run-on value.
fn value_is_runon(v: &str) -> bool {
    const RUNON: [&str; 5] = [
        "\",\"old_str\":",
        "\",\"new_str\":",
        "\",\"new_text\":",
        "\",\"content\":",
        "\",\"path\":",
    ];
    v.contains("\"tool\":") || v.contains("\"tool\" :") || RUNON.iter().any(|m| v.contains(m))
}

/// Recover the REAL tool call from a swallowed reply: the model narrated an illustration whose
/// unterminated string absorbed the real call, so the real, complete `{"tool":…}` sits LATER in
/// the raw text. Take the LAST balanced `{"tool":…}` object and parse it — that's the one the
/// model actually finished writing. Returns the parsed JSON value, or `None` if none parses.
fn recover_swallowed_call(raw: &str) -> Option<serde_json::Value> {
    // Find every `{"tool"` start and try the balanced object from each; keep the LAST that
    // parses AND is not itself swallowed (its string args carry no embedded `"tool":`).
    let mut best: Option<serde_json::Value> = None;
    let mut search_from = 0;
    while let Some(rel) = raw[search_from..].find("{\"tool\"") {
        let start = search_from + rel;
        if let Some(obj) = extract_all_json_objects(&raw[start..]).into_iter().next() {
            let parsed = serde_json::from_str::<serde_json::Value>(obj)
                .or_else(|_| serde_json::from_str(&escape_raw_control_chars_in_strings(obj)))
                .ok();
            if let Some(v) = parsed {
                // Skip a candidate that is itself corrupt — its args embed another call (`"tool":`)
                // or run on into the next key (`","old_str":` …). Reuses the same detection as
                // `looks_swallowed` so a run-on value can't be resurrected here.
                let self_swallowed = ["old_str", "new_str", "new_text", "content", "command"]
                    .iter()
                    .any(|k| {
                        v.get(k)
                            .and_then(|x| x.as_str())
                            .is_some_and(value_is_runon)
                    });
                if !self_swallowed {
                    best = Some(v);
                }
            }
        }
        search_from = start + "{\"tool\"".len();
    }
    best
}

/// Parse + validate every JSON object in a model turn (tolerating raw control chars). Shared
/// by [`ParseRepair::extract`] (picks one) and [`extract_write_batch`] (takes a safe run).
/// Returns the valid calls in emission order plus the most specific error seen (for repair).
fn validated_calls(
    raw: &str,
    registry: &ToolRegistry,
) -> (Vec<ValidatedCall>, Option<RepairError>) {
    let mut valid: Vec<ValidatedCall> = Vec::new();
    let mut last_err: Option<RepairError> = None;
    for json in extract_all_json_objects(raw) {
        // Ignore an incidental `{...}` that isn't a tool call — when a model "thinks out loud"
        // it embeds Python dicts / JSON examples in prose (e.g. `{'n': 5}`), and grabbing the
        // FIRST brace block made the harness try to parse that as the tool call ("key must be a
        // string"). A real tool call always has a `"tool"` key; require it before parsing.
        if !json.contains("\"tool\"") {
            continue;
        }
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
    (valid, last_err)
}

/// The leading run of **distinct-path whole-file writes** a model batched into one turn —
/// the safe-to-execute-in-sequence prefix (thread 3). qwen3-coder-30b emits the entire
/// solution as 20-40 tool calls in ONE turn and the loop ran just the first, discarding the
/// rest. Creating several DIFFERENT files in a row is order-independent and needs no
/// observe→react between them, so we can apply them all. The batch is strictly gated — it
/// stops at the FIRST call that is anything other than a `create_file`/`write_file` to a
/// **new** path:
///   - `edit_file` (anchored — needs the file's current state),
///   - a second write to a path already in the batch (the model is revising — react first),
///   - `run_verification`/`run_command`/`finish`/`read_file`/anything else (needs the result).
/// Returns the ordered batch (length ≥ 0). The caller still dispatches the FIRST call through
/// the normal single-action path; this only says which *additional* leading writes are safe to
/// pre-apply. An empty/length-1 result means "no batching — behave exactly as before".
pub fn extract_write_batch(raw: &str, registry: &ToolRegistry) -> Vec<ValidatedCall> {
    let (valid, _) = validated_calls(raw, registry);
    let mut seen_paths: Vec<String> = Vec::new();
    let mut batch: Vec<ValidatedCall> = Vec::new();
    for call in valid {
        let is_whole_file_write = call.name == "write_file" || call.name == "create_file";
        if !is_whole_file_write {
            break; // gate: anything but a whole-file write ends the safe run
        }
        let Some(path) = call.str("path").map(str::to_string) else {
            break;
        };
        if seen_paths.contains(&path) {
            break; // gate: a re-write of a path already in the batch — react first
        }
        seen_paths.push(path);
        batch.push(call);
    }
    batch
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

/// Recover a `write_file` from a model that replied with a fenced CODE BLOCK instead of a
/// tool call. Despite `/no_think`, qwen3-coder-30b often "thinks out loud" and writes the
/// file as ```python ... ``` — its natural format — which the JSON extractor rejects, costing
/// a turn (observed: a per-file step burned its whole budget this way). When the loop knows
/// the single file the step is writing (`default_path`, the focus file) and the reply has a
/// code fence, synthesize the `write_file(default_path, <block contents>)` call. Only the loop
/// calls this, as a fallback after `extract` errors — the happy path is untouched.
pub fn extract_markdown_write(
    raw: &str,
    default_path: &str,
    registry: &ToolRegistry,
) -> Option<ValidatedCall> {
    let body = fenced_code_block(raw)?;
    if body.trim().is_empty() {
        return None;
    }
    let mut obj = serde_json::Map::new();
    obj.insert(
        "tool".to_string(),
        serde_json::Value::String("write_file".to_string()),
    );
    obj.insert(
        "path".to_string(),
        serde_json::Value::String(default_path.to_string()),
    );
    obj.insert("content".to_string(), serde_json::Value::String(body));
    registry.validate(&serde_json::Value::Object(obj)).ok()
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
    obj.insert(
        "tool".to_string(),
        serde_json::Value::String(tool.to_string()),
    );
    obj.insert("path".to_string(), serde_json::Value::String(path));
    obj.insert("content".to_string(), serde_json::Value::String(content));
    Some(serde_json::Value::Object(obj))
}

/// Whether `raw` would be recovered by the truncation salvage — i.e. it's a `write_file`/
/// `create_file` whose content was cut off mid-string, and neither strict parsing nor the
/// closed-quote repair applies. The loop uses this to steer the model to `append_file` the
/// remainder rather than re-writing (and re-truncating) the whole file. Mirrors the guard
/// order in [`ParseRepair::extract`]: only true when the earlier paths would NOT have fired.
pub fn is_truncated_write_salvage(raw: &str, registry: &ToolRegistry) -> bool {
    let (valid, _) = validated_calls(raw, registry);
    if !valid.is_empty() {
        return false; // strict parse succeeded → not a salvage
    }
    if repair_file_content_call(raw).is_some_and(|v| registry.validate(&v).is_ok()) {
        return false; // closed-quote repair handles it → not a truncation
    }
    repair_truncated_file_write(raw).is_some_and(|v| registry.validate(&v).is_ok())
}

/// Salvage a `write_file`/`create_file`/`append_file` whose `content` string was **truncated** —
/// the model's reply was cut off mid-body, so the value has no closing quote and the object never
/// closes. Distinct from [`repair_file_content_call`], which recovers a body with inner quotes but
/// a present closer; here the closer is genuinely absent (the bytes never arrived). We take the
/// entire remaining text from the content-open-quote to end-of-reply as the partial body.
///
/// The rebuilt tool preserves append semantics: a truncated `append_file` stays `append_file` (a
/// partial chunk is safe to append — it's additive, and the model continues with the NEXT chunk),
/// while `write_file`/`create_file` both rebuild as `write_file` (an idempotent overwrite; create
/// would fail "already exists" if the head landed on a prior attempt). Either way the partial
/// body lands, turning the truncation loop into forward progress.
///
/// Guard: only salvage when the content really is unterminated (a closing unescaped quote would
/// mean a proper closer exists — leave those to the parser / closed-quote repair). Requires a
/// non-trivial partial body so a bare `"content":"` cut isn't written as an empty file.
fn repair_truncated_file_write(raw: &str) -> Option<serde_json::Value> {
    // append_file is checked first so a reply mentioning it isn't mis-tagged as write_file.
    let tool = ["append_file", "write_file", "create_file"]
        .into_iter()
        .find(|t| raw.contains(&format!("\"{t}\"")))?;
    let path = quoted_value_after(raw, "\"path\"")?;

    let key_pos = raw.find("\"content\"")?;
    let after_key = &raw[key_pos + "\"content\"".len()..];
    // Accept the JSON `:` and also a stray `=` a small model sometimes emits in its place
    // (observed live: `"content"=` on an append turn). Take whichever separator comes first.
    let sep = after_key
        .find(':')
        .into_iter()
        .chain(after_key.find('='))
        .min()?;
    let rest = &after_key[sep + 1..];
    let open_q = rest.find('"')?;
    let body_region = &rest[open_q + 1..];

    // Confirm the body is unterminated: scan for an unescaped `"` that would close the value.
    // If one exists, this isn't a truncation — defer to the parser / closed-quote repair.
    let mut escaped = false;
    for ch in body_region.chars() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return None; // a real closer exists → not truncated
        }
    }

    // The whole remaining reply is the partial content. Trim a dangling backslash that would
    // have escaped the next (never-emitted) char, then lenient-unescape the escapes that DID
    // arrive intact.
    let literal = body_region.strip_suffix('\\').unwrap_or(body_region);
    let content = unescape_json_string_lenient(literal);
    if content.trim().is_empty() {
        return None; // nothing meaningful arrived — don't write/append an empty body
    }

    // Preserve append semantics; collapse write/create to an idempotent write.
    let rebuilt = if tool == "append_file" {
        "append_file"
    } else {
        "write_file"
    };
    let mut obj = serde_json::Map::new();
    obj.insert(
        "tool".to_string(),
        serde_json::Value::String(rebuilt.to_string()),
    );
    obj.insert("path".to_string(), serde_json::Value::String(path));
    obj.insert("content".to_string(), serde_json::Value::String(content));
    Some(serde_json::Value::Object(obj))
}

/// Key-aware recovery for an `edit_file` call whose `old_str`/`new_str` bodies broke strict JSON
/// (the model put a multi-line code snippet — raw newlines, `'''` docstrings, inner `"` — into
/// those fields without escaping). Observed live: 19 of 45 captured parse failures were exactly
/// this. We can't brace-count through code, so pull the THREE values out by position: `path` is
/// well-formed and first; `old_str` runs from after its opening quote to the `","new_str":"`
/// separator; `new_str` runs from there to the final closing quote of the object. Each literal
/// is lenient-unescaped and re-inserted, so serde re-serializes it correctly for validation.
fn repair_edit_file_call(raw: &str) -> Option<serde_json::Value> {
    if !raw.contains("\"edit_file\"") {
        return None;
    }
    let path = quoted_value_after(raw, "\"path\"")?;

    // The body region starts after `"old_str":"` and ends at the object's final `"`.
    let old_key = raw.find("\"old_str\"")?;
    let after_old = &raw[old_key + "\"old_str\"".len()..];
    let colon = after_old.find(':')?;
    let rest = &after_old[colon + 1..];
    let open_q = rest.find('"')?;
    let body_region = &rest[open_q + 1..];
    let last_q = body_region.rfind('"')?;
    let body = &body_region[..last_q];

    // Split the two values at the literal separator the model emits between them. Accept a little
    // whitespace variation around the key. If absent (only old_str present), new_str is empty.
    let (old_lit, new_lit) = split_on_new_str(body).unwrap_or((body, ""));

    let mut obj = serde_json::Map::new();
    obj.insert(
        "tool".to_string(),
        serde_json::Value::String("edit_file".to_string()),
    );
    obj.insert("path".to_string(), serde_json::Value::String(path));
    obj.insert(
        "old_str".to_string(),
        serde_json::Value::String(unescape_json_string_lenient(old_lit)),
    );
    obj.insert(
        "new_str".to_string(),
        serde_json::Value::String(unescape_json_string_lenient(new_lit)),
    );
    Some(serde_json::Value::Object(obj))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_tools::default_registry;

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
    fn prefers_the_real_call_over_a_narrated_illustration() {
        // Observed live (2026-07-15): the coder model NARRATES a tool call in prose as an
        // illustration ("Let me edit: {"tool":"edit_file",...truncated...}") and THEN emits the
        // real complete call. The narrated copy is often incomplete/mangled; picking it applies
        // garbage. The complete, well-formed call must win.
        let reg = default_registry();
        let raw = "Let me make the edit: {\"tool\":\"edit_file\",\"path\":\"terrain.rs\",\
                   \"old_str\":\"pub struct Terrain { seed\
                   \n\nActually, here is the real edit:\n\
                   {\"tool\":\"edit_file\",\"path\":\"terrain.rs\",\"old_str\":\"let x = 1;\",\
                   \"new_str\":\"let x = 2;\"}";
        let call = ParseRepair.extract(raw, &reg).unwrap();
        assert_eq!(call.name, "edit_file");
        assert_eq!(
            call.str("old_str"),
            Some("let x = 1;"),
            "picked the complete call"
        );
        assert_eq!(call.str("new_str"), Some("let x = 2;"));
    }

    #[test]
    fn rejects_a_run_on_edit_that_absorbed_the_next_argument() {
        // The ship_render.rs corruption (observed live 2026-07-21): the model's `new_str` value ran
        // past its closing quote and absorbed a following `,"old_str":"…` — the braces still
        // balanced, so it parsed as ONE object with a `new_str` containing raw JSON. Applying it
        // spliced `};","old_str":"use …` into the source. It MUST be rejected, not written.
        let reg = default_registry();
        // The `new_str` VALUE literally contains the run-on marker `","old_str":` — the model's
        // broken quoting embedded the next key inside the string (an escaped inner quote), so it
        // parses as one object with a corrupt `new_str`. This is what landed raw JSON in the file.
        let raw = r#"{"tool":"edit_file","path":"ship_render.rs","old_str":"use foo::{Bar};","new_str":"use foo::{Bar};\n\nuse foo::SeatType;\",\"old_str\":\"use foo::{Bar};"}"#;
        let err = ParseRepair.extract(raw, &reg).unwrap_err();
        assert_eq!(
            err,
            RepairError::Swallowed,
            "run-on edit rejected, not applied"
        );
    }

    #[test]
    fn does_not_reject_a_legit_edit_whose_code_mentions_old_str_as_text() {
        // Guard against a false positive: real code can contain the identifier `old_str` — only a
        // value that RUNS ON into a JSON `","old_str":` framing is corrupt. A clean edit whose body
        // merely mentions the word must still apply.
        let reg = default_registry();
        let raw = r#"{"tool":"edit_file","path":"a.rs","old_str":"let old_str = 1;","new_str":"let old_str = 2; // renamed later"}"#;
        let call = ParseRepair.extract(raw, &reg).unwrap();
        assert_eq!(call.name, "edit_file");
        assert_eq!(
            call.str("new_str"),
            Some("let old_str = 2; // renamed later")
        );
    }

    #[test]
    fn skips_a_swallowed_call_when_a_clean_one_also_parsed() {
        // Both a swallowed call (its old_str embeds another "tool":) and a clean read parsed.
        // The clean one must win, not the corrupt swallowed edit.
        let reg = default_registry();
        let raw = "{\"tool\":\"edit_file\",\"path\":\"a.rs\",\"old_str\":\"x {\\\"tool\\\":\\\"y\\\"}\",\"new_str\":\"z\"}\
                   <tool_call|>{\"tool\":\"read_file\",\"path\":\"a.rs\"}";
        let call = ParseRepair.extract(raw, &reg).unwrap();
        assert_eq!(
            call.name, "read_file",
            "skipped the swallowed edit for the clean read"
        );
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
    fn write_batch_collects_consecutive_distinct_path_writes() {
        // The thread-3 case: the model emits the whole app as create/write calls in one turn.
        // extract_write_batch returns the leading run of DISTINCT-path whole-file writes.
        let reg = default_registry();
        let raw = "{\"tool\":\"create_file\",\"path\":\"store.py\",\"content\":\"a\"}\
                   {\"tool\":\"create_file\",\"path\":\"app.py\",\"content\":\"b\"}\
                   {\"tool\":\"write_file\",\"path\":\"util.py\",\"content\":\"c\"}\
                   {\"tool\":\"run_verification\"}";
        let batch = extract_write_batch(raw, &reg);
        let paths: Vec<_> = batch.iter().filter_map(|c| c.str("path")).collect();
        assert_eq!(
            paths,
            vec!["store.py", "app.py", "util.py"],
            "stops at run_verification"
        );
    }

    #[test]
    fn write_batch_stops_at_an_edit_or_a_repeated_path() {
        let reg = default_registry();
        // Gate 1: an edit_file (anchored — needs current file state) ends the batch.
        let raw_edit = "{\"tool\":\"write_file\",\"path\":\"a.py\",\"content\":\"x\"}\
                        {\"tool\":\"edit_file\",\"path\":\"a.py\",\"old_str\":\"x\",\"new_str\":\"y\"}";
        let b1 = extract_write_batch(raw_edit, &reg);
        assert_eq!(b1.len(), 1, "edit ends the batch: {b1:?}");
        // Gate 2: a re-write of a path already in the batch (revision — react first) ends it.
        let raw_dup = "{\"tool\":\"write_file\",\"path\":\"a.py\",\"content\":\"x\"}\
                       {\"tool\":\"write_file\",\"path\":\"a.py\",\"content\":\"x2\"}";
        let b2 = extract_write_batch(raw_dup, &reg);
        assert_eq!(b2.len(), 1, "duplicate path ends the batch: {b2:?}");
    }

    #[test]
    fn write_batch_is_empty_when_the_turn_does_not_lead_with_a_write() {
        let reg = default_registry();
        // A leading read → no batch (the loop's normal single-action path handles it).
        let raw = "{\"tool\":\"read_file\",\"path\":\"a.py\"}\
                   {\"tool\":\"write_file\",\"path\":\"b.py\",\"content\":\"x\"}";
        assert!(extract_write_batch(raw, &reg).is_empty());
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
    fn edit_file_with_multiline_unescaped_bodies_is_recovered() {
        // The single largest live parse-failure class: edit_file whose old_str/new_str carry a
        // multi-line code snippet with RAW newlines (and inner quotes), which is invalid JSON.
        // strict parse + the control-char escaper both fail (an inner `"` desyncs them); the
        // key-aware edit repair pulls path/old_str/new_str out by position instead.
        let reg = default_registry();
        // Shaped like the real live failures: multi-line bodies with raw newlines (the invalid-
        // JSON part). Single-quoted Python so the boundary detection isn't fighting an inner `"`
        // right at the separator (that pathological case is rare and left to strict parsing).
        let raw = "{\"tool\":\"edit_file\",\"path\":\"app.py\",\"old_str\":\"def f():\n    return 1\n\",\"new_str\":\"def f():\n    return 2\n\"}";
        let call = ParseRepair.extract(raw, &reg).expect("recovers the edit");
        assert_eq!(call.name, "edit_file");
        assert_eq!(call.str("path"), Some("app.py"));
        // Both bodies recovered with their real newlines, split at the right boundary.
        assert!(
            call.str("old_str").unwrap().contains("return 1"),
            "old: {:?}",
            call.str("old_str")
        );
        assert!(
            call.str("new_str").unwrap().contains("return 2"),
            "new: {:?}",
            call.str("new_str")
        );
        assert!(call.str("old_str").unwrap().contains('\n'));
    }

    #[test]
    fn incidental_python_dicts_in_prose_are_not_mistaken_for_a_tool_call() {
        // The model "thinks out loud" with Python dicts in the prose (`{'n': 5}`). The
        // extractor must IGNORE those (no "tool" key) and not try to parse one as the call.
        let reg = default_registry();
        // Pure prose with dicts, no tool call → a clean repairable error (NoJson), not a
        // confusing "key must be a string".
        let prose = "I'll return {'result': 25} when given {'n': 5}. Let me implement it.";
        assert!(ParseRepair.extract(prose, &reg).is_err());
        // Prose dicts FOLLOWED by a real tool call → the real call is found.
        let mixed = "First {'n': 5} then the call:\n{\"tool\":\"finish\"}";
        assert_eq!(ParseRepair.extract(mixed, &reg).unwrap().name, "finish");
    }

    #[test]
    fn a_fenced_code_block_recovers_a_write_to_the_focused_file() {
        // The model replies with a ```python``` block instead of a JSON tool call. With a known
        // focus file, extract_markdown_write synthesizes the write_file it meant.
        let reg = default_registry();
        let raw =
            "Here is the implementation:\n\n```python\ndef square(n):\n    return n * n\n```\n";
        let call = extract_markdown_write(raw, "mathlib.py", &reg).expect("recovered a write");
        assert_eq!(call.name, "write_file");
        assert_eq!(call.str("path"), Some("mathlib.py"));
        assert_eq!(
            call.str("content"),
            Some("def square(n):\n    return n * n\n")
        );
        // No fence → no recovery (don't invent a write from prose).
        assert!(extract_markdown_write("just prose, no code", "x.py", &reg).is_none());
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
        assert!(
            content.contains("\"\"\"doc string\"\"\""),
            "got: {content:?}"
        );
        assert!(content.contains("def f():") && content.contains("return 1"));
    }

    #[test]
    fn truncated_write_file_is_salvaged_to_the_partial_body() {
        // The css-truncation loop: a small model's write_file content runs past its output
        // length and the reply is cut off mid-string — no closing quote, JSON never parses,
        // and both the strict path and the closed-quote repair fail. The salvage must land the
        // partial content that DID arrive (rebuilt as write_file) so the model can append the
        // rest, instead of re-emitting the same over-long content forever.
        let reg = default_registry();
        let raw = "{\"tool\":\"write_file\",\"path\":\"styles.css\",\"content\":\"body {\\n  color: #333;\\n}\\n\\n#home {\\n  padding: 4rem";
        let call = ParseRepair
            .extract(raw, &reg)
            .expect("a truncated write_file must be salvaged, not looped");
        assert_eq!(call.name, "write_file");
        assert_eq!(call.str("path"), Some("styles.css"));
        let content = call.str("content").unwrap();
        // The head that arrived is preserved with real newlines applied.
        assert!(
            content.starts_with("body {\n  color: #333;\n}"),
            "got: {content:?}"
        );
        assert!(
            content.contains("#home {\n  padding: 4rem"),
            "got: {content:?}"
        );
    }

    #[test]
    fn truncated_append_file_stays_append_not_write() {
        // A truncated append_file must be salvaged as append_file (additive — the partial chunk
        // is safe to add and the model continues), NOT rewritten as write_file (which would
        // clobber everything appended so far). This is the site2 gap: append chunks truncated
        // and had no salvage, dropping the #cta rule and leaving a dangling <span>.
        let reg = default_registry();
        let raw = "{\"tool\":\"append_file\",\"path\":\"styles.css\",\"content\":\"#cta {\\n  padding: 15px;\\n}\\n\\n#menu li {\\n  display: flex";
        let call = ParseRepair
            .extract(raw, &reg)
            .expect("a truncated append_file must be salvaged");
        assert_eq!(
            call.name, "append_file",
            "append semantics preserved, not collapsed to write"
        );
        assert_eq!(call.str("path"), Some("styles.css"));
        assert!(call
            .str("content")
            .unwrap()
            .starts_with("#cta {\n  padding: 15px;"));
    }

    #[test]
    fn truncated_write_tolerates_equals_for_colon_separator() {
        // Observed live on an append turn: the model emitted `"content"=` instead of `"content":`.
        // Combined with truncation, strict parsing fails at the `=`; the salvage accepts either
        // separator so the partial body still lands.
        let reg = default_registry();
        let raw = "{\"tool\":\"append_file\",\"path\":\"a.html\",\"content\"=\"  <li>Latte</li>\\n  <li>Mocha";
        let call = ParseRepair
            .extract(raw, &reg)
            .expect("the `=` separator variant must still be salvaged");
        assert_eq!(call.name, "append_file");
        assert!(call.str("content").unwrap().contains("<li>Latte</li>"));
    }

    #[test]
    fn truncation_salvage_does_not_fire_when_content_is_properly_closed() {
        // A complete, well-formed write_file must NOT be treated as truncated — it parses
        // strictly and the salvage never runs. Byte-exact content proves no interference.
        let reg = default_registry();
        let raw = r#"{"tool":"write_file","path":"a.css","content":"body { color: red; }\n"}"#;
        let call = ParseRepair.extract(raw, &reg).unwrap();
        assert_eq!(call.str("content"), Some("body { color: red; }\n"));
    }

    #[test]
    fn truncation_salvage_ignores_an_empty_partial_body() {
        // Cut off right at the opening quote — nothing meaningful arrived. Don't write an
        // empty file; fall through to the normal error so the model retries cleanly.
        let raw = "{\"tool\":\"write_file\",\"path\":\"a.css\",\"content\":\"";
        assert!(repair_truncated_file_write(raw).is_none());
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
        let mut req = sc_model::GenerateRequest::new(vec![]);
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
        let mut req = sc_model::GenerateRequest::new(vec![]);
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
        use sc_model::Capabilities;
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
