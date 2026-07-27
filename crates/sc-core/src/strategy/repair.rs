//! Salvage: recovering a real tool call from a broken model reply.
//!
//! These are the harness's answer to the single largest live failure class — a
//! small model emitting JSON whose string arguments carry raw multi-line code, so
//! the reply either never parses or one argument swallows the next. Each helper
//! fires only after the strict path has already failed, so a well-formed call
//! never touches this code.

use sc_tools::{ToolRegistry, ValidatedCall};

use crate::text::{
    escape_raw_control_chars_in_strings, extract_all_json_objects, fenced_code_block,
    quoted_value_after, split_on_new_str, unescape_json_string_lenient,
};

use super::error::RepairError;

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
pub(super) fn looks_swallowed(call: &ValidatedCall) -> bool {
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
pub(super) fn recover_swallowed_call(raw: &str) -> Option<serde_json::Value> {
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
pub(super) fn validated_calls(
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
///
///   - `edit_file` (anchored — needs the file's current state),
///   - a second write to a path already in the batch (the model is revising — react first),
///   - `run_verification`/`run_command`/`finish`/`read_file`/anything else (needs the result).
///
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

/// Tools that change the workspace or end the run. When a model batches several calls
/// in one turn (e.g. `read → create → verify → finish`), the leading reads are it
/// re-confirming context; the call that actually *makes progress* is the first of
/// these. The loop preserves observe-react by running just that one and feeding back.
pub(super) fn is_progress_tool(name: &str) -> bool {
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
pub(super) fn repair_file_content_call(raw: &str) -> Option<serde_json::Value> {
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
pub(super) fn repair_truncated_file_write(raw: &str) -> Option<serde_json::Value> {
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
pub(super) fn repair_edit_file_call(raw: &str) -> Option<serde_json::Value> {
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
