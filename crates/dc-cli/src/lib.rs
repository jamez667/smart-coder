//! `dumb-coder` CLI — the M0 surface (spec 06): a `doctor` check and a trivial
//! chat loop against a *real* backend.
//!
//! The interesting, testable logic lives here (arg parsing, the doctor report,
//! backend construction); [`crate::main`] is a thin I/O shell over it. This keeps
//! the binary unit-tested in the project's TDD style.
//!
//! M0 scope is deliberately small: prompt → model text → print, **no tools**. The
//! tool-driven agent loop already lives in `dc-core`; wiring it behind a `run`
//! subcommand is M1+ work.

use dc_model::{Capabilities, ModelBackend, OpenAiBackend};
use dc_proto::{DcError, Result};

/// Default OpenAI-compatible endpoint: Ollama's compat server on localhost.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";
/// Default model — the project's primary small-model target (spec 00).
pub const DEFAULT_MODEL: &str = "gemma4:e4b";

/// What the user asked the CLI to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Probe the backend and print the effective configuration.
    Doctor,
    /// Interactive chat REPL (the default with no subcommand).
    Chat,
    /// Print usage.
    Help,
}

/// Which tool-call enforcement to ask the backend for (spec 02). Maps onto the
/// backend variant and the strategy `dc-core` then selects from its capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallingArg {
    /// Plain completion + parse/repair (works against any server).
    None,
    /// OpenAI-style native function-calling.
    Native,
    /// llama.cpp GBNF grammar-constrained decoding.
    Gbnf,
}

/// A fully-resolved invocation: the command plus the backend config to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub command: Command,
    pub base_url: String,
    pub model: String,
    pub tool_calling: ToolCallingArg,
}

impl Cli {
    /// Parse argv (excluding the program name) into a [`Cli`].
    ///
    /// Grammar (M0): `[doctor|chat|help] [--base-url URL] [--model NAME]`. Flags
    /// may appear in any order; an unknown token is an error rather than silently
    /// ignored (spec 00 — fail loud).
    pub fn parse<I, S>(args: I) -> Result<Cli>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut command: Option<Command> = None;
        let mut base_url = DEFAULT_BASE_URL.to_string();
        let mut model = DEFAULT_MODEL.to_string();
        let mut tool_calling = ToolCallingArg::None;

        let mut it = args.into_iter().map(Into::into);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "doctor" if command.is_none() => command = Some(Command::Doctor),
                "chat" if command.is_none() => command = Some(Command::Chat),
                "help" | "--help" | "-h" => command = Some(Command::Help),
                "--base-url" => {
                    base_url = it.next().ok_or_else(|| {
                        DcError::Eval("--base-url requires a URL argument".to_string())
                    })?;
                }
                "--model" => {
                    model = it.next().ok_or_else(|| {
                        DcError::Eval("--model requires a NAME argument".to_string())
                    })?;
                }
                "--tool-calling" => {
                    let v = it.next().ok_or_else(|| {
                        DcError::Eval("--tool-calling requires none|native|gbnf".to_string())
                    })?;
                    tool_calling = match v.as_str() {
                        "none" => ToolCallingArg::None,
                        "native" => ToolCallingArg::Native,
                        "gbnf" => ToolCallingArg::Gbnf,
                        other => {
                            return Err(DcError::Eval(format!(
                                "--tool-calling must be none|native|gbnf, got {other:?}"
                            )))
                        }
                    };
                }
                other => {
                    return Err(DcError::Eval(format!(
                        "unknown argument: {other:?} (try `dumb-coder help`)"
                    )));
                }
            }
        }

        Ok(Cli {
            command: command.unwrap_or(Command::Chat),
            base_url,
            model,
            tool_calling,
        })
    }

    /// Build the configured backend, applying the requested enforcement (spec 02).
    pub fn backend(&self) -> OpenAiBackend {
        match self.tool_calling {
            ToolCallingArg::None => OpenAiBackend::new(self.base_url.clone(), self.model.clone()),
            ToolCallingArg::Native => {
                OpenAiBackend::new(self.base_url.clone(), self.model.clone()).with_native_tools()
            }
            ToolCallingArg::Gbnf => {
                OpenAiBackend::llama_cpp(self.base_url.clone(), self.model.clone())
            }
        }
    }
}

/// Usage text (spec 06 — invocation modes, trimmed to the M0 surface).
pub fn usage() -> &'static str {
    "\
dumb-coder — an agentic coding tool for small models (M0)

USAGE:
    dumb-coder [COMMAND] [OPTIONS]

COMMANDS:
    chat            Interactive chat with the model (default)
    doctor          Check the backend is reachable; print effective config
    help            Show this message

OPTIONS:
    --base-url URL        OpenAI-compatible endpoint  [default: http://localhost:11434/v1]
    --model NAME          Model to use                [default: gemma4:e4b]
    --tool-calling MODE   none | native | gbnf — how the backend enforces tool
                          calls (spec 02)             [default: none]

EXAMPLES:
    dumb-coder doctor
    dumb-coder --model gemma4:e4b --tool-calling native
    dumb-coder doctor --base-url http://localhost:8080/v1 --tool-calling gbnf"
}

