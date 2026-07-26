//! The compliance subcommands (specs 13/14/15): audit-and-serve, pack linting,
//! the drafting eval, and the static redacted export.
//!
//! Only `comply-eval` and the optional narrative/guidance touch a model. The audit
//! itself is deterministic on purpose — an evidence pack has to be reproducible
//! and citable, and a model that could change a status would destroy that.

use std::process::ExitCode;

use sc_cli::Cli;

use super::common::workspace;

/// Gemini's OpenAI-compatible endpoint.
const GEMINI_OPENAI_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

/// The canonical repository URL, for links out of the published site.
const REPO_URL: &str = "https://github.com/jamez667/smart-coder";

/// Audit the current directory against a compliance framework pack and serve the
/// evidence pack as a local dashboard (spec 13).
///
/// No model backend is involved: the built-in collectors are deterministic, which
/// is the point — an evidence pack has to be reproducible and citable.
pub fn comply_task(cli: &Cli, pack_arg: Option<String>) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
    };

    // With --pack, show that one framework. Without, offer ALL shipped packs:
    // the interesting question is usually not "how do we score against SOC 2"
    // but "where do our frameworks overlap, and what is genuinely missing".
    //
    // Parse and validate before binding a port: a malformed pack must fail here,
    // not halfway through an audit whose output someone will sign.
    let frameworks = match build_frameworks(pack_arg) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let framework = frameworks
        .first()
        .map(|f| f.pack.framework.name.clone())
        .unwrap_or_default();
    let controls: usize = frameworks.iter().map(|f| f.pack.controls.len()).sum();
    let count = frameworks.len();
    let spec = sc_web::ComplyRun {
        workspace: workspace.clone(),
        frameworks,
        options: sc_comply::collector::ComplyOptions::default(),
    };

    // An empty token tells the server auth is off. mint_token() never returns
    // empty, so this can only happen via the explicit flag.
    let token = if cli.no_token {
        String::new()
    } else {
        sc_web::mint_token()
    };
    let addr = format!("127.0.0.1:{}", cli.port);
    let result = sc_web::serve_comply(spec, &addr, &token, |url| {
        if count > 1 {
            println!("sc-comply — {count} frameworks, {controls} controls total");
        } else {
            println!("sc-comply — {framework} ({controls} controls)");
        }
        println!("workspace: {}", workspace.display());
        if token.is_empty() {
            println!("evidence pack live at {url}/");
            println!("(--no-token: no URL secret. Bound to 127.0.0.1 only.)");
        } else {
            println!("evidence pack live at {url}/?k={token}");
        }
        println!("command checks are DISABLED by default; review the pack before enabling them.");
        if token.is_empty() {
            // Tailscale would expose an unauthenticated page to the tailnet.
            println!("do NOT `tailscale serve` this run — it has no token; restart without --no-token first.");
        } else {
            println!(
                "to reach it from your phone: run `tailscale serve {}` and open the",
                cli.port
            );
            println!("printed https URL with ?k={token} on the phone (same tailnet).");
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: compliance server failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Critique a compliance pack's own authoring (spec 14).
///
/// Deterministic and model-free: this is the half of the authoring assistant
/// that needs no API key. It exits non-zero on a blocking finding so it can be
/// wired into a check gate.
pub fn comply_lint(pack_arg: Option<String>) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
    };

    let pack = match resolve_pack(pack_arg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The current directory doubles as the sample workspace: the file-dependent
    // lints need real files to test globs and paths against.
    let sample = sc_comply_author::Sample::load(&workspace);
    let report = sc_comply_author::lint_pack(&pack, Some(&sample));

    print!("{}", sc_comply_author::report::markdown(&report));

    let blocking = report.blocking().len();
    if blocking > 0 {
        eprintln!("\n{blocking} blocking finding(s) — the pack needs work before use.");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Run the compliance drafting eval across one or more models (spec 15).
///
/// Unlike every other subcommand here this one spends real tokens on purpose, so
/// it prints the call budget up front and reports progress per control — a
/// twelve-control run against a slow local model takes minutes.
pub fn comply_eval(cli: &Cli, model_specs: Vec<String>) -> ExitCode {
    if model_specs.is_empty() {
        eprintln!(
            "error: comply-eval needs at least one --author-model, e.g.\n  \
             --author-model gemini-pro-latest@https://generativelanguage.googleapis.com/v1beta/openai\n  \
             --author-model qwen3-coder-30b@http://localhost:11435/v1"
        );
        return ExitCode::FAILURE;
    }

    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
    };

    let suite_path = workspace.join("crates/sc-comply-author/evals/controls.toml");
    let suite = match sc_comply_author::eval::EvalSuite::load(&suite_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The repo itself is the sample workspace, so glob-reachability lints have
    // real files to test against.
    let sample = sc_comply_author::Sample::load(&workspace);

    eprintln!(
        "compliance drafting eval — {} controls × {} model(s) = {}+ calls",
        suite.controls.len(),
        model_specs.len(),
        suite.controls.len() * model_specs.len()
    );

    let mut scores = Vec::new();
    for spec in &model_specs {
        let (model, url) = match spec.split_once('@') {
            Some((m, u)) => (m.to_string(), u.to_string()),
            None => (spec.clone(), cli.base_url.clone()),
        };

        // Deliberately NOT chaining with_detected_context(): it probes for
        // llama.cpp's meta.n_ctx, which a hosted provider does not serve, and
        // silently leaves the backend at the 8192 default.
        let mut backend = sc_model::OpenAiBackend::new(&url, &model).with_context_tokens(128_000);
        if let Some(k) = cli
            .api_key
            .clone()
            .or_else(|| std::env::var("GEMINI_API_KEY").ok())
        {
            if !k.trim().is_empty() {
                backend = backend.with_api_key(k);
            }
        }

        eprintln!("\n=== {model} ({url}) ===");
        let mut progress = |i: usize, n: usize, id: &str| {
            eprintln!("  [{i}/{n}] {id}");
        };
        match sc_comply_author::run_suite(&backend, &model, &suite, Some(&sample), &mut progress) {
            Ok(s) => {
                eprintln!(
                    "  -> {} dishonest, {:.0}%",
                    s.dishonest_count(),
                    s.total() * 100.0
                );
                scores.push(s);
            }
            Err(e) => {
                eprintln!("error: {model} failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    print!(
        "{}",
        if scores.len() > 1 {
            sc_comply_author::eval::report::comparison(&suite, &scores)
        } else {
            sc_comply_author::eval::report::markdown(&suite, &scores[0])
        }
    );

    // Any dishonest draft fails the run: that is the property being measured.
    let dishonest: usize = scores.iter().map(|s| s.dishonest_count()).sum();
    if dishonest > 0 {
        eprintln!("\n{dishonest} dishonest draft(s) across all models.");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Audit every shipped framework and write a static, redacted HTML site.
///
/// Redaction happens once, here, immediately after each audit — the pack that
/// reaches the renderer has already had its citations removed, and the renderer
/// asserts that independently. Nothing downstream has to remember.
pub fn comply_export(out_arg: Option<String>) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
    };

    let out_dir = workspace.join(out_arg.unwrap_or_else(|| "docs/compliance".to_string()));
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("error: cannot create {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }

    let options = sc_comply::collector::ComplyOptions::default();
    let generated_at = sc_comply::evidence::now_rfc3339();
    let mut entries: Vec<sc_comply::report::site::IndexEntry> = Vec::new();

    eprintln!(
        "auditing {} frameworks -> {}",
        sc_comply::registry::SHIPPED.len(),
        out_dir.display()
    );

    for shipped in sc_comply::registry::SHIPPED {
        let pack = match sc_comply::registry::load_shipped(shipped.name) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        let audited = match sc_comply::engine::audit_with(
            &workspace,
            &pack,
            &options,
            &sc_comply::collector::Registry::builtin(),
            generated_at.clone(),
        ) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("error: {} failed: {e}", shipped.name);
                return ExitCode::FAILURE;
            }
        };

        // Redact HERE, once. Everything downstream sees only the public pack.
        let public = audited.redacted();
        eprintln!(
            "  {:14} {} pass · {} gap · {} unknown",
            shipped.name, public.score.passed, public.score.gaps, public.score.unknown
        );

        // Auditor guidance for the controls a code scan cannot settle. Optional
        // and best-effort: guidance never changes a status, so its absence
        // costs a worklist, not correctness.
        let guidance = auditor_guidance(&public);

        let href = format!("{}.html", shipped.name);
        let html = sc_comply::report::site::framework_page_with_guidance(
            &public,
            Some("index.html"),
            &guidance,
        );
        if let Err(e) = std::fs::write(out_dir.join(&href), html) {
            eprintln!("error: writing {href}: {e}");
            return ExitCode::FAILURE;
        }
        entries.push(sc_comply::report::site::IndexEntry { href, pack: public });
    }

    // Cross-framework analysis, computed deterministically. This is what makes
    // an executive summary possible: a finding appearing in six of ten
    // frameworks is one fix with six times the leverage, and no per-framework
    // page can show that.
    let packs: Vec<sc_comply::evidence::EvidencePack> =
        entries.iter().map(|e| e.pack.clone()).collect();
    let rollup = sc_comply::rollup::roll_up(&packs);

    let project = workspace
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "this project".to_string());

    // The narrative is OPTIONAL. Without a configured model there is no
    // narrative and no error — the deterministic summary is complete on its own,
    // and most people running this will not have a key.
    let narrative = exec_narrative(&rollup, &project);

    let index =
        sc_comply::report::site::index_page(&entries, &project, &rollup, narrative.as_deref());
    if let Err(e) = std::fs::write(out_dir.join("index.html"), index) {
        eprintln!("error: writing index.html: {e}");
        return ExitCode::FAILURE;
    }

    let mut written = entries.len() + 1;

    // GitHub Pages serves from a directory ROOT, so when the output lands under
    // `docs/` that root needs its own landing page — without one a visitor gets
    // a 404 or a bare directory listing. Only written when the parent actually
    // looks like the docs tree, so `--out somewhere-else` does not scatter files.
    if let Some(site_root) = out_dir.parent() {
        if site_root.join("specs").is_dir() {
            let landing = sc_comply::report::site::landing_page(REPO_URL, &spec_links(site_root));
            match std::fs::write(site_root.join("index.html"), landing) {
                Ok(()) => {
                    written += 1;
                    println!("wrote landing page to {}", site_root.display());
                }
                Err(e) => eprintln!("warning: could not write the docs landing page: {e}"),
            }
            // Jekyll is disabled site-wide, so .nojekyll belongs at the SITE
            // root; a nested one has no effect. Move it if an older run left one.
            if let Err(e) = std::fs::write(site_root.join(".nojekyll"), "") {
                eprintln!("warning: could not write .nojekyll: {e}");
            }
            let _ = std::fs::remove_file(out_dir.join(".nojekyll"));
        } else if let Err(e) = std::fs::write(out_dir.join(".nojekyll"), "") {
            eprintln!("warning: could not write .nojekyll: {e}");
        }
    }

    println!("\nwrote {written} page(s) to {}", out_dir.display());
    println!("citations, file paths and excerpts are REDACTED from every page.");
    println!("review the output before committing, then enable GitHub Pages on docs/.");
    ExitCode::SUCCESS
}

/// Generate the executive summary, or `None` if no model is configured or the
/// output cannot be trusted.
///
/// Deliberately best-effort. An export that failed because a summary could not
/// be written would be a worse outcome than a page without one, and the page is
/// designed to stand alone. Every skip reason is printed so the operator knows
/// why the narrative is missing rather than wondering.
fn exec_narrative(rollup: &sc_comply::rollup::Rollup, project: &str) -> Option<String> {
    let (backend, model) = narrative_backend()?;
    eprintln!("writing the executive summary with {model} ...");

    let mut on_reject = |r: &sc_comply_author::narrative::Rejection| {
        eprintln!("  narrative rejected: {r} — publishing without it");
    };

    match sc_comply_author::narrative::generate(&backend, rollup, project, &mut on_reject) {
        Ok(Some(text)) => {
            eprintln!("  summary written ({} chars)", text.chars().count());
            Some(text)
        }
        Ok(None) => None,
        Err(e) => {
            eprintln!("  narrative unavailable ({e}) — publishing without it");
            None
        }
    }
}

/// Build the Gemini backend for authoring-time features, if a key is set.
///
/// Shared by the executive summary and the auditor worklist. Returns `None`
/// with no key — every feature built on it is optional by design.
fn narrative_backend() -> Option<(sc_model::OpenAiBackend, String)> {
    let key = std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())?;
    let model = std::env::var("SC_NARRATIVE_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "gemini-pro-latest".to_string());

    // NOT chaining with_detected_context(): it probes for llama.cpp's n_ctx,
    // which a hosted provider does not serve, silently capping context at 8192.
    let backend = sc_model::OpenAiBackend::new(GEMINI_OPENAI_URL, &model)
        .with_api_key(key)
        .with_context_tokens(128_000);
    Some((backend, model))
}

/// Auditor guidance for the controls a code scan could not settle.
///
/// The model is asked *what evidence would settle this*, never *is this
/// satisfied* — see `sc_comply_author::worklist`. Guidance carries no status and
/// cannot change a verdict, which is what makes this a safe use of a model on
/// organizational controls.
fn auditor_guidance(
    pack: &sc_comply::evidence::EvidencePack,
) -> Vec<sc_comply::report::site::ControlGuidance> {
    let unknowns = sc_comply_author::worklist::undeterminable(pack).len();
    if unknowns == 0 {
        return Vec::new();
    }
    let Some((backend, _)) = narrative_backend() else {
        return Vec::new();
    };

    let mut on_reject = |r: &sc_comply_author::worklist::Rejection| {
        eprintln!("    guidance rejected: {r}");
    };

    match sc_comply_author::worklist::generate(&backend, pack, &mut on_reject) {
        Ok(items) => {
            if !items.is_empty() {
                eprintln!(
                    "    guidance for {}/{unknowns} manual control(s)",
                    items.len()
                );
            }
            items
                .into_iter()
                .map(|g| sc_comply::report::site::ControlGuidance {
                    control_id: g.control_id,
                    evidence: g.evidence,
                    owner: g.owner,
                    auditor_asks: g.auditor_asks,
                })
                .collect()
        }
        Err(e) => {
            eprintln!("    guidance unavailable ({e})");
            Vec::new()
        }
    }
}

/// Build the spec list for the landing page by reading `docs/specs/`.
///
/// Links point at GitHub rather than at relative `.md` paths: Jekyll is disabled
/// on this site, so a relative link would serve raw Markdown as a download.
///
/// Titles and summaries come from each file's own H1 and first prose line, so a
/// new spec appears on the site without anyone remembering to register it here.
fn spec_links(site_root: &std::path::Path) -> Vec<sc_comply::report::site::SpecLink> {
    let Ok(entries) = std::fs::read_dir(site_root.join("specs")) else {
        return Vec::new();
    };

    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();

    files
        .iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let text = std::fs::read_to_string(path).ok()?;

            let title = text
                .lines()
                .find(|l| l.starts_with("# "))
                .map(|l| l.trim_start_matches("# ").trim().to_string())
                .unwrap_or_else(|| name.clone());

            // The first prose PARAGRAPH after the opening section heading — the
            // spec's own summary of itself. Joined across lines first, because
            // specs are hard-wrapped at ~80 columns and taking a single line
            // would cut most summaries mid-sentence.
            let para: String = text
                .lines()
                .skip_while(|l| !l.starts_with("## "))
                .skip(1)
                .skip_while(|l| l.trim().is_empty())
                .take_while(|l| !l.trim().is_empty() && !l.starts_with('#'))
                .map(|l| l.trim_start_matches('>').trim())
                .collect::<Vec<_>>()
                .join(" ");
            let summary = first_sentence(para.trim());

            Some(sc_comply::report::site::SpecLink {
                title,
                href: format!("{REPO_URL}/blob/main/docs/specs/{name}"),
                summary,
            })
        })
        .collect()
}

/// The first sentence of a line, with Markdown emphasis stripped.
fn first_sentence(line: &str) -> String {
    let plain = line.replace("**", "").replace('`', "");
    match plain.find(". ") {
        Some(i) => plain[..=i].to_string(),
        None => plain,
    }
}

/// Build the framework list the dashboard offers.
///
/// With an explicit `--pack`, just that one. Without, every shipped pack — a
/// user who has not named a framework usually wants to see the landscape.
fn build_frameworks(pack_arg: Option<String>) -> Result<Vec<sc_web::FrameworkEntry>, String> {
    if let Some(spec) = pack_arg {
        let name = sc_comply::registry::find(&spec)
            .map(|p| p.name.to_string())
            .unwrap_or_else(|| {
                // A user-authored path: name it after the file so the selector
                // and the ?framework= query still have something to key on.
                std::path::Path::new(&spec)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "custom".to_string())
            });
        let pack = resolve_pack(Some(spec))?;
        return Ok(vec![sc_web::FrameworkEntry { name, pack }]);
    }

    let mut out = Vec::with_capacity(sc_comply::registry::SHIPPED.len());
    for entry in sc_comply::registry::SHIPPED {
        let pack = sc_comply::registry::load_shipped(entry.name).map_err(|e| e.to_string())?;
        out.push(sc_web::FrameworkEntry {
            name: entry.name.to_string(),
            pack,
        });
    }
    Ok(out)
}

/// Resolve `--pack` to a loaded pack.
///
/// Accepts a shipped pack NAME (`soc2`, `iso27001`, …) or a filesystem path to a
/// pack the user authored. Name first: the shipped packs are embedded, so a name
/// works from any directory against any workspace, whereas a path only works
/// relative to where the user happens to be standing.
///
/// With no argument, defaults to SOC 2 — the most widely requested framework and
/// a reasonable starting point for someone who has not yet chosen.
fn resolve_pack(arg: Option<String>) -> Result<sc_comply::pack::Pack, String> {
    let Some(spec) = arg else {
        return sc_comply::registry::load_shipped("soc2").map_err(|e| e.to_string());
    };

    if sc_comply::registry::find(&spec).is_some() {
        return sc_comply::registry::load_shipped(&spec).map_err(|e| e.to_string());
    }

    let path = std::path::PathBuf::from(&spec);
    if path.is_file() {
        return sc_comply::pack::Pack::load(&path).map_err(|e| e.to_string());
    }

    Err(format!(
        "{spec:?} is neither a shipped pack name nor a readable file.\n\n{}",
        sc_comply::registry::listing()
    ))
}
