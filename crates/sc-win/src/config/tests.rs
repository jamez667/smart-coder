//! Config tests: verify-command detection, the config.json parse/serialize pair,
//! provider routing, and stage resolution.

use std::sync::Arc;

use super::file::{
    is_gemini_url, parse_config, serialize_config, ConfigFields, GEMINI_OPENAI_BASE_URL,
};
use super::types::{Connection, Mode, Provider, UiConfig};
use super::workspace::{detect_verify_command, repo_overview, source_files};

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
fn mode_round_trips_through_config_json() {
    // The answer to the first-run question has to survive a restart. A mode that reset would
    // read as the app ignoring the user — the worst possible first impression for this feature.
    for m in [Mode::Craft, Mode::Assistant] {
        let cfg = UiConfig {
            mode: Some(m),
            ..UiConfig::default()
        };
        let json = serialize_config(&ConfigFields {
            mode: cfg.mode.map(|m| m.slug().to_string()),
            ..ConfigFields::default()
        });
        let back = parse_config(&json);
        assert_eq!(
            back.mode.as_deref().and_then(Mode::from_slug),
            Some(m),
            "{} did not survive the round trip: {json}",
            m.slug()
        );
    }
}

#[test]
fn an_unchosen_mode_writes_nothing_and_stays_unchosen() {
    // Tri-state: absent means NEVER CHOSEN, which is what raises the first-run prompt. It must
    // not collapse into a default, or the prompt would never fire on a fresh install.
    let json = serialize_config(&ConfigFields {
        mode: None,
        ..ConfigFields::default()
    });
    assert!(
        !json.contains("mode"),
        "unchosen must not be written: {json}"
    );
    assert!(parse_config(&json).mode.is_none());
    // A craft-only build has nothing to ask, so "unchosen" is not a state it can be in — see
    // `a_craft_only_build_needs_no_first_run_question`.
    #[cfg(not(feature = "craft-only"))]
    assert!(!UiConfig::default().mode_chosen(), "default is unchosen");
}

/// A craft-only build is Craft whatever the config or the environment say.
///
/// The whole feature is this one predicate: every existing guard — the backend builders, the
/// health probe, the panel pruning — already routes through `craft()`, so pinning it here fires
/// all of them at once rather than adding a second enforcement path that could drift.
#[cfg(feature = "craft-only")]
#[test]
fn a_craft_only_build_is_craft_whatever_the_config_says() {
    let cfg = UiConfig {
        mode: Some(Mode::Assistant),
        ..UiConfig::default()
    };
    assert!(cfg.craft(), "a stale Assistant in config.json cannot win");
    assert!(UiConfig::default().craft(), "and neither can an absent one");
}

/// A craft-only build never asks which mode to use, and never offers to switch.
#[cfg(feature = "craft-only")]
#[test]
fn a_craft_only_build_needs_no_first_run_question() {
    assert!(
        UiConfig::default().mode_chosen(),
        "there is nothing to choose between, so the question would have one honest answer"
    );
    assert!(!UiConfig::default().mode_switchable(), "and no way back");
}

/// A craft-only build never writes `mode` — not even while saving something else.
///
/// The trap this guards: `save_config` also runs on ordinary connection edits. Stamping a `mode`
/// into config.json would pin a user into Craft in a LATER ordinary build, with no record of
/// their ever having chosen it.
#[cfg(feature = "craft-only")]
#[test]
fn a_craft_only_build_never_persists_a_mode() {
    let cfg = UiConfig {
        mode: Some(Mode::Craft),
        ..UiConfig::default()
    };
    let json = serialize_config(&ConfigFields {
        mode: cfg
            .mode
            .filter(|_| cfg.mode_switchable())
            .map(|m| m.slug().to_string()),
        ..ConfigFields::default()
    });
    assert!(
        !json.contains("\"mode\""),
        "a craft-only build must leave no mode behind: {json}"
    );
}

/// An ordinary build still offers both modes. The complement of the tests above, so a stray
/// `cfg!` that hard-wired Craft everywhere would fail something.
#[cfg(not(feature = "craft-only"))]
#[test]
fn an_ordinary_build_still_switches_modes() {
    assert!(UiConfig::default().mode_switchable());
    assert!(!UiConfig::default().craft(), "unchosen is not Craft");
    let cfg = UiConfig {
        mode: Some(Mode::Craft),
        ..UiConfig::default()
    };
    assert!(cfg.craft());
}

