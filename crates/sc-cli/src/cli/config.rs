//! Turning parsed flags into the things a run actually needs: backends, the agent
//! config, the swarm config, the permission policy, the plan gate/think policies.
//!
//! The two `impl Cli` blocks that used to sit at either end of `lib.rs` are merged
//! here — they were the same concern split by distance, not by design.

use sc_model::OpenAiBackend;

use super::types::{Cli, ToolCallingArg};

impl Cli {
    /// Build the per-phase thinking policy for `plan` (spec 09) from the flags:
    /// start from `--think-all`/`--no-think-all` (or the smart default), then apply
    /// each `--think <phase>` / `--nothink <phase>` override.
    pub fn think_policy(&self) -> sc_workflow::ThinkPolicy {
        use sc_workflow::{Phase, ThinkPolicy};
        let mut policy = match self.think_base {
            Some(false) => ThinkPolicy::always_think(),
            Some(true) => ThinkPolicy::never_think(),
            None => ThinkPolicy::default(),
        };
        for (slug, suppress) in &self.think_steps {
            if let Some(phase) = Phase::ALL.iter().copied().find(|p| p.slug() == slug) {
                policy = policy.with(phase, *suppress);
            }
        }
        policy
    }

    /// Resolve the set of phases that stop at a human gate for `plan` (spec 09 —
    /// "Scaling the ceremony"):
    /// 1. an explicit `--gates` list wins (precise control);
    /// 2. else the `--ceremony` tier's set;
    /// 3. else `Full` — so bare `--interactive` still gates every phase (the
    ///    behavior before adaptive ceremony existed).
    pub fn ceremony_gates(&self) -> sc_workflow::PhaseSet {
        if let Some(gates) = self.gates {
            gates
        } else {
            self.ceremony.unwrap_or(sc_workflow::Ceremony::Full).gates()
        }
    }

    /// Whether `plan` should put a human at the gates at all. A run is gated if the
    /// user asked for `--interactive`/`--gate` *or* named any ceremony policy
    /// (`--ceremony`/`--gates`) — naming a policy implies wanting the gates.
    pub fn plan_is_gated(&self, interactive: bool) -> bool {
        interactive || self.ceremony.is_some() || self.gates.is_some()
    }

    /// Build the orchestrator (decomposer) backend for `swarm`: its own
    /// `--orchestrator-url`/`--orchestrator` if set, else the worker endpoint/model.
    pub fn orchestrator(&self) -> OpenAiBackend {
        let url = self
            .orchestrator_url
            .clone()
            .unwrap_or_else(|| self.base_url.clone());
        let model = self
            .orchestrator_model
            .clone()
            .unwrap_or_else(|| self.model.clone());
        // The planner key (Gemini's, when the planner is Gemini) — its own key, else the coder's.
        let key = self
            .orchestrator_key
            .clone()
            .or_else(|| self.api_key.clone());
        apply_key(OpenAiBackend::new(url, model), &key)
    }

    /// Build the advisor (senior) backend, if `--advisor` was given — on its own
    /// `--advisor-url` if set, else the coder's endpoint ("junior asks senior",
    /// spec 02; a different *server* lets the swarm run both co-resident).
    pub fn advisor(&self) -> Option<OpenAiBackend> {
        let url = self
            .advisor_url
            .clone()
            .unwrap_or_else(|| self.base_url.clone());
        let key = self
            .orchestrator_key
            .clone()
            .or_else(|| self.api_key.clone());
        self.advisor_model
            .as_ref()
            .map(|m| apply_key(OpenAiBackend::new(url.clone(), m.clone()), &key))
    }

