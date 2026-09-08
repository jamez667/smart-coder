//! The gateway arm of the A/B: one `ask` tool instead of the reading half of
//! the measured six.
//!
//! # What is actually being compared
//!
//! Not "gateway versus tools" — the gateway is read-only, so it cannot replace
//! editing or verification and does not try to. Both arms get the same edit
//! tools, the same verify command, the same model, the same step cap. The only
//! difference is how the model **looks at the workspace**:
//!
//! | | control | gateway |
//! |---|---|---|
//! | look | `read_file` | `ask` |
//! | change | `edit_file`, `write_file` | same |
//! | check | `run_verification` | same |
//! | stop | `finish` | same |
//! | shell | `run_command` | *absent* |
//!
//! `run_command` is the one asymmetry, and it is deliberate: on the control arm
//! it is the model's investigation tool (measured: six tools got it 12/12), and
//! on the gateway arm that job is exactly what `ask` exists to do. Leaving both
//! would measure a model with two ways to look rather than the two designs. It
//! is reported in the scorecard so the asymmetry is never silent.
//!
//! # Why this lives here and not in `sc-core`
//!
//! `sc-core` gained a generic [`ExternalTool`] seam and no dependency on
//! `sc-gateway`. Wiring the gateway into the agent loop directly would ship the
//! thing being measured — the experiment has to be able to say "no" and leave
//! nothing behind.
//!
//! [`ExternalTool`]: sc_core::ExternalTool

use std::path::Path;
use std::sync::Mutex;

use sc_core::ExternalTool;
use sc_gateway::{Ctx, Gateway, Level, Need};
use sc_tools::{
    ParamSpec, ParamType, Permission, SideEffect, ToolOutcome, ToolSpec, ValidatedCall,
};

/// What the gateway arm did, for the report.
///
/// A solve rate alone cannot distinguish "the gateway answered well" from "the
/// gateway refused everything and the model guessed its way to green", so the
/// counts that separate those are kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskStats {
    /// Calls the model made to `ask`.
    pub calls: usize,
    /// Calls that reached a capability.
    pub routed: usize,
    /// Calls the classifier declined.
    pub refused: usize,
    /// Bytes the capabilities produced before simplification.
    pub raw_bytes: usize,
    /// Bytes handed back to the model.
    pub out_bytes: usize,
    /// Which capabilities were reached, most-used first when reported.
    pub by_capability: Vec<(String, usize)>,
    /// The needs the classifier declined, with the reason, most-frequent first.
    ///
    /// **The richest signal the A/B produces, and the first run threw it away.**
    /// A refusal rate is a number you can only shrug at; the actual phrasings a
    /// model typed and the gateway could not route are a to-do list for the
    /// capability table. Deduplicated with a count, because a model that asks
    /// the same unanswerable thing twenty times is one missing capability, not
    /// twenty.
    pub refusals: Vec<(String, String, usize)>,
}

impl AskStats {
    /// Share of calls that got an answer rather than a refusal.
    ///
    /// The number to watch: a high solve rate with a low answer rate means the
    /// gateway is not what solved the task.
    pub fn answer_percent(&self) -> u32 {
        if self.calls == 0 {
            return 0;
        }
        ((self.routed as f64 / self.calls as f64) * 100.0).round() as u32
    }

    pub fn retained_percent(&self) -> u32 {
        if self.raw_bytes == 0 {
            return 100;
        }
        ((self.out_bytes as f64 / self.raw_bytes as f64) * 100.0).round() as u32
    }

    fn record(&mut self, need: &str, trace: &sc_gateway::Trace) {
        self.calls += 1;
        self.raw_bytes += trace.raw_bytes;
        self.out_bytes += trace.out_bytes;
        match &trace.route {
            None => {
                self.refused += 1;
                let key = need.trim().to_lowercase();
                match self.refusals.iter_mut().find(|(n, _, _)| *n == key) {
                    Some((_, _, c)) => *c += 1,
                    None => self.refusals.push((key, trace.reason.clone(), 1)),
                }
            }
            Some(name) => {
                self.routed += 1;
                match self.by_capability.iter_mut().find(|(n, _)| n == name) {
                    Some((_, n)) => *n += 1,
                    None => self.by_capability.push((name.clone(), 1)),
                }
            }
        }
    }
}

/// The `ask` tool, backed by the gateway.
///
/// One free-text parameter and one optional scope. That narrowness IS the
/// hypothesis: a small model spends nothing on choosing a tool or filling a
/// schema, and the routing happens in code that can be tested without a model.
pub struct GatewayTool {
    gateway: Gateway,
    stats: Mutex<AskStats>,
    /// The verify command and sandbox for the task currently being solved.
    ///
    /// Per-task rather than per-solver because each ladder task brings its own
    /// command, and the gateway must never invent one. Set by the solver before
    /// each solve; `None` means verification is genuinely unavailable and
    /// `verify.run` refuses, which is the honest answer rather than a guess.
    verify: Mutex<Option<(sc_verify::Sandbox, String)>>,
}

