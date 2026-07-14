//! [`UiConfig`] — the GUI's config surface, a plain owned mirror of the `dc-cli`
//! `Cli` fields the settings panel edits (spec 06/12). It carries no borrows and no
//! iced types, so it is `Send` and host-testable: the worker thread builds the
//! backends and the `dc_core::AgentConfig` / `dc_swarm::SwarmConfig` *from an owned
//! clone*, exactly the way `Cli::backend()` / `agent_config()` / `swarm_config()` do
//! — the GUI is just another front-end producing the same config (spec 01).

use std::sync::Arc;

use dc_core::{AgentConfig, Confirmer};
use dc_model::OpenAiBackend;
use dc_swarm::SwarmConfig;
use dc_tools::PermissionPolicy;

/// How the worker endpoint enforces tool calling — the GUI mirror of the CLI's
/// `--tool-calling` (spec 02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolCalling {
    /// Plain text prompting (the small-model default that always works).
    #[default]
    None,
    /// The backend's native function-calling API.
    Native,
    /// llama.cpp GBNF-constrained decoding.
    Gbnf,
}

/// The full GUI config surface. Every field maps to a `Cli` field; the settings
/// panel edits this struct and the run/swarm worker consumes a clone of it.
#[derive(Debug, Clone)]
pub struct UiConfig {
    // --- Coder (worker) backend ---
    pub base_url: String,
    pub model: String,
    pub tool_calling: ToolCalling,

    // --- Optional advisor ("junior asks senior", spec 02) ---
    pub advisor_url: Option<String>,
    pub advisor_model: Option<String>,

    // --- Optional orchestrator (swarm decomposer, T1) ---
    pub orchestrator_url: Option<String>,
    pub orchestrator_model: Option<String>,

    // --- Verification + planning ---
    pub verify_command: Option<String>,
    pub plan_first: bool,
    pub system_suffix: Option<String>,

    // --- Swarm knobs ---
    pub max_workers: usize,
    pub max_subtask_retries: usize,
    pub frozen_paths: Vec<String>,

    // --- Permission posture (spec 04/06) ---
    pub yolo: bool,
    pub allow: Vec<String>,
    pub dry_run: bool,
    pub verbose: bool,

    /// The directory a run reads and writes in. Defaults to an isolated scratch dir
    /// under the system temp dir — NEVER the launch/current dir, so a swarm can never
    /// scatter generated files into the user's source tree. (The CLI uses the cwd
    /// because the user invokes it deliberately there; the GUI has no such intent.)
    pub workspace: std::path::PathBuf,

    /// Run the verify command inside a per-run Docker container (spec 12) instead of on
    /// the host — a pinned Python toolkit + known layout, so a build can't depend on or
    /// pollute the host. On by default (the recommended sandbox).
    pub use_docker: bool,
    /// The Docker image to verify in (built from `docker/pyenv/Dockerfile`).
    pub docker_image: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        // The live-test defaults from MEMORY: coder on :11435, advisor on :11434.
        Self {
            // ONE model does everything now (plan + implement) — no swarm, no advisor.
            // qwen3-coder-30b-a3b (scripts/coder-30b.ps1): llama.cpp on :11435, alias
            // `qwen3-coder-30b`, an MoE split across both GPUs (--tensor-split 12,8).
            // It strictly beats the 8B (clears the whole difficulty ladder) and is
            // actually faster (~112 tok/s) since only ~3B activates per token. Edit in
            // settings for a different endpoint/model; scripts/pool-8b.ps1 is the 8B
            // fallback pool (:11439/:11440) for the parallel MCP swarm.
            base_url: "http://localhost:11435/v1".to_string(),
            model: "qwen3-coder-30b".to_string(),
            tool_calling: ToolCalling::None,
            // No separate advisor/orchestrator: the workflow planner and the implement
            // agent both use the single backend above (orchestrator()/advisor() fall back
            // to base_url/model when unset). The single-agent pivot dropped the swarm.
            advisor_url: None,
            advisor_model: None,
            orchestrator_url: None,
            orchestrator_model: None,
            // The TDD build needs a verify command to drive the implementation against
            // the frozen tests. Default to pytest (the live boxes are Python); editable
            // in settings. Without it the build stops at "plan + tests written".
            verify_command: Some("python -m pytest -q".to_string()),
            plan_first: false,
            // No system suffix. The historical `/no_think` was for early Qwen3 reasoning
            // models that burned the budget on a `<think>` block; the current coder model
            // (qwen3-coder-30b) has NO thinking mode — confirmed live: zero <think> tags in
            // a full ladder run — so `/no_think` was dead text bloating every system prompt
            // and the model ignored it anyway. Editable in settings if a thinking model is used.
            system_suffix: None,
            max_workers: 2,
            max_subtask_retries: 2,
            frozen_paths: Vec::new(),
            yolo: false,
            allow: Vec::new(),
            dry_run: false,
            verbose: false,
            workspace: default_workspace(),
            use_docker: true,
            docker_image: "dumb-coder-pyenv".to_string(),
        }
    }
}

