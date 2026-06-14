//! Tool specifications and validation (spec 04 — Tool contract).
//!
//! Every tool declares a **strict** schema: named, typed parameters with no
//! free-form "kitchen-sink" object. A call is validated against its spec *before*
//! execution, and a bad call produces a precise, structured error the model can
//! act on in one turn — never a silent failure (spec 04, spec 03 repair loop).
//!
//! The schema is deliberately hand-rolled and tiny rather than pulling in a full
//! JSON-Schema engine: the v1 tool surface is small and narrow by design, and the
//! gateway is dependency-light. It is still rich enough to (a) reject malformed
//! calls with actionable messages and (b) emit a JSON-Schema / GBNF grammar for
//! constrained decoding (see `grammar`).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

/// The type of a single tool parameter. Kept minimal — the v1 tools need only
/// strings and optional strings — but typed so validation is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    /// A required, non-empty string.
    String,
    /// An optional string (may be absent; if present must be a string).
    OptionalString,
    /// A required integer.
    Integer,
    /// An optional integer.
    OptionalInteger,
}

impl ParamType {
    /// Is this parameter required to be present?
    pub fn required(self) -> bool {
        matches!(self, ParamType::String | ParamType::Integer)
    }
}

/// One declared parameter of a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParamSpec {
    pub name: &'static str,
    pub ty: ParamType,
    /// One-line, action-oriented description (shown to the model).
    pub description: &'static str,
}

impl ParamSpec {
    pub const fn new(name: &'static str, ty: ParamType, description: &'static str) -> Self {
        Self {
            name,
            ty,
            description,
        }
    }
}

/// How a tool affects the world — drives the permission layer (spec 04).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    /// Reads only; safe to auto-allow.
    ReadOnly,
    /// Mutates the workspace (edits, commits).
    Mutating,
    /// Arbitrary/destructive (shell). Confirm-gated by default.
    Destructive,
}

/// Default permission policy for a tool (spec 04 — permission layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Run without asking.
    Auto,
    /// Ask the human each call.
    Confirm,
    /// Denied unless explicitly enabled.
    DenyByDefault,
}

/// A tool's full contract: identity, schema, and safety class (spec 04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolSpec {
    /// Short, unambiguous name (the value of the `"tool"` field on the wire).
    pub name: &'static str,
    /// One-line, action-oriented description.
    pub description: &'static str,
    pub params: Vec<ParamSpec>,
    pub side_effect: SideEffect,
    pub permission: Permission,
}

impl ToolSpec {
    /// Look up a declared parameter by name.
    pub fn param(&self, name: &str) -> Option<&ParamSpec> {
        self.params.iter().find(|p| p.name == name)
    }
}

/// Why a tool call failed schema validation. Structured so the loop can turn it
/// into a precise repair message (spec 03/04).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// No `"tool"` field, or it wasn't a string.
    MissingToolName,
    /// The named tool isn't in the registry. Carries known names for a hint.
    UnknownTool { name: String, known: Vec<String> },
    /// The top-level call wasn't a JSON object.
    NotAnObject,
    /// A required parameter was absent.
    MissingParam { tool: String, param: &'static str },
    /// A parameter had the wrong JSON type.
    WrongType {
        tool: String,
        param: &'static str,
        expected: ParamType,
    },
    /// A required string was present but empty.
    EmptyString { tool: String, param: &'static str },
    /// A field not declared by the tool's schema was supplied (strict schemas —
    /// no kitchen-sink objects, spec 04).
    UnknownParam { tool: String, param: String },
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::MissingToolName => {
                write!(f, "missing required string field \"tool\"")
            }
            ValidationError::UnknownTool { name, known } => write!(
                f,
                "unknown tool {name:?}; available tools: {}",
                known.join(", ")
            ),
            ValidationError::NotAnObject => write!(f, "tool call must be a JSON object"),
            ValidationError::MissingParam { tool, param } => {
                write!(f, "tool {tool:?} requires parameter {param:?}")
            }
            ValidationError::WrongType {
                tool,
                param,
                expected,
            } => write!(
                f,
                "tool {tool:?} parameter {param:?} must be a {}",
                type_word(*expected)
            ),
            ValidationError::EmptyString { tool, param } => {
                write!(f, "tool {tool:?} parameter {param:?} must not be empty")
            }
            ValidationError::UnknownParam { tool, param } => {
                write!(f, "tool {tool:?} has no parameter {param:?}")
            }
        }
    }
}

fn type_word(t: ParamType) -> &'static str {
    match t {
        ParamType::String | ParamType::OptionalString => "string",
        ParamType::Integer | ParamType::OptionalInteger => "integer",
    }
}

/// A validated tool call: the tool name plus its argument object. Produced only
/// after [`ToolRegistry::validate`] succeeds, so downstream execution can trust
/// the shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCall {
    pub name: String,
    pub args: BTreeMap<String, Value>,
}

impl ValidatedCall {
    /// Fetch a string argument that validation has already proven present/typed.
    pub fn str(&self, param: &str) -> Option<&str> {
        self.args.get(param).and_then(Value::as_str)
    }

    /// Fetch an optional integer argument.
    pub fn int(&self, param: &str) -> Option<i64> {
        self.args.get(param).and_then(Value::as_i64)
    }
}

/// The set of tools the model may use, with validation (spec 04 — Tool Registry).
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    specs: Vec<ToolSpec>,
}

impl ToolRegistry {
    /// Build a registry from a list of specs.
    pub fn new(specs: Vec<ToolSpec>) -> Self {
        Self { specs }
    }