impl GatewayTool {
    pub fn new() -> Self {
        Self {
            // Extraction only. The lossy summarizer would put a SECOND model in
            // the loop, and an A/B with two models on one arm measures nothing
            // you can attribute.
            gateway: Gateway::new().with_level(Level::Extract),
            stats: Mutex::new(AskStats::default()),
            verify: Mutex::new(None),
        }
    }

    /// Point `verify.run` at the task's own test command.
    ///
    /// **This is not a "live" capability.** It spawns a test process and
    /// involves no model at all, so admitting it does NOT put a second model in
    /// the A/B loop. The first run of this experiment left it unwired on the
    /// grounds that live capabilities are unattributable — conflating "needs a
    /// seam" with "needs a model" — and the result was a gateway arm that
    /// refused every "what is failing" while the control arm had a shell. The
    /// one capability whose reduction actually works was unreachable, and the
    /// run measured nothing.
    pub fn arm_verification(&self, sandbox: sc_verify::Sandbox, command: impl Into<String>) {
        *self.verify.lock().expect("verify lock") = Some((sandbox, command.into()));
    }

    /// What this tool did across the run.
    pub fn stats(&self) -> AskStats {
        let mut s = self.stats.lock().expect("stats lock").clone();
        s.by_capability
            .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        s.refusals.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        s
    }

    /// The `ask` spec, for the registry the gateway arm offers.
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: "ask",
            description: "Ask for anything you need to know about this workspace in plain \
                          English — a file's contents, where a symbol is defined, what is \
                          in a directory, what the tests say. One question per call.",
            params: vec![
                ParamSpec::new(
                    "need",
                    ParamType::String,
                    "what you need to know, in plain English",
                ),
                ParamSpec::new(
                    "path",
                    ParamType::OptionalString,
                    "the file or directory to look at, when you already know it",
                ),
            ],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        }
    }
}

impl Default for GatewayTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ExternalTool for GatewayTool {
    fn execute(&self, call: &ValidatedCall, workspace: &Path) -> Option<ToolOutcome> {
        if call.name != "ask" {
            return None;
        }
        let need = match call.str("path") {
            Some(p) if !p.is_empty() => Need::scoped(call.str("need").unwrap_or_default(), p),
            _ => Need::new(call.str("need").unwrap_or_default()),
        };
        // No model and no web: those capabilities WOULD put a second model (or a
        // network) in the loop and make the result unattributable, so they
        // refuse and the refusal is counted. Verification is different — it
        // spawns a test process and involves no model — so it is wired, and
        // "what is failing" is answerable through the gateway exactly as it is
        // through the control arm's shell.
        let armed = self.verify.lock().expect("verify lock").clone();
        let verify = armed.as_ref().map(|(sandbox, command)| sc_gateway::Verify {
            sandbox,
            command: command.as_str(),
        });
        let ctx = Ctx {
            workspace,
            model: None,
            web: None,
            verify: verify.as_ref(),
        };
        let answer = self.gateway.ask_with(&need, &ctx);
        self.stats
            .lock()
            .expect("stats lock")
            .record(&need.text, &answer.trace);
        Some(ToolOutcome::Observation(answer.text))
    }
}

/// The control arm: the measured six, exactly as `solver::task_registry` builds
/// them. Duplicated here rather than shared so a change to the eval's own
/// registry cannot silently move the baseline this experiment is measured
/// against.
pub fn control_registry() -> sc_tools::ToolRegistry {
    keep(&[
        "read_file",
        "edit_file",
        "write_file",
        "run_command",
        "run_verification",
        "finish",
    ])
}

/// The gateway arm: `ask` replaces `read_file`, and `run_command` goes with it.
///
/// Five tools against the control's six — the reading half collapses into one
/// door. Every mutating tool is identical, so a difference in solve rate is a
/// difference in how the model LOOKED, not in what it could change.
///
/// # `run_verification` stays a first-class tool, deliberately
///
/// `ask` can answer "what is failing" — the seam is wired and the reduction
/// applies — but the model keeps a direct verification tool too, and in practice
/// reaches for it. That is the arm AS IT WOULD SHIP: no real deployment would
/// hide the test command behind a classifier when a direct tool is right there.
///
/// The measurable cost is that the reduction rarely fires in this A/B, so what
/// is being compared is mostly ROUTING (does one plain-English door beat
/// choosing among tools) rather than context savings. The reduction is measured
/// separately and properly by `evals/gateway/output.toml`, against captured
/// cargo output, where it does not depend on a model choosing to use it.
pub fn gateway_registry() -> sc_tools::ToolRegistry {
    let mut specs: Vec<ToolSpec> = keep(&["edit_file", "write_file", "run_verification", "finish"])
        .specs()
        .to_vec();
    specs.insert(0, GatewayTool::spec());
    sc_tools::ToolRegistry::new(specs)
}

