//! Turning a resolved [`UiConfig`] into the things a run needs: the per-stage
//! backends, the agent/swarm config, the permission policy, and the sandbox.

use std::sync::Arc;

use sc_core::{AgentConfig, Confirmer};
use sc_model::OpenAiBackend;
use sc_swarm::SwarmConfig;
use sc_tools::PermissionPolicy;

use super::types::{apply_key, Connection, Provider, ToolCalling, UiConfig};

impl UiConfig {
    /// Resolve a fresh per-run workspace: a `run-<stamp>` folder under the base
    /// `workspace`, created on demand. Each prompt gets its own datetime-stamped dir so
    /// outputs never pile up or overwrite. `stamp` is caller-supplied (e.g.
    /// `2026-06-14_17-42-09`) so this stays host-testable — the GUI passes the real
    /// local time. Falls back to the base dir if creation fails.
    pub fn run_workspace(&self, stamp: &str) -> std::path::PathBuf {
        let dir = self.workspace.join(format!("run-{stamp}"));
        if std::fs::create_dir_all(&dir).is_ok() {
            dir
        } else {
            let _ = std::fs::create_dir_all(&self.workspace);
            self.workspace.clone()
        }
    }

    /// The [`Connection`] for a given provider.
    pub fn connection(&self, p: Provider) -> &Connection {
        match p {
            Provider::Local => &self.local_conn,
            Provider::Gemini => &self.gemini_conn,
        }
    }

    /// Flatten the connection + per-stage routing into the legacy scalar fields the backend
    /// builders read. Call this after editing connections/routing (the settings-panel commit does)
    /// and it's also run at the end of [`load`], so `backend()`/`orchestrator()`/`advisor()` never
    /// need to know connections exist.
    ///
    /// * CODER → `base_url` + `key` from the coder's connection.
    /// * PLANNER → `orchestrator_url` + `orchestrator_key` from the planner's connection. Set to
    ///   `None` when the planner is on the SAME connection as the coder, so `orchestrator()` falls
    ///   back to the coder endpoint exactly as before (no redundant duplicate persisted).
    /// * ADVISOR → `advisor_url` from the advisor's connection (same-as-coder ⇒ `None`). The
    ///   advisor key still rides the orchestrator/coder key in `advisor()` (unchanged).
    pub fn resolve_stages(&mut self) {
        // Coder is the base: its connection populates the primary endpoint/key.
        let coder = self.connection(self.coder_provider).clone();
        self.base_url = coder.base_url.clone();
        self.key = coder.key.clone();

        // Planner: only set orchestrator_* when it differs from the coder connection; otherwise
        // leave None so the existing coder-fallback in `orchestrator()` applies. Crucially, also
        // clear `orchestrator_model` when the planner shares the coder's connection — otherwise a
        // stale model name from a previous routing (e.g. `gemini-2.5-flash-lite` left over after
        // switching the planner back to Local) is sent to the LOCAL endpoint, which serves whatever
        // is loaded under that bogus name. Clearing it falls back to `self.model` (the local coder
        // model), which is what "planner = same connection as coder" means. (Observed live
        // 2026-07-21: planner routed Local still carried a Gemini model name, so the local model ran
        // the planning phases mislabeled as Gemini.)
        if self.planner_provider == self.coder_provider {
            self.orchestrator_url = None;
            self.orchestrator_key = None;
            self.orchestrator_model = None;
        } else {
            let plan = self.connection(self.planner_provider).clone();
            self.orchestrator_url = Some(plan.base_url);
            self.orchestrator_key = plan.key;
        }

        // Advisor endpoint follows its connection (same-as-coder ⇒ None ⇒ falls back to base_url).
        if self.advisor_provider == self.coder_provider {
            self.advisor_url = None;
        } else {
            self.advisor_url = Some(self.connection(self.advisor_provider).base_url.clone());
        }
    }

    /// Build the coder/worker backend, applying the requested tool-calling
    /// enforcement — the mirror of `Cli::backend()`.
    pub fn backend(&self) -> OpenAiBackend {
        let b = match self.tool_calling {
            ToolCalling::None => OpenAiBackend::new(self.base_url.clone(), self.model.clone()),
            ToolCalling::Native => {
                OpenAiBackend::new(self.base_url.clone(), self.model.clone()).with_native_tools()
            }
            ToolCalling::Gbnf => {
                OpenAiBackend::llama_cpp(self.base_url.clone(), self.model.clone())
            }
        };
        // Attach the coder API key if one is set (hosted providers like Gemini need it; local
        // servers ignore it). Do this BEFORE context detection so the `/models` probe is
        // authenticated too.
        let b = apply_key(b, &self.key);
        // Adopt the real context window the server serves the model at (e.g. 24576) instead
        // of the conservative 8192 default — best-effort, falls back to the default if the
        // server doesn't advertise it. This is the worker backend that drives the agent
        // loop, where the under-budget hurt most.
        b.with_detected_context()
    }

