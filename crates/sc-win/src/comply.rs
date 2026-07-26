//! Generating a compliance evidence report from the IDE.
//!
//! Runs the `sc-comply` audit over the open workspace and writes the same
//! redacted HTML site the CLI's `comply-export` produces, then opens it.
//!
//! # The model is optional, and it never decides anything
//!
//! The audit itself is **fully deterministic** — the same workspace always
//! yields the same control results, and `sc-comply` cannot reach a model at all
//! (it has no `sc-model` dependency, enforced by the crate graph rather than by
//! a rule anyone has to remember).
//!
//! A model, when chosen, writes exactly two things:
//!
//! - the **executive summary** on the index page, and
//! - **auditor guidance** for controls a code scan could not settle — *what
//!   evidence would answer this*, never *is this satisfied*.
//!
//! Neither can change a control's status. That is why offering a choice of model
//! here — including "none" — is safe: the numbers are identical whichever way
//! the user picks, and only the prose differs.
//!
//! # Why "none" is the default
//!
//! Most people opening a project have no model configured for this, and a
//! compliance report is complete and useful without a word of prose. Generating
//! it should not silently spend API credits on a menu click.

use std::path::{Path, PathBuf};

use crate::config::{Provider, UiConfig};

/// Which model writes the summary and guidance, if any.
///
/// Deliberately a separate choice from the coder/planner/advisor routing in
/// [`UiConfig`]: a user may well want a local model driving their code and a
/// hosted one writing an audit document, or — most often — no model here at all
/// while still using one to write code.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ComplyModel {
    /// Deterministic only. No executive summary, no auditor guidance.
    ///
    /// The default, and never a failure: every control result is present and the
    /// report stands on its own.
    #[default]
    None,
    /// The configured local server (llama.cpp / Ollama).
    Local,
    /// Gemini via its OpenAI-compatible endpoint.
    Gemini,
}

impl ComplyModel {
    /// Every option, in display order.
    pub const ALL: [ComplyModel; 3] = [ComplyModel::None, ComplyModel::Local, ComplyModel::Gemini];

    pub fn label(self) -> &'static str {
        match self {
            ComplyModel::None => "None",
            ComplyModel::Local => "Local",
            ComplyModel::Gemini => "Gemini",
        }
    }

    /// The provider this maps to, or `None` for the deterministic path.
    pub fn provider(self) -> Option<Provider> {
        match self {
            ComplyModel::None => None,
            ComplyModel::Local => Some(Provider::Local),
            ComplyModel::Gemini => Some(Provider::Gemini),
        }
    }

    /// A one-line caveat to show under the picker, if this choice warrants one.
    ///
    /// Local models are **not evaluated** for compliance writing. The failure
    /// mode is specific and worth naming: a small model produces fluent,
    /// confident prose that reads as authoritative on a document an auditor or a
    /// customer may read. That is different from a wrong code suggestion, which
    /// fails visibly at compile time.
    pub fn caveat(self) -> Option<&'static str> {
        match self {
            ComplyModel::None => None,
            ComplyModel::Local => Some(
                "Local models are not evaluated for compliance writing. Review the summary \
                 and guidance before sharing the report. Control results are unaffected — \
                 they never use a model.",
            ),
            ComplyModel::Gemini => Some(
                "The summary and guidance are model-written prose over the audit's own \
                 figures. Read them before sharing. Control results are unaffected — they \
                 never use a model.",
            ),
        }
    }
}

/// Why a report could not be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComplyError {
    /// No project folder is open.
    NoWorkspace,
    /// The chosen provider has no API key configured.
    MissingKey(&'static str),
    /// The audit or the write failed.
    Failed(String),
}