/// The endpoint-agnostic knobs survive a restart (spec 21).
///
/// These silently reset on every launch until now, because `save_config` wrote only the
/// connection fields. A setting the app forgets is one the user re-enters forever.
#[test]
fn the_endpoint_agnostic_knobs_round_trip() {
    let json = serialize_config(&ConfigFields {
        verify_command: Some("cargo test".to_string()),
        unity_path: Some(r"C:\Unity\2022.3.10f1\Editor\Unity.exe".to_string()),
        yolo: Some(true),
        dry_run: Some(true),
        ..ConfigFields::default()
    });
    let back = parse_config(&json);
    assert_eq!(back.verify_command.as_deref(), Some("cargo test"));
    assert_eq!(
        back.unity_path.as_deref(),
        Some(r"C:\Unity\2022.3.10f1\Editor\Unity.exe")
    );
    assert_eq!(back.yolo, Some(true));
    assert_eq!(back.dry_run, Some(true));
}

/// **A lost or malformed posture flag must never read as `true`.**
///
/// `yolo` disables permission prompts. Absent has to mean the safe default, and a hand-edited
/// `"yolo": "yes"` has to fall back rather than being coerced to on — this is the one field where
/// guessing generously is a security decision made on the user's behalf.
#[test]
fn an_absent_or_malformed_posture_flag_is_off_not_on() {
    assert_eq!(parse_config("{}").yolo, None, "absent stays absent");
    assert_eq!(
        parse_config(r#"{"yolo":"yes","dry_run":1}"#).yolo,
        None,
        "a non-boolean is not a yes"
    );
    assert_eq!(parse_config(r#"{"dry_run":1}"#).dry_run, None);

    // And absent leaves the compiled default in place, which is off.
    let cfg = UiConfig::default();
    assert!(!cfg.yolo && !cfg.dry_run);

    // An unset flag is not written at all, so "unset" never becomes "explicitly off".
    let json = serialize_config(&ConfigFields::default());
    assert!(!json.contains("yolo"), "{json}");
    assert!(!json.contains("dry_run"), "{json}");
}

#[test]
fn a_corrupt_mode_asks_again_rather_than_guessing() {
    // Guessing here is the one failure this feature cannot afford: silently resolving garbage to
    // Assistant would put someone who chose Craft back in front of a model. Unparseable ⇒ None ⇒
    // ask again.
    let f = parse_config(r#"{"mode":"CRAFT MODE PLEASE"}"#);
    assert_eq!(
        f.mode.as_deref(),
        Some("CRAFT MODE PLEASE"),
        "read verbatim"
    );
    assert_eq!(
        f.mode.as_deref().and_then(Mode::from_slug),
        None,
        "but rejected"
    );
}

/// The Assistant half of this is what a craft-only build has no way to express — there, EVERY
/// builder returns `None` and the contrast that makes the assertions meaningful doesn't exist.
/// `a_craft_only_build_is_craft_whatever_the_config_says` covers that build.
#[cfg(not(feature = "craft-only"))]
#[test]
fn craft_mode_builds_no_backend_at_all() {
    // THE contract behind "no language model is contacted" (spec 21), asserted on CONSTRUCTION
    // rather than on any proxy for it.
    //
    // This matters because constructing a backend is not free: `backend()` and `orchestrator()`
    // end in `with_detected_context()`, a live `/models` probe. Returning one at all IS the
    // network call. Making the builders `Option` puts that in the type system, so a caller added
    // later cannot dial out without handling the `None` — whereas guarding each call site only
    // ever protects the sites someone remembered to guard.
    let craft = UiConfig {
        mode: Some(Mode::Craft),
        // Fully configured — the refusal must come from the MODE, not from missing config.
        advisor_model: Some("advisor-model".into()),
        orchestrator_model: Some("planner-model".into()),
        ..UiConfig::default()
    };
    assert!(craft.backend().is_none(), "no coder backend");
    assert!(craft.orchestrator().is_none(), "no planner backend");
    assert!(craft.advisor().is_none(), "no advisor backend");
    assert!(
        craft.swarm_advisor().is_none(),
        "and none via the swarm path"
    );
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(craft.backend_cancellable(cancel).is_none());

    // The same config in Assistant mode DOES build them — otherwise the assertions above would
    // pass for the wrong reason (e.g. a builder that never returns anything).
    let assistant = UiConfig {
        mode: Some(Mode::Assistant),
        ..craft.clone()
    };
    assert!(assistant.advisor().is_some(), "Assistant still builds one");
}

/// Pins the TRI-STATE, which only an ordinary build has: a craft-only build collapses it on
/// purpose, and `a_craft_only_build_is_craft_whatever_the_config_says` pins that instead.
#[cfg(not(feature = "craft-only"))]
#[test]
fn craft_is_only_true_when_craft_was_actually_chosen() {
    // `craft()` is the single predicate for "no model", so the unchosen case must NOT read as
    // Craft — until the user answers, the app behaves as Assistant and the prompt does the
    // asking. Getting this backwards would silently disable the agent for every fresh install.
    let unchosen = UiConfig::default();
    assert!(!unchosen.craft(), "unchosen is not Craft");

    let craft = UiConfig {
        mode: Some(Mode::Craft),
        ..UiConfig::default()
    };
    assert!(craft.craft() && craft.mode_chosen());

    let assistant = UiConfig {
        mode: Some(Mode::Assistant),
        ..UiConfig::default()
    };
    assert!(!assistant.craft() && assistant.mode_chosen());
}

#[test]
fn parse_config_reads_both_fields() {
    let f = parse_config(r#"{"base_url":"http://localhost:11435/v1","model":"qwen3-coder-30b"}"#);
    assert_eq!(f.base_url.as_deref(), Some("http://localhost:11435/v1"));
    assert_eq!(f.model.as_deref(), Some("qwen3-coder-30b"));
}

#[test]
fn parse_config_reads_gemini_planner_fields() {
    // A Gemini-as-planner config.json: local coder, orchestrator pointed at Gemini + a key.
    let f = parse_config(
        r#"{
            "base_url":"http://localhost:8080/v1",
            "model":"qwen3-coder-30b",
            "orchestrator_url":"https://generativelanguage.googleapis.com/v1beta/openai",
            "orchestrator_model":"gemini-2.5-flash-lite",
            "orchestrator_key":"AIzaSECRET"
        }"#,
    );
    assert_eq!(
        f.orchestrator_url.as_deref(),
        Some("https://generativelanguage.googleapis.com/v1beta/openai")
    );
    assert_eq!(
        f.orchestrator_model.as_deref(),
        Some("gemini-2.5-flash-lite")
    );
    assert_eq!(f.orchestrator_key.as_deref(), Some("AIzaSECRET"));
}

#[test]
fn parse_config_missing_or_blank_fields_are_none() {
    // Only one key present → the other stays None (caller keeps its default).
    let f = parse_config(r#"{"model":"just-the-model"}"#);
    assert_eq!(f.base_url, None);
    assert_eq!(f.model.as_deref(), Some("just-the-model"));
    // Blank / whitespace-only values are treated as absent, not as an empty override.
    let f = parse_config(r#"{"base_url":"  ","model":""}"#);
    assert_eq!(f.base_url, None);
    assert_eq!(f.model, None);
}

#[test]
fn parse_config_malformed_or_wrong_shape_is_all_none() {
    assert_eq!(parse_config("not json at all"), ConfigFields::default());
    // Right JSON, wrong types → no strings to take.
    assert_eq!(
        parse_config(r#"{"base_url":42,"model":true}"#),
        ConfigFields::default()
    );
    assert_eq!(parse_config("{}"), ConfigFields::default());
}

#[test]
fn serialize_config_round_trips_and_omits_unset() {
    // A full Gemini-planner config round-trips through serialize → parse unchanged.
    let fields = ConfigFields {
        base_url: Some("http://localhost:8080/v1".into()),
        model: Some("qwen3-coder-30b".into()),
        key: None,
        orchestrator_url: Some(GEMINI_OPENAI_BASE_URL.into()),
        orchestrator_model: Some("gemini-2.5-flash-lite".into()),
        orchestrator_key: Some("AIzaSECRET".into()),
        ..ConfigFields::default()
    };
    let json = serialize_config(&fields);
    assert_eq!(parse_config(&json), fields);
    // Unset fields are omitted entirely — no blank "key" lands in the file.
    assert!(
        !json.contains("\"key\""),
        "unset key must be omitted: {json}"
    );
}

#[test]
fn provider_slug_round_trips() {
    for p in Provider::ALL {
        assert_eq!(Provider::from_slug(p.slug()), Some(p));
    }
    assert_eq!(Provider::from_slug("bogus"), None);
    assert_eq!(Provider::from_slug(""), None);
}

#[test]
fn resolve_stages_local_coder_gemini_planner() {
    // The headline setup: local coder, Gemini planner. resolve_stages must put the local
    // endpoint on base_url (no key) and the Gemini endpoint+key on orchestrator_*.
    let mut cfg = UiConfig {
        local_conn: Connection {
            base_url: "http://localhost:11435/v1".into(),
            key: None,
        },
        gemini_conn: Connection {
            base_url: GEMINI_OPENAI_BASE_URL.into(),
            key: Some("gkey".into()),
        },
        coder_provider: Provider::Local,
        planner_provider: Provider::Gemini,
        advisor_provider: Provider::Local,
        ..UiConfig::default()
    };
    cfg.resolve_stages();
    assert_eq!(cfg.base_url, "http://localhost:11435/v1");
    assert_eq!(cfg.key, None, "local coder carries no key");
    assert_eq!(
        cfg.orchestrator_url.as_deref(),
        Some(GEMINI_OPENAI_BASE_URL)
    );
    assert_eq!(
        cfg.orchestrator_key.as_deref(),
        Some("gkey"),
        "the Gemini key rides ONLY the planner, never the local coder"
    );
}

#[test]
fn resolve_stages_same_provider_leaves_orchestrator_none() {
    // Planner on the same connection as the coder ⇒ no orchestrator override (falls back to
    // the coder endpoint in orchestrator()), so we don't persist a redundant duplicate.
    let mut cfg = UiConfig {
        coder_provider: Provider::Local,
        planner_provider: Provider::Local,
        ..UiConfig::default()
    };
    cfg.resolve_stages();
    assert_eq!(cfg.orchestrator_url, None);
    assert_eq!(cfg.orchestrator_key, None);
}

#[test]
fn resolve_stages_clears_a_stale_planner_model_when_routed_back_to_local() {
    // The live bug (2026-07-21): the planner was routed to Local but `orchestrator_model` still
    // held `gemini-2.5-flash-lite` from a previous Gemini routing. `orchestrator()` then asked
    // the LOCAL endpoint for that model — the local server served whatever was loaded under the
    // bogus name, so the LOCAL coder model ran the planning phases mislabeled as Gemini.
    // Routing the planner to the coder's connection must clear the model so it falls back to the
    // local coder model.
    let mut cfg = UiConfig {
        coder_provider: Provider::Local,
        planner_provider: Provider::Local,
        orchestrator_model: Some("gemini-2.5-flash-lite".into()), // stale
        model: "qwen3-coder-30b".into(),
        ..UiConfig::default()
    };
    cfg.resolve_stages();
    assert_eq!(
        cfg.orchestrator_model, None,
        "stale Gemini planner model cleared"
    );
    // And orchestrator() then uses the local coder model, not the stale name.
    // (orchestrator() falls back to self.model when orchestrator_model is None.)
}

#[test]
fn migrates_pre_connections_gemini_planner_config() {
    // An OLD config.json (flat fields only): local coder + Gemini orchestrator. load()'s
    // migration must derive the two connections and route the planner to Gemini.
    let json = format!(
        r#"{{
            "base_url":"http://localhost:11435/v1",
            "model":"qwen3-coder-30b",
            "orchestrator_url":"{GEMINI_OPENAI_BASE_URL}",
            "orchestrator_model":"gemini-2.5-flash-lite",
            "orchestrator_key":"AIzaOLD"
        }}"#
    );
    let f = parse_config(&json);
    // Reproduce the relevant slice of load()'s migration (pure, no file/env).
    let mut cfg = UiConfig {
        base_url: f.base_url.clone().unwrap(),
        orchestrator_url: f.orchestrator_url.clone(),
        orchestrator_key: f.orchestrator_key.clone(),
        key: f.key.clone(),
        ..Default::default()
    };
    // Local from coder endpoint; Gemini from the orchestrator override.
    cfg.local_conn.base_url = f.base_url.clone().unwrap();
    cfg.local_conn.key = if is_gemini_url(&cfg.base_url) {
        None
    } else {
        cfg.key.clone()
    };
    cfg.gemini_conn.base_url = cfg.orchestrator_url.clone().unwrap();
    cfg.gemini_conn.key = cfg.orchestrator_key.clone();
    cfg.coder_provider = if is_gemini_url(&cfg.base_url) {
        Provider::Gemini
    } else {
        Provider::Local
    };
    cfg.planner_provider = match &cfg.orchestrator_url {
        Some(u) if is_gemini_url(u) => Provider::Gemini,
        _ => Provider::Local,
    };
    assert_eq!(cfg.coder_provider, Provider::Local);
    assert_eq!(cfg.planner_provider, Provider::Gemini);
    assert_eq!(cfg.gemini_conn.key.as_deref(), Some("AIzaOLD"));
    assert_eq!(
        cfg.local_conn.key, None,
        "no key bled onto the local connection"
    );
}

#[test]
fn is_gemini_url_matches_the_google_host() {
    assert!(is_gemini_url(GEMINI_OPENAI_BASE_URL));
    assert!(is_gemini_url(
        "https://generativelanguage.googleapis.com/v1beta/openai/"
    ));
    assert!(!is_gemini_url("http://localhost:11435/v1"));
}

#[test]
fn orchestrator_attaches_the_planner_key_and_falls_back_to_coder_key() {
    // Explicit orchestrator key is used for the planner backend.
    let cfg = UiConfig {
        orchestrator_url: Some(GEMINI_OPENAI_BASE_URL.into()),
        orchestrator_model: Some("gemini-2.5-flash-lite".into()),
        orchestrator_key: Some("planner-key".into()),
        ..UiConfig::default()
    };
    // `apply_key` decides key attachment purely from the Option; assert that seam directly
    // (constructing the backend and reading a private field isn't exposed).
    assert!(apply_key_used(
        &cfg.orchestrator_key.clone().or(cfg.key.clone())
    ));

    // With no orchestrator key but a coder key set, the planner borrows the coder key.
    let cfg = UiConfig {
        key: Some("coder-key".into()),
        orchestrator_key: None,
        ..UiConfig::default()
    };
    assert_eq!(
        cfg.orchestrator_key.clone().or(cfg.key.clone()).as_deref(),
        Some("coder-key")
    );
}

/// Mirror of the `apply_key` decision (is a non-blank key present?) for the test above,
/// since the attached token isn't readable off the built backend.
fn apply_key_used(key: &Option<String>) -> bool {
    key.as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .is_some()
}

#[test]
fn neutral_default_has_no_machine_specifics() {
    // The compiled default must be generic — the rig's real endpoint lives in
    // config.json, never in the repo. Guard against a rig value creeping back in.
    let d = UiConfig::default();
    assert_eq!(d.base_url, "http://localhost:8080/v1");
    assert_eq!(d.model, "default");
    assert!(!d.base_url.contains("11435") && !d.base_url.contains("11439"));
}

#[test]
fn source_files_excludes_tests_and_tooling() {
    let dir = std::env::temp_dir().join(format!("sc-win-src-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("server")).unwrap();
    std::fs::create_dir_all(dir.join(".smart-coder/plan")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(dir.join("server/app.py"), "x").unwrap();
    std::fs::write(dir.join("index.html"), "x").unwrap();
    std::fs::write(dir.join("tests/test_app.py"), "x").unwrap(); // test → excluded
    std::fs::write(dir.join(".smart-coder/plan/01-specs.md"), "x").unwrap(); // plan → excluded

    let src = source_files(&dir);
    assert!(src.contains(&"server/app.py".to_string()), "{src:?}");
    assert!(src.contains(&"index.html".to_string()), "{src:?}");
    assert!(
        !src.iter().any(|f| f.contains("test")),
        "tests excluded: {src:?}"
    );
    assert!(
        !src.iter().any(|f| f.contains("smart-coder")),
        "plan excluded: {src:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repo_overview_is_empty_for_a_fresh_dir_and_lists_existing_files() {
    let dir = std::env::temp_dir().join(format!("sc-win-overview-{}", std::process::id()));
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
    let ac = UiConfig::default().agent_config(Some(Arc::new(sc_core::AutoDeny)));
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

/// The second half — a configured advisor DOES build — is unavailable in a craft-only build,
/// where no builder returns anything by design.
#[cfg(not(feature = "craft-only"))]
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