    /// Like [`backend`], but the returned backend honours `cancel`: setting it true aborts an
    /// in-flight streaming chat turn. Used by the chat composer's Cancel button.
    ///
    /// [`backend`]: UiConfig::backend
    pub fn backend_cancellable(
        &self,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> OpenAiBackend {
        self.backend().with_cancel(cancel)
    }

    /// Build the advisor backend if a model was set — its own URL if given, else the
    /// coder endpoint (mirror of `Cli::advisor()`).
    pub fn advisor(&self) -> Option<OpenAiBackend> {
        let url = self
            .advisor_url
            .clone()
            .unwrap_or_else(|| self.base_url.clone());
        let key = self.orchestrator_key.clone().or_else(|| self.key.clone());
        self.advisor_model
            .as_ref()
            .map(|m| apply_key(OpenAiBackend::new(url.clone(), m.clone()), &key))
    }

    /// Build the orchestrator (decomposer) backend — its own URL/model if set, else
    /// the worker endpoint/model (mirror of `Cli::orchestrator()`).
    pub fn orchestrator(&self) -> OpenAiBackend {
        let url = self
            .orchestrator_url
            .clone()
            .unwrap_or_else(|| self.base_url.clone());
        let model = self
            .orchestrator_model
            .clone()
            .unwrap_or_else(|| self.model.clone());
        // The planner key: an explicit orchestrator key if set, else fall back to the coder key
        // (so a single key set on the coder also authenticates a same-provider planner). This is
        // the seam that lets Gemini do the breakdown.
        let key = self.orchestrator_key.clone().or_else(|| self.key.clone());
        let b = apply_key(OpenAiBackend::new(url, model), &key);
        // Detect the server's real context window (like `backend()` does) — the workflow phases
        // ground on real file CONTENTS, which need the full window; at the hardcoded 8192 a
        // large source file is clipped and the design hallucinates around the missing code.
        b.with_detected_context()
    }

    /// The swarm's advisor: an explicit advisor if set, else the orchestrator
    /// (mirror of `Cli::swarm_advisor()`).
    pub fn swarm_advisor(&self) -> OpenAiBackend {
        self.advisor().unwrap_or_else(|| self.orchestrator())
    }

    /// The permission policy from the posture flags (mirror of
    /// `Cli::permission_policy()`): `--yolo` opens shell, `--allow` prefixes the
    /// allowlist, frozen paths are passed through.
    pub fn permission_policy(&self) -> PermissionPolicy {
        PermissionPolicy {
            frozen_paths: self.frozen_paths.clone(),
            allow_shell: self.yolo,
            shell_allowlist: self.allow.clone(),
        }
    }

    /// The single-run [`AgentConfig`], with an optional human confirmer wired into
    /// the new core seam (Part A). Mirror of `Cli::agent_config()` plus the
    /// confirmer the GUI supplies.
    pub fn agent_config(&self, confirmer: Option<Arc<dyn Confirmer>>) -> AgentConfig {
        AgentConfig {
            verify_command: self.verify_command.clone(),
            plan_first: self.plan_first,
            system_suffix: self.system_suffix.clone(),
            permission: self.permission_policy(),
            dry_run: self.dry_run,
            verbose: self.verbose,
            confirmer,
            ..AgentConfig::default()
        }
    }

    /// The [`SwarmConfig`] for a swarm run (mirror of `Cli::swarm_config()` +
    /// `swarm_config_with_frozen`). Workers default to `/no_think` to keep small
    /// models from burning budget in a reasoning block (see `system_suffix` doc).
    /// The per-subtask confirmer is shared across workers.
    pub fn swarm_config(&self, confirmer: Option<Arc<dyn Confirmer>>) -> SwarmConfig {
        let mut worker = self.agent_config(confirmer);
        if worker.system_suffix.is_none() {
            worker.system_suffix = Some("/no_think".to_string());
        }
        // The integration merge enforces frozen paths separately; the worker policy
        // also pins them so a worker never edits a contract test.
        worker.permission.frozen_paths = self.frozen_paths.clone();
        SwarmConfig {
            max_workers: self.max_workers,
            worker,
            verify_command: self.verify_command.clone(),
            frozen_paths: self.frozen_paths.clone(),
            max_subtask_retries: self.max_subtask_retries,
            sandbox: self.sandbox(),
        }
    }

    /// Where verify/agent commands run. A runtime `sandbox_override` (the GUI's live session
    /// container) wins; otherwise the `use_docker` decision: a per-run Docker container or the
    /// host.
    pub fn sandbox(&self) -> sc_verify::Sandbox {
        if let Some(s) = &self.sandbox_override {
            return s.clone();
        }
        if self.use_docker {
            sc_verify::Sandbox::Docker {
                image: self.docker_image.clone(),
            }
        } else {
            sc_verify::Sandbox::Host
        }
    }
}