fn keep(names: &[&str]) -> sc_tools::ToolRegistry {
    let specs: Vec<ToolSpec> = sc_tools::default_registry()
        .specs()
        .iter()
        .filter(|s| names.contains(&s.name))
        .cloned()
        .collect();
    debug_assert_eq!(specs.len(), names.len(), "a kept tool is missing by name");
    sc_tools::ToolRegistry::new(specs)
}

// ---------------------------------------------------------------------------
// The solver that runs one arm.
//
// Deliberately a THIN wrapper over the same `run_agent_observed` the normal
// AgentSolver uses, with the same per-task config. Everything that could differ
// between the arms other than the tool surface — step cap, context budget,
// frozen tests, verify command, strategy — is therefore identical by
// construction rather than by care.
// ---------------------------------------------------------------------------

use std::sync::Arc;

use sc_core::{AgentConfig, ToolCallMetrics};
use sc_model::ModelBackend;
use sc_proto::Result;

use crate::solver::{RunInfo, Solver};
use crate::task::EvalTask;

/// Which arm of the ladder A/B to run.
///
/// `Control` and `Gateway` share the `sc_core` agent loop and differ only in
/// their registry ([`ArmSolver`]). `Raw` and `Pi` are whole other solvers
/// ([`crate::raw_arm::RawSolver`], [`crate::pi_arm::PiSolver`]) run through the
/// same `run_task`, so the TDD invariants hold for every arm identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    /// The measured six: `read_file` + `run_command` to look.
    Control,
    /// The model driven with no harness at all.
    Raw,
    /// The pi-style loop.
    Pi,
    /// One `ask` to look, same tools to change.
    Gateway,
}

impl Arm {
    /// Every arm, in the order a full comparison runs them.
    pub const ALL: [Arm; 4] = [Arm::Control, Arm::Raw, Arm::Pi, Arm::Gateway];

    pub fn label(self) -> &'static str {
        match self {
            Arm::Control => "control(6)",
            Arm::Raw => "raw",
            Arm::Pi => "pi",
            Arm::Gateway => "gateway(5)",
        }
    }

    /// The name accepted on the command line (`--arms control,raw,pi,gateway`).
    pub fn name(self) -> &'static str {
        match self {
            Arm::Control => "control",
            Arm::Raw => "raw",
            Arm::Pi => "pi",
            Arm::Gateway => "gateway",
        }
    }

    pub fn parse(s: &str) -> Option<Arm> {
        let s = s.trim();
        Arm::ALL
            .into_iter()
            .find(|a| a.name() == s || a.label() == s)
    }

    /// Parse a comma-separated list, rejecting the first name that is not an arm.
    pub fn parse_list(s: &str) -> std::result::Result<Vec<Arm>, String> {
        let mut arms = Vec::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let arm = Arm::parse(part).ok_or_else(|| {
                let known: Vec<&str> = Arm::ALL.iter().map(|a| a.name()).collect();
                format!("unknown arm {part:?}; known: {}", known.join(","))
            })?;
            if !arms.contains(&arm) {
                arms.push(arm);
            }
        }
        if arms.is_empty() {
            return Err("no arms given".to_string());
        }
        Ok(arms)
    }
}

/// Runs one agent-loop arm (`Control` or `Gateway`) against a real backend.
pub struct ArmSolver<'a> {
    backend: &'a dyn ModelBackend,
    arm: Arm,
    cfg: AgentConfig,
    tool: Arc<GatewayTool>,
    last: std::cell::Cell<Option<ToolCallMetrics>>,
    last_run: std::cell::RefCell<Option<RunInfo>>,
    /// Optional observer for the agent's event stream (a run log, a metrics
    /// counter). `None` means the run is a black box beyond its report.
    sink: Option<&'a dyn sc_core::EventSink>,
}

impl<'a> ArmSolver<'a> {
    /// `arm` must be `Control` or `Gateway`; the other arms are other solvers,
    /// built by [`build_arm`].
    pub fn new(backend: &'a dyn ModelBackend, arm: Arm, cfg: AgentConfig) -> Self {
        debug_assert!(
            matches!(arm, Arm::Control | Arm::Gateway),
            "{arm:?} is not an agent-loop arm; use build_arm"
        );
        Self {
            backend,
            arm,
            cfg,
            tool: Arc::new(GatewayTool::new()),
            last: std::cell::Cell::new(None),
            last_run: std::cell::RefCell::new(None),
            sink: None,
        }
    }