    /// All registered specs.
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Look up a spec by tool name.
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.specs.iter().find(|s| s.name == name)
    }

    fn known_names(&self) -> Vec<String> {
        self.specs.iter().map(|s| s.name.to_string()).collect()
    }

    /// Validate a parsed tool-call value against the registry.
    ///
    /// Expects an object of the form `{"tool":"<name>", ...params}`. On success
    /// returns the [`ValidatedCall`]; on failure a structured [`ValidationError`]
    /// that the loop renders into a repair prompt.
    pub fn validate(&self, value: &Value) -> Result<ValidatedCall, ValidationError> {
        let obj = value.as_object().ok_or(ValidationError::NotAnObject)?;

        let name = obj
            .get("tool")
            .and_then(Value::as_str)
            .ok_or(ValidationError::MissingToolName)?
            .to_string();

        let spec = self
            .get(&name)
            .ok_or_else(|| ValidationError::UnknownTool {
                name: name.clone(),
                known: self.known_names(),
            })?;

        // Reject undeclared fields (strict schema — spec 04).
        for key in obj.keys() {
            if key == "tool" {
                continue;
            }
            if spec.param(key).is_none() {
                return Err(ValidationError::UnknownParam {
                    tool: name.clone(),
                    param: key.clone(),
                });
            }
        }

        // Check each declared param.
        let mut args = BTreeMap::new();
        for p in &spec.params {
            match obj.get(p.name) {
                None => {
                    if p.ty.required() {
                        return Err(ValidationError::MissingParam {
                            tool: name.clone(),
                            param: p.name,
                        });
                    }
                }
                Some(v) => {
                    validate_value(&name, p, v)?;
                    args.insert(p.name.to_string(), v.clone());
                }
            }
        }

        Ok(ValidatedCall { name, args })
    }
}

fn validate_value(tool: &str, p: &ParamSpec, v: &Value) -> Result<(), ValidationError> {
    let is_string = matches!(p.ty, ParamType::String | ParamType::OptionalString);
    if is_string {
        let s = v.as_str().ok_or_else(|| ValidationError::WrongType {
            tool: tool.to_string(),
            param: p.name,
            expected: p.ty,
        })?;
        if p.ty == ParamType::String && s.is_empty() {
            return Err(ValidationError::EmptyString {
                tool: tool.to_string(),
                param: p.name,
            });
        }
    } else {
        // Integer types. serde_json distinguishes ints from floats.
        if v.as_i64().is_none() {
            return Err(ValidationError::WrongType {
                tool: tool.to_string(),
                param: p.name,
                expected: p.ty,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reg() -> ToolRegistry {
        ToolRegistry::new(vec![
            ToolSpec {
                name: "read_file",
                description: "Read a file.",
                params: vec![
                    ParamSpec::new("path", ParamType::String, "relative path"),
                    ParamSpec::new(
                        "max_lines",
                        ParamType::OptionalInteger,
                        "cap lines returned",
                    ),
                ],
                side_effect: SideEffect::ReadOnly,
                permission: Permission::Auto,
            },
            ToolSpec {
                name: "finish",
                description: "Done.",
                params: vec![],
                side_effect: SideEffect::ReadOnly,
                permission: Permission::Auto,
            },
        ])
    }

    #[test]
    fn validates_a_well_formed_call() {
        let call = reg()
            .validate(&json!({"tool":"read_file","path":"a.txt"}))
            .unwrap();
        assert_eq!(call.name, "read_file");
        assert_eq!(call.str("path"), Some("a.txt"));
    }

    #[test]
    fn accepts_optional_param_when_present_and_correctly_typed() {
        let call = reg()
            .validate(&json!({"tool":"read_file","path":"a.txt","max_lines":20}))
            .unwrap();
        assert_eq!(call.int("max_lines"), Some(20));
    }

    #[test]
    fn rejects_unknown_tool_with_a_helpful_list() {
        let err = reg().validate(&json!({"tool":"frobnicate"})).unwrap_err();
        match &err {
            ValidationError::UnknownTool { name, known } => {
                assert_eq!(name, "frobnicate");
                assert!(known.contains(&"read_file".to_string()));
            }
            other => panic!("expected UnknownTool, got {other:?}"),
        }
        assert!(err.to_string().contains("read_file"));
    }

    #[test]
    fn rejects_missing_required_param() {
        let err = reg().validate(&json!({"tool":"read_file"})).unwrap_err();
        assert_eq!(
            err,
            ValidationError::MissingParam {
                tool: "read_file".into(),
                param: "path"
            }
        );
    }

    #[test]
    fn rejects_wrong_type() {
        let err = reg()
            .validate(&json!({"tool":"read_file","path":123}))
            .unwrap_err();
        assert!(matches!(err, ValidationError::WrongType { .. }));
        assert!(err.to_string().contains("must be a string"));
    }

    #[test]
    fn rejects_empty_required_string() {
        let err = reg()
            .validate(&json!({"tool":"read_file","path":""}))
            .unwrap_err();
        assert!(matches!(err, ValidationError::EmptyString { .. }));
    }

    #[test]
    fn rejects_undeclared_field() {
        let err = reg()
            .validate(&json!({"tool":"read_file","path":"a","bogus":1}))
            .unwrap_err();
        assert_eq!(
            err,
            ValidationError::UnknownParam {
                tool: "read_file".into(),
                param: "bogus".into()
            }
        );
    }

    #[test]
    fn rejects_missing_tool_name_and_non_objects() {
        assert_eq!(
            reg().validate(&json!({"path":"a"})).unwrap_err(),
            ValidationError::MissingToolName
        );
        assert_eq!(
            reg().validate(&json!([1, 2, 3])).unwrap_err(),
            ValidationError::NotAnObject
        );
    }

    #[test]
    fn finish_takes_no_params() {
        let call = reg().validate(&json!({"tool":"finish"})).unwrap();
        assert_eq!(call.name, "finish");
        assert!(call.args.is_empty());
    }
}