/// Render the `doctor` report. `reachable` carries the probe result so the
/// formatting is testable without a live server.
pub fn doctor_report(cli: &Cli, caps: &Capabilities, reachable: &Result<()>) -> String {
    let status = match reachable {
        Ok(()) => "reachable ✓".to_string(),
        Err(e) => format!("UNREACHABLE ✗ — {e}"),
    };
    format!(
        "dumb-coder doctor\n\
         \x20 backend:        openai-compat\n\
         \x20 base url:       {}\n\
         \x20 model:          {}\n\
         \x20 status:         {}\n\
         \x20 context budget: {} tokens\n\
         \x20 tool calling:   {}",
        cli.base_url,
        cli.model,
        status,
        caps.max_context_tokens,
        tool_calling_word(caps.tool_calling),
    )
}

fn tool_calling_word(tc: dc_model::ToolCalling) -> &'static str {
    match tc {
        dc_model::ToolCalling::None => "parse+repair (no enforcement)",
        dc_model::ToolCalling::OpenAiStyle => "native function-calling",
        dc_model::ToolCalling::Gbnf => "GBNF grammar-constrained",
    }
}

/// Probe the backend with a tiny generation to confirm it's actually serving the
/// model — not just that the port is open (spec 06: "model is pulled").
pub fn probe(backend: &dyn ModelBackend) -> Result<()> {
    use dc_model::{GenerateRequest, Message};
    let req = GenerateRequest::new(vec![Message::user("ping")]);
    backend.generate(&req).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_chat_with_default_backend() {
        let cli = Cli::parse(Vec::<String>::new()).unwrap();
        assert_eq!(cli.command, Command::Chat);
        assert_eq!(cli.base_url, DEFAULT_BASE_URL);
        assert_eq!(cli.model, DEFAULT_MODEL);
        assert_eq!(cli.tool_calling, ToolCallingArg::None);
    }

    #[test]
    fn parses_tool_calling_modes_and_maps_to_backend() {
        use dc_model::{ModelBackend, ToolCalling};
        let native = Cli::parse(["--tool-calling", "native"]).unwrap();
        assert_eq!(native.tool_calling, ToolCallingArg::Native);
        assert_eq!(
            native.backend().capabilities().tool_calling,
            ToolCalling::OpenAiStyle
        );

        let gbnf = Cli::parse(["--tool-calling", "gbnf"]).unwrap();
        assert_eq!(
            gbnf.backend().capabilities().tool_calling,
            ToolCalling::Gbnf
        );

        assert!(Cli::parse(["--tool-calling", "bogus"]).is_err());
    }

    #[test]
    fn parses_doctor_with_overrides_in_any_order() {
        let cli = Cli::parse([
            "--model",
            "qwen2:1.5b",
            "doctor",
            "--base-url",
            "http://host:8000/v1",
        ])
        .unwrap();
        assert_eq!(cli.command, Command::Doctor);
        assert_eq!(cli.model, "qwen2:1.5b");
        assert_eq!(cli.base_url, "http://host:8000/v1");
    }

    #[test]
    fn help_is_recognized() {
        assert_eq!(Cli::parse(["help"]).unwrap().command, Command::Help);
        assert_eq!(Cli::parse(["--help"]).unwrap().command, Command::Help);
        assert_eq!(Cli::parse(["-h"]).unwrap().command, Command::Help);
    }

    #[test]
    fn unknown_argument_is_an_error_not_silently_ignored() {
        let err = Cli::parse(["--frobnicate"]).unwrap_err();
        assert!(err.to_string().contains("unknown argument"), "got: {err}");
    }

    #[test]
    fn flag_without_value_errors() {
        assert!(Cli::parse(["--model"]).is_err());
        assert!(Cli::parse(["--base-url"]).is_err());
    }

    #[test]
    fn doctor_report_shows_reachable_status_and_budget() {
        let cli = Cli::parse(["doctor"]).unwrap();
        let caps = cli.backend().capabilities();
        let report = doctor_report(&cli, &caps, &Ok(()));
        assert!(report.contains("reachable ✓"), "got: {report}");
        assert!(report.contains("8192 tokens"), "got: {report}");
        assert!(report.contains(DEFAULT_MODEL), "got: {report}");
    }

    #[test]
    fn doctor_report_surfaces_an_unreachable_backend() {
        let cli = Cli::parse(["doctor"]).unwrap();
        let caps = cli.backend().capabilities();
        let report = doctor_report(
            &cli,
            &caps,
            &Err(DcError::Backend("connection refused".to_string())),
        );
        assert!(report.contains("UNREACHABLE ✗"), "got: {report}");
        assert!(report.contains("connection refused"), "got: {report}");
    }
}