impl std::fmt::Display for ComplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComplyError::NoWorkspace => {
                write!(
                    f,
                    "open a project folder first — the audit reads the workspace"
                )
            }
            ComplyError::MissingKey(p) => write!(
                f,
                "{p} has no API key. Set one in Settings → Connections, or choose None to \
                 generate the report without a summary."
            ),
            ComplyError::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// What a completed run produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ComplyReport {
    /// The index page — what gets opened.
    pub index: PathBuf,
    /// Per-framework totals, for the confirmation line.
    pub frameworks: usize,
    pub controls: usize,
    pub passed: usize,
    pub gaps: usize,
    pub unknown: usize,
    /// Whether a model actually contributed prose. False when the model was
    /// unreachable, so the UI can say so rather than implying a summary exists.
    pub narrated: bool,
}

/// Build the backend for the chosen model, or `None` for the deterministic path.
///
/// Returns `Err` only when the user asked for a model that cannot be reached —
/// a missing key is worth stopping for, because silently downgrading to "no
/// summary" would look like the feature is broken.
pub fn backend_for(
    cfg: &UiConfig,
    choice: ComplyModel,
) -> Result<Option<sc_model::OpenAiBackend>, ComplyError> {
    let Some(provider) = choice.provider() else {
        return Ok(None);
    };
    let conn = cfg.connection(provider);
    let key = conn.key.as_deref().map(str::trim).filter(|k| !k.is_empty());

    if provider == Provider::Gemini && key.is_none() {
        return Err(ComplyError::MissingKey("Gemini"));
    }

    let model = model_for(cfg, provider);
    let mut backend = sc_model::OpenAiBackend::new(conn.base_url.clone(), model);
    if let Some(k) = key {
        backend = backend.with_api_key(k);
    }
    // NOT with_detected_context(): that probes for llama.cpp's `n_ctx`, which a
    // hosted provider does not serve — the probe fails and the context silently
    // caps at the 8192 default, truncating a reasoning model mid-summary.
    Ok(Some(backend.with_context_tokens(128_000)))
}

/// Which model name to use for the chosen provider.
///
/// For Gemini this is a Pro-tier alias rather than the coder model: the coder is
/// typically a small fast model, and the summary is the one place in this
/// feature where writing quality is the whole point. For Local there is only one
/// model served, so the configured one is the only sensible choice.
fn model_for(cfg: &UiConfig, provider: Provider) -> String {
    match provider {
        Provider::Gemini => std::env::var("SC_NARRATIVE_MODEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "gemini-pro-latest".to_string()),
        Provider::Local => cfg.model.clone(),
    }
}

/// Audit every shipped framework and write the redacted HTML site.
///
/// **Blocking** — callers must run this off the UI thread. A full ten-framework
/// audit walks the workspace once per pack and, with a model chosen, makes one
/// summary call plus a batched guidance call per framework.
///
/// `out_dir` is created if absent. Every page is redacted before rendering; the
/// site renderer asserts that independently and panics rather than publish a
/// citation, so a bug here fails loudly instead of leaking a file path.
pub fn run(
    workspace: &Path,
    out_dir: &Path,
    choice: ComplyModel,
    cfg: &UiConfig,
) -> Result<ComplyReport, ComplyError> {
    if !workspace.is_dir() {
        return Err(ComplyError::NoWorkspace);
    }
    let backend = backend_for(cfg, choice)?;
    std::fs::create_dir_all(out_dir)
        .map_err(|e| ComplyError::Failed(format!("cannot create {}: {e}", out_dir.display())))?;

    let options = sc_comply::collector::ComplyOptions::default();
    let generated_at = sc_comply::evidence::now_rfc3339();
    let registry = sc_comply::collector::Registry::builtin();

    let mut entries: Vec<sc_comply::report::site::IndexEntry> = Vec::new();
    let mut packs: Vec<sc_comply::evidence::EvidencePack> = Vec::new();

    for shipped in sc_comply::registry::SHIPPED {
        let pack = sc_comply::registry::load_shipped(shipped.name)
            .map_err(|e| ComplyError::Failed(format!("{}: {e}", shipped.name)))?;
        let audited = sc_comply::engine::audit_with(
            workspace,
            &pack,
            &options,
            &registry,
            generated_at.clone(),
        )
        .map_err(|e| ComplyError::Failed(format!("{} failed: {e}", shipped.name)))?;

        // Redact HERE, once. Everything downstream sees only the public pack.
        let public = audited.redacted();
        let guidance = backend
            .as_ref()
            .map(|b| guidance_for(b, &public))
            .unwrap_or_default();

        let href = format!("{}.html", shipped.name);
        let html = sc_comply::report::site::framework_page_with_guidance(
            &public,
            Some("index.html"),
            &guidance,
        );
        std::fs::write(out_dir.join(&href), html)
            .map_err(|e| ComplyError::Failed(format!("writing {href}: {e}")))?;

        entries.push(sc_comply::report::site::IndexEntry {
            href,
            pack: public.clone(),
        });
        packs.push(public);
    }

    let rollup = sc_comply::rollup::roll_up(&packs);
    let project = workspace
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "this project".to_string());

    // The narrative is best-effort by design: a report without a summary is
    // complete, and failing the whole export over a model timeout would be a
    // far worse outcome than a missing paragraph.
    let narrative = backend
        .as_ref()
        .and_then(|b| narrative_for(b, &rollup, &project));

    let index = out_dir.join("index.html");
    std::fs::write(
        &index,
        sc_comply::report::site::index_page(&entries, &project, &rollup, narrative.as_deref()),
    )
    .map_err(|e| ComplyError::Failed(format!("writing index.html: {e}")))?;

    Ok(ComplyReport {
        index,
        frameworks: rollup.frameworks,
        controls: rollup.controls,
        passed: rollup.passed,
        gaps: rollup.gaps,
        unknown: rollup.unknown,
        narrated: narrative.is_some(),
    })
}

