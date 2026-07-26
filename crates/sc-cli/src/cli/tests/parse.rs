//! Argv parsing: subcommands, flag positions, the run-tail peel, and loud errors.

use crate::{Cli, Command, ToolCallingArg, DEFAULT_BASE_URL, DEFAULT_MODEL};

#[test]
fn defaults_to_chat_with_default_backend() {
    let cli = Cli::parse(Vec::<String>::new()).unwrap();
    assert_eq!(cli.command, Command::Chat);
    assert_eq!(cli.base_url, DEFAULT_BASE_URL);
    assert_eq!(cli.model, DEFAULT_MODEL);
    assert_eq!(cli.tool_calling, ToolCallingArg::None);
}

#[test]
fn parses_run_with_task_verify_and_plan() {
    let cli = Cli::parse([
        "run",
        "make",
        "the",
        "test",
        "pass",
        "--verify",
        "sh test.sh",
        "--plan",
    ])
    .unwrap();
    match &cli.command {
        Command::Run { task } => assert_eq!(task, "make the test pass"),
        other => panic!("expected Run, got {other:?}"),
    }
    assert_eq!(cli.verify_command.as_deref(), Some("sh test.sh"));
    assert!(cli.plan_first);
    // The config reflects the flags.
    let cfg = cli.agent_config();
    assert_eq!(cfg.verify_command.as_deref(), Some("sh test.sh"));
    assert!(cfg.plan_first);
}

#[test]
fn run_requires_a_task() {
    assert!(Cli::parse(["run"]).is_err());
}

#[test]
fn parses_gemini_planner_flags() {
    // The Gemini-as-planner invocation: local coder, orchestrator pointed at Gemini + a key.
    let cli = Cli::parse([
        "run",
        "build a todo app",
        "--orchestrator",
        "gemini-2.5-flash-lite",
        "--orchestrator-url",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        "--orchestrator-key",
        "AIzaSECRET",
    ])
    .unwrap();
    assert_eq!(
        cli.orchestrator_model.as_deref(),
        Some("gemini-2.5-flash-lite")
    );
    assert_eq!(
        cli.orchestrator_url.as_deref(),
        Some("https://generativelanguage.googleapis.com/v1beta/openai")
    );
    assert_eq!(cli.orchestrator_key.as_deref(), Some("AIzaSECRET"));
    // The coder was NOT given a key on the command line, so it stays local (no accidental
    // key bleed) — unless the test environment happens to export GEMINI_API_KEY, which is a
    // legitimate fallback the parser honors.
    if std::env::var("GEMINI_API_KEY").is_err() {
        assert_eq!(cli.api_key, None);
    }
}

#[test]
fn coder_key_falls_through_to_the_planner_when_no_orchestrator_key() {
    // A single --key set on the coder also authenticates a same-provider planner.
    let cli = Cli::parse(["run", "task", "--key", "shared-key"]).unwrap();
    assert_eq!(cli.api_key.as_deref(), Some("shared-key"));
    assert_eq!(cli.orchestrator_key.as_deref(), Some("shared-key"));
}

#[test]
fn parses_swarm_with_orchestrator_and_workers() {
    let cli = Cli::parse([
        "swarm",
        "add validation",
        "--base-url",
        "http://localhost:11435/v1",
        "--model",
        "coder-0",
        "--orchestrator-url",
        "http://localhost:11434/v1",
        "--orchestrator",
        "advisor-e4b",
        "--max-workers",
        "3",
        "--max-retries",
        "4",
        "--frozen",
        "tests/test_a.py, tests\\test_b.py",
        "--verify",
        "pytest -q",
    ])
    .unwrap();
    match &cli.command {
        // The flags after the task must be peeled off, not swept into the goal
        // (regression: `--max-retries` once leaked into the task string).
        Command::Swarm { task } => assert_eq!(task, "add validation"),
        other => panic!("expected Swarm, got {other:?}"),
    }
    assert_eq!(cli.model, "coder-0"); // workers
    assert_eq!(cli.orchestrator_model.as_deref(), Some("advisor-e4b"));
    assert_eq!(
        cli.orchestrator_url.as_deref(),
        Some("http://localhost:11434/v1")
    );
    assert_eq!(cli.max_workers, 3);
    assert_eq!(cli.max_subtask_retries, 4);
    // `--frozen` parses + normalizes separators, and survives the task-peel.
    assert_eq!(cli.frozen_paths, vec!["tests/test_a.py", "tests/test_b.py"]);
    // The swarm config carries the verify command (gates integration) + workers.
    let sc = cli.swarm_config();
    assert_eq!(sc.max_workers, 3);
    assert_eq!(sc.max_subtask_retries, 4);
    assert_eq!(sc.frozen_paths, vec!["tests/test_a.py", "tests/test_b.py"]);
    assert_eq!(sc.verify_command.as_deref(), Some("pytest -q"));
}

