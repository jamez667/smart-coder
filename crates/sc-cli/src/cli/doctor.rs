//! The `doctor` report and the backend reachability probes.
//!
//! [`probe`] is a real (tiny) generation, not a port check: a server can accept
//! connections without having the model loaded, and that distinction is the whole
//! point of `doctor`. [`preflight`] applies the same probe to every backend a run
//! needs, so a dead server fails at the start with a clear message instead of
//! producing empty artifacts halfway through.

use sc_model::{Capabilities, ModelBackend};
use sc_proto::{DcError, Result};

use super::types::Cli;

/// Render the `doctor` report. `reachable` carries the probe result so the
/// formatting is testable without a live server.
pub fn doctor_report(cli: &Cli, caps: &Capabilities, reachable: &Result<()>) -> String {
    let status = match reachable {
        Ok(()) => "reachable ✓".to_string(),
        Err(e) => format!("UNREACHABLE ✗ — {e}"),
    };
    format!(
        "smart-coder doctor\n\
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

fn tool_calling_word(tc: sc_model::ToolCalling) -> &'static str {
    match tc {
        sc_model::ToolCalling::None => "parse+repair (no enforcement)",
        sc_model::ToolCalling::OpenAiStyle => "native function-calling",
        sc_model::ToolCalling::Gbnf => "GBNF grammar-constrained",
    }
}

/// Probe the backend with a tiny generation to confirm it's actually serving the
/// model — not just that the port is open (spec 06: "model is pulled").
pub fn probe(backend: &dyn ModelBackend) -> Result<()> {
    use sc_model::{GenerateRequest, Message};
    let req = GenerateRequest::new(vec![Message::user("ping")]);
    backend.generate(&req).map(|_| ())
}

/// Preflight every backend a run needs, by `(label, backend)`. Returns a clear
/// error naming the first unreachable one, so a dead/crashed server fails fast at
/// the start with a useful message instead of producing empty artifacts mid-run.
/// Backends sharing a `name()` (same model+endpoint) are only probed once.
pub fn preflight(backends: &[(&str, &dyn ModelBackend)]) -> Result<()> {
    let mut seen: Vec<String> = Vec::new();
    for (label, backend) in backends {
        // On-device backends have no endpoint to probe.
        if backend.capabilities().on_device {
            continue;
        }
        let key = backend.name().to_string();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if let Err(e) = probe(*backend) {
            return Err(DcError::Eval(format!(
                "preflight: the {label} backend ({}) isn't responding ({e}). \
                 Is the server up and the model loaded?",
                backend.name()
            )));
        }
    }
    Ok(())
}