/// The default GUI workspace: an isolated scratch dir under the system temp dir. This
/// is deliberately NOT the current/launch dir — a swarm writing whole files must never
/// land in the user's source tree.
pub fn default_workspace() -> std::path::PathBuf {
    std::env::temp_dir().join("dumb-coder-workspace")
}

/// List the **source** files in `workspace` (workspace-relative, sorted) — i.e. the
/// real output, excluding tests, the plan dir, and tooling caches. This is what a run
/// actually *built*, so the UI can show "5 files built" / "0 files built" plainly.
pub fn source_files(workspace: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // Skip hidden/dot dirs (.dumb-coder, .pytest_cache, .git), caches, deps.
            if name.starts_with('.') || name == "__pycache__" || name == "node_modules" {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    let rel = path
                        .strip_prefix(workspace)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if !is_test_file(&rel) {
                        out.push(rel);
                    }
                }
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// Whether a workspace-relative path looks like a test file (so it's excluded from the
/// "source files built" count).
fn is_test_file(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.contains("test_")
        || lower.contains(".test.")
        || lower.contains("_test.")
        || lower.contains(".spec.")
}

/// Pick the verify command that matches the tests that were actually written, so a
/// JS/JSX project isn't checked with `pytest` (it wrote JS tests it can't run —
/// observed live 2026-06-14). Detects the dominant test language from the test file
/// extensions and returns the conventional runner. Falls back to `python -m pytest`
/// (the configured default) when nothing recognizable was written.
pub fn detect_verify_command(test_files: &[String], fallback: &str) -> String {
    let mut py = 0usize;
    let mut js = 0usize;
    let mut rs = 0usize;
    let mut go = 0usize;
    for f in test_files {
        let lower = f.to_ascii_lowercase();
        if lower.ends_with(".py") {
            py += 1;
        } else if lower.ends_with(".js")
            || lower.ends_with(".jsx")
            || lower.ends_with(".ts")
            || lower.ends_with(".tsx")
        {
            js += 1;
        } else if lower.ends_with(".rs") {
            rs += 1;
        } else if lower.ends_with("_test.go") || lower.ends_with(".go") {
            go += 1;
        }
    }
    // Pick the language with the most test files; ties favour the fallback's spirit.
    let max = py.max(js).max(rs).max(go);
    if max == 0 {
        return fallback.to_string();
    }
    if js == max {
        // Vitest runs jest-style tests and is the lightest to invoke headlessly.
        "npx vitest run".to_string()
    } else if py == max {
        "python -m pytest -q".to_string()
    } else if rs == max {
        "cargo test".to_string()
    } else {
        "go test ./...".to_string()
    }
}

/// Build a short overview of the files already in `workspace`, for the decomposer's
/// `repo_overview` — so when iterating on an existing project the orchestrator plans
/// *edits to existing files* (and new files) instead of assuming a blank slate. Returns
/// an empty string for an empty/missing dir (the from-scratch case). Walks recursively,
/// listing workspace-relative paths with byte sizes; capped so a huge tree can't blow
/// the prompt budget.
pub fn repo_overview(workspace: &std::path::Path) -> String {
    /// Cap on listed files (keep the decomposer prompt bounded).
    const MAX_FILES: usize = 200;

    let mut files: Vec<(String, u64)> = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // Skip VCS/build/generated noise so the overview is the user's actual sources
            // (and doesn't get swamped by e.g. a `screenshots/` folder full of PNGs).
            if crate::filetree::is_noise_dir(name) {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    let rel = path
                        .strip_prefix(workspace)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    files.push((rel, size));
                }
                _ => {}
            }
        }
    }

    if files.is_empty() {
        return String::new();
    }
    files.sort();
    let truncated = files.len() > MAX_FILES;
    let mut out = String::from("Existing files (edit these in place where the task applies):\n");
    for (rel, size) in files.iter().take(MAX_FILES) {
        out.push_str(&format!("  {rel} ({size} bytes)\n"));
    }
    if truncated {
        out.push_str(&format!("  … and {} more\n", files.len() - MAX_FILES));
    }
    out
}

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
        // Adopt the real context window the server serves the model at (e.g. 24576) instead
        // of the conservative 8192 default — best-effort, falls back to the default if the
        // server doesn't advertise it. This is the worker backend that drives the agent
        // loop, where the under-budget hurt most.
        b.with_detected_context()
    }

    /// Build the advisor backend if a model was set — its own URL if given, else the
    /// coder endpoint (mirror of `Cli::advisor()`).
    pub fn advisor(&self) -> Option<OpenAiBackend> {
        let url = self
            .advisor_url
            .clone()
            .unwrap_or_else(|| self.base_url.clone());
        self.advisor_model
            .as_ref()
            .map(|m| OpenAiBackend::new(url.clone(), m.clone()))
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
        OpenAiBackend::new(url, model)
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

    /// Where verify runs: a per-run Docker container (the default) or the host.
    pub fn sandbox(&self) -> dc_verify::Sandbox {
        if self.use_docker {
            dc_verify::Sandbox::Docker {
                image: self.docker_image.clone(),
            }
        } else {
            dc_verify::Sandbox::Host
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_verify_command_matches_the_test_language() {
        let js = detect_verify_command(
            &["client/Home.test.jsx".into(), "api.test.js".into()],
            "python -m pytest -q",
        );
        assert_eq!(js, "npx vitest run");

        let py = detect_verify_command(&["test_app.py".into()], "x");
        assert_eq!(py, "python -m pytest -q");

        let rs = detect_verify_command(&["src/foo_test.rs".into()], "x");
        assert_eq!(rs, "cargo test");

        // Nothing recognizable → the fallback.
        assert_eq!(detect_verify_command(&[], "the-fallback"), "the-fallback");
    }

    #[test]
    fn source_files_excludes_tests_and_tooling() {
        let dir = std::env::temp_dir().join(format!("dc-win-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("server")).unwrap();
        std::fs::create_dir_all(dir.join(".dumb-coder/plan")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("server/app.py"), "x").unwrap();
        std::fs::write(dir.join("index.html"), "x").unwrap();
        std::fs::write(dir.join("tests/test_app.py"), "x").unwrap(); // test → excluded
        std::fs::write(dir.join(".dumb-coder/plan/01-specs.md"), "x").unwrap(); // plan → excluded

        let src = source_files(&dir);
        assert!(src.contains(&"server/app.py".to_string()), "{src:?}");
        assert!(src.contains(&"index.html".to_string()), "{src:?}");
        assert!(
            !src.iter().any(|f| f.contains("test")),
            "tests excluded: {src:?}"
        );
        assert!(
            !src.iter().any(|f| f.contains("dumb-coder")),
            "plan excluded: {src:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repo_overview_is_empty_for_a_fresh_dir_and_lists_existing_files() {
        let dir = std::env::temp_dir().join(format!("dc-win-overview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("server")).unwrap();

        // Empty workspace ⇒ from-scratch ⇒ no overview.
        assert!(repo_overview(&dir).is_empty());

        // With files, the overview lists relative paths so the decomposer can plan edits.
        std::fs::write(dir.join("server/app.py"), "print('hi')").unwrap();
        std::fs::write(dir.join("index.html"), "<html></html>").unwrap();
        let ov = repo_overview(&dir);
        assert!(ov.contains("server/app.py"), "{ov}");
        assert!(ov.contains("index.html"), "{ov}");
        assert!(ov.contains("Existing files"), "{ov}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_workspace_makes_a_stamped_subfolder() {
        let cfg = UiConfig::default();
        let ws = cfg.run_workspace("2026-06-14_17-42-09");
        assert!(
            ws.ends_with("run-2026-06-14_17-42-09"),
            "got {}",
            ws.display()
        );
        assert!(
            ws.starts_with(&cfg.workspace),
            "run dir lives under the base"
        );
        assert!(ws.is_dir(), "the run dir is created");
        // Two different stamps ⇒ two different dirs (no overwrite between prompts).
        let other = cfg.run_workspace("2026-06-14_18-00-00");
        assert_ne!(ws, other);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn default_workspace_is_a_scratch_dir_not_the_cwd() {
        // The whole point of the fix: the GUI must never default to the launch dir,
        // or a swarm scatters files into the user's source tree.
        let ws = UiConfig::default().workspace;
        let cwd = std::env::current_dir().unwrap();
        assert_ne!(ws, cwd, "default workspace must not be the current dir");
        assert!(
            ws.starts_with(std::env::temp_dir()),
            "default workspace should live under the temp dir, got {}",
            ws.display()
        );
    }

    #[test]
    fn agent_config_mirrors_posture_flags() {
        let cfg = UiConfig {
            yolo: true,
            allow: vec!["git ".to_string()],
            dry_run: true,
            verify_command: Some("python -m pytest".to_string()),
            frozen_paths: vec!["tests/contract.py".to_string()],
            ..UiConfig::default()
        };
        let ac = cfg.agent_config(None);
        assert!(ac.permission.allow_shell, "yolo opens shell");
        assert_eq!(ac.permission.shell_allowlist, vec!["git ".to_string()]);
        assert!(ac.dry_run);
        assert_eq!(ac.verify_command.as_deref(), Some("python -m pytest"));
        assert_eq!(ac.permission.frozen_paths, vec!["tests/contract.py"]);
        assert!(ac.confirmer.is_none());
    }

    #[test]
    fn agent_config_carries_the_confirmer() {
        let ac = UiConfig::default().agent_config(Some(Arc::new(dc_core::AutoDeny)));
        assert!(
            ac.confirmer.is_some(),
            "the GUI's confirmer must thread through"
        );
    }

    #[test]
    fn swarm_workers_default_to_no_think_and_pin_frozen() {
        let cfg = UiConfig {
            max_workers: 3,
            max_subtask_retries: 1,
            frozen_paths: vec!["tests/a.py".to_string()],
            ..UiConfig::default()
        };
        let sc = cfg.swarm_config(None);
        assert_eq!(sc.max_workers, 3);
        assert_eq!(sc.max_subtask_retries, 1);
        assert_eq!(sc.frozen_paths, vec!["tests/a.py"]);
        assert_eq!(sc.worker.system_suffix.as_deref(), Some("/no_think"));
        assert_eq!(sc.worker.permission.frozen_paths, vec!["tests/a.py"]);
    }

    #[test]
    fn advisor_requires_a_model() {
        // No advisor model ⇒ no advisor backend.
        let none = UiConfig {
            advisor_model: None,
            ..UiConfig::default()
        };
        assert!(none.advisor().is_none(), "no advisor model ⇒ no advisor");

        let with = UiConfig {
            advisor_model: Some("senior".to_string()),
            ..UiConfig::default()
        };
        assert!(with.advisor().is_some());
    }

    #[test]
    fn single_model_pivot_has_no_separate_advisor_or_orchestrator() {
        // The pivot: ONE capable model (Qwen3-8B) does plan + implement. There is no
        // swarm and no advisor — both the workflow planner (orchestrator()) and the
        // implement agent fall back to the single backend, and no advisor is wired.
        let cfg = UiConfig::default();
        assert!(
            !cfg.model.is_empty(),
            "the single model must be set by default"
        );
        assert!(
            cfg.orchestrator_model.is_none() && cfg.orchestrator_url.is_none(),
            "no separate orchestrator — the planner uses the one model"
        );
        assert!(
            cfg.advisor().is_none(),
            "no advisor in the single-model setup (the harness self-recovers instead)"
        );
    }
}