#[test]
fn staged_subcommand_parses_task_and_verify() {
    // `staged` is the headless entry; the task must peel cleanly and
    // `--verify` (the per-stage gate override) must survive the peel.
    let cli = Cli::parse(["staged", "wire the invite list", "--verify", "cargo check"]).unwrap();
    match &cli.command {
        Command::Staged { task } => assert_eq!(task, "wire the invite list"),
        other => panic!("expected Staged, got {other:?}"),
    }
    assert_eq!(cli.verify_command.as_deref(), Some("cargo check"));
}

#[test]
fn parses_swarm_cli_and_json_flags() {
    // `--cli` in the swarm tail switches to the line renderer.
    let cli = Cli::parse(["swarm", "add a test", "--cli"]).unwrap();
    assert!(cli.cli, "--cli should set the line-render flag");
    assert!(!cli.json);

    // As a top-level flag too (flags may appear in any order, spec 00).
    let cli = Cli::parse(["--cli", "swarm", "add a test"]).unwrap();
    assert!(cli.cli);

    // `--json` is parsed independently; the `--json ⇒ cli` implication is
    // applied at the call site, not here, so a bare --json leaves cli false.
    let cli = Cli::parse(["swarm", "add a test", "--json"]).unwrap();
    assert!(cli.json);
    assert!(!cli.cli);
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
fn parses_json_log_yolo_allow_dry_run_top_level_and_in_run_tail() {
    // Top-level (before the subcommand).
    let top = Cli::parse([
        "--json",
        "--dry-run",
        "--yolo",
        "--allow",
        "cargo test",
        "--log",
        "out.jsonl",
        "run",
        "do it",
    ])
    .unwrap();
    assert!(top.json && top.dry_run && top.yolo);
    assert_eq!(top.allow, vec!["cargo test".to_string()]);
    assert_eq!(top.log.as_deref(), Some("out.jsonl"));

    // In the run tail (after the task) — and --allow repeats.
    let tail = Cli::parse([
        "run",
        "do it",
        "--json",
        "--dry-run",
        "--yolo",
        "--allow",
        "git status",
        "--allow",
        "ls",
        "--log",
        "x.jsonl",
    ])
    .unwrap();
    assert!(tail.json && tail.dry_run && tail.yolo);
    assert_eq!(tail.allow, vec!["git status".to_string(), "ls".to_string()]);
    assert_eq!(tail.log.as_deref(), Some("x.jsonl"));
    match &tail.command {
        Command::Run { task } => assert_eq!(task, "do it"),
        other => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn verbose_flag_parses_both_spellings_and_positions_and_wires_config() {
    for flag in ["--verbose", "-v"] {
        // Top-level and in the run tail.
        let top = Cli::parse([flag, "run", "x"]).unwrap();
        assert!(top.verbose, "top-level {flag}");
        let tail = Cli::parse(["run", "x", flag]).unwrap();
        assert!(tail.verbose, "run-tail {flag}");
        assert!(tail.agent_config().verbose, "{flag} → AgentConfig.verbose");
    }
    assert!(!Cli::parse(["run", "x"]).unwrap().verbose);
}

#[test]
fn parses_replay_and_requires_an_id() {
    let cli = Cli::parse(["replay", "1718000000000"]).unwrap();
    assert_eq!(
        cli.command,
        Command::Replay {
            session: "1718000000000".to_string()
        }
    );
    assert!(Cli::parse(["replay"]).is_err());
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
fn parses_comply_with_and_without_a_pack() {
    let bare = Cli::parse(["comply"]).unwrap();
    assert_eq!(bare.command, Command::Comply { pack: None });

    let with_pack = Cli::parse(["comply", "--pack", "packs/iso27001.toml"]).unwrap();
    assert_eq!(
        with_pack.command,
        Command::Comply {
            pack: Some("packs/iso27001.toml".to_string())
        }
    );
}

#[test]
fn comply_honours_the_port_flag() {
    let cli = Cli::parse(["comply", "--port", "9001"]).unwrap();
    assert_eq!(cli.command, Command::Comply { pack: None });
    assert_eq!(cli.port, 9001);
}

#[test]
fn pack_without_a_value_errors() {
    assert!(Cli::parse(["comply", "--pack"]).is_err());
}

#[test]
fn pack_outside_comply_is_rejected() {
    // Silently ignoring it would let `smart-coder run --pack x` look like it
    // did something.
    let err = Cli::parse(["doctor", "--pack", "x.toml"]).unwrap_err();
    assert!(format!("{err}").contains("only applies to"), "{err}");
}

#[test]
fn parses_comply_lint_with_and_without_a_pack() {
    let bare = Cli::parse(["comply-lint"]).unwrap();
    assert_eq!(bare.command, Command::ComplyLint { pack: None });

    let with_pack = Cli::parse(["comply-lint", "--pack", "packs/iso.toml"]).unwrap();
    assert_eq!(
        with_pack.command,
        Command::ComplyLint {
            pack: Some("packs/iso.toml".to_string())
        }
    );
}

#[test]
fn comply_and_comply_lint_are_distinct_subcommands() {
    // `comply` audits a codebase; `comply-lint` critiques the pack itself.
    // Conflating them would run an audit when a critique was asked for.
    assert_ne!(
        Cli::parse(["comply"]).unwrap().command,
        Cli::parse(["comply-lint"]).unwrap().command
    );
}

#[test]
fn no_token_is_off_by_default_and_opt_in() {
    // The security-relevant default. Auth must never disable itself.
    assert!(!Cli::parse(["comply"]).unwrap().no_token);
    assert!(Cli::parse(["comply", "--no-token"]).unwrap().no_token);
}

#[test]
fn parses_list_packs() {
    assert_eq!(
        Cli::parse(["--list-packs"]).unwrap().command,
        Command::ListPacks
    );
}

#[test]
fn pack_accepts_a_shipped_name_not_just_a_path() {
    // The interface that makes ten packs usable: `--pack iso27001` rather
    // than a path into the crate's internals.
    let cli = Cli::parse(["comply", "--pack", "iso27001"]).unwrap();
    assert_eq!(
        cli.command,
        Command::Comply {
            pack: Some("iso27001".to_string())
        }
    );
}

#[test]
fn comply_eval_collects_repeated_author_models() {
    // Repeatable is the whole point: one run, two models, side by side.
    let cli = Cli::parse([
        "comply-eval",
        "--author-model",
        "gemini-pro-latest@https://example.invalid/v1",
        "--author-model",
        "qwen3-coder-30b@http://localhost:11435/v1",
    ])
    .unwrap();
    match cli.command {
        Command::ComplyEval { models } => {
            assert_eq!(models.len(), 2);
            assert!(models[0].starts_with("gemini-pro-latest@"));
            assert!(models[1].starts_with("qwen3-coder-30b@"));
        }
        other => panic!("wrong command: {other:?}"),
    }
}

#[test]
fn comply_eval_accepts_a_bare_model_name() {
    let cli = Cli::parse(["comply-eval", "--author-model", "local-model"]).unwrap();
    assert_eq!(
        cli.command,
        Command::ComplyEval {
            models: vec!["local-model".to_string()]
        }
    );
}

#[test]
fn author_model_without_a_value_errors() {
    assert!(Cli::parse(["comply-eval", "--author-model"]).is_err());
}

#[test]
fn author_model_outside_comply_eval_is_rejected() {
    let err = Cli::parse(["doctor", "--author-model", "x"]).unwrap_err();
    assert!(
        format!("{err}").contains("only applies to `comply-eval`"),
        "{err}"
    );
}
