//! The failure vocabulary ([`RepairError`]) and the strategy contract.
//!
//! Every strategy shares one post-condition: turn a model turn into either a
//! validated call or a structured error precise enough to re-prompt from — telling
//! a small model "you got it wrong" isn't enough; showing it a valid call is.

use sc_model::GenerateRequest;
use sc_tools::{ToolRegistry, ValidatedCall, ValidationError};

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
