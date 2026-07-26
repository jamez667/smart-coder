//! Name → implementation: the executor the agent loop calls with a validated tool call.

use std::path::Path;

use crate::spec::ValidatedCall;

use super::guards::{is_code_path, looks_like_tool_call_json};
use super::read::{list_dir, read_file, read_function, search_code};
use super::write::{append_file, create_file, edit_file, edit_function, edit_lines, write_file};

/// The result of executing a validated tool call.
pub enum ToolOutcome {
    /// Text fed back to the model as the next observation.
    Observation(String),
    /// The model called `finish`.
    Finished,
}

/// Execute a *validated* call against `workspace`.
///
/// Because the call already passed [`ToolRegistry::validate`], the arguments are
/// known to be present and well-typed. Runtime failures (missing file, bad path)
/// still become observations, never panics.
///
/// [`ToolRegistry::validate`]: crate::spec::ToolRegistry::validate
pub fn execute(call: &ValidatedCall, workspace: &Path) -> ToolOutcome {
    match call.name.as_str() {
        "finish" => ToolOutcome::Finished,
        "read_file" => ToolOutcome::Observation(read_file(
            workspace,
            arg(call, "path"),
            call.int("start"),
            call.int("limit"),
        )),
        "list_dir" => ToolOutcome::Observation(list_dir(workspace, arg(call, "path"))),
        "search_code" => ToolOutcome::Observation(search_code(workspace, arg(call, "query"))),
        "read_function" => ToolOutcome::Observation(read_function(
            workspace,
            arg(call, "path"),
            arg(call, "name"),
        )),
        "edit_function" => {
            let path = arg(call, "path");
            let body = arg(call, "new_body");
            // Same nested-tool-call guard as the other writers.
            if is_code_path(path) && looks_like_tool_call_json(body) {
                ToolOutcome::Observation(format!(
                    "edit_function {path} rejected: the new_body you sent is a tool-call JSON \
                     object, not source code. Send the RAW function text as new_body."
                ))
            } else {
                ToolOutcome::Observation(edit_function(workspace, path, arg(call, "name"), body))
            }
        }
        "write_file" | "create_file" | "append_file" | "edit_file" | "edit_lines" => {
            // Guard: the model sometimes nests its NEXT tool call (or a ```json fence wrapping one)
            // inside the content/new_str field, and we'd write that raw JSON scaffolding into the
            // source file — corrupting it with `{"tool":"edit_file",...}` text (observed live on
            // the lakes render stage: mod.rs got a literal nested edit_file object as its body).
            // Reject it before the write so the model re-sends real file text, not a tool call.
            let body_key = match call.name.as_str() {
                "edit_file" => "new_str",
                "edit_lines" => "new_text",
                _ => "content",
            };
            let body = arg(call, body_key);
            let path = arg(call, "path");
            if is_code_path(path) && looks_like_tool_call_json(body) {
                ToolOutcome::Observation(format!(
                    "{} {path} rejected: the {body_key} you sent is a tool-call JSON object (or a \
                     ```json fence), not source code — writing it would corrupt the file. Send the \
                     RAW file text as {body_key} (no surrounding JSON, no code fences, no nested \
                     {{\"tool\":...}}). One tool call per reply.",
                    call.name
                ))
            } else {
                match call.name.as_str() {
                    "write_file" => ToolOutcome::Observation(write_file(workspace, path, body)),
                    "create_file" => ToolOutcome::Observation(create_file(workspace, path, body)),
                    "append_file" => ToolOutcome::Observation(append_file(workspace, path, body)),
                    "edit_file" => ToolOutcome::Observation(edit_file(
                        workspace,
                        path,
                        arg(call, "old_str"),
                        body,
                    )),
                    "edit_lines" => ToolOutcome::Observation(edit_lines(
                        workspace,
                        path,
                        call.int("start"),
                        call.int("end"),
                        body,
                    )),
                    _ => unreachable!(),
                }
            }
        }
        // run_command / run_verification execute processes and need run config, so
        // the agent loop (sc-core) handles them; they never reach this fs executor.
        // The registry only dispatches names it knows; an unknown name here means
        // a tool was registered without a matching arm. Surface it loudly.
        other => ToolOutcome::Observation(format!("internal: no executor for tool {other:?}")),
    }
}

/// Pull a validated string arg. Safe to unwrap-with-default because validation
/// guaranteed required strings are present; optional/absent yields "".
fn arg<'a>(call: &'a ValidatedCall, name: &str) -> &'a str {
    call.str(name).unwrap_or_default()
}