    /// Tee every agent event to `sink`, exactly as `AgentSolver::with_sink` does.
    pub fn with_sink(mut self, sink: &'a dyn sc_core::EventSink) -> Self {
        self.sink = Some(sink);
        self
    }

    /// What `ask` did across every task this solver ran. Empty on the control
    /// arm, which never calls it.
    pub fn ask_stats(&self) -> AskStats {
        self.tool.stats()
    }
}

/// One arm, ready to run: the solver plus the handle needed to read what its
/// `ask` tool did afterwards.
pub struct ArmRun<'a> {
    pub arm: Arm,
    pub solver: Box<dyn Solver + 'a>,
    gateway: Option<Arc<GatewayTool>>,
}

impl ArmRun<'_> {
    /// What `ask` did on this arm; `None` for arms that do not have it.
    pub fn ask_stats(&self) -> Option<AskStats> {
        self.gateway.as_ref().map(|g| g.stats())
    }
}

/// The arm factory: one place that knows how each arm is built, so the bin
/// only knows their names.
///
/// `url` and `model` are what `backend` was built from; the pi arm drives the
/// endpoint itself rather than through a `ModelBackend`. `sink` is attached to
/// the agent-loop arms; the raw and pi solvers own their own loops and are
/// handed nothing, so process metrics for them come only from what they report.
pub fn build_arm<'a>(
    arm: Arm,
    backend: &'a sc_model::OpenAiBackend,
    url: &str,
    model: &str,
    cfg: AgentConfig,
    sink: Option<&'a dyn sc_core::EventSink>,
) -> ArmRun<'a> {
    match arm {
        Arm::Control | Arm::Gateway => {
            let mut solver = ArmSolver::new(backend, arm, cfg);
            if let Some(s) = sink {
                solver = solver.with_sink(s);
            }
            let gateway = (arm == Arm::Gateway).then(|| solver.tool.clone());
            ArmRun {
                arm,
                solver: Box::new(solver),
                gateway,
            }
        }
        Arm::Raw => ArmRun {
            arm,
            solver: Box::new(crate::raw_arm::RawSolver::new(backend, cfg)),
            gateway: None,
        },
        Arm::Pi => ArmRun {
            arm,
            solver: Box::new(crate::pi_arm::PiSolver::new(url, model, &cfg)),
            gateway: None,
        },
    }
}

impl Solver for ArmSolver<'_> {
    fn name(&self) -> &str {
        self.arm.label()
    }

    fn solve(&self, task: &EvalTask, workspace: &Path) -> Result<()> {
        let instruction = format!(
            "Task: {}

The change is verified by running: {}
             Make that command exit 0. Do not edit any test files.",
            task.description, task.verify_cmd
        );
        // The same per-task layering the normal solver applies, so the arms
        // differ ONLY in their registry.
        let mut cfg = crate::solver::task_config(self.cfg.clone(), task);
        let registry = match self.arm {
            Arm::Control => control_registry(),
            Arm::Gateway => {
                // The same command the control arm's `run_verification` uses, so
                // both arms can answer "what is failing" — one through a shell,
                // one through the gateway. That parity is the whole experiment.
                self.tool
                    .arm_verification(cfg.sandbox.clone(), task.verify_cmd.clone());
                cfg.external_tool = Some(self.tool.clone());
                gateway_registry()
            }
            Arm::Raw | Arm::Pi => {
                return Err(sc_proto::DcError::Eval(format!(
                    "{} is not an agent-loop arm; build it with build_arm",
                    self.arm.label()
                )))
            }
        };
        let strategy = sc_core::select_strategy(&self.backend.capabilities());
        let report = sc_core::run_agent_observed(
            self.backend,
            None,
            &registry,
            strategy.as_ref(),
            &instruction,
            workspace,
            &cfg,
            self.sink.unwrap_or(&sc_core::NullSink),
        )?;
        self.last.set(Some(report.metrics));
        self.last_run.replace(Some(RunInfo {
            steps: report.steps,
            stop_reason: format!("{:?}", report.stop_reason),
            self_verified: report.verified,
            interventions: report.interventions,
            total_prompt_tokens: report.total_prompt_tokens,
            total_cached_prompt_tokens: report.total_cached_prompt_tokens,
            total_prefilled_prompt_tokens: report.total_prefilled_prompt_tokens,
            peak_reply_tokens: report.peak_reply_tokens,
            harness_faults: report.harness_faults.clone(),
        }));
        Ok(())
    }

    fn last_metrics(&self) -> Option<ToolCallMetrics> {
        self.last.get()
    }

    fn last_run(&self) -> Option<RunInfo> {
        self.last_run.borrow().clone()
    }
}