    /// Build the configured backend, applying the requested enforcement (spec 02).
    pub fn backend(&self) -> OpenAiBackend {
        let b = match self.tool_calling {
            ToolCallingArg::None => OpenAiBackend::new(self.base_url.clone(), self.model.clone()),
            ToolCallingArg::Native => {
                OpenAiBackend::new(self.base_url.clone(), self.model.clone()).with_native_tools()
            }
            ToolCallingArg::Gbnf => {
                OpenAiBackend::llama_cpp(self.base_url.clone(), self.model.clone())
            }
        };
        // Attach the coder API key (hosted providers like Gemini need it; local ignore it),
        // before context detection so the `/models` probe is authenticated too.
        let b = apply_key(b, &self.api_key);
        // Adopt the real context window the server serves the model at (e.g. 12288/slot)
        // instead of the conservative 8192 default — best-effort, falls back to the default
        // if the server doesn't advertise it. Mirrors `sc_win::config::backend()`; without
        // it the prompt budget is squeezed to 5120 even on a pool served at -c 36864.
        b.with_detected_context()
    }

    /// Build the agent config from the parsed flags (used by `run`).
    pub fn agent_config(&self) -> sc_core::AgentConfig {
        sc_core::AgentConfig {
            verify_command: self.verify_command.clone(),
            plan_first: self.plan_first,
            system_suffix: self.system_suffix.clone(),
            permission: self.permission_policy(),
            dry_run: self.dry_run,
            verbose: self.verbose,
            ..Default::default()
        }
    }

    /// The permission policy from the safety flags (spec 04/06): `--yolo`
    /// pre-approves all shell, `--allow <prefix>` extends the allowlist. Frozen
    /// paths stay empty here — the swarm sets those separately.
    pub fn permission_policy(&self) -> sc_tools::PermissionPolicy {
        sc_tools::PermissionPolicy {
            allow_shell: self.yolo,
            shell_allowlist: self.allow.clone(),
            ..Default::default()
        }
    }

    /// Build the swarm config from the parsed flags (used by `swarm`). Workers run
    /// the per-worker agent config; the verify command also gates integration.
    ///
    /// Swarm workers are tiny, reasoning-prone models (Qwen3-1.7B and the like).
    /// Their `/no_think` suffix can't rely on the model-name auto-detect — a swarm
    /// run usually aliases the worker (`coder-0`) so the name never contains
    /// "qwen3". Default the worker suffix to `/no_think` unless one is set.
    pub fn swarm_config(&self) -> sc_swarm::SwarmConfig {
        let mut worker = self.agent_config();
        if worker.system_suffix.is_none() {
            worker.system_suffix = Some("/no_think".to_string());
        }
        sc_swarm::SwarmConfig {
            max_workers: self.max_workers,
            worker,
            verify_command: self.verify_command.clone(),
            // The explicit `--frozen` list, if given. When empty, the caller
            // (`main`) auto-detects test files from the workspace — done there
            // because it needs filesystem access this `&self` method lacks.
            frozen_paths: self.frozen_paths.clone(),
            max_subtask_retries: self.max_subtask_retries,
            // The CLI runs on the host (the user controls their own environment); the
            // GUI defaults to the Docker sandbox.
            sandbox: sc_swarm::Sandbox::Host,
            review: sc_swarm::ReviewConfig {
                enabled: self.review,
                action: self.review_action,
                gate_at: self.review_gate,
                ..Default::default()
            },
        }
    }

    /// The advisor swarm workers consult when they stall ("junior asks senior").
    /// Prefer an explicit `--advisor`; otherwise fall back to the orchestrator —
    /// the bigger, smarter model is already in VRAM, so workers should be able to
    /// ask it for help even when no separate advisor was named.
    pub fn swarm_advisor(&self) -> OpenAiBackend {
        self.advisor().unwrap_or_else(|| self.orchestrator())
    }
}

/// Attach `key` as a bearer token when set and non-blank; else return the backend unchanged
/// (the local-server default). Mirrors `sc_win::config`'s helper so both front-ends decide
/// key attachment identically.
fn apply_key(backend: OpenAiBackend, key: &Option<String>) -> OpenAiBackend {
    match key.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(k) => backend.with_api_key(k),
        None => backend,
    }
}