/// Auditor guidance for the controls a code scan could not settle.
///
/// The model is asked *what evidence would settle this*, never *is this
/// satisfied* — guidance carries no status and cannot change a verdict, which is
/// what makes a model safe to point at organizational controls at all.
fn guidance_for(
    backend: &dyn sc_model::ModelBackend,
    pack: &sc_comply::evidence::EvidencePack,
) -> Vec<sc_comply::report::site::ControlGuidance> {
    if sc_comply_author::worklist::undeterminable(pack).is_empty() {
        return Vec::new();
    }
    let mut drop_it = |_: &sc_comply_author::worklist::Rejection| {};
    sc_comply_author::worklist::generate(backend, pack, &mut drop_it)
        .unwrap_or_default()
        .into_iter()
        .map(|g| sc_comply::report::site::ControlGuidance {
            control_id: g.control_id,
            evidence: g.evidence,
            owner: g.owner,
            auditor_asks: g.auditor_asks,
        })
        .collect()
}

/// The executive summary, or `None` if the model declined or was rejected.
fn narrative_for(
    backend: &dyn sc_model::ModelBackend,
    rollup: &sc_comply::rollup::Rollup,
    project: &str,
) -> Option<String> {
    let mut drop_it = |_: &sc_comply_author::narrative::Rejection| {};
    sc_comply_author::narrative::generate(backend, rollup, project, &mut drop_it)
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(local: &str, gemini_key: Option<&str>) -> UiConfig {
        let mut cfg = UiConfig::default();
        cfg.local_conn.base_url = local.to_string();
        cfg.gemini_conn.key = gemini_key.map(str::to_string);
        cfg
    }

    #[test]
    fn none_is_the_default_and_builds_no_backend() {
        // The whole safety argument for the default: a menu click must not
        // silently spend API credits, and the report is complete without prose.
        assert_eq!(ComplyModel::default(), ComplyModel::None);
        let cfg = cfg_with("http://localhost:11435/v1", None);
        assert!(backend_for(&cfg, ComplyModel::None).expect("ok").is_none());
    }

    #[test]
    fn local_needs_no_key() {
        let cfg = cfg_with("http://localhost:11435/v1", None);
        assert!(backend_for(&cfg, ComplyModel::Local).expect("ok").is_some());
    }

    #[test]
    fn gemini_without_a_key_is_an_error_not_a_silent_downgrade() {
        // Falling back to "no summary" would look like the feature is broken.
        // The user asked for a summary; say why they are not getting one.
        let cfg = cfg_with("http://localhost:11435/v1", None);
        assert_eq!(
            backend_for(&cfg, ComplyModel::Gemini).err(),
            Some(ComplyError::MissingKey("Gemini"))
        );
    }

    #[test]
    fn gemini_with_a_key_builds_a_backend() {
        let cfg = cfg_with("http://localhost:11435/v1", Some("k-123"));
        assert!(backend_for(&cfg, ComplyModel::Gemini)
            .expect("ok")
            .is_some());
    }

    #[test]
    fn a_blank_gemini_key_counts_as_missing() {
        // A whitespace-only key would otherwise build a backend that 401s
        // halfway through a ten-framework audit.
        let cfg = cfg_with("http://localhost:11435/v1", Some("   "));
        assert_eq!(
            backend_for(&cfg, ComplyModel::Gemini).err(),
            Some(ComplyError::MissingKey("Gemini"))
        );
    }

    #[test]
    fn local_is_offered_with_an_explicit_caveat() {
        // The user chose to offer Local rather than restrict to Gemini. That is
        // only honest if the UI says the output is unevaluated for this use.
        let c = ComplyModel::Local
            .caveat()
            .expect("Local must carry a caveat");
        assert!(c.contains("not evaluated"), "{c}");
        assert!(
            c.contains("Control results are unaffected"),
            "the caveat must scope itself to the prose, or it reads as doubt \
             about the audit itself: {c}"
        );
    }

    #[test]
    fn choosing_no_model_carries_no_caveat() {
        // Nothing to warn about: no prose is generated at all.
        assert!(ComplyModel::None.caveat().is_none());
    }

    #[test]
    fn the_local_model_comes_from_config_not_a_hardcoded_name() {
        let mut cfg = cfg_with("http://localhost:11435/v1", None);
        cfg.model = "my-own-model".into();
        assert_eq!(model_for(&cfg, Provider::Local), "my-own-model");
    }

    #[test]
    fn an_absent_workspace_is_reported_before_any_model_is_built() {
        // Ordering matters: a user with no folder open should get "open a
        // folder", not "Gemini has no API key".
        let cfg = cfg_with("http://localhost:11435/v1", None);
        let missing = Path::new("./definitely-not-a-real-directory-xyz");
        assert_eq!(
            run(missing, Path::new("."), ComplyModel::Gemini, &cfg).err(),
            Some(ComplyError::NoWorkspace)
        );
    }

    #[test]
    fn every_choice_is_listed_in_display_order() {
        assert_eq!(ComplyModel::ALL.len(), 3);
        assert_eq!(ComplyModel::ALL[0], ComplyModel::None);
    }

    /// The deterministic path produces a full report with no model at all.
    #[test]
    fn a_report_generates_with_no_model_configured() {
        let tmp = std::env::temp_dir().join("sc-win-comply-test-none");
        let _ = std::fs::remove_dir_all(&tmp);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf();

        let cfg = cfg_with("http://localhost:11435/v1", None);
        let report = run(&ws, &tmp, ComplyModel::None, &cfg).expect("audit runs");

        assert!(report.index.is_file(), "index page written");
        assert_eq!(report.frameworks, sc_comply::registry::SHIPPED.len());
        assert!(report.controls > 0);
        assert!(!report.narrated, "no model was configured");

        // The published page must never carry a citation.
        let html = std::fs::read_to_string(&report.index).expect("readable");
        assert!(
            !html.contains("crates/sc-comply/src"),
            "leaked a source path"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
